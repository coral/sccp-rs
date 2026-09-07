//! Bounded admission that follows work through queueing, execution and cleanup.
//!
//! A receiver must retain the admission permit until the operation has finished,
//! including compensation. Removing work from the channel does not free capacity.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc};

pub(crate) const RUNTIME_MAILBOX_CAPACITY: usize = 65_536;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct QueueSnapshot {
    pub capacity: usize,
    pub queued: usize,
    pub outstanding: usize,
    pub high_water: usize,
    pub admission_failures: usize,
    pub expired: usize,
}

struct Admission {
    slots: Arc<Semaphore>,
    closing: Notify,
    capacity: usize,
    queued: AtomicUsize,
    outstanding: AtomicUsize,
    high_water: AtomicUsize,
    admission_failures: AtomicUsize,
    expired: AtomicUsize,
}

/// Read-only counters. Holding this never keeps a sender or receiver alive.
#[derive(Clone)]
pub(crate) struct QueueMonitor {
    admission: Arc<Admission>,
}

impl QueueMonitor {
    pub fn snapshot(&self) -> QueueSnapshot {
        QueueSnapshot {
            capacity: self.admission.capacity,
            queued: self.admission.queued.load(Ordering::Relaxed),
            outstanding: self.admission.outstanding.load(Ordering::Relaxed),
            high_water: self.admission.high_water.load(Ordering::Relaxed),
            admission_failures: self.admission.admission_failures.load(Ordering::Relaxed),
            expired: self.admission.expired.load(Ordering::Relaxed),
        }
    }
}

/// A completion slot as well as an admission slot. Keep this in owner-side
/// pending records and dispatched workers, never in a secondary uncounted queue.
pub(crate) struct WorkPermit {
    admission: Arc<Admission>,
    _slot: OwnedSemaphorePermit,
}

impl Drop for WorkPermit {
    fn drop(&mut self) {
        self.admission.outstanding.fetch_sub(1, Ordering::Relaxed);
    }
}

pub(crate) struct Admitted<T> {
    pub value: T,
    pub deadline: Option<Instant>,
    permit: WorkPermit,
}

impl<T> Admitted<T> {
    pub fn from_parts(value: T, deadline: Option<Instant>, permit: WorkPermit) -> Self {
        Self {
            value,
            deadline,
            permit,
        }
    }
    pub fn map<U>(self, transform: impl FnOnce(T) -> U) -> Admitted<U> {
        Admitted {
            value: transform(self.value),
            deadline: self.deadline,
            permit: self.permit,
        }
    }

    pub fn is_expired(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|deadline| now >= deadline)
    }

    /// Call once when rejecting expired work, before sending its failure reply.
    pub fn record_expiration(&self) {
        self.permit
            .admission
            .expired
            .fetch_add(1, Ordering::Relaxed);
    }

    pub fn into_parts(self) -> (T, WorkPermit) {
        (self.value, self.permit)
    }
}

struct Queued<T>(Option<Admitted<T>>);

impl<T> Queued<T> {
    fn take(mut self) -> Admitted<T> {
        let value = self.0.take().expect("queued work is consumed once");
        value
            .permit
            .admission
            .queued
            .fetch_sub(1, Ordering::Relaxed);
        value
    }
}

impl<T> Drop for Queued<T> {
    fn drop(&mut self) {
        if let Some(value) = &self.0 {
            value
                .permit
                .admission
                .queued
                .fetch_sub(1, Ordering::Relaxed);
        }
    }
}

pub(crate) struct MailboxSender<T> {
    sender: mpsc::Sender<Queued<T>>,
    admission: Arc<Admission>,
}

impl<T> Clone for MailboxSender<T> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            admission: Arc::clone(&self.admission),
        }
    }
}

pub(crate) struct MailboxReceiver<T> {
    receiver: mpsc::Receiver<Queued<T>>,
    admission: Arc<Admission>,
}

/// Capacity reserved before admitting a lifetime that will need cleanup. The
/// physical channel permit survives receiver close, so accepted cleanup does
/// not compete with new commands and remains deliverable during shutdown.
pub(crate) struct MailboxReservation<T> {
    channel: mpsc::OwnedPermit<Queued<T>>,
    permit: WorkPermit,
}

