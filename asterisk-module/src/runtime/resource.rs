//! Quiescent replacement for retained resources used across callback threads.

use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BindingPhase {
    Active,
    Suspended,
    Closed,
}

#[derive(Debug)]
struct BindingState<T> {
    phase: BindingPhase,
    active_operations: usize,
    current: Option<T>,
}

/// Owns the current retained resource and serializes replacement against use.
pub(crate) struct ResourceBinding<T: Clone> {
    state: Mutex<BindingState<T>>,
    changed: Condvar,
}

impl<T: Clone> ResourceBinding<T> {
    pub(crate) fn new(resource: T) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BindingState {
                phase: BindingPhase::Active,
                active_operations: 0,
                current: Some(resource),
            }),
            changed: Condvar::new(),
        })
    }

    pub(crate) fn enter(self: &Arc<Self>) -> Option<ResourcePermit<T>> {
        let mut state = lock(&self.state);
        while state.phase == BindingPhase::Suspended {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if state.phase == BindingPhase::Closed {
            return None;
        }
        let resource = state.current.as_ref()?.clone();
        state.active_operations += 1;
        Some(ResourcePermit {
            binding: Arc::clone(self),
            resource,
        })
    }

    /// Enter only when the resource is immediately available. Multi-resource
    /// operations use this to avoid holding one permit while waiting for a
    /// different resource to finish replacement.
    pub(crate) fn try_enter(self: &Arc<Self>) -> Option<ResourcePermit<T>> {
        let mut state = lock(&self.state);
        if state.phase != BindingPhase::Active {
            return None;
        }
        let resource = state.current.as_ref()?.clone();
        state.active_operations += 1;
        Some(ResourcePermit {
            binding: Arc::clone(self),
            resource,
        })
    }

    pub(crate) fn suspend(&self) -> bool {
        let mut state = lock(&self.state);
        if state.phase == BindingPhase::Closed {
            return false;
        }
        state.phase = BindingPhase::Suspended;
        while state.active_operations != 0 && state.phase != BindingPhase::Closed {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        state.phase != BindingPhase::Closed
    }

    /// Close admission only if no operation is in flight. Native unload paths
    /// must not wait while holding loader locks needed by an active operation,
    /// or when an operation re-enters unload on its own callback thread.
    pub(crate) fn try_suspend(&self) -> bool {
        let mut state = lock(&self.state);
        if state.phase != BindingPhase::Active || state.active_operations != 0 {
            return false;
        }
        state.phase = BindingPhase::Suspended;
        true
    }

    pub(crate) fn resume(&self) -> bool {
        let mut state = lock(&self.state);
        if state.phase != BindingPhase::Suspended {
            return false;
        }
        state.phase = BindingPhase::Active;
        self.changed.notify_all();
        true
    }

    pub(crate) fn replace_quiescent(&self, resource: T) -> bool {
        let mut state = lock(&self.state);
        if state.phase == BindingPhase::Closed || state.active_operations != 0 {
            return false;
        }
        state.current = Some(resource);
        true
    }

    pub(crate) fn close(&self) -> Option<T> {
        let mut state = lock(&self.state);
        state.phase = BindingPhase::Closed;
        self.changed.notify_all();
        state.current.take()
    }

    pub(crate) fn is_closed(&self) -> bool {
        lock(&self.state).phase == BindingPhase::Closed
    }
}

/// Keeps an operation's retained resource alive and releases its gate on drop.
pub(crate) struct ResourcePermit<T: Clone> {
    binding: Arc<ResourceBinding<T>>,
    resource: T,
}

impl<T: Clone> ResourcePermit<T> {
    pub(crate) const fn resource(&self) -> &T {
        &self.resource
    }
}

impl<T: Clone> Drop for ResourcePermit<T> {
    fn drop(&mut self) {
        let mut state = lock(&self.binding.state);
        debug_assert_ne!(state.active_operations, 0);
        state.active_operations -= 1;
        if state.active_operations == 0 {
            self.binding.changed.notify_all();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct Token(u32);

    #[test]
    fn suspension_quiesces_operations_before_replacement_and_resume() {
        let binding = ResourceBinding::new(Token(1));
        let active = binding.enter().expect("initial operation");

        let suspending = Arc::clone(&binding);
        let suspended = thread::spawn(move || suspending.suspend());
        while lock(&binding.state).phase != BindingPhase::Suspended {
            thread::yield_now();
        }

        let waiting = Arc::clone(&binding);
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let entrant = thread::spawn(move || {
            entered_tx.send(waiting.enter()).expect("entry result");
        });
        assert!(matches!(
            entered_rx.recv_timeout(Duration::from_millis(25)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));

        drop(active);
        assert!(suspended.join().expect("suspension thread"));
        assert!(binding.replace_quiescent(Token(2)));
        assert!(binding.resume());

        let entered = entered_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("resumed entry")
            .expect("open binding");
        assert_eq!(entered.resource(), &Token(2));
        drop(entered);
        entrant.join().expect("entry thread");

        assert_eq!(binding.close(), Some(Token(2)));
        assert!(binding.is_closed());
        assert!(binding.enter().is_none());
    }

    #[test]
    fn active_operations_prevent_uncoordinated_replacement() {
        let binding = ResourceBinding::new(Token(1));
        let active = binding.enter().expect("active operation");
        assert!(!binding.replace_quiescent(Token(2)));
        assert_eq!(
            binding.try_enter().map(|permit| *permit.resource()),
            Some(Token(1))
        );
        drop(active);
        assert!(binding.replace_quiescent(Token(2)));
        assert_eq!(
            binding.try_enter().map(|permit| *permit.resource()),
            Some(Token(2))
        );
    }

    #[test]
    fn closing_a_suspended_binding_releases_waiters_without_resuming() {
        let binding = ResourceBinding::new(Token(1));
        let active = binding.enter().expect("active operation");

        let suspending = Arc::clone(&binding);
        let suspended = thread::spawn(move || suspending.suspend());
        while lock(&binding.state).phase != BindingPhase::Suspended {
            thread::yield_now();
        }

        assert_eq!(binding.close(), Some(Token(1)));
        drop(active);
        assert!(!suspended.join().expect("suspension thread"));
        assert!(!binding.resume());
        assert!(binding.enter().is_none());
    }

    #[test]
    fn nonblocking_entry_never_bypasses_suspension() {
        let binding = ResourceBinding::new(Token(1));
        assert!(binding.suspend());
        assert!(binding.try_enter().is_none());
        assert!(binding.resume());
        assert_eq!(
            binding.try_enter().map(|permit| *permit.resource()),
            Some(Token(1))
        );
    }
    #[test]
    fn reentrant_unload_refuses_inflight_operations_without_closing_admission() {
        let binding = ResourceBinding::new(());
        let operation = binding.try_enter().unwrap();
        assert!(!binding.try_suspend());
        assert!(binding.try_enter().is_some());
        drop(operation);
        assert!(binding.try_suspend());
        assert!(binding.try_enter().is_none());
        assert!(binding.resume());
        assert!(binding.try_enter().is_some());
    }
}
