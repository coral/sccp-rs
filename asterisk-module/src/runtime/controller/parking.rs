//! Parking attempts and registry transitions share the controller's lifetime.

use super::*;
use crate::call::parking::{ParkingEvent, ParkingEventKind, ParkingRegistry};

#[derive(Clone, Debug)]
pub(crate) struct PendingPark {
    pub pbx_id: PbxCallId,
    pub device_id: DeviceId,
    pub requested_lot: Option<String>,
    pub parkee_unique_id: Option<String>,
    pub deadline: Instant,
}
#[derive(Clone, Debug)]
pub(crate) struct PendingRetrieval {
    pub pbx_id: PbxCallId,
    pub device_id: DeviceId,
    pub lot: String,
    pub slot: u32,
    pub deadline: Instant,
}
#[derive(Clone, Debug)]
pub(crate) struct PendingParkingNotification {
    pub device_id: DeviceId,
    pub call_id: CallId,
    pub deadline: Instant,
}
#[derive(Clone, Default)]
pub(super) struct ParkingRuntime {
    pub(super) registry: ParkingRegistry,
    claims: HashMap<CallId, (String, u32, Instant)>,
    pub(super) parks: HashMap<CallId, PendingPark>,
    pub(super) retrievals: HashMap<CallId, PendingRetrieval>,
    notifications: HashMap<CallId, PendingParkingNotification>,
    retired_peers: HashSet<String>,
}
#[derive(Default)]
pub(crate) struct ParkingUpdate {
    pub retired_peers: Vec<String>,
    pub effects: Vec<DriverEffect>,
    pub prompts: Vec<(DeviceId, CallId, u32, &'static str)>,
    pub close: Vec<(DeviceId, CallId)>,
}
impl Controller {
    pub(crate) fn claim_parking(
        &mut self,
        lot: String,
        slot: u32,
        device: DeviceId,
        call: CallId,
        deadline: Instant,
    ) -> bool {
        if !self.parking.has_capacity() || deadline <= Instant::now() {
            return false;
        }
        if !self.parking.registry.claim(&lot, slot, device, call) {
            return false;
        }
        self.parking.claims.insert(call, (lot, slot, deadline));
        true
    }
    pub(crate) fn release_parking_claim(&mut self, lot: String, slot: u32, call: CallId) -> bool {
        self.parking.claims.remove(&call);
        self.parking.registry.release_claim(&lot, slot, call)
    }
    pub(crate) fn set_park_peer(&mut self, pbx_id: PbxCallId, unique_id: String) {
        if let Some(attempt) = self
            .parking
            .parks
            .values_mut()
            .find(|attempt| attempt.pbx_id == pbx_id)
        {
            attempt.parkee_unique_id = Some(unique_id);
        }
    }
    pub(crate) fn fail_parking_operation(
        &mut self,
        operation: ParkingOperation,
        notification_deadline: Instant,
    ) -> ParkingUpdate {
        let mut update = ParkingUpdate::default();
        match operation {
            ParkingOperation::Park {
                call_id: pbx_id, ..
            } => {
                let call = self
                    .parking
                    .parks
                    .iter()
                    .find(|(_, p)| p.pbx_id == pbx_id)
                    .map(|(id, _)| *id);
                if let Some((call_id, pending)) =
                    call.and_then(|id| self.parking.parks.remove(&id).map(|p| (id, p)))
                {
                    update
                        .retired_peers
                        .extend(pending.parkee_unique_id.clone());
                    update.effects = self.parking_failed(call_id);
                    update
                        .prompts
                        .push((pending.device_id, call_id, 4, "Unable to park call"));
                }
            }
            ParkingOperation::Retrieve {
                call_id: pbx_id, ..
            } => {
                let call = self
                    .parking
                    .retrievals
                    .iter()
                    .find(|(_, p)| p.pbx_id == pbx_id)
                    .map(|(id, _)| *id);
                if let Some((call_id, pending)) =
                    call.and_then(|id| self.parking.retrievals.remove(&id).map(|p| (id, p)))
                {
                    self.parking
                        .registry
                        .release_claim(&pending.lot, pending.slot, call_id);
                    update.effects = self.parking_retrieval_failed(call_id);
                    update.prompts.push((
                        pending.device_id.clone(),
                        call_id,
                        3,
                        "Parked call unavailable",
                    ));
                    self.schedule_parking_notification(PendingParkingNotification {
                        device_id: pending.device_id,
                        call_id,
                        deadline: notification_deadline,
                    });
                }
            }
        }
        update
    }
    fn schedule_parking_notification(&mut self, notification: PendingParkingNotification) {
        self.parking
            .notifications
            .insert(notification.call_id, notification);
    }
    fn take_matching_park(&mut self, event: &ParkingEvent) -> Option<(CallId, PendingPark)> {
        let selected = self
            .parking
            .parks
            .iter()
            .filter(|(_, attempt)| {
                attempt
                    .parkee_unique_id
                    .as_deref()
                    .is_some_and(|id| id == event.parkee_unique_id)
                    || (attempt.parkee_unique_id.is_none()
                        && attempt
                            .requested_lot
                            .as_deref()
                            .is_none_or(|lot| lot == event.lot))
            })
            .min_by_key(|(call, attempt)| (attempt.deadline, call.0))
            .map(|(call, _)| *call)?;
        self.parking
            .parks
            .remove(&selected)
            .map(|attempt| (selected, attempt))
    }
    pub(crate) fn apply_parking_event(
        &mut self,
        event: ParkingEvent,
        retriever: Option<CallId>,
        notification_deadline: Instant,
    ) -> ParkingUpdate {
        let change = self.parking.registry.apply(&event);
        let mut update = ParkingUpdate::default();
        match event.kind {
            ParkingEventKind::Parked | ParkingEventKind::Swap => {
                if let Some((call_id, pending)) = self.take_matching_park(&event) {
                    update.effects = self.parking_confirmed(call_id, event.slot);
                    self.schedule_parking_notification(PendingParkingNotification {
                        device_id: pending.device_id,
                        call_id,
                        deadline: notification_deadline,
                    });
                }
            }
            ParkingEventKind::Retrieved => {
                if let Some(call_id) = retriever.or_else(|| change.claim.map(|claim| claim.call_id))
                {
                    if self.parking.retrievals.remove(&call_id).is_some() {
                        update.effects = self.parking_retrieved(call_id);
                    }
                }
            }
            ParkingEventKind::Failed => {
                if let Some((call_id, pending)) = self.take_matching_park(&event) {
                    update
                        .retired_peers
                        .extend(pending.parkee_unique_id.clone());
                    update.effects = self.parking_failed(call_id);
                    update
                        .prompts
                        .push((pending.device_id, call_id, 4, "Unable to park call"));
                }
            }
            ParkingEventKind::Timeout | ParkingEventKind::GiveUp => {}
        }
        update
    }
    pub(crate) fn expire_parking_attempts(
        &mut self,
        now: Instant,
        notification_deadline: Instant,
    ) -> ParkingUpdate {
        let mut update = ParkingUpdate {
            retired_peers: self.parking.retired_peers.drain().collect(),
            ..ParkingUpdate::default()
        };
        self.parking.claims.retain(|call, (lot, slot, deadline)| {
            if *deadline > now {
                return true;
            }
            self.parking.registry.release_claim(lot, *slot, *call);
            false
        });
        let parks: Vec<_> = self
            .parking
            .parks
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for call_id in parks {
            if let Some(pending) = self.parking.parks.remove(&call_id) {
                update
                    .retired_peers
                    .extend(pending.parkee_unique_id.clone());
                update.effects.extend(self.parking_failed(call_id));
                update
                    .prompts
                    .push((pending.device_id, call_id, 4, "Parking timed out"));
            }
        }
        let retrievals: Vec<_> = self
            .parking
            .retrievals
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for call_id in retrievals {
            if let Some(pending) = self.parking.retrievals.remove(&call_id) {
                self.parking
                    .registry
                    .release_claim(&pending.lot, pending.slot, call_id);
                update
                    .effects
                    .extend(self.parking_retrieval_failed(call_id));
                update.prompts.push((
                    pending.device_id.clone(),
                    call_id,
                    3,
                    "Parked call unavailable",
                ));
                self.schedule_parking_notification(PendingParkingNotification {
                    device_id: pending.device_id,
                    call_id,
                    deadline: notification_deadline,
                });
            }
        }
        self.parking.notifications.retain(|_, notification| {
            if notification.deadline > now {
                return true;
            }
            update
                .close
                .push((notification.device_id.clone(), notification.call_id));
            false
        });
        update
    }
    pub(super) fn retire_parking_attempts(&mut self) {
        self.parking.parks.retain(|call, pending| {
            let live = self.call_registry.by_sccp.contains_key(call);
            if !live && let Some(peer) = &pending.parkee_unique_id {
                self.parking.retired_peers.insert(peer.clone());
            }
            live
        });
        let ended: Vec<_> = self
            .parking
            .retrievals
            .keys()
            .filter(|call| !self.call_registry.by_sccp.contains_key(call))
            .copied()
            .collect();
        for call in ended {
            if let Some(pending) = self.parking.retrievals.remove(&call) {
                self.parking
                    .registry
                    .release_claim(&pending.lot, pending.slot, call);
            }
        }
    }
}
impl ControllerSnapshot {
    pub(crate) fn parking(&self) -> &ParkingRegistry {
        &self.parking.registry
    }
    #[cfg(any(test, feature = "telemetry"))]
    pub(crate) fn pending_parks(&self) -> &HashMap<CallId, PendingPark> {
        &self.parking.parks
    }
    pub(crate) fn pending_retrievals(&self) -> &HashMap<CallId, PendingRetrieval> {
        &self.parking.retrievals
    }
}

impl ParkingRuntime {
    pub(super) fn has_capacity(&self) -> bool {
        self.claims.len() + self.parks.len() + self.retrievals.len() + self.notifications.len()
            < crate::runtime::mailbox::RUNTIME_MAILBOX_CAPACITY
    }
    pub(super) fn commit_claim(&mut self, call: CallId, lot: &str, slot: u32) -> bool {
        if !self
            .claims
            .get(&call)
            .is_some_and(|(claimed_lot, claimed_slot, deadline)| {
                claimed_lot == lot && *claimed_slot == slot && *deadline > Instant::now()
            })
        {
            return false;
        }
        self.claims.remove(&call);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: ParkingEventKind) -> ParkingEvent {
        ParkingEvent {
            kind,
            lot: "default".into(),
            slot: 701,
            timeout_seconds: 30,
            duration_seconds: 0,
            parker_dial_string: String::new(),
            parkee_channel: String::new(),
            parkee_unique_id: "parked-peer".into(),
            caller_name: String::new(),
            caller_number: String::new(),
            connected_name: String::new(),
            connected_number: String::new(),
            retriever_channel: String::new(),
        }
    }