impl<T> MailboxReservation<T> {
    pub fn send(self, value: T, deadline: Option<Instant>) -> Result<(), AdmissionError> {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            self.permit
                .admission
                .expired
                .fetch_add(1, Ordering::Relaxed);
            return Err(AdmissionError::Expired);
        }
        self.permit.admission.queued.fetch_add(1, Ordering::Relaxed);
        self.channel.send(Queued(Some(Admitted {
            value,
            deadline,
            permit: self.permit,
        })));
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum AdmissionError {
    #[error("runtime admission is exhausted")]
    Full,
    #[error("runtime admission is closed")]
    Closed,
    #[error("runtime request deadline has elapsed")]
    Expired,
}

pub(crate) fn mailbox<T>(capacity: usize) -> (MailboxSender<T>, MailboxReceiver<T>) {
    assert!(capacity > 0);
    let (sender, receiver) = mpsc::channel(capacity);
    let admission = Arc::new(Admission {
        slots: Arc::new(Semaphore::new(capacity)),
        closing: Notify::new(),
        capacity,
        queued: AtomicUsize::new(0),
        outstanding: AtomicUsize::new(0),
        high_water: AtomicUsize::new(0),
        admission_failures: AtomicUsize::new(0),
        expired: AtomicUsize::new(0),
    });
    (
        MailboxSender {
            sender,
            admission: Arc::clone(&admission),
        },
        MailboxReceiver {
            receiver,
            admission,
        },
    )
}

impl<T> MailboxSender<T> {
    pub fn try_reserve(&self) -> Result<MailboxReservation<T>, AdmissionError> {
        let slot = Arc::clone(&self.admission.slots)
            .try_acquire_owned()
            .map_err(|error| {
                self.failure(match error {
                    tokio::sync::TryAcquireError::Closed => AdmissionError::Closed,
                    tokio::sync::TryAcquireError::NoPermits => AdmissionError::Full,
                })
            })?;
        let channel = self.sender.clone().try_reserve_owned().map_err(|error| {
            self.failure(match error {
                mpsc::error::TrySendError::Full(_) => AdmissionError::Full,
                mpsc::error::TrySendError::Closed(_) => AdmissionError::Closed,
            })
        })?;
        let outstanding = self.admission.outstanding.fetch_add(1, Ordering::Relaxed) + 1;
        self.admission
            .high_water
            .fetch_max(outstanding, Ordering::Relaxed);
        Ok(MailboxReservation {
            channel,
            permit: WorkPermit {
                admission: Arc::clone(&self.admission),
                _slot: slot,
            },
        })
    }

