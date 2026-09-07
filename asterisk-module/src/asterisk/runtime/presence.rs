use super::presence_owner::PresenceCommand;
use super::{
    Access, BTreeSet, ButtonDefinition, CallState, DeviceId, DeviceState, DndMode, Instant,
};

pub use super::presence_owner::{StagedMwiSubscriptions, StagedRegistrationContexts};

pub fn publish_registration_contexts(
    access: &Access,
    config: std::sync::Arc<super::ModuleConfig>,
    registered: Vec<DeviceId>,
    device: DeviceId,
    lease: &crate::runtime::configuration_transaction::ConfigurationLease,
) -> Result<(), crate::pbx::registration::RegistrationRegistryError> {
    access
        .shared
        .presence
        .register_contexts(config, registered, device, lease)
}

pub fn retire_registration_contexts(access: &Access) {
    access.shared.presence.retire_registration_contexts();
}

pub fn publish_device_lines(access: &Access, device: &DeviceId) {
    let config = access.config();
    let mut lines = BTreeSet::new();
    for definition in config.device_definitions() {
        if &definition.id == device {
            for line in definition.lines() {
                lines.insert(line.number.clone());
            }
        }
    }
    lines.extend(
        access
            .shared
            .controller
            .snapshot()
            .mobility()
            .appearances_for_device(device)
            .map(|appearance| appearance.binding.line.number.clone()),
    );
    for line in lines {
        publish_line(access, &line);
    }
}

pub fn publish_line(access: &Access, line: &str) {
    access
        .shared
        .presence
        .enqueue(PresenceCommand::PublishLine {
            line: line.to_owned(),
            state: device_state(access, line),
        });
}

pub fn install_blf(access: &Access, device_id: &DeviceId) {
    let config = access.config();
    let Some(device) = config.devices.get(device_id) else {
        uninstall_device_blf(access, device_id);
        return;
    };
    let Some(generation) = access
        .shared
        .controller
        .snapshot()
        .registered_device(device_id)
        .map(|device| device.session_generation)
    else {
        return;
    };
    let plan = device
        .buttons
        .iter()
        .filter_map(|button| {
            let ButtonDefinition::BlfSpeedDial(definition) = button else {
                return None;
            };
            device
                .blf_targets
                .get(&definition.instance)
                .map(|target| (definition.clone(), target.clone()))
        })
        .collect();
    access.shared.presence.enqueue(PresenceCommand::InstallBlf {
        device: device_id.clone(),
        generation,
        plan,
    });
}

pub fn retry_blf(access: &Access, now: Instant) {
    access
        .shared
        .presence
        .enqueue(PresenceCommand::RetryBlf(now));
}

pub fn uninstall_device_blf(access: &Access, device_id: &DeviceId) {
    access.shared.presence.enqueue(PresenceCommand::RemoveBlf {
        device: device_id.clone(),
        generation: None,
    });
}

pub fn uninstall_device_blf_for_session(
    access: &Access,
    device_id: &DeviceId,
    generation: sccp_protocol::SessionGeneration,
) {
    access.shared.presence.enqueue(PresenceCommand::RemoveBlf {
        device: device_id.clone(),
        generation: Some(generation),
    });
}

pub fn install_mwi(access: &Access) {
    let subscriptions = access
        .config()
        .lines
        .values()
        .filter_map(|line| {
            line.mailbox
                .as_ref()
                .map(|mailbox| (line.number.clone(), mailbox.clone()))
        })
        .collect();
    access
        .shared
        .presence
        .enqueue(PresenceCommand::InstallMwi(subscriptions));
}

pub fn uninstall_mwi(access: &Access) {
    access.shared.presence.enqueue(PresenceCommand::ClearMwi);
}

pub fn device_state(access: &Access, line: &str) -> DeviceState {
    let config = access.config();
    let mut appearances = config
        .appearances_for_line(line)
        .map(|binding| binding.device_id.clone())
        .collect::<BTreeSet<_>>();
    appearances.extend(
        access
            .shared
            .controller
            .snapshot()
            .mobility()
            .appearances_for_line(line)
            .map(|appearance| appearance.binding.device_id.clone()),
    );
    let (registered_dnd, states) = {
        let controller = access.shared.controller.snapshot();
        let registered_dnd = appearances
            .iter()
            .filter(|device| controller.is_registered(device))
            .map(|device| {
                controller
                    .feature_state(device)
                    .map_or(DndMode::Off, |features| features.dnd)
            })
            .collect::<Vec<_>>();
        let states = controller
            .calls()
            .filter(|call| call.line == line)
            .map(|call| call.state)
            .collect::<Vec<_>>();
        (registered_dnd, states)
    };
    aggregate_device_state(!appearances.is_empty(), &registered_dnd, &states)
}