    #[test]
    fn parking_confirmation_retires_attempt_atomically_and_duplicate_events_cannot_repeat_cleanup()
    {
        let mut controller = super::super::tests::shared_inbound_controller();
        controller.phone_answer(CallId(2));
        let now = Instant::now();
        let (id, effects) =
            controller.prepare_parking(CallId(2), true, None, now + Duration::from_secs(3));
        assert_eq!(id, Some(PbxCallId(8)));
        assert!(effects.is_ok());
        assert_eq!(controller.snapshot().pending_parks().len(), 1);
        controller.set_park_peer(PbxCallId(8), "parked-peer".into());
        let update = controller.apply_parking_event(
            event(ParkingEventKind::Parked),
            None,
            now + Duration::from_secs(5),
        );
        assert!(
            matches!(&update.effects[0], DriverEffect::Handset(HandsetEffect::SetCallInfo { info, .. }) if info.called_number == "701")
        );
        controller.retire_ended_call_records();
        assert!(controller.snapshot().pending_parks().is_empty());
        assert!(controller.call(CallId(2)).is_none());
        assert!(
            controller
                .apply_parking_event(
                    event(ParkingEventKind::Parked),
                    None,
                    now + Duration::from_secs(9)
                )
                .effects
                .is_empty()
        );
        let expired = controller
            .expire_parking_attempts(now + Duration::from_secs(5), now + Duration::from_secs(10));
        assert_eq!(
            expired.close,
            [(DeviceId::new("SEP001122334455").unwrap(), CallId(2))]
        );
        assert!(
            controller
                .expire_parking_attempts(
                    now + Duration::from_secs(10),
                    now + Duration::from_secs(15)
                )
                .close
                .is_empty()
        );
    }