    /// Reuses a lifetime's admission budget after its message is consumed.
    /// Only the owner of this receiver may recycle a permit from its messages.
    pub fn recycle(&self, permit: WorkPermit) -> Result<MailboxReservation<T>, AdmissionError> {
        assert!(
            Arc::ptr_eq(&self.admission, &permit.admission),
            "a lifetime permit is recycled into its original mailbox"
        );
        let channel = self
            .sender
            .clone()
            .try_reserve_owned()
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => AdmissionError::Full,
                mpsc::error::TrySendError::Closed(_) => AdmissionError::Closed,
            })?;
        Ok(MailboxReservation { channel, permit })
    }

    pub fn monitor(&self) -> QueueMonitor {
        QueueMonitor {
            admission: Arc::clone(&self.admission),
        }
    }

    pub fn snapshot(&self) -> QueueSnapshot {
        self.monitor().snapshot()
    }

    pub fn is_closed(&self) -> bool {
        self.admission.slots.is_closed()
    }

    /// Reject new admission and wake the owner to drain accepted work.
    pub fn close(&self) {
        self.admission.slots.close();
        self.admission.closing.notify_one();
    }

    fn failure(&self, error: AdmissionError) -> AdmissionError {
        if error == AdmissionError::Expired {
            self.admission.expired.fetch_add(1, Ordering::Relaxed);
        } else {
            self.admission
                .admission_failures
                .fetch_add(1, Ordering::Relaxed);
        }
        error
    }

    fn enqueue(
        &self,
        value: T,
        deadline: Option<Instant>,
        slot: OwnedSemaphorePermit,
    ) -> Result<(), AdmissionError> {
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            return Err(self.failure(AdmissionError::Expired));
        }
        let outstanding = self.admission.outstanding.fetch_add(1, Ordering::Relaxed) + 1;
        self.admission
            .high_water
            .fetch_max(outstanding, Ordering::Relaxed);
        self.admission.queued.fetch_add(1, Ordering::Relaxed);
        // Every queued message owns a slot, so the physical mailbox cannot be
        // full after admission. try_send also handles receiver shutdown safely.
        self.sender
            .try_send(Queued(Some(Admitted {
                value,
                deadline,
                permit: WorkPermit {
                    admission: Arc::clone(&self.admission),
                    _slot: slot,
                },
            })))
            .map_err(|error| {
                self.failure(match error {
                    mpsc::error::TrySendError::Full(_) => AdmissionError::Full,
                    mpsc::error::TrySendError::Closed(_) => AdmissionError::Closed,
                })
            })
    }

    /// Native synchronous interfaces reject saturation without blocking.
    pub fn try_send(&self, value: T, deadline: Option<Instant>) -> Result<(), AdmissionError> {
        let slot = Arc::clone(&self.admission.slots)
            .try_acquire_owned()
            .map_err(|error| {
                self.failure(match error {
                    tokio::sync::TryAcquireError::Closed => AdmissionError::Closed,
                    tokio::sync::TryAcquireError::NoPermits => AdmissionError::Full,
                })
            })?;
        self.enqueue(value, deadline, slot)
    }

    /// Async producers wait for end-to-end capacity, bounded by their deadline.
    pub async fn send(&self, value: T, deadline: Option<Instant>) -> Result<(), AdmissionError> {
        let acquire = Arc::clone(&self.admission.slots).acquire_owned();
        let slot = match deadline {
            Some(deadline) => tokio::time::timeout_at(deadline.into(), acquire)
                .await
                .map_err(|_| self.failure(AdmissionError::Expired))?,
            None => acquire.await,
        }
        .map_err(|_| self.failure(AdmissionError::Closed))?;
        self.enqueue(value, deadline, slot)
    }
}

impl<T> MailboxReceiver<T> {
    pub fn close(&mut self) {
        self.admission.slots.close();
        self.receiver.close();
    }

    pub fn try_recv(&mut self) -> Result<Admitted<T>, mpsc::error::TryRecvError> {
        if self.admission.slots.is_closed() {
            self.receiver.close();
        }
        self.receiver.try_recv().map(Queued::take)
    }

    pub async fn recv(&mut self) -> Option<Admitted<T>> {
        // notify_one retains a wake if close races the check or select.
        if self.admission.slots.is_closed() {
            self.receiver.close();
        }
        tokio::select! {
            value = self.receiver.recv() => value.map(Queued::take),
            _ = self.admission.closing.notified() => {
                self.receiver.close();
                self.receiver.recv().await.map(Queued::take)
            }
        }
    }
}

