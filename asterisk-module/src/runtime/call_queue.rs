//! Ordered call delivery with teardown capacity reserved for each admitted call.
//!
//! A lease is a native lifetime token, not shared call state. The queue owns
//! scheduling and retirement. Retirement reaches the owner even at saturation,
//! signals cancellation immediately, and runs cleanup after prior native work.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use tokio::sync::watch;
use tokio::task::{AbortHandle, Id, JoinSet};

use super::mailbox::{
    AdmissionError, Admitted, MailboxReceiver, MailboxReservation, MailboxSender, QueueSnapshot,
    WorkPermit, mailbox,
};

#[derive(Clone)]
struct Generation(Arc<()>);

impl PartialEq for Generation {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for Generation {}
impl PartialOrd for Generation {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Generation {
    fn cmp(&self, other: &Self) -> Ordering {
        Arc::as_ptr(&self.0).cmp(&Arc::as_ptr(&other.0))
    }
}
impl Hash for Generation {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct RequestId(Generation);

struct Queued<K, C> {
    key: K,
    generation: Generation,
    command: C,
    terminal: bool,
}

pub(crate) struct CallQueueHandle<K, C> {
    sender: MailboxSender<Queued<K, C>>,
}

struct Teardown<K, C> {
    reserved: MailboxReservation<Queued<K, C>>,
    command: C,
}

/// The short mutex linearizes signal admission against terminal delivery. It
/// protects delivery/lifetime only; logical call state stays with its owner.
pub(crate) struct CallLease<K: Clone, C> {
    key: K,
    generation: Generation,
    sender: MailboxSender<Queued<K, C>>,
    teardown: Mutex<Option<Teardown<K, C>>>,
}

impl<K: Clone, C> CallLease<K, C> {
    pub fn try_send(&self, command: C, deadline: Option<Instant>) -> Result<(), AdmissionError> {
        let teardown = self.teardown.lock().unwrap_or_else(PoisonError::into_inner);
        if teardown.is_none() {
            return Err(AdmissionError::Closed);
        }
        self.sender.try_send(
            Queued {
                key: self.key.clone(),
                generation: self.generation.clone(),
                command,
                terminal: false,
            },
            deadline,
        )
    }

    pub fn retire(&self, command: C) {
        let teardown = self
            .teardown
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(teardown) = teardown {
            // Reserved cleanup has no deadline and cannot lose to saturation.
            let _ = teardown.reserved.send(
                Queued {
                    key: self.key.clone(),
                    generation: self.generation.clone(),
                    command,
                    terminal: true,
                },
                None,
            );
        }
    }
}

impl<K: Clone, C> Drop for CallLease<K, C> {
    fn drop(&mut self) {
        if let Some(teardown) = self
            .teardown
            .get_mut()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            let _ = teardown.reserved.send(
                Queued {
                    key: self.key.clone(),
                    generation: self.generation.clone(),
                    command: teardown.command,
                    terminal: true,
                },
                None,
            );
        }
    }
}

impl<K: Clone, C> CallQueueHandle<K, C> {
    pub fn admit(&self, key: K, terminal: C) -> Result<CallLease<K, C>, AdmissionError> {
        Ok(CallLease {
            key,
            generation: Generation(Arc::new(())),
            sender: self.sender.clone(),
            teardown: Mutex::new(Some(Teardown {
                reserved: self.sender.try_reserve()?,
                command: terminal,
            })),
        })
    }

