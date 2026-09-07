//! Owned mobility appearances and single-use handset prompt identities.

use sccp_protocol::{ButtonDefinition, ButtonType, DeviceId, TransactionId};

use super::{Controller, ControllerSnapshot};
use crate::call::mobility::{
    MobilityPreparation, MobilityRegistryError, MobilitySlot, PreparedMobilityTransaction,
    RoamingAppearance,
};
use crate::config::{LineBinding, LineConfig, ModuleConfig};

const MAX_PENDING_MOBILITY_PROMPTS: usize = 65_536;

pub(crate) struct MobilityPrompt {
    pub target: sccp_protocol::StationSessionTarget,
    pub transaction_id: TransactionId,
    pub replaced: Vec<TransactionId>,
}

#[derive(Default)]
pub(crate) struct MobilityReconciliation {
    pub removed: Vec<RoamingAppearance>,
    pub cancelled_prompts: Vec<(sccp_protocol::StationSessionTarget, TransactionId)>,
}

pub(crate) struct MobilitySnapshot {
    appearances: Vec<RoamingAppearance>,
    pending: bool,
}

#[cfg_attr(
    all(test, feature = "development"),
    expect(
        dead_code,
        reason = "the native mobility and configuration adapters read this snapshot surface"
    )
)]
impl MobilitySnapshot {
    pub fn appearance_for_slot(&self, slot: &MobilitySlot) -> Option<&RoamingAppearance> {
        self.appearances
            .iter()
            .find(|appearance| &appearance.slot == slot)
    }
    pub fn binding_for_device(&self, device: &DeviceId, instance: u32) -> Option<&LineBinding> {
        self.appearances
            .iter()
            .find(|appearance| {
                &appearance.slot.device_id == device && appearance.binding.line_instance == instance
            })
            .map(|appearance| &appearance.binding)
    }
    pub fn appearances_for_device<'a>(
        &'a self,
        device: &'a DeviceId,
    ) -> impl Iterator<Item = &'a RoamingAppearance> {
        self.appearances
            .iter()
            .filter(move |appearance| &appearance.slot.device_id == device)
    }
    pub fn appearances_for_line<'a>(
        &'a self,
        line: &'a str,
    ) -> impl Iterator<Item = &'a RoamingAppearance> {
        self.appearances
            .iter()
            .filter(move |appearance| appearance.binding.line.number == line)
    }
    pub fn has_pending_transaction(&self) -> bool {
        self.pending
    }
}

impl ControllerSnapshot {
    #[cfg_attr(
        all(test, feature = "development"),
        expect(dead_code, reason = "the native mobility adapter reads this snapshot")
    )]
    pub fn mobility(&self) -> &MobilitySnapshot {
        &self.mobility
    }
}

