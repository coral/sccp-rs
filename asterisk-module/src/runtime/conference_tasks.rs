use std::collections::HashMap;
use std::future::Future;

use tokio::runtime::Handle;
use tokio::task::{Id, JoinSet};

use super::backend::PbxCallId;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ConferenceTaskToken {
    call_id: PbxCallId,
    generation: u64,
}

pub(crate) trait ConferenceTaskCancellation: Send + 'static {
    fn cancel(self);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConferenceTaskStartError {
    AlreadyRunning,
    ShuttingDown,
    GenerationExhausted,
    Exhausted,
}

struct ActiveTask<C> {
    token: ConferenceTaskToken,
    cancellation: C,
}

pub(crate) struct ConferenceTaskRegistry<C> {
    capacity: usize,
    next_generation: u64,
    active: HashMap<PbxCallId, ActiveTask<C>>,
    tasks: JoinSet<ConferenceTaskToken>,
    task_tokens: HashMap<Id, ConferenceTaskToken>,
    shutting_down: bool,
}

impl<C> Default for ConferenceTaskRegistry<C> {
    fn default() -> Self {
        Self::with_capacity(super::mailbox::RUNTIME_MAILBOX_CAPACITY)
    }
}

impl<C> ConferenceTaskRegistry<C> {
    fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0);
        Self {
            capacity,
            next_generation: 1,
            active: HashMap::new(),
            tasks: JoinSet::new(),
            task_tokens: HashMap::new(),
            shutting_down: false,
        }
    }
}

