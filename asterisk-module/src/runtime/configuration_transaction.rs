//! Configuration mutation reservations spanning preparation, I/O, and commit.
//!
//! Reload, feature persistence, schedules, and mobility share one ordered lane.
//! Its owner stays responsive while a granted transaction performs I/O. The
//! completion channel is allocated before grant and needs no command capacity.

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::oneshot;
use tokio::task::{AbortHandle, JoinSet};

use super::mailbox::{AdmissionError, QueueSnapshot, RUNTIME_MAILBOX_CAPACITY};
use super::owner::{EffectExecutor, OperationId, RuntimeHandle, RuntimeOwner, RuntimeState, Step};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ConfigurationOperation {
    Features,
    Schedules,
    Mobility,
    Reload,
    Registration,
}

#[derive(Clone)]
pub(crate) struct ConfigurationLease {
    // The final participant's Drop releases reserved completion capacity.
    _release: Arc<oneshot::Sender<()>>,
}

enum Grant {
    Sync(std::sync::mpsc::SyncSender<ConfigurationLease>),
    Async(oneshot::Sender<ConfigurationLease>),
}

impl Grant {
    fn send(self, lease: ConfigurationLease) {
        match self {
            Self::Sync(reply) => {
                let _ = reply.send(lease);
            }
            Self::Async(reply) => {
                let _ = reply.send(lease);
            }
        }
    }
}

pub(crate) struct Begin {
    operation: ConfigurationOperation,
    grant: Grant,
}

#[derive(Clone)]
pub(crate) struct ConfigurationTransactions {
    handle: RuntimeHandle<Begin, ()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ConfigurationTransactionError {
    #[error(transparent)]
    Admission(#[from] AdmissionError),
    #[error("configuration transaction was not granted before its deadline")]
    Unavailable,
}

impl ConfigurationTransactions {
    pub fn begin(
        &self,
        operation: ConfigurationOperation,
        deadline: Instant,
    ) -> Result<ConfigurationLease, ConfigurationTransactionError> {
        let (grant, receipt) = std::sync::mpsc::sync_channel(1);
        self.handle.try_request(
            Begin {
                operation,
                grant: Grant::Sync(grant),
            },
            Some(deadline),
        )?;
        receipt
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| ConfigurationTransactionError::Unavailable)
    }

    pub async fn begin_async(
        &self,
        operation: ConfigurationOperation,
        deadline: Instant,
    ) -> Result<ConfigurationLease, ConfigurationTransactionError> {
        let (grant, receipt) = oneshot::channel();
        let request = self.handle.request(
            Begin {
                operation,
                grant: Grant::Async(grant),
            },
            Some(deadline),
        );
        tokio::pin!(request);
        tokio::select! {
            lease = tokio::time::timeout_at(deadline.into(), receipt) => lease
                .map_err(|_| ConfigurationTransactionError::Unavailable)?
                .map_err(|_| ConfigurationTransactionError::Unavailable),
            _ = &mut request => Err(ConfigurationTransactionError::Unavailable),
        }
    }

    pub fn close(&self) {
        self.handle.close();
    }
    pub fn snapshot(&self) -> QueueSnapshot {
        self.handle.snapshot()
    }
}

#[derive(Default)]
pub(crate) struct TransactionState {
    pending: Option<(OperationId, ConfigurationOperation)>,
}

impl RuntimeState for TransactionState {
    type Command = Begin;
    type Reply = ();
    type Resource = ();
    type Effect = oneshot::Receiver<()>;
    type Completion = ();

    fn resources(&self, _: &Begin) -> Vec<()> {
        vec![()]
    }
    fn prepare(&mut self, id: OperationId, command: Begin) -> Step<(), Self::Effect> {
        debug_assert!(self.pending.is_none());
        self.pending = Some((id, command.operation));
        let (release, completion) = oneshot::channel();
        command.grant.send(ConfigurationLease {
            _release: Arc::new(release),
        });
        Step::Effect(completion)
    }
    fn complete(&mut self, id: OperationId, (): ()) -> Step<(), Self::Effect> {
        debug_assert_eq!(self.pending.map(|pending| pending.0), Some(id));
        self.pending = None;
        Step::Finished(())
    }
    fn failed(&mut self, id: OperationId) -> Step<(), Self::Effect> {
        self.complete(id, ())
    }
}

pub(crate) struct CompletionExecutor;
impl EffectExecutor<oneshot::Receiver<()>, ()> for CompletionExecutor {
    fn spawn(&self, completion: oneshot::Receiver<()>, workers: &mut JoinSet<()>) -> AbortHandle {
        workers.spawn(async move {
            let _ = completion.await;
        })
    }
}

pub(crate) fn configuration_transactions() -> (
    ConfigurationTransactions,
    RuntimeOwner<TransactionState, CompletionExecutor>,
) {
    let (handle, owner) = RuntimeOwner::new(
        TransactionState::default(),
        CompletionExecutor,
        RUNTIME_MAILBOX_CAPACITY,
    );
    (ConfigurationTransactions { handle }, owner)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn expired_reload_never_runs_after_a_feature_transaction_finishes() {
        let (transactions, owner) = configuration_transactions();
        let transactions = Arc::new(transactions);
        let task = tokio::spawn(owner.run());
        let feature = transactions
            .begin_async(
                ConfigurationOperation::Features,
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        assert!(matches!(
            transactions
                .begin_async(
                    ConfigurationOperation::Reload,
                    Instant::now() + Duration::from_millis(10)
                )
                .await,
            Err(ConfigurationTransactionError::Unavailable)
        ));
        drop(feature);
        let mobility = transactions
            .begin_async(
                ConfigurationOperation::Mobility,
                Instant::now() + Duration::from_secs(1),
            )
            .await
            .unwrap();
        transactions.close();
        drop(mobility);
        assert!(task.await.unwrap().pending.is_none());
        assert_eq!(transactions.snapshot().outstanding, 0);
        assert_eq!(transactions.snapshot().expired, 1);
    }
}
