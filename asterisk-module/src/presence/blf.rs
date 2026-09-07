//! Runtime ownership and race-safe delivery for monitored speed-dial hints.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sccp_protocol::{BlfCallerInfo, BlfSpeedDialDefinition, BlfState, DeviceId};
use thiserror::Error;
use tokio::sync::mpsc;

use crate::config::HintTarget;
use crate::presence::hints::{ExtensionState, HintUpdateReason};

pub use crate::presence::hints::{HintCallback, HintProvider, HintSnapshot};

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct BlfKey {
    device_id: DeviceId,
    instance: u32,
}

/// A normalized handset update tagged with its subscription generation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BlfEvent {
    pub device_id: DeviceId,
    pub instance: u32,
    pub state: BlfState,
    pub caller: Option<BlfCallerInfo>,
    terminal: bool,
    generation: u64,
}

impl BlfEvent {
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn is_terminal(&self) -> bool {
        self.terminal
    }
}

struct Entry<Subscription> {
    generation: u64,
    _subscription: Subscription,
}

#[derive(Clone, Copy, Debug)]
struct RetryState {
    attempts: u8,
    retry_at: Instant,
}

#[derive(Default)]
struct InitialGate {
    initializing: bool,
    pending: Option<HintSnapshot>,
}

/// Owns all active monitored-button subscriptions.
pub struct BlfSubscriptions<Provider: HintProvider> {
    provider: Provider,
    events: Arc<dyn Fn(BlfEvent) + Send + Sync>,
    entries: HashMap<BlfKey, Entry<Provider::Subscription>>,
    retries: HashMap<BlfKey, RetryState>,
    next_generation: u64,
}

impl<Provider: HintProvider> BlfSubscriptions<Provider> {
    pub fn new(provider: Provider, events: mpsc::UnboundedSender<BlfEvent>) -> Self {
        Self::with_sink(
            provider,
            Arc::new(move |event| {
                let _ = events.send(event);
            }),
        )
    }

    pub(crate) fn with_sink(
        provider: Provider,
        events: Arc<dyn Fn(BlfEvent) + Send + Sync>,
    ) -> Self {
        Self {
            provider,
            events,
            entries: HashMap::new(),
            retries: HashMap::new(),
            next_generation: 1,
        }
    }

    /// Installs or replaces one monitored button and emits exactly one initial
    /// snapshot before live updates can pass its initialization gate.
    pub fn subscribe(
        &mut self,
        device_id: DeviceId,
        definition: &BlfSpeedDialDefinition,
        target: &HintTarget,
    ) -> Result<(), BlfSubscriptionError> {
        let key = BlfKey {
            device_id,
            instance: definition.instance,
        };
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(BlfSubscriptionError::GenerationExhausted)?;
        self.entries.remove(&key);
        let gate = Arc::new(Mutex::new(InitialGate {
            initializing: true,
            pending: None,
        }));
        let events = self.events.clone();
        let callback_key = key.clone();
        let callback_gate = Arc::clone(&gate);
        let callback: HintCallback = Arc::new(move |update| {
            let mut gate = callback_gate
                .lock()
                .expect("BLF initialization lock poisoned");
            if gate.initializing {
                if gate
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.state.raw() >= 0)
                {
                    gate.pending = Some(update);
                }
                return;
            }
            events(normalize_event(&callback_key, generation, update));
        });

        let subscription = match self.provider.subscribe(target, callback) {
            Ok(subscription) => subscription,
            Err(error) => {
                self.record_failure(key);
                return Err(BlfSubscriptionError::Provider(error.to_string()));
            }
        };
        self.entries.insert(
            key.clone(),
            Entry {
                generation,
                _subscription: subscription,
            },
        );

        let snapshot = match self.provider.lookup(target) {
            Ok(Some(snapshot)) => snapshot,
            Ok(None) => unknown_snapshot(target),
            Err(error) => {
                self.entries.remove(&key);
                self.record_failure(key);
                return Err(BlfSubscriptionError::Provider(error.to_string()));
            }
        };