impl Controller {
    pub(super) fn mobility_snapshot(&self) -> MobilitySnapshot {
        MobilitySnapshot {
            appearances: self.mobility.committed().cloned().collect(),
            pending: self.mobility.has_pending_transaction(),
        }
    }
    pub(crate) fn reserve_mobility_prompt(&mut self, slot: MobilitySlot) -> Option<MobilityPrompt> {
        let target = sccp_protocol::StationSessionTarget::new(
            slot.device_id.clone(),
            self.registered_device(&slot.device_id)?.session_generation,
        );
        let replaced = self
            .mobility_prompts
            .iter()
            .filter(|(_, current)| *current == &slot)
            .map(|((_, id), _)| *id)
            .collect();
        self.mobility_prompts.retain(|_, current| current != &slot);
        if self.mobility_prompts.len() >= MAX_PENDING_MOBILITY_PROMPTS {
            return None;
        }
        let next = self.next_mobility_prompt.checked_add(1)?;
        let id = TransactionId::new(next);
        self.next_mobility_prompt = next;
        self.mobility_prompts
            .insert((slot.device_id.clone(), id), slot);
        Some(MobilityPrompt {
            target,
            transaction_id: id,
            replaced,
        })
    }
    pub(crate) fn take_mobility_prompt(
        &mut self,
        device: &DeviceId,
        id: TransactionId,
    ) -> Option<MobilitySlot> {
        self.mobility_prompts.remove(&(device.clone(), id))
    }
    pub(crate) fn clear_mobility_prompts(&mut self, device: &DeviceId) {
        self.mobility_prompts
            .retain(|(pending, _), _| pending != device);
    }
    pub(crate) fn prepare_mobility_login(
        &mut self,
        slot: MobilitySlot,
        line: LineConfig,
        instances: Vec<u32>,
    ) -> Result<MobilityPreparation, MobilityRegistryError> {
        self.mobility.prepare_login(slot, line, instances)
    }
    pub(crate) fn prepare_mobility_logout(
        &mut self,
        slot: &MobilitySlot,
    ) -> Result<PreparedMobilityTransaction, MobilityRegistryError> {
        self.mobility.prepare_logout(slot)
    }
    pub(crate) fn commit_mobility(
        &mut self,
        transaction: &PreparedMobilityTransaction,
    ) -> Result<(), MobilityRegistryError> {
        // Calls may arrive while handset I/O is in flight. Revalidate before
        // removing an appearance that a newly admitted call now uses.
        if let Some(previous) = transaction.previous() {
            if self.calls().any(|call| {
                call.device_id == previous.slot.device_id
                    && call.line_instance == previous.binding.line_instance
            }) {
                return Err(MobilityRegistryError::TransactionInProgress);
            }
        }
        self.mobility.commit(transaction)
    }
    pub(crate) fn abort_mobility(
        &mut self,
        transaction: &PreparedMobilityTransaction,
    ) -> Result<(), MobilityRegistryError> {
        self.mobility.abort(transaction)
    }
    pub(crate) fn reconcile_mobility(
        &mut self,
        config: ModuleConfig,
    ) -> Result<MobilityReconciliation, MobilityRegistryError> {
        let prompts = self.mobility_prompts.keys().cloned().collect::<Vec<_>>();
        let cancelled_prompts = prompts
            .into_iter()
            .filter_map(|(device, id)| {
                let generation = self.registered_device(&device)?.session_generation;
                Some((
                    sccp_protocol::StationSessionTarget::new(device, generation),
                    id,
                ))
            })
            .collect();
        let removed = self.mobility.remove_invalid(|appearance| {
            let slot = &appearance.slot;
            let line = &appearance.binding.line.number;
            config.devices.get(&slot.device_id).is_some_and(|device| device.buttons.iter().any(|button|
                matches!(button, ButtonDefinition::Feature(feature) if feature.instance == slot.button_instance && feature.feature == ButtonType::Mobility)))
                && config.lines.contains_key(line)
                && config.mobility_for_line(line).is_some_and(|mobility| mobility.pin.is_some())
                && !config.appearances_for_device(&slot.device_id).any(|binding| binding.line.number == *line || binding.line_instance == appearance.binding.line_instance)
        })?;
        self.mobility_prompts.clear();
        Ok(MobilityReconciliation {
            removed,
            cancelled_prompts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replacing_or_reloading_prompts_returns_the_exact_response_reservations_to_cancel() {
        let mut controller = super::super::tests::shared_inbound_controller();
        let device = DeviceId::new("SEP001122334455").unwrap();
        let slot = MobilitySlot::new(device.clone(), 1).unwrap();
        let first = controller.reserve_mobility_prompt(slot.clone()).unwrap();
        let target = sccp_protocol::StationSessionTarget::new(
            device.clone(),
            controller
                .registered_device(&device)
                .unwrap()
                .session_generation,
        );
        assert_eq!(first.target, target);
        assert!(first.replaced.is_empty());
        let second = controller.reserve_mobility_prompt(slot.clone()).unwrap();
        assert_eq!(second.replaced, [first.transaction_id]);
        assert_ne!(first.transaction_id, second.transaction_id);
        assert!(
            controller
                .take_mobility_prompt(&device, first.transaction_id)
                .is_none()
        );
        let config = ModuleConfig::parse(include_str!("../../../sccp.conf.example")).unwrap();
        let reload = controller.reconcile_mobility(config).unwrap();
        assert!(reload.removed.is_empty());
        assert_eq!(reload.cancelled_prompts.len(), 1);
        assert_eq!(reload.cancelled_prompts[0].0, target);
        assert_eq!(reload.cancelled_prompts[0].1, second.transaction_id);
        assert!(
            controller
                .take_mobility_prompt(&device, second.transaction_id)
                .is_none()
        );
        let unknown = MobilitySlot::new(DeviceId::new("SEPFFEEDDCCBBAA").unwrap(), 1).unwrap();
        assert!(controller.reserve_mobility_prompt(unknown).is_none());
    }
}
