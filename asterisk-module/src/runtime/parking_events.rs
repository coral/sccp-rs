//! Native parking callbacks reserve their initial and terminal delivery before
//! the native operation starts. The callback lock protects delivery capacity;
//! parking policy and registry state remain with the controller.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use super::mailbox::{
    AdmissionError, MailboxReceiver, MailboxReservation, MailboxSender, QueueSnapshot, mailbox,
};
use crate::call::parking::{ParkingEvent, ParkingEventKind};

pub(crate) struct ParkingEventBatch {
    pub first: ParkingEvent,
    pub latest: Option<ParkingEvent>,
}

impl From<ParkingEvent> for ParkingEventBatch {
    fn from(first: ParkingEvent) -> Self {
        Self {
            first,
            latest: None,
        }
    }
}

struct Pending {
    generation: Arc<()>,
    active_native: usize,
    retired: bool,
    latest: Option<ParkingEvent>,
    initial: Option<MailboxReservation<ParkingEventBatch>>,
    terminal: MailboxReservation<ParkingEventBatch>,
}

struct Inner {
    pending: Mutex<HashMap<String, Pending>>,
    events: MailboxSender<ParkingEventBatch>,
    updates: Notify,
}

#[derive(Clone)]
pub(crate) struct ParkingEvents(Arc<Inner>);

pub(crate) struct ParkingAdmission {
    events: ParkingEvents,
    key: Option<(String, Arc<()>)>,
    created: bool,
}

impl ParkingAdmission {
    pub fn commit(mut self) {
        self.finish(true);
    }

    fn finish(&mut self, succeeded: bool) {
        let Some((key, generation)) = self.key.take() else {
            return;
        };
        let mut pending = self
            .events
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(entry) = pending
            .get_mut(&key)
            .filter(|entry| Arc::ptr_eq(&entry.generation, &generation))
        else {
            return;
        };
        entry.active_native -= 1;
        if entry.active_native == 0
            && entry.initial.is_some()
            && (entry.retired || (!succeeded && self.created))
        {
            pending.remove(&key);
        }
    }
}

impl Drop for ParkingAdmission {
    fn drop(&mut self) {
        self.finish(false);
    }
}

pub(crate) fn parking_events(
    capacity: usize,
) -> (ParkingEvents, MailboxReceiver<ParkingEventBatch>) {
    let (events, receiver) = mailbox(capacity);
    (
        ParkingEvents(Arc::new(Inner {
            pending: Mutex::new(HashMap::new()),
            events,
            updates: Notify::new(),
        })),
        receiver,
    )
}

impl ParkingEvents {
    pub fn reserve(&self, key: String) -> Result<ParkingAdmission, AdmissionError> {
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(entry) = pending.get_mut(&key) {
            entry.active_native = entry
                .active_native
                .checked_add(1)
                .ok_or(AdmissionError::Full)?;
            return Ok(ParkingAdmission {
                events: self.clone(),
                key: Some((key, Arc::clone(&entry.generation))),
                created: false,
            });
        }
        let initial = self.0.events.try_reserve()?;
        let terminal = self.0.events.try_reserve()?;
        let generation = Arc::new(());
        pending.insert(
            key.clone(),
            Pending {
                generation: Arc::clone(&generation),
                active_native: 1,
                retired: false,
                latest: None,
                initial: Some(initial),
                terminal,
            },
        );
        Ok(ParkingAdmission {
            events: self.clone(),
            key: Some((key, generation)),
            created: true,
        })
    }

    pub fn publish(&self, event: ParkingEvent) -> Result<(), AdmissionError> {
        let key = event.parkee_unique_id.clone();
        let terminal = !matches!(
            event.kind,
            ParkingEventKind::Parked | ParkingEventKind::Swap
        );
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if terminal {
            return match pending.remove(&key) {
                Some(entry) => entry.terminal.send(
                    match entry.latest {
                        Some(first) => ParkingEventBatch {
                            first,
                            latest: Some(event),
                        },
                        None => event.into(),
                    },
                    None,
                ),
                None => self.0.events.try_send(event.into(), None),
            };
        }
        if !pending.contains_key(&key) {
            let initial = self.0.events.try_reserve()?;
            let terminal = self.0.events.try_reserve()?;
            pending.insert(
                key.clone(),
                Pending {
                    generation: Arc::new(()),
                    active_native: 0,
                    retired: false,
                    latest: None,
                    initial: Some(initial),
                    terminal,
                },
            );
        }
        let entry = pending
            .get_mut(&key)
            .expect("parking delivery capacity exists");
        match entry.initial.take() {
            Some(initial) => initial.send(event.into(), None),
            None => {
                entry.latest = Some(event);
                self.0.updates.notify_one();
                Ok(())
            }
        }
    }