    pub fn close(&self) {
        self.sender.close();
    }
    pub fn snapshot(&self) -> QueueSnapshot {
        self.sender.snapshot()
    }
}

pub(crate) struct CallEffect<K, C> {
    pub key: K,
    pub command: C,
    pub sequence: u64,
    /// A terminal callback is processable while a previous effect is running.
    /// Workers finish/compensate native work rather than aborting it on this flag.
    pub retiring: watch::Receiver<bool>,
}

pub(crate) trait CallExecutor<K, C>: Send + 'static {
    /// Prepare logical teardown independently of the running native effect.
    /// The returned command keeps its reserved terminal admission until cleanup.
    fn prepare_terminal(&self, key: K, command: C) -> impl Future<Output = C> + Send + 'static;
    fn spawn(&self, effect: CallEffect<K, C>, workers: &mut JoinSet<()>) -> AbortHandle;
}

struct Lane {
    waiting: VecDeque<RequestId>,
    running: bool,
    retiring: watch::Sender<bool>,
}

struct Running {
    generation: Generation,
    terminal: bool,
    _permit: WorkPermit,
}

enum QueueTurn<K, C> {
    Deadline,
    Completed(Id),
    Request(Option<Admitted<Queued<K, C>>>),
    Prepared(Option<Result<(RequestId, Admitted<Queued<K, C>>), tokio::task::JoinError>>),
}

pub(crate) struct CallQueue<K, C, E> {
    receiver: MailboxReceiver<Queued<K, C>>,
    executor: E,
    lanes: HashMap<Generation, Lane>,
    pending: HashMap<RequestId, Admitted<Queued<K, C>>>,
    preparing: HashMap<RequestId, Generation>,
    preparations: JoinSet<(RequestId, Admitted<Queued<K, C>>)>,
    deadlines: BTreeSet<(Instant, RequestId)>,
    workers: JoinSet<()>,
    running: HashMap<Id, Running>,
    next_sequence: u64,
}

pub(crate) struct CallMailbox<K, C>(MailboxReceiver<Queued<K, C>>);

pub(crate) fn call_queue<K: Clone, C>(
    capacity: usize,
) -> (CallQueueHandle<K, C>, CallMailbox<K, C>) {
    let (sender, receiver) = mailbox(capacity);
    (CallQueueHandle { sender }, CallMailbox(receiver))
}

impl<K, C, E> CallQueue<K, C, E> {
    pub fn new(mailbox: CallMailbox<K, C>, executor: E) -> Self {
        Self {
            receiver: mailbox.0,
            executor,
            lanes: HashMap::new(),
            pending: HashMap::new(),
            preparing: HashMap::new(),
            preparations: JoinSet::new(),
            deadlines: BTreeSet::new(),
            workers: JoinSet::new(),
            running: HashMap::new(),
            next_sequence: 1,
        }
    }
}

impl<K: Clone + Send + 'static, C: Send + 'static, E: CallExecutor<K, C>> CallQueue<K, C, E> {
    pub async fn run(mut self) {
        let mut open = true;
        while open || !self.workers.is_empty() || !self.preparations.is_empty() {
            let deadline = self.deadlines.first().map(|(deadline, _)| *deadline);
            let turn = tokio::select! {
                biased;
                _ = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
                        None => std::future::pending().await,
                    }
                } => QueueTurn::Deadline,
                result = self.workers.join_next_with_id(), if !self.workers.is_empty() => {
                    let task = match result.expect("nonempty call worker set") {
                        Ok((task, ())) => task,
                        Err(error) => error.id(),
                    };
                    QueueTurn::Completed(task)
                }
                prepared = self.preparations.join_next(), if !self.preparations.is_empty() => QueueTurn::Prepared(prepared),
                request = self.receiver.recv(), if open => QueueTurn::Request(request),
            };
            if let Err(panic) = catch_unwind(AssertUnwindSafe(|| match turn {
                QueueTurn::Deadline => self.expire(),
                QueueTurn::Completed(task) => {
                    let running = self.running.remove(&task).expect("tracked call worker");
                    if running.terminal {
                        self.lanes.remove(&running.generation);
                    } else {
                        self.lanes
                            .get_mut(&running.generation)
                            .expect("active call lane")
                            .running = false;
                        self.start(&running.generation);
                    }
                }
                QueueTurn::Prepared(Some(Ok((id, request)))) => {
                    let generation = self
                        .preparing
                        .remove(&id)
                        .expect("tracked terminal preparation");
                    self.pending.insert(id, request);
                    self.start(&generation);
                }
                QueueTurn::Prepared(Some(Err(error))) => std::panic::panic_any(error),
                QueueTurn::Prepared(None) => {}
                QueueTurn::Request(None) => open = false,
                QueueTurn::Request(Some(request)) => self.accept(request),
            })) {
                self.receiver.close();
                self.pending.clear();
                while self.receiver.recv().await.is_some() {}
                while self.preparations.join_next().await.is_some() {}
                while self.workers.join_next().await.is_some() {}
                self.running.clear();
                resume_unwind(panic);
            }
        }