fn aggregate_device_state(
    has_appearance: bool,
    registered_dnd: &[DndMode],
    states: &[CallState],
) -> DeviceState {
    if !has_appearance {
        return DeviceState::Removed;
    }
    if registered_dnd.is_empty() {
        return DeviceState::Unavailable;
    }

    let ringing = states.contains(&CallState::Ringing);
    let on_hold = states
        .iter()
        .any(|state| matches!(state, CallState::Held | CallState::SharedHeld));
    let in_use = states.iter().any(|state| {
        matches!(
            state,
            CallState::Collecting
                | CallState::PickupCollecting
                | CallState::Calling
                | CallState::Connected
                | CallState::Parking
                | CallState::Retrieving
                | CallState::RemoteInUse
                | CallState::Barged
                | CallState::TransferCollecting
        )
    });

    if ringing && (in_use || on_hold) {
        DeviceState::RingInUse
    } else if ringing {
        DeviceState::Ringing
    } else if in_use {
        DeviceState::InUse
    } else if on_hold {
        DeviceState::OnHold
    } else if registered_dnd.iter().all(|mode| *mode == DndMode::Reject) {
        DeviceState::Busy
    } else {
        DeviceState::NotInUse
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(registered_dnd: &[DndMode], calls: &[CallState]) -> DeviceState {
        aggregate_device_state(true, registered_dnd, calls)
    }

    #[test]
    fn absent_and_unregistered_lines_have_distinct_states() {
        assert_eq!(
            aggregate_device_state(false, &[], &[]),
            DeviceState::Removed
        );
        assert_eq!(state(&[], &[]), DeviceState::Unavailable);
    }

    #[test]
    fn idle_dnd_is_busy_only_when_every_registered_appearance_rejects() {
        assert_eq!(state(&[DndMode::Reject], &[]), DeviceState::Busy);
        assert_eq!(
            state(&[DndMode::Reject, DndMode::Reject], &[]),
            DeviceState::Busy
        );
        assert_eq!(state(&[DndMode::Silent], &[]), DeviceState::NotInUse);
        assert_eq!(
            state(&[DndMode::Reject, DndMode::Off], &[]),
            DeviceState::NotInUse
        );
        assert_eq!(
            state(&[DndMode::Reject, DndMode::Silent], &[]),
            DeviceState::NotInUse
        );
    }

    #[test]
    fn call_activity_takes_precedence_over_idle_dnd() {
        assert_eq!(
            state(&[DndMode::Reject], &[CallState::Connected]),
            DeviceState::InUse
        );
        assert_eq!(
            state(&[DndMode::Reject], &[CallState::Held]),
            DeviceState::OnHold
        );
    }

    #[test]
    fn active_held_and_ringing_calls_publish_rich_states() {
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Ringing]),
            DeviceState::Ringing
        );
        assert_eq!(
            state(&[DndMode::Off], &[CallState::SharedHeld]),
            DeviceState::OnHold
        );
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Held, CallState::Connected]),
            DeviceState::InUse
        );
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Ringing, CallState::Connected]),
            DeviceState::RingInUse
        );
        assert_eq!(
            state(
                &[DndMode::Off],
                &[CallState::Ringing, CallState::SharedHeld]
            ),
            DeviceState::RingInUse
        );
    }

    #[test]
    fn every_live_nonterminal_call_state_is_accounted_for() {
        for call in [
            CallState::Collecting,
            CallState::PickupCollecting,
            CallState::Calling,
            CallState::Connected,
            CallState::Parking,
            CallState::Retrieving,
            CallState::RemoteInUse,
            CallState::Barged,
            CallState::TransferCollecting,
        ] {
            assert_eq!(state(&[DndMode::Off], &[call]), DeviceState::InUse);
        }
        assert_eq!(
            state(&[DndMode::Off], &[CallState::Ended]),
            DeviceState::NotInUse
        );
    }
}