    #[cfg(any(feature = "asterisk-22", feature = "asterisk-latest"))]
    pub async fn updated(&self) {
        self.0.updates.notified().await;
    }

    pub fn take_latest(&self) -> Option<(ParkingEvent, MailboxReservation<ParkingEventBatch>)> {
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let admission = self.0.events.try_reserve().ok()?;
        let event = pending.values_mut().find_map(|entry| entry.latest.take())?;
        if pending.values().any(|entry| entry.latest.is_some()) {
            self.0.updates.notify_one();
        }
        Some((event, admission))
    }

    pub fn retire(&self, key: &str) {
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        // Native operations retain their accepted completion slot until their
        // actual worker returns; confirmed parked calls retain terminal capacity.
        if let Some(entry) = pending.get_mut(key) {
            entry.retired = true;
            if entry.active_native == 0 && entry.initial.is_some() {
                pending.remove(key);
            }
        }
    }

    /// Native subscription callbacks have been drained before closing delivery.
    pub fn close(&self) {
        self.0.events.close();
        let mut pending = self
            .0
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for (_, entry) in pending.drain() {
            if let Some(latest) = entry.latest {
                let _ = entry.terminal.send(latest.into(), None);
            }
        }
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        self.0.events.snapshot()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(key: &str, kind: ParkingEventKind) -> ParkingEvent {
        ParkingEvent {
            kind,
            lot: "default".into(),
            slot: 701,
            timeout_seconds: 30,
            duration_seconds: 0,
            parker_dial_string: String::new(),
            parkee_channel: String::new(),
            parkee_unique_id: key.into(),
            caller_name: String::new(),
            caller_number: String::new(),
            connected_name: String::new(),
            connected_number: String::new(),
            retriever_channel: String::new(),
        }
    }

    #[tokio::test]
    async fn accepted_initial_and_terminal_survive_unrelated_callback_saturation() {
        let (events, mut receiver) = parking_events(4);
        events.reserve("accepted".into()).unwrap().commit();
        events
            .publish(event("other", ParkingEventKind::Parked))
            .unwrap();
        assert!(
            events
                .publish(event("full", ParkingEventKind::Parked))
                .is_err()
        );
        events
            .publish(event("accepted", ParkingEventKind::Parked))
            .unwrap();
        events
            .publish(event("accepted", ParkingEventKind::Retrieved))
            .unwrap();
        assert_eq!(events.snapshot().outstanding, 4);
        let other = receiver.recv().await.unwrap();
        let initial = receiver.recv().await.unwrap();
        let terminal = receiver.recv().await.unwrap();
        assert_eq!(initial.value.first.kind, ParkingEventKind::Parked);
        assert_eq!(terminal.value.first.kind, ParkingEventKind::Retrieved);
        assert_eq!(events.snapshot().outstanding, 4);
        drop((other, initial, terminal));
        events.close();
        assert!(receiver.recv().await.is_none());
        assert_eq!(events.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn failed_native_operation_returns_reserved_capacity_and_shutdown_drains_callbacks() {
        let (events, mut receiver) = parking_events(2);
        let admission = events.reserve("failed".into()).unwrap();
        assert_eq!(events.snapshot().outstanding, 2);
        drop(admission);
        assert_eq!(events.snapshot().outstanding, 0);
        events.reserve("active".into()).unwrap().commit();
        events
            .publish(event("active", ParkingEventKind::Parked))
            .unwrap();
        events.close();
        assert_eq!(
            receiver.recv().await.unwrap().value.first.kind,
            ParkingEventKind::Parked
        );
        assert!(receiver.recv().await.is_none());
    }

    #[tokio::test]
    async fn repeated_updates_coalesce_before_reserved_terminal_delivery() {
        let (events, mut receiver) = parking_events(2);
        events.reserve("peer".into()).unwrap().commit();
        events
            .publish(event("peer", ParkingEventKind::Parked))
            .unwrap();
        for duration in 1..=1_000 {
            let mut update = event("peer", ParkingEventKind::Swap);
            update.duration_seconds = duration;
            events.publish(update).unwrap();
        }
        events
            .publish(event("peer", ParkingEventKind::Retrieved))
            .unwrap();
        assert_eq!(events.snapshot().outstanding, 2);
        assert_eq!(
            receiver.recv().await.unwrap().value.first.kind,
            ParkingEventKind::Parked
        );
        let terminal = receiver.recv().await.unwrap();
        assert_eq!(terminal.value.first.kind, ParkingEventKind::Swap);
        assert_eq!(terminal.value.first.duration_seconds, 1_000);
        assert_eq!(
            terminal.value.latest.as_ref().unwrap().kind,
            ParkingEventKind::Retrieved
        );
        assert!(events.take_latest().is_none());
    }

    #[tokio::test]
    async fn timeout_releases_unconfirmed_capacity_but_preserves_native_parked_lifetime() {
        let (events, mut receiver) = parking_events(2);
        events.reserve("expired".into()).unwrap().commit();
        events.retire("expired");
        assert_eq!(events.snapshot().outstanding, 0);
        events.reserve("parked".into()).unwrap().commit();
        events
            .publish(event("parked", ParkingEventKind::Parked))
            .unwrap();
        drop(receiver.recv().await.unwrap());
        events.retire("parked");
        assert_eq!(events.snapshot().outstanding, 1);
        events
            .publish(event("parked", ParkingEventKind::GiveUp))
            .unwrap();
        drop(receiver.recv().await.unwrap());
        assert_eq!(events.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn late_abort_does_not_remove_reused_peer_reservation() {
        let (events, mut receiver) = parking_events(2);
        let previous = events.reserve("peer".into()).unwrap();
        events
            .publish(event("peer", ParkingEventKind::Failed))
            .unwrap();
        drop(receiver.recv().await.unwrap());
        events.reserve("peer".into()).unwrap().commit();
        drop(previous);
        assert_eq!(events.snapshot().outstanding, 2);
        events
            .publish(event("peer", ParkingEventKind::Parked))
            .unwrap();
        events
            .publish(event("peer", ParkingEventKind::Retrieved))
            .unwrap();
        assert_eq!(
            receiver.recv().await.unwrap().value.first.kind,
            ParkingEventKind::Parked
        );
        assert_eq!(
            receiver.recv().await.unwrap().value.first.kind,
            ParkingEventKind::Retrieved
        );
    }

    #[tokio::test]
    async fn timeout_cannot_take_capacity_from_a_native_operation_still_running() {
        let (events, mut receiver) = parking_events(2);
        let native = events.reserve("stalled".into()).unwrap();
        events.retire("stalled");
        assert_eq!(events.snapshot().outstanding, 2);
        assert!(events.reserve("other".into()).is_err());
        events
            .publish(event("stalled", ParkingEventKind::Parked))
            .unwrap();
        native.commit();
        events
            .publish(event("stalled", ParkingEventKind::GiveUp))
            .unwrap();
        drop(receiver.recv().await.unwrap());
        drop(receiver.recv().await.unwrap());
        assert_eq!(events.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn close_counts_coalesced_delivery_until_its_worker_finishes() {
        let (events, mut receiver) = parking_events(2);
        events
            .publish(event("peer", ParkingEventKind::Parked))
            .unwrap();
        drop(receiver.recv().await.unwrap());
        events
            .publish(event("peer", ParkingEventKind::Swap))
            .unwrap();
        let (latest, working) = events.take_latest().unwrap();
        assert_eq!(latest.kind, ParkingEventKind::Swap);
        events.close();
        assert_eq!(events.snapshot().outstanding, 1);
        drop(working);
        assert!(receiver.recv().await.is_none());
        assert_eq!(events.snapshot().outstanding, 0);
    }
}
