//! Typed state-owner and effect-completion harness, independent of Asterisk.
//!
//! State transitions are synchronous and return owned plans. Effects run as
//! tracked async or blocking jobs and cannot borrow the state. Admitted work
//! retains resource reservations and completion capacity through every effect and
//! compensation step, even when its requester has stopped waiting.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::hash::Hash;
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::time::Instant;

use tokio::sync::oneshot;
use tokio::task::{AbortHandle, Id, JoinError, JoinSet};

use super::mailbox::{
    AdmissionError, Admitted, MailboxReceiver, MailboxSender, QueueSnapshot, WorkPermit, mailbox,
};

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct OperationId(u64);

pub(crate) enum Step<R, E> {
    Finished(R),
    Effect(E),
}

/// Implementations own their logical registries. No handles to mutable state or
/// arbitrary caller-supplied closures cross this boundary. Completion IDs are
/// assigned by the owner and remain stable during multi-step compensation.
pub(crate) trait RuntimeState: Send + 'static {
    type Command: Send + 'static;
    type Reply: Send + 'static;
    type Resource: Clone + Eq + Hash + Send + 'static;
    type Effect: Send + 'static;
    type Completion: Send + 'static;

    /// Return conservative resource keys that remain valid while queued. Prepare
    /// must revalidate current identities and generations before producing I/O.
    fn resources(&self, command: &Self::Command) -> Vec<Self::Resource>;
    /// Record resource intents before a command waits. This lets later commands
    /// reserve members introduced by an earlier queued operation.
    fn admit(&mut self, _id: OperationId, command: &Self::Command) -> Vec<Self::Resource> {
        self.resources(command)
    }
    /// Retire admission intents when a queued operation expires without effects.
    fn expired(&mut self, _id: OperationId) {}

    fn prepare(
        &mut self,
        id: OperationId,
        command: Self::Command,
    ) -> Step<Self::Reply, Self::Effect>;
    fn complete(
        &mut self,
        id: OperationId,
        result: Self::Completion,
    ) -> Step<Self::Reply, Self::Effect>;
    /// A worker panic/cancellation is an uncertain effect outcome. The state
    /// must finish or compensate it, rather than silently freeing reservations.
    fn failed(&mut self, id: OperationId) -> Step<Self::Reply, Self::Effect>;
}

pub(crate) trait EffectExecutor<E, C>: Send + 'static {
    /// Register the actual effect in this set, using spawn_blocking for native
    /// work. Do not return a wrapper task that can detach a nested native job.
    fn spawn(&self, effect: E, workers: &mut JoinSet<C>) -> AbortHandle;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum RequestError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("runtime request expired before execution")]
    ExpiredBeforeExecution,
    #[error("runtime response deadline elapsed; execution may have started")]
    ResponseDeadlineElapsed,
    #[error("runtime owner stopped before replying; execution may have started")]
    OwnerStopped,
    #[error("runtime operation identifier space is exhausted")]
    OperationIdExhausted,
}

enum Reply<R> {
    Async(oneshot::Sender<Result<R, RequestError>>),
    Sync(std::sync::mpsc::SyncSender<Result<R, RequestError>>),
}

impl<R> Reply<R> {
    fn send(self, result: Result<R, RequestError>) {
        match self {
            Self::Async(reply) => {
                let _ = reply.send(result);
            }
            Self::Sync(reply) => {
                let _ = reply.send(result);
            }
        }
    }
}

struct Request<C, R> {
    command: C,
    reply: Reply<R>,
}

pub(crate) struct RuntimeHandle<C, R> {
    requests: MailboxSender<Request<C, R>>,
}

impl<C, R> Clone for RuntimeHandle<C, R> {
    fn clone(&self) -> Self {
        Self {
            requests: self.requests.clone(),
        }
    }
}

impl<C, R> RuntimeHandle<C, R> {
    pub fn snapshot(&self) -> QueueSnapshot {
        self.requests.snapshot()
    }

    pub fn close(&self) {
        self.requests.close();
    }