        let mut gate = gate.lock().expect("BLF initialization lock poisoned");
        let initial = gate.pending.take().unwrap_or(snapshot);
        (self.events)(normalize_event(&key, generation, initial));
        gate.initializing = false;
        self.retries.remove(&key);
        Ok(())
    }

    /// Returns whether a missing subscription is due for another attempt.
    pub fn retry_due(&self, device_id: &DeviceId, instance: u32, now: Instant) -> bool {
        let key = BlfKey {
            device_id: device_id.clone(),
            instance,
        };
        !self.entries.contains_key(&key)
            && self
                .retries
                .get(&key)
                .is_some_and(|retry| retry.retry_at <= now)
    }

    /// Retires a watcher after Asterisk removed/deactivated its hint and
    /// schedules a generation-safe resubscription.
    pub fn retry_terminal(&mut self, event: &BlfEvent) {
        let key = BlfKey {
            device_id: event.device_id.clone(),
            instance: event.instance,
        };
        if !event.terminal
            || !self
                .entries
                .get(&key)
                .is_some_and(|entry| entry.generation == event.generation)
        {
            return;
        }
        self.entries.remove(&key);
        self.record_failure(key);
    }

    /// Schedules a retry when the owner's callback capacity cannot yet admit a watcher.
    #[cfg(any(feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn defer(&mut self, device_id: DeviceId, instance: u32) {
        self.record_failure(BlfKey {
            device_id,
            instance,
        });
    }

    fn record_failure(&mut self, key: BlfKey) {
        let attempts = self
            .retries
            .get(&key)
            .map_or(1, |retry| retry.attempts.saturating_add(1));
        let exponent = u32::from(attempts.saturating_sub(1).min(5));
        let seconds = 1u64.checked_shl(exponent).unwrap_or(32).min(60);
        self.retries.insert(
            key,
            RetryState {
                attempts,
                retry_at: Instant::now() + Duration::from_secs(seconds),
            },
        );
    }

    /// Returns whether an event belongs to the currently installed generation.
    pub fn is_current(&self, event: &BlfEvent) -> bool {
        self.entries
            .get(&BlfKey {
                device_id: event.device_id.clone(),
                instance: event.instance,
            })
            .is_some_and(|entry| entry.generation == event.generation)
    }

    /// Unsubscribes every monitored button owned by one device.
    pub fn remove_device(&mut self, device_id: &DeviceId) {
        self.entries.retain(|key, _| &key.device_id != device_id);
        self.retries.retain(|key, _| &key.device_id != device_id);
    }

    /// Unsubscribes every monitored button, including during reload/unload.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.retries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BlfSubscriptionError {
    #[error("hint subscription generations exhausted")]
    GenerationExhausted,
    #[error("hint service failed: {0}")]
    Provider(String),
}

pub fn map_extension_state(state: ExtensionState) -> BlfState {
    if state.raw() < 0 {
        BlfState::Unknown
    } else if state.contains(ExtensionState::UNAVAILABLE) {
        BlfState::Unavailable
    } else if state.contains(ExtensionState::RINGING) {
        BlfState::Ringing
    } else if state.contains(ExtensionState::ON_HOLD) {
        BlfState::Held
    } else if state.contains(ExtensionState::BUSY) || state.contains(ExtensionState::IN_USE) {
        BlfState::Busy
    } else if state.contains(ExtensionState::IDLE) {
        BlfState::Idle
    } else {
        BlfState::Unknown
    }
}

fn unknown_snapshot(target: &HintTarget) -> HintSnapshot {
    HintSnapshot {
        target: target.clone(),
        state: ExtensionState::DEACTIVATED,
        reason: HintUpdateReason::Device,
        caller: None,
    }
}