        debug_assert!(self.pending.is_empty());
        debug_assert!(self.lanes.is_empty());
    }

    fn accept(&mut self, request: Admitted<Queued<K, C>>) {
        let generation = request.value.generation.clone();
        let lane = self
            .lanes
            .entry(generation.clone())
            .or_insert_with(|| Lane {
                waiting: VecDeque::new(),
                running: false,
                retiring: watch::channel(false).0,
            });
        if request.value.terminal {
            lane.retiring.send_replace(true);
        }
        let id = RequestId(Generation(Arc::new(())));
        lane.waiting.push_back(id.clone());
        if let Some(deadline) = request.deadline {
            self.deadlines.insert((deadline, id.clone()));
        }
        if request.value.terminal {
            let key = request.value.key.clone();
            self.preparing.insert(id.clone(), generation.clone());
            let (request_value, permit) = request.into_parts();
            let preparation = self.executor.prepare_terminal(key, request_value.command);
            self.preparations.spawn(async move {
                let command = preparation.await;
                (
                    id,
                    Admitted::from_parts(
                        Queued {
                            command,
                            ..request_value
                        },
                        None,
                        permit,
                    ),
                )
            });
        } else {
            self.pending.insert(id, request);
        }
        self.start(&generation);
    }

    fn start(&mut self, generation: &Generation) {
        let lane = self.lanes.get_mut(generation).expect("admitted call lane");
        if lane.running {
            return;
        }
        while let Some(id) = lane.waiting.pop_front() {
            if self.preparing.contains_key(&id) {
                lane.waiting.push_front(id);
                break;
            }
            let Some(request) = self.pending.remove(&id) else {
                continue;
            };
            if let Some(deadline) = request.deadline {
                self.deadlines.remove(&(deadline, id));
            }
            if request.is_expired(Instant::now()) {
                request.record_expiration();
                continue;
            }
            let (request, permit) = request.into_parts();
            lane.running = true;
            let terminal = request.terminal;
            let sequence = self.next_sequence;
            self.next_sequence = sequence.saturating_add(1);
            let effect = CallEffect {
                key: request.key,
                command: request.command,
                sequence,
                retiring: lane.retiring.subscribe(),
            };
            let task = self.executor.spawn(effect, &mut self.workers);
            self.running.insert(
                task.id(),
                Running {
                    generation: generation.clone(),
                    terminal,
                    _permit: permit,
                },
            );
            break;
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        while let Some((deadline, id)) = self.deadlines.first().cloned() {
            if deadline > now {
                break;
            }
            self.deadlines.remove(&(deadline, id.clone()));
            if let Some(request) = self.pending.remove(&id) {
                request.record_expiration();
                self.lanes
                    .get_mut(&request.value.generation)
                    .expect("queued call lane")
                    .waiting
                    .retain(|queued| queued != &id);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::*;

    struct Command {
        value: u64,
        gate: Option<oneshot::Receiver<()>>,
        retirement_seen: Option<oneshot::Sender<()>>,
    }
    fn command(value: u64) -> Command {
        Command {
            value,
            gate: None,
            retirement_seen: None,
        }
    }
    struct Executor(Arc<Mutex<Vec<(u64, u64)>>>);
    impl CallExecutor<u64, Command> for Executor {
        fn prepare_terminal(
            &self,
            _key: u64,
            command: Command,
        ) -> impl Future<Output = Command> + Send + 'static {
            async move { command }
        }
        fn spawn(
            &self,
            mut effect: CallEffect<u64, Command>,
            workers: &mut JoinSet<()>,
        ) -> AbortHandle {
            let completed = Arc::clone(&self.0);
            workers.spawn(async move {
                if let Some(retirement_seen) = effect.command.retirement_seen {
                    effect
                        .retiring
                        .wait_for(|retiring| *retiring)
                        .await
                        .unwrap();
                    let _ = retirement_seen.send(());
                }
                if let Some(gate) = effect.command.gate {
                    let _ = gate.await;
                }
                assert!(effect.sequence > 0);
                completed
                    .lock()
                    .unwrap()
                    .push((effect.key, effect.command.value));
            })
        }
    }

    #[tokio::test]
    async fn saturated_calls_accept_retirement_while_other_calls_and_expiry_progress() {
        let completed = Arc::new(Mutex::new(Vec::new()));
        let (handle, mailbox) = call_queue(5);
        let owner = CallQueue::new(mailbox, Executor(Arc::clone(&completed)));
        let first = handle.admit(1, command(99)).unwrap();
        let second = handle.admit(2, command(98)).unwrap();
        let (release, gate) = oneshot::channel();
        let (retirement_seen, retirement) = oneshot::channel();
        first
            .try_send(
                Command {
                    value: 1,
                    gate: Some(gate),
                    retirement_seen: Some(retirement_seen),
                },
                None,
            )
            .unwrap();
        first
            .try_send(command(7), Some(Instant::now() + Duration::from_millis(30)))
            .unwrap();
        second.try_send(command(2), None).unwrap();
        assert_eq!(first.try_send(command(8), None), Err(AdmissionError::Full));
        first.retire(command(3));
        first.retire(command(4));
        assert_eq!(
            first.try_send(command(9), None),
            Err(AdmissionError::Closed)
        );
        let task = tokio::spawn(owner.run());
        tokio::time::timeout(Duration::from_secs(1), retirement)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while handle.snapshot().expired == 0 || !completed.lock().unwrap().contains(&(2, 2)) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!completed.lock().unwrap().iter().any(|(key, _)| *key == 1));
        handle.close();
        drop(second);
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            completed
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| *key == 1)
                .copied()
                .collect::<Vec<_>>(),
            [(1, 1), (1, 3)]
        );
        assert_eq!(
            completed
                .lock()
                .unwrap()
                .iter()
                .filter(|(key, _)| *key == 2)
                .copied()
                .collect::<Vec<_>>(),
            [(2, 2), (2, 98)]
        );
        assert_eq!(handle.snapshot().outstanding, 0);
        assert_eq!(handle.snapshot().high_water, 5);
    }

    #[tokio::test]
    async fn dropping_an_unpublished_call_delivers_exactly_one_reserved_cleanup() {
        let completed = Arc::new(Mutex::new(Vec::new()));
        let (handle, mailbox) = call_queue(1);
        let owner = CallQueue::new(mailbox, Executor(Arc::clone(&completed)));
        let call = handle.admit(1, command(3)).unwrap();
        handle.close();
        let task = tokio::spawn(owner.run());
        drop(call);
        task.await.unwrap();
        assert_eq!(*completed.lock().unwrap(), [(1, 3)]);
        assert_eq!(handle.snapshot().outstanding, 0);
    }
}