    pub async fn request(&self, command: C, deadline: Option<Instant>) -> Result<R, RequestError> {
        let (reply, receipt) = oneshot::channel();
        self.requests
            .send(
                Request {
                    command,
                    reply: Reply::Async(reply),
                },
                deadline,
            )
            .await?;
        match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline.into(), receipt)
                .await
                .map_err(|_| RequestError::ResponseDeadlineElapsed)?,
            None => receipt.await,
        }
        .map_err(|_| RequestError::OwnerStopped)?
    }

    /// Admission is immediate. Native callers wait on the returned receipt only
    /// after releasing native locks required by the planned effect.
    pub fn try_request(
        &self,
        command: C,
        deadline: Option<Instant>,
    ) -> Result<std::sync::mpsc::Receiver<Result<R, RequestError>>, AdmissionError> {
        let (reply, receipt) = std::sync::mpsc::sync_channel(1);
        self.requests.try_send(
            Request {
                command,
                reply: Reply::Sync(reply),
            },
            deadline,
        )?;
        Ok(receipt)
    }
}

struct Waiting<S: RuntimeState> {
    id: OperationId,
    resources: Vec<S::Resource>,
    request: Admitted<Request<S::Command, S::Reply>>,
    ready: bool,
}

struct Active<S: RuntimeState> {
    resources: Vec<S::Resource>,
    reply: Reply<S::Reply>,
    _completion_slot: WorkPermit,
}

pub(crate) struct RuntimeOwner<S: RuntimeState, E> {
    state: S,
    executor: E,
    requests: MailboxReceiver<Request<S::Command, S::Reply>>,
    waiting: HashMap<OperationId, Waiting<S>>,
    ready: VecDeque<OperationId>,
    deadlines: BTreeSet<(Instant, OperationId)>,
    order: HashMap<S::Resource, VecDeque<OperationId>>,
    active: HashMap<OperationId, Active<S>>,
    workers: JoinSet<S::Completion>,
    worker_ids: HashMap<Id, OperationId>,
    next_operation: u64,
}

enum OwnerTurn<C, R, T> {
    Deadline,
    Request(Option<Admitted<Request<C, R>>>),
    Completion(Result<(Id, T), JoinError>),
}

