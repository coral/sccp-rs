//! Parking event subscriptions and deterministic parked-call state.

use std::collections::{BTreeMap, HashMap};

use sccp_protocol::{CallId, DeviceId};
use serde::Serialize;
use thiserror::Error;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParkingEventKind {
    Parked,
    Timeout,
    GiveUp,
    Retrieved,
    Failed,
    Swap,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkingEvent {
    pub kind: ParkingEventKind,
    pub lot: String,
    pub slot: u32,
    pub timeout_seconds: u64,
    pub duration_seconds: u64,
    pub parker_dial_string: String,
    pub parkee_channel: String,
    pub parkee_unique_id: String,
    pub caller_name: String,
    pub caller_number: String,
    pub connected_name: String,
    pub connected_number: String,
    pub retriever_channel: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkedCall {
    pub lot: String,
    pub slot: u32,
    pub timeout_seconds: u64,
    pub duration_seconds: u64,
    pub parker_dial_string: String,
    pub parkee_channel: String,
    pub parkee_unique_id: String,
    pub caller_name: String,
    pub caller_number: String,
    pub connected_name: String,
    pub connected_number: String,
}

impl From<&ParkingEvent> for ParkedCall {
    fn from(event: &ParkingEvent) -> Self {
        Self {
            lot: event.lot.clone(),
            slot: event.slot,
            timeout_seconds: event.timeout_seconds,
            duration_seconds: event.duration_seconds,
            parker_dial_string: event.parker_dial_string.clone(),
            parkee_channel: event.parkee_channel.clone(),
            parkee_unique_id: event.parkee_unique_id.clone(),
            caller_name: event.caller_name.clone(),
            caller_number: event.caller_number.clone(),
            connected_name: event.connected_name.clone(),
            connected_number: event.connected_number.clone(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetrievalClaim {
    pub device_id: DeviceId,
    pub call_id: CallId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkingChange {
    pub current: Option<ParkedCall>,
    pub removed: Option<ParkedCall>,
    pub claim: Option<RetrievalClaim>,
}

#[derive(Clone, Debug, Default)]
pub struct ParkingRegistry {
    calls: BTreeMap<(String, u32), ParkedCall>,
    claims: HashMap<(String, u32), RetrievalClaim>,
}

impl ParkingRegistry {
    pub fn apply(&mut self, event: &ParkingEvent) -> ParkingChange {
        let key = (event.lot.clone(), event.slot);
        match event.kind {
            ParkingEventKind::Parked | ParkingEventKind::Swap if event.slot != 0 => {
                let current = ParkedCall::from(event);
                self.calls.insert(key, current.clone());
                ParkingChange {
                    current: Some(current),
                    removed: None,
                    claim: None,
                }
            }
            ParkingEventKind::Timeout | ParkingEventKind::GiveUp | ParkingEventKind::Retrieved => {
                ParkingChange {
                    current: None,
                    removed: self.calls.remove(&key),
                    claim: self.claims.remove(&key),
                }
            }
            ParkingEventKind::Failed | ParkingEventKind::Parked | ParkingEventKind::Swap => {
                ParkingChange {
                    current: None,
                    removed: None,
                    claim: None,
                }
            }
        }
    }

    pub fn claim(&mut self, lot: &str, slot: u32, device_id: DeviceId, call_id: CallId) -> bool {
        let key = (lot.to_owned(), slot);
        if !self.calls.contains_key(&key) || self.claims.contains_key(&key) {
            return false;
        }
        self.claims
            .insert(key, RetrievalClaim { device_id, call_id });
        true
    }

    pub fn release_claim(&mut self, lot: &str, slot: u32, call_id: CallId) -> bool {
        let key = (lot.to_owned(), slot);
        if self
            .claims
            .get(&key)
            .is_some_and(|claim| claim.call_id == call_id)
        {
            self.claims.remove(&key);
            true
        } else {
            false
        }
    }

    pub fn calls_in_lot(&self, lot: &str) -> Vec<ParkedCall> {
        self.calls
            .range((lot.to_owned(), 0)..=(lot.to_owned(), u32::MAX))
            .map(|(_, call)| call.clone())
            .collect()
    }

    pub fn call(&self, lot: &str, slot: u32) -> Option<&ParkedCall> {
        self.calls.get(&(lot.to_owned(), slot))
    }

    pub fn lot_has_calls(&self, lot: &str) -> bool {
        self.calls
            .range((lot.to_owned(), 0)..=(lot.to_owned(), u32::MAX))
            .next()
            .is_some()
    }

    pub fn lot_json(&self, lot: &str) -> String {
        #[derive(Serialize)]
        struct ParkingLotSnapshot<'a> {
            lot: &'a str,
            calls: Vec<ParkingCallSnapshot<'a>>,
        }
        #[derive(Serialize)]
        struct ParkingCallSnapshot<'a> {
            slot: u32,
            caller_name: &'a str,
            caller_number: &'a str,
            connected_name: &'a str,
            connected_number: &'a str,
            timeout_seconds: u64,
            duration_seconds: u64,
        }
        let calls = self.calls_in_lot(lot);
        serde_json::to_string(&ParkingLotSnapshot {
            lot,
            calls: calls
                .iter()
                .map(|call| ParkingCallSnapshot {
                    slot: call.slot,
                    caller_name: &call.caller_name,
                    caller_number: &call.caller_number,
                    connected_name: &call.connected_name,
                    connected_number: &call.connected_number,
                    timeout_seconds: call.timeout_seconds,
                    duration_seconds: call.duration_seconds,
                })
                .collect(),
        })
        .expect("parking lot snapshot contains only serializable values")
    }
}

pub fn handset_call_id_from_channel(channel: &str) -> Option<CallId> {
    if !channel.starts_with("SCCP/") {
        return None;
    }
    let suffix = channel.rsplit_once('-')?.1;
    let value = u64::from_str_radix(suffix, 16).ok()?;
    (value != 0).then_some(CallId(value))
}

/// Typed parking-event port implemented by PBX adapters and domain fakes.
pub trait ParkingEventSource {
    fn subscribe<F>(&self, callback: F) -> Result<ParkingSubscription, ParkingError>
    where
        F: Fn(ParkingEvent) + Send + Sync + 'static;
}

pub struct ParkingSubscription {
    inner: Box<dyn ParkingSubscriptionControl>,
}

impl ParkingSubscription {
    pub fn new(inner: Box<dyn ParkingSubscriptionControl>) -> Self {
        Self { inner }
    }

    pub fn unsubscribe(self) {
        self.inner.unsubscribe();
    }
}

/// Typed lifetime boundary implemented by the native Stasis adapter and by
/// domain fakes. Dropping or explicitly consuming the handle must complete the
/// adapter's callback-drain policy.
pub trait ParkingSubscriptionControl: Send {
    fn unsubscribe(self: Box<Self>);
}

#[derive(Debug, Error)]
pub enum ParkingError {
    #[error("unable to subscribe to Asterisk parking events")]
    SubscribeFailed,
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct FakeSubscription(Arc<AtomicUsize>);

    impl ParkingSubscriptionControl for FakeSubscription {
        fn unsubscribe(self: Box<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn event(kind: ParkingEventKind, lot: &str, slot: u32) -> ParkingEvent {
        ParkingEvent {
            kind,
            lot: lot.into(),
            slot,
            timeout_seconds: 45,
            duration_seconds: 3,
            parker_dial_string: "SCCP/1001-00000007".into(),
            parkee_channel: "PJSIP/carrier-00000001".into(),
            parkee_unique_id: "unique-1".into(),
            caller_name: "Taylor \"T\"".into(),
            caller_number: "2100".into(),
            connected_name: "Desk".into(),
            connected_number: "1001".into(),
            retriever_channel: String::new(),
        }
    }

    #[test]
    fn registry_orders_slots_serializes_json_and_removes_terminal_events() {
        let mut registry = ParkingRegistry::default();
        registry.apply(&event(ParkingEventKind::Parked, "main", 702));
        registry.apply(&event(ParkingEventKind::Parked, "main", 701));

        assert_eq!(
            registry
                .calls_in_lot("main")
                .iter()
                .map(|call| call.slot)
                .collect::<Vec<_>>(),
            [701, 702]
        );
        assert_eq!(
            registry.lot_json("main"),
            "{\"lot\":\"main\",\"calls\":[{\"slot\":701,\"caller_name\":\"Taylor \\\"T\\\"\",\"caller_number\":\"2100\",\"connected_name\":\"Desk\",\"connected_number\":\"1001\",\"timeout_seconds\":45,\"duration_seconds\":3},{\"slot\":702,\"caller_name\":\"Taylor \\\"T\\\"\",\"caller_number\":\"2100\",\"connected_name\":\"Desk\",\"connected_number\":\"1001\",\"timeout_seconds\":45,\"duration_seconds\":3}]}"
        );

        let change = registry.apply(&event(ParkingEventKind::Timeout, "main", 701));
        assert_eq!(change.removed.unwrap().slot, 701);
        assert_eq!(registry.calls_in_lot("main").len(), 1);
    }

    #[test]
    fn management_json_preserves_redacted_native_identities() {
        let mut redacted = event(ParkingEventKind::Parked, "private", 701);
        redacted.caller_name.clear();
        redacted.caller_number.clear();
        redacted.connected_name.clear();
        redacted.connected_number.clear();

        let mut registry = ParkingRegistry::default();
        registry.apply(&redacted);

        assert_eq!(
            registry.lot_json("private"),
            "{\"lot\":\"private\",\"calls\":[{\"slot\":701,\"caller_name\":\"\",\"caller_number\":\"\",\"connected_name\":\"\",\"connected_number\":\"\",\"timeout_seconds\":45,\"duration_seconds\":3}]}"
        );
    }

    #[test]
    fn retrieval_claim_has_one_deterministic_winner_and_can_retry_after_failure() {
        let mut registry = ParkingRegistry::default();
        registry.apply(&event(ParkingEventKind::Parked, "main", 701));
        let first_device = DeviceId::new("SEP001122334455").unwrap();
        let second_device = DeviceId::new("SEP112233445566").unwrap();

        assert!(registry.claim("main", 701, first_device, CallId(10)));
        assert!(!registry.claim("main", 701, second_device.clone(), CallId(11)));
        assert!(!registry.release_claim("main", 701, CallId(11)));
        assert!(registry.release_claim("main", 701, CallId(10)));
        assert!(registry.claim("main", 701, second_device, CallId(11)));

        let mut retrieved = event(ParkingEventKind::Retrieved, "main", 701);
        retrieved.retriever_channel = "SCCP/1001-0000000b".into();
        let change = registry.apply(&retrieved);
        assert_eq!(change.claim.unwrap().call_id, CallId(11));
        assert!(!registry.lot_has_calls("main"));
    }

    #[test]
    fn handset_call_identity_is_recovered_from_backend_channel_names() {
        assert_eq!(
            handset_call_id_from_channel("SCCP/1001-0000000b"),
            Some(CallId(11))
        );
        assert_eq!(handset_call_id_from_channel("PJSIP/2100-0000000b"), None);
        assert_eq!(handset_call_id_from_channel("SCCP/1001-invalid"), None);
        assert_eq!(handset_call_id_from_channel("SCCP/1001-00000000"), None);
    }

    #[test]
    fn typed_subscription_fake_is_consumed_exactly_once() {
        let calls = Arc::new(AtomicUsize::new(0));
        ParkingSubscription {
            inner: Box::new(FakeSubscription(Arc::clone(&calls))),
        }
        .unsubscribe();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}