impl<T> Drop for MailboxReceiver<T> {
    fn drop(&mut self) {
        // Also release async producers blocked on admission when an owner fails.
        self.admission.slots.close();
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[tokio::test]
    async fn diagnostics_do_not_keep_mailbox_admission_alive() {
        let (sender, mut receiver) = mailbox(1);
        let monitor = sender.monitor();
        sender.try_send(7, None).unwrap();
        drop(sender);
        let work = receiver.recv().await.unwrap();
        assert_eq!(monitor.snapshot().outstanding, 1);
        assert_eq!(monitor.snapshot().queued, 0);
        drop(work);
        assert!(receiver.recv().await.is_none());
        assert_eq!(monitor.snapshot().outstanding, 0);
        assert_eq!(monitor.snapshot().high_water, 1);
    }

    #[tokio::test]
    async fn work_remains_counted_after_dequeue_until_completion() {
        let (tx, mut rx) = mailbox(1);
        tx.try_send(1, None).unwrap();
        let work = rx.recv().await.unwrap();
        assert_eq!(tx.snapshot().queued, 0);
        assert_eq!(tx.snapshot().outstanding, 1);
        assert_eq!(tx.try_send(2, None), Err(AdmissionError::Full));
        let (_, completion_slot) = work.into_parts();
        let producer = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(3, None).await }
        });
        tokio::task::yield_now().await;
        assert!(!producer.is_finished());
        drop(completion_slot);
        producer.await.unwrap().unwrap();
        assert_eq!(rx.recv().await.unwrap().value, 3);
        assert_eq!(tx.snapshot().high_water, 1);
        assert_eq!(tx.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn closed_admission_drains_and_wakes_waiting_producers() {
        let (tx, mut rx) = mailbox(1);
        tx.try_send(1, None).unwrap();
        let producer = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(2, None).await }
        });
        tx.close();
        assert_eq!(producer.await.unwrap(), Err(AdmissionError::Closed));
        assert_eq!(rx.recv().await.unwrap().value, 1);
        assert!(rx.recv().await.is_none());
        assert_eq!(tx.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn expired_requests_do_not_reappear_after_capacity_is_released() {
        let (tx, mut rx) = mailbox(1);
        tx.try_send(1, None).unwrap();
        assert_eq!(
            tx.send(2, Some(Instant::now())).await,
            Err(AdmissionError::Expired)
        );
        drop(rx.recv().await);
        assert_eq!(tx.snapshot().expired, 1);
        assert_eq!(tx.snapshot().outstanding, 0);
        tx.close();
        assert!(rx.recv().await.is_none());
    }

    #[tokio::test]
    async fn receiver_loss_releases_queued_work_and_waiting_producers() {
        let (tx, rx) = mailbox(1);
        tx.try_send(1, None).unwrap();
        let producer = tokio::spawn({
            let tx = tx.clone();
            async move { tx.send(2, None).await }
        });
        drop(rx);
        assert_eq!(producer.await.unwrap(), Err(AdmissionError::Closed));
        assert_eq!(tx.snapshot().queued, 0);
        assert_eq!(tx.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn thousands_of_operations_drain_with_bounded_accounting() {
        let (tx, mut rx) = mailbox(31);
        let producer = tokio::spawn({
            let tx = tx.clone();
            async move {
                for value in 0..10_000 {
                    tx.send(value, None).await.unwrap();
                }
                tx.close();
            }
        });
        let mut count = 0;
        while let Some(work) = rx.recv().await {
            assert_eq!(work.value, count);
            count += 1;
        }
        producer.await.unwrap();
        assert_eq!(count, 10_000);
        assert!(tx.snapshot().high_water <= 31);
        assert_eq!(tx.snapshot().outstanding, 0);
    }

    #[tokio::test]
    async fn close_wakes_an_idle_receiver() {
        let (tx, mut rx) = mailbox::<()>(1);
        let consumer = tokio::spawn(async move { rx.recv().await.is_none() });
        tokio::task::yield_now().await;
        tx.close();
        assert!(
            tokio::time::timeout(Duration::from_secs(1), consumer)
                .await
                .unwrap()
                .unwrap()
        );
    }

    #[tokio::test]
    async fn reserved_cleanup_survives_saturation_and_external_admission_close() {
        let (tx, mut rx) = mailbox(2);
        let cleanup = tx.try_reserve().unwrap();
        tx.try_send(1, None).unwrap();
        assert_eq!(tx.try_send(2, None), Err(AdmissionError::Full));
        tx.close();
        assert_eq!(rx.recv().await.unwrap().value, 1);
        assert!(
            tokio::time::timeout(Duration::from_millis(10), rx.recv())
                .await
                .is_err()
        );
        cleanup.send(3, None).unwrap();
        assert_eq!(rx.recv().await.unwrap().value, 3);
        assert!(rx.recv().await.is_none());
        assert_eq!(tx.snapshot().outstanding, 0);
        assert_eq!(tx.snapshot().high_water, 2);
    }
}