async fn wait_for_deadline(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

impl<S, E> RuntimeOwner<S, E>
where
    S: RuntimeState,
    E: EffectExecutor<S::Effect, S::Completion>,
{
    pub fn new(
        state: S,
        executor: E,
        capacity: usize,
    ) -> (RuntimeHandle<S::Command, S::Reply>, Self) {
        let (sender, requests) = mailbox(capacity);
        (
            RuntimeHandle { requests: sender },
            Self {
                state,
                executor,
                requests,
                waiting: HashMap::new(),
                ready: VecDeque::new(),
                deadlines: BTreeSet::new(),
                order: HashMap::new(),
                active: HashMap::new(),
                workers: JoinSet::new(),
                worker_ids: HashMap::new(),
                next_operation: 1,
            },
        )
    }

    /// Closing admission drains accepted work and joins every dispatched task.
    /// This future must itself be joined by its module; aborting it is not unload.
    pub async fn run(mut self) -> S {
        let mut accepting = true;
        while accepting || !self.waiting.is_empty() || !self.workers.is_empty() {
            let deadline = self.deadlines.first().map(|(deadline, _)| *deadline);
            let turn = tokio::select! {
                // Deadlines precede completions so continuously ready effects
                // cannot starve expiry. Completed results never need admission.
                biased;
                _ = wait_for_deadline(deadline) => OwnerTurn::Deadline,
                result = self.workers.join_next_with_id(), if !self.workers.is_empty() => {
                    OwnerTurn::Completion(result.expect("nonempty worker set"))
                }
                request = self.requests.recv(), if accepting => OwnerTurn::Request(request),
            };
            // A failed transition can leave domain state inconsistent. Reject
            // remaining work, but join native effects before propagating panic.
            // Otherwise unwinding would drop JoinSet and detach blocking work.
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| {
                self.process_turn(turn, &mut accepting);
                self.start_ready();
            })) {
                self.requests.close();
                self.waiting.clear();
                while let Some(request) = self.requests.recv().await {
                    request.value.reply.send(Err(RequestError::OwnerStopped));
                }
                while self.workers.join_next().await.is_some() {}
                self.active.clear();
                resume_unwind(panic);
            }
        }
        debug_assert!(self.active.is_empty());
        debug_assert!(self.worker_ids.is_empty());
        self.state
    }

    fn process_turn(
        &mut self,
        turn: OwnerTurn<S::Command, S::Reply, S::Completion>,
        accepting: &mut bool,
    ) {
        match turn {
            OwnerTurn::Deadline => self.expire_waiting(),
            OwnerTurn::Completion(result) => {
                let step = match result {
                    Ok((task, completion)) => {
                        let id = self.worker_ids.remove(&task).expect("tracked worker");
                        (id, self.state.complete(id, completion))
                    }
                    Err(error) => {
                        let id = self.worker_ids.remove(&error.id()).expect("tracked worker");
                        (id, self.state.failed(id))
                    }
                };
                self.advance(step.0, step.1);
            }
            OwnerTurn::Request(None) => *accepting = false,
            OwnerTurn::Request(Some(request)) => {
                let Some(next) = self.next_operation.checked_add(1) else {
                    request
                        .value
                        .reply
                        .send(Err(RequestError::OperationIdExhausted));
                    return;
                };
                let id = OperationId(self.next_operation);
                self.next_operation = next;
                let mut resources = self.state.admit(id, &request.value.command);
                let mut unique = std::collections::HashSet::new();
                resources.retain(|key| unique.insert(key.clone()));
                for resource in &resources {
                    self.order
                        .entry(resource.clone())
                        .or_default()
                        .push_back(id);
                }
                if let Some(deadline) = request.deadline {
                    self.deadlines.insert((deadline, id));
                }
                self.waiting.insert(
                    id,
                    Waiting {
                        id,
                        resources,
                        request,
                        ready: false,
                    },
                );
                self.mark_ready(id);
            }
        }
    }

    fn mark_ready(&mut self, id: OperationId) {
        let Some(waiting) = self.waiting.get_mut(&id) else {
            return;
        };
        if !waiting.ready
            && waiting
                .resources
                .iter()
                .all(|key| self.order.get(key).and_then(|queue| queue.front()) == Some(&id))
        {
            waiting.ready = true;
            self.ready.push_back(id);
        }
    }

    fn release_resources(&mut self, id: OperationId, resources: Vec<S::Resource>) {
        let mut candidates = Vec::new();
        for resource in resources {
            let queue = self.order.get_mut(&resource).expect("reserved resource");
            // Expired waiting requests can be in the middle of a resource lane.
            if queue.front() == Some(&id) {
                queue.pop_front();
            } else {
                queue.retain(|queued| *queued != id);
            }
            if let Some(next) = queue.front() {
                candidates.push(*next);
            } else {
                self.order.remove(&resource);
            }
        }
        for next in candidates {
            self.mark_ready(next);
        }
    }

    fn expire_waiting(&mut self) {
        let now = Instant::now();
        while let Some((deadline, id)) = self.deadlines.first().copied() {
            if deadline > now {
                break;
            }
            self.deadlines.remove(&(deadline, id));
            if let Some(waiting) = self.waiting.remove(&id) {
                self.state.expired(id);
                waiting.request.record_expiration();
                waiting
                    .request
                    .value
                    .reply
                    .send(Err(RequestError::ExpiredBeforeExecution));
                self.release_resources(id, waiting.resources);
            }
        }
    }

    fn start_ready(&mut self) {
        while let Some(id) = self.ready.pop_front() {
            let Some(waiting) = self.waiting.remove(&id) else {
                continue;
            };
            if let Some(deadline) = waiting.request.deadline {
                self.deadlines.remove(&(deadline, id));
            }
            if waiting.request.is_expired(Instant::now()) {
                self.state.expired(id);
                waiting.request.record_expiration();
                waiting
                    .request
                    .value
                    .reply
                    .send(Err(RequestError::ExpiredBeforeExecution));
                self.release_resources(id, waiting.resources);
                continue;
            }
            let (request, permit) = waiting.request.into_parts();
            self.active.insert(
                waiting.id,
                Active {
                    resources: waiting.resources,
                    reply: request.reply,
                    _completion_slot: permit,
                },
            );
            let step = self.state.prepare(waiting.id, request.command);
            self.advance(waiting.id, step);
        }
    }

    fn advance(&mut self, id: OperationId, step: Step<S::Reply, S::Effect>) {
        match step {
            Step::Finished(reply) => {
                let active = self.active.remove(&id).expect("active operation");
                self.release_resources(id, active.resources);
                active.reply.send(Ok(reply));
            }
            Step::Effect(effect) => {
                let task = self.executor.spawn(effect, &mut self.workers);
                self.worker_ids.insert(task.id(), id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct Command {
        keys: Vec<u64>,
        value: u64,
        gate: Option<oneshot::Receiver<()>>,
        panic: bool,
    }

    #[derive(Default)]
    struct State {
        starts: Vec<u64>,
        pending: HashMap<OperationId, u64>,
        commits: Vec<u64>,
        failures: usize,
        panic_on_prepare: Option<u64>,
        admitted: HashMap<OperationId, (u64, Vec<u64>)>,
        expired: Vec<u64>,
        inherit_queued_members: bool,
    }

    impl RuntimeState for State {
        type Command = Command;
        type Reply = u64;
        type Resource = u64;
        type Effect = Command;
        type Completion = u64;
        fn resources(&self, c: &Command) -> Vec<u64> {
            c.keys.clone()
        }
        fn admit(&mut self, id: OperationId, c: &Command) -> Vec<u64> {
            self.admitted.insert(id, (c.value, c.keys.clone()));
            let mut resources = self.resources(c);
            if self.inherit_queued_members {
                for (_, keys) in self.admitted.values() {
                    if keys.first() == c.keys.first() {
                        resources.extend(keys);
                    }
                }
            }
            resources
        }
        fn expired(&mut self, id: OperationId) {
            self.expired.push(self.admitted.remove(&id).unwrap().0);
        }
        fn prepare(&mut self, id: OperationId, c: Command) -> Step<u64, Command> {
            assert_ne!(self.panic_on_prepare, Some(c.value), "fake state failure");
            self.starts.push(c.value);
            self.pending.insert(id, c.value);
            Step::Effect(c)
        }
        fn complete(&mut self, id: OperationId, result: u64) -> Step<u64, Command> {
            self.admitted.remove(&id);
            assert_eq!(self.pending.remove(&id), Some(result));
            self.commits.push(result);
            Step::Finished(result)
        }
        fn failed(&mut self, id: OperationId) -> Step<u64, Command> {
            self.failures += 1;
            let value = self.pending[&id];
            // A failed native effect can require a second effect to compensate.
            Step::Effect(Command {
                keys: vec![],
                value,
                gate: None,
                panic: false,
            })
        }
    }

    struct Executor;
    impl EffectExecutor<Command, u64> for Executor {
        fn spawn(&self, c: Command, workers: &mut JoinSet<u64>) -> AbortHandle {
            workers.spawn(async move {
                if let Some(gate) = c.gate {
                    let _ = gate.await;
                }
                assert!(!c.panic, "fake native failure");
                c.value
            })
        }
    }

    fn command(keys: &[u64], value: u64) -> Command {
        Command {
            keys: keys.to_vec(),
            value,
            gate: None,
            panic: false,
        }
    }

    async fn receive(
        rx: std::sync::mpsc::Receiver<Result<u64, RequestError>>,
    ) -> Result<u64, RequestError> {
        tokio::time::timeout(Duration::from_secs(2), async move {
            loop {
                match rx.try_recv() {
                    Ok(value) => return value,
                    Err(std::sync::mpsc::TryRecvError::Empty) => tokio::task::yield_now().await,
                    Err(error) => panic!("missing reply: {error}"),
                }
            }
        })
        .await
        .expect("reply deadline")
    }

    #[tokio::test]
    async fn stalled_effect_does_not_block_other_resources_or_expiry() {
        let (handle, owner) = RuntimeOwner::new(State::default(), Executor, 4);
        let (release, gate) = oneshot::channel();
        let slow = handle
            .try_request(
                Command {
                    gate: Some(gate),
                    ..command(&[1], 1)
                },
                None,
            )
            .unwrap();
        let expired = handle
            .try_request(
                command(&[1], 2),
                Some(Instant::now() + Duration::from_millis(20)),
            )
            .unwrap();
        let fast = handle.try_request(command(&[2], 3), None).unwrap();
        let task = tokio::spawn(owner.run());
        assert_eq!(receive(fast).await, Ok(3));
        assert_eq!(
            receive(expired).await,
            Err(RequestError::ExpiredBeforeExecution)
        );
        assert_eq!(handle.snapshot().outstanding, 1);
        handle.close();
        assert!(!task.is_finished());
        release.send(()).unwrap();
        assert_eq!(receive(slow).await, Ok(1));
        let state = task.await.unwrap();
        assert_eq!(state.starts, [1, 3]);
        assert_eq!(state.expired, [2]);
        assert!(state.admitted.is_empty());
        assert_eq!(handle.snapshot().expired, 1);
        assert_eq!(handle.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn overlapping_multi_resource_operations_preserve_causal_order() {
        let (handle, owner) = RuntimeOwner::new(State::default(), Executor, 4);
        let (release, gate) = oneshot::channel();
        let first = handle
            .try_request(
                Command {
                    gate: Some(gate),
                    ..command(&[1], 1)
                },
                None,
            )
            .unwrap();
        let second = handle.try_request(command(&[1, 2], 2), None).unwrap();
        let third = handle.try_request(command(&[2], 3), None).unwrap();
        let fourth = handle.try_request(command(&[3], 4), None).unwrap();
        let task = tokio::spawn(owner.run());
        assert_eq!(receive(fourth).await, Ok(4));
        assert!(second.try_recv().is_err());
        assert!(third.try_recv().is_err());
        release.send(()).unwrap();
        assert_eq!(receive(first).await, Ok(1));
        assert_eq!(receive(second).await, Ok(2));
        assert_eq!(receive(third).await, Ok(3));
        handle.close();
        assert_eq!(task.await.unwrap().starts, [1, 4, 2, 3]);
    }

    #[tokio::test]
    async fn destruction_reserves_members_introduced_by_an_earlier_queued_operation() {
        let state = State {
            inherit_queued_members: true,
            ..State::default()
        };
        let (handle, owner) = RuntimeOwner::new(state, Executor, 8);
        let (release_create, create_gate) = oneshot::channel();
        let (release_destroy, destroy_gate) = oneshot::channel();
        let create = handle
            .try_request(
                Command {
                    gate: Some(create_gate),
                    ..command(&[1], 1)
                },
                None,
            )
            .unwrap();
        let add = handle.try_request(command(&[1, 2], 2), None).unwrap();
        let destroy = handle
            .try_request(
                Command {
                    gate: Some(destroy_gate),
                    ..command(&[1], 3)
                },
                None,
            )
            .unwrap();
        let same_call = handle.try_request(command(&[2], 4), None).unwrap();
        let unrelated = handle.try_request(command(&[3], 5), None).unwrap();
        let task = tokio::spawn(owner.run());
        assert_eq!(receive(unrelated).await, Ok(5));
        release_create.send(()).unwrap();
        assert_eq!(receive(create).await, Ok(1));
        assert_eq!(receive(add).await, Ok(2));
        assert!(same_call.try_recv().is_err());
        release_destroy.send(()).unwrap();
        assert_eq!(receive(destroy).await, Ok(3));
        assert_eq!(receive(same_call).await, Ok(4));
        handle.close();
        let state = task.await.unwrap();
        assert_eq!(state.starts, [1, 5, 2, 3, 4]);
        assert!(state.admitted.is_empty());
    }

    #[tokio::test]
    async fn completion_and_compensation_progress_at_full_admission() {
        let (handle, owner) = RuntimeOwner::new(State::default(), Executor, 1);
        let receipt = handle
            .try_request(
                Command {
                    panic: true,
                    ..command(&[1], 7)
                },
                None,
            )
            .unwrap();
        assert!(matches!(
            handle.try_request(command(&[2], 8), None),
            Err(AdmissionError::Full)
        ));
        let task = tokio::spawn(owner.run());
        assert_eq!(receive(receipt).await, Ok(7));
        handle.close();
        let state = task.await.unwrap();
        assert_eq!(state.failures, 1);
        assert_eq!(state.commits, [7]);
        assert_eq!(handle.snapshot().high_water, 1);
    }

    #[tokio::test]
    async fn abandoned_reply_does_not_cancel_an_admitted_effect() {
        let (handle, owner) = RuntimeOwner::new(State::default(), Executor, 1);
        let receipt = handle.try_request(command(&[1], 9), None).unwrap();
        drop(receipt);
        handle.close();
        assert_eq!(owner.run().await.commits, [9]);
        assert_eq!(handle.snapshot().outstanding, 0);
    }

    struct BlockingExecutor {
        started: tokio::sync::mpsc::Sender<()>,
    }

    impl EffectExecutor<Command, u64> for BlockingExecutor {
        fn spawn(&self, c: Command, workers: &mut JoinSet<u64>) -> AbortHandle {
            let started = self.started.clone();
            workers.spawn_blocking(move || {
                started.blocking_send(()).unwrap();
                if let Some(gate) = c.gate {
                    let _ = gate.blocking_recv();
                }
                c.value
            })
        }
    }

    #[tokio::test]
    async fn state_panic_rejects_admission_but_joins_native_work_before_propagating() {
        let (started, mut started_rx) = tokio::sync::mpsc::channel(1);
        let (handle, owner) = RuntimeOwner::new(
            State {
                panic_on_prepare: Some(2),
                ..State::default()
            },
            BlockingExecutor { started },
            2,
        );
        let (release, gate) = oneshot::channel();
        let _slow = handle
            .try_request(
                Command {
                    gate: Some(gate),
                    ..command(&[1], 1)
                },
                None,
            )
            .unwrap();
        let task = tokio::spawn(owner.run());
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let _failed = handle.try_request(command(&[2], 2), None).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !handle.requests.is_closed() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            !task.is_finished(),
            "native work still owns the module lifetime"
        );
        assert!(matches!(
            handle.try_request(command(&[3], 3), None),
            Err(AdmissionError::Closed)
        ));
        release.send(()).unwrap();
        assert!(
            matches!(tokio::time::timeout(Duration::from_secs(2), task).await.unwrap(), Err(error) if error.is_panic())
        );
        assert_eq!(handle.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn response_timeout_does_not_claim_that_started_native_work_expired() {
        let (started, mut started_rx) = tokio::sync::mpsc::channel(1);
        let (handle, owner) = RuntimeOwner::new(State::default(), BlockingExecutor { started }, 1);
        let (release, gate) = oneshot::channel();
        let task = tokio::spawn(owner.run());
        let response = tokio::spawn({
            let handle = handle.clone();
            async move {
                handle
                    .request(
                        Command {
                            gate: Some(gate),
                            ..command(&[1], 1)
                        },
                        Some(Instant::now() + Duration::from_millis(100)),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.await.unwrap(),
            Err(RequestError::ResponseDeadlineElapsed)
        );
        assert_eq!(handle.snapshot().outstanding, 1);
        assert_eq!(handle.snapshot().expired, 0);
        release.send(()).unwrap();
        handle.close();
        assert_eq!(task.await.unwrap().commits, [1]);
    }

    #[tokio::test]
    async fn async_requests_receive_completed_delivery() {
        let (handle, owner) = RuntimeOwner::new(State::default(), Executor, 1);
        let task = tokio::spawn(owner.run());
        assert_eq!(handle.request(command(&[1], 10), None).await, Ok(10));
        handle.close();
        assert_eq!(task.await.unwrap().commits, [10]);
    }

    #[tokio::test]
    async fn burst_of_overlapping_operations_keeps_admission_until_delivery() {
        let (handle, owner) = RuntimeOwner::new(State::default(), Executor, 32);
        let task = tokio::spawn(owner.run());
        let producer = tokio::spawn({
            let handle = handle.clone();
            async move {
                for value in 0..5_000 {
                    let (reply, _) = oneshot::channel();
                    handle
                        .requests
                        .send(
                            Request {
                                command: command(&[1, 2], value),
                                reply: Reply::Async(reply),
                            },
                            None,
                        )
                        .await
                        .unwrap();
                }
                handle.close();
            }
        });
        producer.await.unwrap();
        let state = task.await.unwrap();
        assert_eq!(state.commits, (0..5_000).collect::<Vec<_>>());
        assert!(handle.snapshot().high_water <= 32);
        assert_eq!(handle.snapshot().outstanding, 0);
    }
}