impl<C: ConferenceTaskCancellation> ConferenceTaskRegistry<C> {
    pub(crate) fn start<F, Fut>(
        &mut self,
        runtime: &Handle,
        call_id: PbxCallId,
        cancellation: C,
        task: F,
    ) -> Result<ConferenceTaskToken, ConferenceTaskStartError>
    where
        F: FnOnce(ConferenceTaskToken) -> Fut,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if self.shutting_down {
            return Err(ConferenceTaskStartError::ShuttingDown);
        }
        if self.active.contains_key(&call_id) {
            return Err(ConferenceTaskStartError::AlreadyRunning);
        }
        if self.tasks.len() == self.capacity {
            return Err(ConferenceTaskStartError::Exhausted);
        }
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .ok_or(ConferenceTaskStartError::GenerationExhausted)?;
        let token = ConferenceTaskToken {
            call_id,
            generation,
        };
        self.active.insert(
            call_id,
            ActiveTask {
                token,
                cancellation,
            },
        );
        let future = task(token);
        let worker_runtime = runtime.clone();
        let task = self.tasks.spawn_blocking_on(
            move || {
                worker_runtime.block_on(future);
                token
            },
            runtime,
        );
        self.task_tokens.insert(task.id(), token);
        Ok(token)
    }

    pub(crate) fn complete(&mut self, token: ConferenceTaskToken) -> bool {
        if self
            .active
            .get(&token.call_id)
            .is_none_or(|active| active.token != token)
        {
            return false;
        }
        self.active.remove(&token.call_id);
        true
    }

    pub(crate) fn cancel(&mut self, call_id: PbxCallId) -> Option<C> {
        self.active
            .remove(&call_id)
            .map(|active| active.cancellation)
    }

    pub(crate) fn begin_shutdown(&mut self) -> (Vec<C>, JoinSet<ConferenceTaskToken>) {
        self.shutting_down = true;
        let cancellations = self
            .active
            .drain()
            .map(|(_, active)| active.cancellation)
            .collect();
        self.task_tokens.clear();
        (cancellations, std::mem::take(&mut self.tasks))
    }

    pub(crate) fn reap_finished(&mut self) -> Vec<C> {
        let mut cancellations = Vec::new();
        while let Some(result) = self.tasks.try_join_next_with_id() {
            let token = match result {
                Ok((task_id, _)) => {
                    self.task_tokens.remove(&task_id);
                    continue;
                }
                Err(error) => {
                    let task_id = error.id();
                    let Some(token) = self.task_tokens.remove(&task_id) else {
                        continue;
                    };
                    token
                }
            };
            if self
                .active
                .get(&token.call_id)
                .is_some_and(|active| active.token == token)
                && let Some(active) = self.active.remove(&token.call_id)
            {
                cancellations.push(active.cancellation);
            }
        }
        cancellations
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use tokio::sync::Barrier;

    use super::*;

    #[derive(Clone)]
    struct FakeCancellation {
        calls: Arc<Mutex<Vec<PbxCallId>>>,
        call_id: PbxCallId,
    }

    impl ConferenceTaskCancellation for FakeCancellation {
        fn cancel(self) {
            self.calls.lock().unwrap().push(self.call_id);
        }
    }

    fn cancellation(calls: &Arc<Mutex<Vec<PbxCallId>>>, call_id: u64) -> FakeCancellation {
        FakeCancellation {
            calls: Arc::clone(calls),
            call_id: PbxCallId(call_id),
        }
    }

    #[tokio::test]
    async fn stale_completion_cannot_release_a_replacement_task() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(Barrier::new(2));
        let completions = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ConferenceTaskRegistry::default();
        let first_barrier = Arc::clone(&barrier);
        let first_completions = Arc::clone(&completions);
        let first = registry
            .start(
                &Handle::current(),
                PbxCallId(7),
                cancellation(&calls, 7),
                move |token| async move {
                    first_barrier.wait().await;
                    first_completions.lock().unwrap().push(token);
                },
            )
            .unwrap();
        registry.cancel(PbxCallId(7)).unwrap().cancel();
        let second = registry
            .start(
                &Handle::current(),
                PbxCallId(7),
                cancellation(&calls, 7),
                |_| async {},
            )
            .unwrap();

        barrier.wait().await;
        tokio::task::yield_now().await;
        assert!(!registry.complete(first));
        assert!(registry.complete(second));
        assert_eq!(*calls.lock().unwrap(), [PbxCallId(7)]);
    }

    #[tokio::test]
    async fn cancellation_is_exact_and_independent() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ConferenceTaskRegistry::default();
        let first = registry
            .start(
                &Handle::current(),
                PbxCallId(1),
                cancellation(&calls, 1),
                |_| async {},
            )
            .unwrap();
        let second = registry
            .start(
                &Handle::current(),
                PbxCallId(2),
                cancellation(&calls, 2),
                |_| async {},
            )
            .unwrap();

        registry.cancel(PbxCallId(1)).unwrap().cancel();
        assert!(registry.cancel(PbxCallId(1)).is_none());
        assert!(!registry.complete(first));
        assert!(registry.complete(second));
        assert!(!registry.complete(second));
        assert_eq!(*calls.lock().unwrap(), [PbxCallId(1)]);
    }

    #[tokio::test]
    async fn shutdown_cancels_every_task_and_joins_blocked_work() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new(Barrier::new(3));
        let mut registry = ConferenceTaskRegistry::default();
        for call_id in [1, 2] {
            let barrier = Arc::clone(&barrier);
            registry
                .start(
                    &Handle::current(),
                    PbxCallId(call_id),
                    cancellation(&calls, call_id),
                    move |_| async move {
                        barrier.wait().await;
                    },
                )
                .unwrap();
        }

        let (cancellations, mut tasks) = registry.begin_shutdown();
        for cancellation in cancellations {
            cancellation.cancel();
        }
        assert_eq!(
            registry.start(
                &Handle::current(),
                PbxCallId(3),
                cancellation(&calls, 3),
                |_| async {},
            ),
            Err(ConferenceTaskStartError::ShuttingDown)
        );
        barrier.wait().await;
        while tasks.join_next().await.is_some() {}
        let mut cancelled = calls.lock().unwrap().clone();
        cancelled.sort_by_key(|call_id| call_id.0);
        assert_eq!(cancelled, [PbxCallId(1), PbxCallId(2)]);
    }

    #[tokio::test]
    async fn cancelled_native_work_retains_admission_until_its_actual_job_is_joined() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ConferenceTaskRegistry::with_capacity(1);
        let (started, running) = tokio::sync::oneshot::channel();
        let (release, blocked) = tokio::sync::oneshot::channel();
        registry
            .start(
                &Handle::current(),
                PbxCallId(1),
                cancellation(&calls, 1),
                |_| async move {
                    let _ = started.send(());
                    let _ = blocked.await;
                },
            )
            .unwrap();
        running.await.unwrap();
        registry.cancel(PbxCallId(1)).unwrap().cancel();
        assert_eq!(
            registry.start(
                &Handle::current(),
                PbxCallId(2),
                cancellation(&calls, 2),
                |_| async {}
            ),
            Err(ConferenceTaskStartError::Exhausted)
        );
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !registry.tasks.is_empty() {
                assert!(registry.reap_finished().is_empty());
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            registry
                .start(
                    &Handle::current(),
                    PbxCallId(2),
                    cancellation(&calls, 2),
                    |_| async {}
                )
                .is_ok()
        );
        let (cancellations, mut jobs) = registry.begin_shutdown();
        for cancellation in cancellations {
            cancellation.cancel();
        }
        while jobs.join_next().await.is_some() {}
        assert_eq!(*calls.lock().unwrap(), [PbxCallId(1), PbxCallId(2)]);
    }

    #[tokio::test]
    async fn panicked_task_releases_only_its_exact_generation() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = ConferenceTaskRegistry::default();
        registry
            .start(
                &Handle::current(),
                PbxCallId(7),
                cancellation(&calls, 7),
                |_| async { panic!("injected conference task failure") },
            )
            .unwrap();

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let cancellations = registry.reap_finished();
                assert!(calls.lock().unwrap().is_empty());
                for cancellation in cancellations {
                    cancellation.cancel();
                }
                if !registry.active.contains_key(&PbxCallId(7)) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();

        assert!(!registry.active.contains_key(&PbxCallId(7)));
        assert_eq!(*calls.lock().unwrap(), [PbxCallId(7)]);
        assert!(
            registry
                .start(
                    &Handle::current(),
                    PbxCallId(7),
                    cancellation(&calls, 7),
                    |_| async {},
                )
                .is_ok()
        );
    }
}
