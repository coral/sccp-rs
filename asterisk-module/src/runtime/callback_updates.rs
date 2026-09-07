//! Bounded native callback delivery. Each key retains its first update and the
//! latest subsequent value; a terminal update stays sticky until consumption.
//! Producers never wait for the worker that may be unregistering their callback.

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::Mutex;

use tokio::sync::Notify;

struct Pending<V> {
    generation: u64,
    first: V,
    latest: Option<V>,
    terminal: bool,
}

struct Queue<K, V> {
    entries: HashMap<K, Pending<V>>,
    order: VecDeque<K>,
}

pub(crate) struct CallbackUpdates<K, V> {
    pending: Mutex<Queue<K, V>>,
    capacity: usize,
    wake: Notify,
}

impl<K: Clone + Eq + Hash, V> CallbackUpdates<K, V> {
    pub fn new(capacity: usize) -> Self {
        Self {
            pending: Mutex::new(Queue {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
            capacity,
            wake: Notify::new(),
        }
    }

    /// A replacement generation supersedes queued values for an old subscription.
    /// Terminal updates cannot be overwritten by later callbacks from that generation.
    pub fn push(&self, key: K, generation: u64, value: V, terminal: bool) -> bool {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let full = pending.entries.len() >= self.capacity;
        match pending.entries.get_mut(&key) {
            Some(entry) if entry.generation == generation => {
                if !entry.terminal {
                    entry.latest = Some(value);
                    entry.terminal = terminal;
                }
            }
            Some(entry) if entry.generation > generation => return true,
            Some(entry) => {
                *entry = Pending {
                    generation,
                    first: value,
                    latest: None,
                    terminal,
                }
            }
            None if full => return false,
            None => {
                pending.order.push_back(key.clone());
                pending.entries.insert(
                    key,
                    Pending {
                        generation,
                        first: value,
                        latest: None,
                        terminal,
                    },
                );
            }
        }
        drop(pending);
        self.wake.notify_one();
        true
    }

    /// Call only after the source has been unregistered and its callbacks joined.
    pub fn discard(&self, mut retired: impl FnMut(&K) -> bool) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        pending.entries.retain(|key, _| !retired(key));
        let Queue { entries, order } = &mut *pending;
        order.retain(|key| entries.contains_key(key));
    }

    pub async fn notified(&self) {
        self.wake.notified().await;
    }

    /// The worker consumes one key at a time, keeping the secondary backlog bounded
    /// to two updates even while native callbacks continue producing.
    pub fn pop(&self) -> Option<(V, Option<V>)> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let key = pending.order.pop_front()?;
        let removed = pending
            .entries
            .remove(&key)
            .map(|entry| (entry.first, entry.latest));
        if !pending.order.is_empty() {
            self.wake.notify_one();
        }
        removed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_keep_initial_and_latest_updates_and_sticky_terminal_state() {
        let updates = CallbackUpdates::new(1);
        assert!(updates.push("button", 1, "initial", false));
        for _ in 0..10_000 {
            assert!(updates.push("button", 1, "intermediate", false));
        }
        assert!(updates.push("button", 1, "removed", true));
        assert!(updates.push("button", 1, "late", false));
        assert!(!updates.push("other", 1, "overflow", false));
        assert_eq!(updates.pop(), Some(("initial", Some("removed"))));
        assert_eq!(updates.pop(), None);
        assert!(updates.push("other", 1, "accepted", false));
    }

    #[test]
    fn replacement_rejects_late_old_generation_callbacks_before_delivery() {
        let updates = CallbackUpdates::new(1);
        updates.push("button", 1, "old", false);
        updates.push("button", 2, "replacement", false);
        updates.push("button", 1, "late old", true);
        assert_eq!(updates.pop(), Some(("replacement", None)));
    }

    #[tokio::test]
    async fn callback_before_wait_preserves_its_wakeup() {
        let updates = CallbackUpdates::new(1);
        updates.push("button", 1, "initial", false);
        updates.notified().await;
        assert_eq!(updates.pop(), Some(("initial", None)));
    }
    #[tokio::test]
    async fn a_continuous_source_does_not_starve_other_subscription_keys() {
        let updates = CallbackUpdates::new(2);
        updates.push("busy", 1, "first", false);
        updates.push("other", 1, "other", false);
        updates.notified().await;
        assert_eq!(updates.pop(), Some(("first", None)));
        updates.push("busy", 1, "replacement", false);
        updates.notified().await;
        assert_eq!(updates.pop(), Some(("other", None)));
        updates.notified().await;
        assert_eq!(updates.pop(), Some(("replacement", None)));
        updates.push("retired", 1, "discard", false);
        updates.discard(|key| *key == "retired");
        assert_eq!(updates.pop(), None);
    }
}