    #[test]
    fn expired_retrieval_claim_cannot_start_a_call_after_handset_delivery_returns() {
        let mut controller = super::super::tests::shared_inbound_controller();
        let now = Instant::now();
        controller.apply_parking_event(event(ParkingEventKind::Parked), None, now);
        let device = DeviceId::new("SEP001122334455").unwrap();
        assert!(controller.claim_parking(
            "default".into(),
            701,
            device.clone(),
            CallId(42),
            now + Duration::from_secs(1)
        ));
        assert!(!controller.claim_parking(
            "default".into(),
            701,
            device.clone(),
            CallId(43),
            now + Duration::from_secs(1)
        ));
        controller
            .expire_parking_attempts(now + Duration::from_secs(1), now + Duration::from_secs(2));
        assert!(!controller.parking.commit_claim(CallId(42), "default", 701));
        assert!(controller.claim_parking(
            "default".into(),
            701,
            device,
            CallId(43),
            now + Duration::from_secs(2)
        ));
    }

    #[test]
    fn timeout_releases_native_callback_admission_once() {
        let mut controller = super::super::tests::shared_inbound_controller();
        controller.phone_answer(CallId(2));
        let now = Instant::now();
        let (_, effects) =
            controller.prepare_parking(CallId(2), true, None, now + Duration::from_secs(1));
        assert!(effects.is_ok());
        controller.set_park_peer(PbxCallId(8), "native-peer".into());
        let expired = controller
            .expire_parking_attempts(now + Duration::from_secs(1), now + Duration::from_secs(2));
        assert_eq!(expired.retired_peers, ["native-peer"]);
        assert!(
            controller
                .expire_parking_attempts(now + Duration::from_secs(2), now + Duration::from_secs(3))
                .retired_peers
                .is_empty()
        );
    }
}
