//! Tracks admitted async reconciliation through completion. A
//! producer reserves delivery before starting work that will need reconciliation.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;

use tokio::task::JoinSet;

use super::mailbox::{
    AdmissionError, MailboxReceiver, MailboxReservation, MailboxSender, QueueSnapshot, WorkPermit,
    mailbox,
};

type Work = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Clone)]
pub(crate) struct WorkerHandle {
    requests: MailboxSender<Work>,
}

pub(crate) struct WorkerReservation {
    request: MailboxReservation<Work>,
}

pub(crate) struct Workers {
    requests: MailboxReceiver<Work>,
}

#[cfg(any(feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn workers() -> (WorkerHandle, Workers) {
    workers_with_capacity(super::mailbox::RUNTIME_MAILBOX_CAPACITY)
}

fn workers_with_capacity(capacity: usize) -> (WorkerHandle, Workers) {
    let (requests, receiver) = mailbox(capacity);
    (WorkerHandle { requests }, Workers { requests: receiver })
}

impl WorkerHandle {
    pub fn try_reserve(&self) -> Result<WorkerReservation, AdmissionError> {
        self.requests
            .try_reserve()
            .map(|request| WorkerReservation { request })
    }

    pub fn close(&self) {
        self.requests.close();
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        self.requests.snapshot()
    }
}

impl WorkerReservation {
    pub fn spawn(self, future: impl Future<Output = ()> + Send + 'static) {
        // No deadline is attached to already reserved reconciliation.
        let _ = self.request.send(Box::pin(future), None);
    }
}

impl Workers {
    /// Close admission before awaiting this owner. Task failures are counted and
    /// never abort or detach sibling reconciliation.
    pub async fn run(mut self) -> usize {
        let mut jobs = JoinSet::<()>::new();
        let mut permits = HashMap::<tokio::task::Id, WorkPermit>::new();
        let mut closed = false;
        let mut failures = 0usize;
        loop {
            tokio::select! {
                biased;
                Some(result) = jobs.join_next_with_id(), if !jobs.is_empty() => {
                    let id = match result {
                        Ok((id, ())) => id,
                        Err(error) => { failures = failures.saturating_add(1); error.id() }
                    };
                    permits.remove(&id);
                }
                request = self.requests.recv(), if !closed => match request {
                    Some(request) => {
                        let (work, permit) = request.into_parts();
                        let job = jobs.spawn(work);
                        permits.insert(job.id(), permit);
                    }
                    None => closed = true,
                },
                else => return failures,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn closing_admission_retains_reserved_cleanup_and_waits_for_completion() {
        let (handle, owner) = workers_with_capacity(1);
        let reserved = handle.try_reserve().unwrap();
        let task = tokio::spawn(owner.run());
        handle.close();
        assert!(matches!(handle.try_reserve(), Err(AdmissionError::Closed)));
        let (started, start) = oneshot::channel();
        let (release, finish) = oneshot::channel();
        reserved.spawn(async move {
            let _ = started.send(());
            let _ = finish.await;
        });
        start.await.unwrap();
        assert!(!task.is_finished());
        assert_eq!(handle.snapshot().outstanding, 1);
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap(), 0);
        assert_eq!(handle.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn panicking_work_does_not_detach_unrelated_reconciliation() {
        let (handle, owner) = workers_with_capacity(2);
        let blocking = handle.try_reserve().unwrap();
        let failing = handle.try_reserve().unwrap();
        let task = tokio::spawn(owner.run());
        let (started, start) = oneshot::channel();
        let (release, finish) = oneshot::channel();
        blocking.spawn(async move {
            let _ = started.send(());
            let _ = finish.await;
        });
        failing.spawn(async { panic!("controlled worker failure") });
        handle.close();
        start.await.unwrap();
        assert!(!task.is_finished());
        release.send(()).unwrap();
        assert_eq!(task.await.unwrap(), 1);
        assert_eq!(handle.snapshot().outstanding, 0);
    }
}