fn normalize_event(key: &BlfKey, generation: u64, snapshot: HintSnapshot) -> BlfEvent {
    let state = map_extension_state(snapshot.state);
    let terminal = snapshot.state.raw() < 0;
    let caller = matches!(state, BlfState::Ringing | BlfState::Busy | BlfState::Held)
        .then(|| snapshot.caller)
        .flatten()
        .filter(|caller| caller.presentation_allowed())
        .map(|caller| BlfCallerInfo {
            name: caller.name.unwrap_or_default(),
            number: caller.number.unwrap_or_default(),
        });
    BlfEvent {
        device_id: key.device_id.clone(),
        instance: key.instance,
        state,
        caller,
        terminal,
        generation,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::presence::hints::HintCaller;

    use super::*;

    #[derive(Clone)]
    struct FakeHints {
        state: Arc<Mutex<FakeState>>,
        drops: Arc<AtomicUsize>,
    }

    struct FakeState {
        snapshot: ExtensionState,
        updates_during_lookup: Vec<HintSnapshot>,
        callback: Option<HintCallback>,
        targets: Vec<HintTarget>,
    }

    struct FakeSubscription(Arc<AtomicUsize>);

    impl Drop for FakeSubscription {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    impl HintProvider for FakeHints {
        type Subscription = FakeSubscription;
        type Error = &'static str;

        fn lookup(&self, target: &HintTarget) -> Result<Option<HintSnapshot>, Self::Error> {
            let (callback, update, snapshot) = {
                let mut state = self.state.lock().unwrap();
                state.targets.push(target.clone());
                (
                    state.callback.clone(),
                    std::mem::take(&mut state.updates_during_lookup),
                    state.snapshot,
                )
            };
            if let Some(callback) = callback {
                for update in update {
                    callback(update);
                }
            }
            Ok(Some(HintSnapshot {
                target: target.clone(),
                state: snapshot,
                reason: HintUpdateReason::Device,
                caller: None,
            }))
        }

        fn subscribe(
            &self,
            target: &HintTarget,
            callback: HintCallback,
        ) -> Result<Self::Subscription, Self::Error> {
            let mut state = self.state.lock().unwrap();
            state.targets.push(target.clone());
            state.callback = Some(callback);
            Ok(FakeSubscription(Arc::clone(&self.drops)))
        }
    }

    fn target() -> HintTarget {
        HintTarget::parse("4000@internal").unwrap()
    }

    fn update(state: ExtensionState) -> HintSnapshot {
        HintSnapshot {
            target: target(),
            state,
            reason: HintUpdateReason::Device,
            caller: None,
        }
    }

    fn setup(
        snapshot: ExtensionState,
        during_lookup: Option<HintSnapshot>,
    ) -> (
        BlfSubscriptions<FakeHints>,
        mpsc::UnboundedReceiver<BlfEvent>,
        Arc<Mutex<FakeState>>,
        Arc<AtomicUsize>,
    ) {
        let state = Arc::new(Mutex::new(FakeState {
            snapshot,
            updates_during_lookup: during_lookup.into_iter().collect(),
            callback: None,
            targets: Vec::new(),
        }));
        let drops = Arc::new(AtomicUsize::new(0));
        let provider = FakeHints {
            state: Arc::clone(&state),
            drops: Arc::clone(&drops),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        (BlfSubscriptions::new(provider, tx), rx, state, drops)
    }

    fn definition() -> BlfSpeedDialDefinition {
        BlfSpeedDialDefinition {
            instance: 2,
            number: "4000".into(),
            display_name: "Desk".into(),
        }
    }

    #[test]
    fn maps_every_required_extension_state() {
        let cases = [
            (ExtensionState::IDLE, BlfState::Idle),
            (ExtensionState::RINGING, BlfState::Ringing),
            (ExtensionState::IN_USE, BlfState::Busy),
            (ExtensionState::BUSY, BlfState::Busy),
            (ExtensionState::ON_HOLD, BlfState::Held),
            (ExtensionState::UNAVAILABLE, BlfState::Unavailable),
            (ExtensionState::REMOVED, BlfState::Unknown),
        ];
        for (extension, blf) in cases {
            assert_eq!(map_extension_state(extension), blf);
        }
        assert_eq!(
            map_extension_state(ExtensionState::from_raw(
                ExtensionState::RINGING.raw() | ExtensionState::IN_USE.raw()
            )),
            BlfState::Ringing
        );
    }

    #[test]
    fn update_during_initial_lookup_wins_over_the_stale_snapshot() {
        let (mut subscriptions, mut events, _, _) =
            setup(ExtensionState::IDLE, Some(update(ExtensionState::ON_HOLD)));
        subscriptions
            .subscribe(
                DeviceId::new("SEP001122334455").unwrap(),
                &definition(),
                &target(),
            )
            .unwrap();

        let event = events.try_recv().unwrap();
        assert_eq!(event.state, BlfState::Held);
        assert!(events.try_recv().is_err());
        assert!(subscriptions.is_current(&event));
    }

    #[test]
    fn terminal_hint_during_initialization_is_not_overwritten_by_a_late_snapshot() {
        let (mut subscriptions, mut events, state, _) = setup(ExtensionState::IDLE, None);
        state.lock().unwrap().updates_during_lookup = vec![
            update(ExtensionState::REMOVED),
            update(ExtensionState::IDLE),
        ];
        let device = DeviceId::new("SEP001122334455").unwrap();
        subscriptions
            .subscribe(device.clone(), &definition(), &target())
            .unwrap();
        let event = events.try_recv().unwrap();
        assert!(event.is_terminal());
        assert_eq!(event.state, BlfState::Unknown);
        subscriptions.retry_terminal(&event);
        assert!(!subscriptions.is_current(&event));
        assert!(subscriptions.retry_due(
            &device,
            definition().instance,
            Instant::now() + Duration::from_secs(2)
        ));
    }

    #[test]
    fn replacing_and_removing_a_device_invalidates_queued_generations() {
        let (mut subscriptions, mut events, _, drops) = setup(ExtensionState::IDLE, None);
        let device = DeviceId::new("SEP001122334455").unwrap();
        subscriptions
            .subscribe(device.clone(), &definition(), &target())
            .unwrap();
        let old = events.try_recv().unwrap();

        subscriptions
            .subscribe(device.clone(), &definition(), &target())
            .unwrap();
        let current = events.try_recv().unwrap();
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!subscriptions.is_current(&old));
        assert_ne!(old.generation(), current.generation());
        assert!(subscriptions.is_current(&current));

        subscriptions.remove_device(&device);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(!subscriptions.is_current(&current));
        assert!(subscriptions.is_empty());
    }

    #[test]
    fn clear_unsubscribes_every_device_for_reload_or_unload() {
        let (mut subscriptions, mut events, _, drops) = setup(ExtensionState::IDLE, None);
        subscriptions
            .subscribe(
                DeviceId::new("SEP001122334455").unwrap(),
                &definition(),
                &target(),
            )
            .unwrap();
        let mut second = definition();
        second.instance = 3;
        subscriptions
            .subscribe(
                DeviceId::new("SEP00AABBCCDDEE").unwrap(),
                &second,
                &target(),
            )
            .unwrap();
        let first_event = events.try_recv().unwrap();
        let second_event = events.try_recv().unwrap();

        subscriptions.clear();

        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(subscriptions.is_empty());
        assert!(!subscriptions.is_current(&first_event));
        assert!(!subscriptions.is_current(&second_event));
    }

    #[test]
    fn terminal_hint_update_retires_generation_and_schedules_retry() {
        let (mut subscriptions, mut events, state, drops) = setup(ExtensionState::IDLE, None);
        let device = DeviceId::new("SEP001122334455").unwrap();
        subscriptions
            .subscribe(device.clone(), &definition(), &target())
            .unwrap();
        let initial = events.try_recv().unwrap();
        assert!(subscriptions.is_current(&initial));

        state.lock().unwrap().callback.as_ref().unwrap()(update(ExtensionState::REMOVED));
        let removed = events.try_recv().unwrap();
        assert_eq!(removed.state, BlfState::Unknown);
        assert!(removed.is_terminal());
        subscriptions.retry_terminal(&removed);

        assert!(!subscriptions.is_current(&removed));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(!subscriptions.retry_due(&device, definition().instance, Instant::now()));
        assert!(subscriptions.retry_due(
            &device,
            definition().instance,
            Instant::now() + Duration::from_secs(2)
        ));
    }

    #[test]
    fn same_state_caller_only_update_is_delivered() {
        let (mut subscriptions, mut events, state, _) = setup(ExtensionState::RINGING, None);
        subscriptions
            .subscribe(
                DeviceId::new("SEP001122334455").unwrap(),
                &definition(),
                &target(),
            )
            .unwrap();
        let initial = events.try_recv().unwrap();
        assert_eq!(initial.state, BlfState::Ringing);
        assert_eq!(initial.caller, None);

        state.lock().unwrap().callback.as_ref().unwrap()(HintSnapshot {
            target: target(),
            state: ExtensionState::RINGING,
            reason: HintUpdateReason::Device,
            caller: Some(HintCaller {
                name: Some("Taylor".into()),
                number: Some("5550100".into()),
                name_presentation: 0,
                number_presentation: 0,
            }),
        });

        assert_eq!(
            events.try_recv().unwrap().caller,
            Some(BlfCallerInfo {
                name: "Taylor".into(),
                number: "5550100".into(),
            })
        );
    }

    #[test]
    fn restricted_caller_information_is_removed_before_delivery() {
        let key = BlfKey {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            instance: 2,
        };
        let allowed = normalize_event(
            &key,
            1,
            HintSnapshot {
                caller: Some(HintCaller {
                    name: Some("Taylor".into()),
                    number: Some("5550100".into()),
                    name_presentation: 0,
                    number_presentation: 1,
                }),
                ..update(ExtensionState::RINGING)
            },
        );
        assert_eq!(
            allowed.caller,
            Some(BlfCallerInfo {
                name: "Taylor".into(),
                number: "5550100".into(),
            })
        );

        let restricted = normalize_event(
            &key,
            1,
            HintSnapshot {
                caller: Some(HintCaller {
                    name: Some("Taylor".into()),
                    number: Some("5550100".into()),
                    name_presentation: 0,
                    number_presentation: 0x20,
                }),
                ..update(ExtensionState::RINGING)
            },
        );
        assert_eq!(restricted.caller, None);
    }
}
