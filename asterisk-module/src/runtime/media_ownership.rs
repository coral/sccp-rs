//! Media-anchor and announcement state owned by one responsive command loop.
//!
//! Reservations carry release capacity before native work starts. Releasing a
//! reservation or anchor never needs ordinary mailbox admission.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread;
use std::time::Instant;

use tokio::sync::oneshot;
use tokio::task::JoinSet;

use sccp_protocol::ConferenceId;

use super::backend::PbxCallId;
use super::conference_announcement::{AnnouncementGeneration, allocate_generation};
use super::mailbox::{
    MailboxReceiver, MailboxReservation, MailboxSender, RUNTIME_MAILBOX_CAPACITY, WorkPermit,
    mailbox,
};
use crate::media::direct::{MediaAnchorReason, MediaAnchorRegistry, MediaAnchorRestores};

pub(crate) trait MediaEffects<C>: Clone + Send + Sync + 'static {
    fn complete_announcement(
        &self,
        conference_id: ConferenceId,
        generation: AnnouncementGeneration,
    ) -> bool;
    fn restore_cancelled_anchor(&self, call_id: PbxCallId, restore: &C);
}

pub(crate) struct ActiveConferenceAnnouncement<E: MediaEffects<C>, C: Clone + Send + Sync + 'static>
{
    pub(crate) generation: AnnouncementGeneration,
    pub(crate) call_ids: Vec<PbxCallId>,
    pub(crate) completion: Option<AnnouncementTimer<E>>,
    pub(crate) anchors: Vec<AnchorLease<E, C>>,
    pub(crate) direct_calls: Vec<C>,
    pub(crate) restore_attempts: u8,
}

#[derive(Clone)]
pub(crate) struct MediaHandle<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> {
    commands: MailboxSender<MediaCommand<E, C>>,
    anchored: Arc<RwLock<Arc<HashMap<PbxCallId, AnchorSnapshot<C>>>>>,
}

struct AnchorSnapshot<C: Clone + Send + Sync + 'static> {
    count: usize,
    reason: MediaAnchorReason,
    restore: Option<C>,
}

struct OwnedAnchor<E: Clone + Send + Sync + 'static> {
    call_id: PbxCallId,
    reason: MediaAnchorReason,
    access: E,
}

enum MediaCompletion<E: Clone + Send + Sync + 'static> {
    Announcement(
        ConferenceId,
        AnnouncementGeneration,
        Option<AnnouncementTimer<E>>,
    ),
    CancelledAnchor {
        id: u64,
        call_id: PbxCallId,
        _admission: WorkPermit,
    },
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
enum MediaResource {
    Call(PbxCallId),
    Conference(ConferenceId),
}

pub(crate) struct MediaReservation<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> {
    id: u64,
    commit: Mutex<Option<MailboxReservation<MediaCommand<E, C>>>>,
    release: Option<MailboxReservation<MediaCommand<E, C>>>,
}

impl<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> MediaReservation<E, C> {
    pub(crate) fn commit_announcement(
        &self,
        id: ConferenceId,
        active: ActiveConferenceAnnouncement<E, C>,
    ) {
        if let Some(commit) = self
            .commit
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let (reply, receipt) = mpsc::sync_channel(1);
            let _ = commit.send(MediaCommand::PutAnnouncement(id, active, reply), None);
            let _ = receipt.recv();
        }
    }
}

impl<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> Drop for MediaReservation<E, C> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(MediaCommand::ReleaseReservation(self.id), None);
        }
    }
}

pub(crate) struct AnchorLease<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> {
    pub(crate) call_id: PbxCallId,
    pub(crate) reason: MediaAnchorReason,
    id: u64,
    owner: MediaHandle<E, C>,
    release: Option<MailboxReservation<MediaCommand<E, C>>>,
}

impl<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> AnchorLease<E, C> {
    pub(crate) fn release(&mut self) {
        if let Some(release) = self.release.take() {
            let (reply, receipt) = mpsc::sync_channel(1);
            let _ = release.send(
                MediaCommand::ReleaseAnchor {
                    id: self.id,
                    reply: Some(reply),
                },
                None,
            );
            let _ = receipt.recv();
        }
    }

    pub(crate) fn is_last(&self) -> bool {
        self.release.is_some()
            && !self
                .owner
                .anchored_for_other_reason(self.call_id, self.reason)
    }

    pub(crate) fn restore_call(&self) -> Option<C> {
        self.is_last()
            .then(|| self.owner.restore(self.call_id))
            .flatten()
    }
}

impl<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> Drop for AnchorLease<E, C> {
    fn drop(&mut self) {
        if let Some(release) = self.release.take() {
            let _ = release.send(
                MediaCommand::ReleaseAnchor {
                    id: self.id,
                    reply: None,
                },
                None,
            );
        }
    }
}

pub(crate) struct AnnouncementTimer<E: Clone + Send + Sync + 'static> {
    access: E,
    deadline: Instant,
}

impl<E: Clone + Send + Sync + 'static> AnnouncementTimer<E> {
    pub(crate) fn new(access: &E, delay: std::time::Duration) -> Self {
        Self {
            access: access.clone(),
            deadline: Instant::now() + delay,
        }
    }
}

enum ReservationReply<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> {
    Async(oneshot::Sender<Option<MediaReservation<E, C>>>),
    Sync(mpsc::SyncSender<Option<MediaReservation<E, C>>>),
}

impl<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> ReservationReply<E, C> {
    fn send(self, reservation: Option<MediaReservation<E, C>>) {
        match self {
            Self::Async(reply) => {
                let _ = reply.send(reservation);
            }
            Self::Sync(reply) => {
                let _ = reply.send(reservation);
            }
        }
    }
}

enum MediaCommand<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> {
    Reserve {
        resources: Vec<MediaResource>,
        release: MailboxReservation<MediaCommand<E, C>>,
        commit: MailboxReservation<MediaCommand<E, C>>,
        wait: bool,
        reply: ReservationReply<E, C>,
    },
    ReleaseReservation(u64),
    AcquireAnchor {
        reservation: u64,
        call_id: PbxCallId,
        reason: MediaAnchorReason,
        restore: Option<C>,
        owner: MediaHandle<E, C>,
        access: E,
        release: MailboxReservation<MediaCommand<E, C>>,
        reply: mpsc::SyncSender<Option<AnchorLease<E, C>>>,
    },
    ReleaseAnchor {
        id: u64,
        reply: Option<mpsc::SyncSender<()>>,
    },
    RemoveCall(PbxCallId, mpsc::SyncSender<()>),
    NextGeneration(mpsc::SyncSender<Option<AnnouncementGeneration>>),
    TakeAnnouncement(
        ConferenceId,
        mpsc::SyncSender<Option<ActiveConferenceAnnouncement<E, C>>>,
    ),
    PutAnnouncement(
        ConferenceId,
        ActiveConferenceAnnouncement<E, C>,
        mpsc::SyncSender<()>,
    ),
    AnnouncementGeneration(
        ConferenceId,
        mpsc::SyncSender<Option<AnnouncementGeneration>>,
    ),
    AnnouncementIds(mpsc::SyncSender<Vec<ConferenceId>>),
    DeferAnnouncement(ConferenceId, AnnouncementGeneration, AnnouncementTimer<E>),
}

pub(crate) struct MediaOwner<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> {
    commands: MailboxReceiver<MediaCommand<E, C>>,
    anchored: Arc<RwLock<Arc<HashMap<PbxCallId, AnchorSnapshot<C>>>>>,
    anchors: MediaAnchorRegistry,
    restores: MediaAnchorRestores<C>,
    leases: HashMap<u64, OwnedAnchor<E>>,
    cancelled_anchors: VecDeque<(u64, WorkPermit)>,
    announcements: HashMap<ConferenceId, ActiveConferenceAnnouncement<E, C>>,
    reserved: HashSet<MediaResource>,
    reservations: HashMap<u64, (Vec<MediaResource>, WorkPermit)>,
    waiting: VecDeque<(MediaCommand<E, C>, WorkPermit)>,
    next_id: u64,
    next_generation: std::sync::atomic::AtomicU64,
    workers: JoinSet<MediaCompletion<E>>,
}

impl<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> MediaHandle<E, C> {
    fn request<T>(
        &self,
        command: impl FnOnce(mpsc::SyncSender<T>) -> MediaCommand<E, C>,
    ) -> Option<T> {
        let (reply, receipt) = mpsc::sync_channel(1);
        self.commands.try_send(command(reply), None).ok()?;
        receipt.recv().ok()
    }

    pub(crate) fn snapshot(&self) -> super::mailbox::QueueSnapshot {
        self.commands.snapshot()
    }

    pub(crate) fn close(&self) {
        self.commands.close();
    }

    fn reservation_request(
        &self,
        calls: &[PbxCallId],
        conference: Option<ConferenceId>,
        wait: bool,
        reply: ReservationReply<E, C>,
    ) -> Option<()> {
        let release = self.commands.try_reserve().ok()?;
        let commit = self.commands.try_reserve().ok()?;
        let mut resources = calls
            .iter()
            .copied()
            .map(MediaResource::Call)
            .collect::<Vec<_>>();
        resources.extend(conference.map(MediaResource::Conference));
        self.commands
            .try_send(
                MediaCommand::Reserve {
                    resources,
                    release,
                    commit,
                    wait,
                    reply,
                },
                None,
            )
            .ok()
    }

    pub(crate) async fn reserve(
        &self,
        calls: &[PbxCallId],
        conference: Option<ConferenceId>,
    ) -> Option<MediaReservation<E, C>> {
        let (reply, receipt) = oneshot::channel();
        self.reservation_request(calls, conference, true, ReservationReply::Async(reply))?;
        receipt.await.ok().flatten()
    }

    pub(crate) fn try_reserve(
        &self,
        calls: &[PbxCallId],
        conference: Option<ConferenceId>,
    ) -> Option<MediaReservation<E, C>> {
        let (reply, receipt) = mpsc::sync_channel(1);
        self.reservation_request(calls, conference, false, ReservationReply::Sync(reply))?;
        receipt.recv().ok().flatten()
    }

    pub(crate) fn acquire(
        &self,
        reservation: &MediaReservation<E, C>,
        access: &E,
        call_id: PbxCallId,
        reason: MediaAnchorReason,
        restore: Option<C>,
    ) -> Option<AnchorLease<E, C>> {
        let release = self.commands.try_reserve().ok()?;
        self.request(|reply| MediaCommand::AcquireAnchor {
            reservation: reservation.id,
            call_id,
            reason,
            restore,
            owner: self.clone(),
            access: access.clone(),
            release,
            reply,
        })
        .flatten()
    }

    pub(crate) fn restore(&self, call_id: PbxCallId) -> Option<C> {
        self.anchored
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&call_id)
            .and_then(|snapshot| snapshot.restore.clone())
    }

    pub(crate) fn is_anchored(&self, call_id: PbxCallId) -> bool {
        self.anchored
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains_key(&call_id)
    }

    pub(crate) fn anchored_for_other_reason(
        &self,
        call_id: PbxCallId,
        reason: MediaAnchorReason,
    ) -> bool {
        self.anchored
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&call_id)
            .is_some_and(|snapshot| snapshot.count > 1 || snapshot.reason != reason)
    }

    pub(crate) fn remove_call(&self, call_id: PbxCallId) {
        self.request(|reply| MediaCommand::RemoveCall(call_id, reply));
    }

    pub(crate) fn next_generation(&self) -> Option<AnnouncementGeneration> {
        self.request(MediaCommand::NextGeneration).flatten()
    }

    pub(crate) fn take_announcement(
        &self,
        id: ConferenceId,
    ) -> Option<ActiveConferenceAnnouncement<E, C>> {
        self.request(|reply| MediaCommand::TakeAnnouncement(id, reply))
            .flatten()
    }

    pub(crate) fn generation(&self, id: ConferenceId) -> Option<AnnouncementGeneration> {
        self.request(|reply| MediaCommand::AnnouncementGeneration(id, reply))
            .flatten()
    }

    pub(crate) fn announcement_ids(&self) -> Vec<ConferenceId> {
        self.request(MediaCommand::AnnouncementIds)
            .unwrap_or_default()
    }

    pub(crate) fn defer(
        &self,
        id: ConferenceId,
        generation: AnnouncementGeneration,
        timer: AnnouncementTimer<E>,
    ) {
        let _ = self
            .commands
            .try_send(MediaCommand::DeferAnnouncement(id, generation, timer), None);
    }
}

impl<E: MediaEffects<C>, C: Clone + Send + Sync + 'static> MediaOwner<E, C> {
    pub(crate) fn start() -> Result<(MediaHandle<E, C>, thread::JoinHandle<()>), std::io::Error> {
        Self::with_capacity(RUNTIME_MAILBOX_CAPACITY)
    }

    fn with_capacity(
        capacity: usize,
    ) -> Result<(MediaHandle<E, C>, thread::JoinHandle<()>), std::io::Error> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        let (commands, receiver) = mailbox(capacity);
        let anchored = Arc::new(RwLock::new(Arc::new(HashMap::new())));
        let owner = Self {
            anchored: Arc::clone(&anchored),
            commands: receiver,
            anchors: MediaAnchorRegistry::default(),
            restores: MediaAnchorRestores::default(),
            leases: HashMap::new(),
            cancelled_anchors: VecDeque::new(),
            announcements: HashMap::new(),
            reserved: HashSet::new(),
            reservations: HashMap::new(),
            waiting: VecDeque::new(),
            next_id: 1,
            next_generation: std::sync::atomic::AtomicU64::new(1),
            workers: JoinSet::new(),
        };
        let task = thread::Builder::new()
            .name("sccp-media-owner".into())
            .spawn(move || runtime.block_on(owner.run()))?;
        Ok((MediaHandle { commands, anchored }, task))
    }

    fn publish_snapshot(&self) {
        let mut snapshots = HashMap::new();
        for anchor in self.leases.values() {
            let snapshot = snapshots
                .entry(anchor.call_id)
                .or_insert_with(|| AnchorSnapshot {
                    count: 0,
                    reason: anchor.reason,
                    restore: self.restores.get(anchor.call_id).cloned(),
                });
            snapshot.count += 1;
        }
        *self
            .anchored
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Arc::new(snapshots);
    }

    fn release_anchor(&mut self, id: u64) {
        if let Some(anchor) = self.leases.remove(&id) {
            if self.anchors.release(anchor.call_id, anchor.reason)
                && !self.anchors.is_anchored(anchor.call_id)
            {
                self.restores.remove_call(anchor.call_id);
            }
        }
        self.publish_snapshot();
    }

    fn dispatch_cancelled_anchors(&mut self) {
        let cancelled = std::mem::take(&mut self.cancelled_anchors);
        for (id, admission) in cancelled {
            let Some(anchor) = self.leases.get(&id) else {
                continue;
            };
            let call_id = anchor.call_id;
            if self.reserved.contains(&MediaResource::Call(call_id)) {
                self.cancelled_anchors.push_back((id, admission));
                continue;
            }
            let restore = self.restores.get(call_id).cloned().filter(|_| {
                !self
                    .anchors
                    .is_anchored_for_other_reason(call_id, anchor.reason)
            });
            let Some(restore) = restore else {
                self.release_anchor(id);
                continue;
            };
            let access = anchor.access.clone();
            self.reserved.insert(MediaResource::Call(call_id));
            self.workers.spawn_blocking(move || {
                access.restore_cancelled_anchor(call_id, &restore);
                MediaCompletion::CancelledAnchor {
                    id,
                    call_id,
                    _admission: admission,
                }
            });
        }
    }

    fn allocate_id(&mut self) -> Option<u64> {
        let id = self.next_id;
        self.next_id = id.checked_add(1)?;
        Some(id)
    }

    async fn run(mut self) {
        loop {
            let deadline = self
                .announcements
                .values()
                .filter_map(|active| active.completion.as_ref().map(|timer| timer.deadline))
                .min();
            tokio::select! {
                biased;
                _ = async { match deadline { Some(deadline) => tokio::time::sleep_until(deadline.into()).await, None => std::future::pending().await } } => self.dispatch_due(),
                result = self.workers.join_next(), if !self.workers.is_empty() => {
                    if let Some(Ok(completion)) = result { self.complete(completion); }
                },
                command = self.commands.recv() => match command { Some(command) => { let (command, permit) = command.into_parts(); self.process(command, permit); self.dispatch_cancelled_anchors(); }, None => break },
            }
        }
        self.waiting.clear();
        self.announcements.clear();
        while let Some(result) = self.workers.join_next().await {
            if let Ok(completion) = result {
                self.complete(completion);
            }
        }
    }

    fn complete(&mut self, completion: MediaCompletion<E>) {
        match completion {
            MediaCompletion::Announcement(id, generation, timer) => {
                if let Some(timer) = timer {
                    if let Some(active) = self.announcements.get_mut(&id) {
                        if active.generation == generation {
                            active.completion = Some(timer);
                        }
                    }
                }
            }
            MediaCompletion::CancelledAnchor { id, call_id, .. } => {
                self.release_anchor(id);
                self.reserved.remove(&MediaResource::Call(call_id));
                self.dispatch_cancelled_anchors();
                let waiting = std::mem::take(&mut self.waiting);
                for (command, permit) in waiting {
                    self.process(command, permit);
                }
            }
        }
        self.dispatch_cancelled_anchors();
    }

    fn dispatch_due(&mut self) {
        let now = Instant::now();
        for (id, active) in &mut self.announcements {
            if active
                .completion
                .as_ref()
                .is_some_and(|timer| timer.deadline <= now)
            {
                if let Some(timer) = active.completion.take() {
                    let id = *id;
                    let generation = active.generation;
                    self.workers.spawn_blocking(move || {
                        let retry = timer.access.complete_announcement(id, generation);
                        let timer = retry.then(|| {
                            AnnouncementTimer::new(
                                &timer.access,
                                std::time::Duration::from_millis(10),
                            )
                        });
                        MediaCompletion::Announcement(id, generation, timer)
                    });
                }
            }
        }
    }

    fn process(&mut self, command: MediaCommand<E, C>, permit: WorkPermit) {
        match command {
            MediaCommand::Reserve {
                mut resources,
                release,
                commit,
                wait,
                reply,
            } => {
                // Include the previous announcement's full restoration set before
                // handing off its state to a replacement or cancellation worker.
                for resource in resources.clone() {
                    if let MediaResource::Conference(id) = resource {
                        if let Some(active) = self.announcements.get(&id) {
                            resources.extend(
                                active
                                    .anchors
                                    .iter()
                                    .map(|anchor| MediaResource::Call(anchor.call_id)),
                            );
                        }
                    }
                }
                if resources
                    .iter()
                    .any(|resource| self.reserved.contains(resource) || self.waiting.iter().any(|(command, _)| matches!(command, MediaCommand::Reserve { resources, .. } if resources.contains(resource))))
                {
                    if wait {
                        self.waiting.push_back((
                            MediaCommand::Reserve {
                                resources,
                                release,
                                commit,
                                wait,
                                reply,
                            },
                            permit,
                        ));
                    } else {
                        let _ = reply.send(None);
                    }
                    return;
                }
                let Some(id) = self.allocate_id() else {
                    let _ = reply.send(None);
                    return;
                };
                self.reserved.extend(resources.iter().copied());
                self.reservations.insert(id, (resources, permit));
                let _ = reply.send(Some(MediaReservation {
                    id,
                    commit: Mutex::new(Some(commit)),
                    release: Some(release),
                }));
            }
            MediaCommand::ReleaseReservation(id) => {
                if let Some((resources, _permit)) = self.reservations.remove(&id) {
                    for resource in resources {
                        self.reserved.remove(&resource);
                    }
                }
                self.dispatch_cancelled_anchors();
                let waiting = std::mem::take(&mut self.waiting);
                for (command, permit) in waiting {
                    self.process(command, permit);
                }
            }
            MediaCommand::AcquireAnchor {
                reservation,
                call_id,
                reason,
                restore,
                owner,
                access,
                release,
                reply,
            } => {
                if !self
                    .reservations
                    .get(&reservation)
                    .is_some_and(|(resources, _)| resources.contains(&MediaResource::Call(call_id)))
                {
                    let _ = reply.send(None);
                    return;
                }
                let Some(id) = self.allocate_id() else {
                    let _ = reply.send(None);
                    return;
                };
                if let Some(restore) = restore {
                    self.restores.remember(call_id, restore);
                }
                self.anchors.acquire(call_id, reason);
                self.leases.insert(
                    id,
                    OwnedAnchor {
                        call_id,
                        reason,
                        access,
                    },
                );
                self.publish_snapshot();
                let _ = reply.send(Some(AnchorLease {
                    call_id,
                    reason,
                    id,
                    owner,
                    release: Some(release),
                }));
            }
            MediaCommand::ReleaseAnchor { id, reply } => match reply {
                Some(reply) => {
                    self.release_anchor(id);
                    let _ = reply.send(());
                }
                None => {
                    self.cancelled_anchors.push_back((id, permit));
                }
            },
            MediaCommand::RemoveCall(id, reply) => {
                self.anchors.remove_call(id);
                self.restores.remove_call(id);
                self.leases.retain(|_, anchor| anchor.call_id != id);
                self.publish_snapshot();
                let _ = reply.send(());
            }
            MediaCommand::NextGeneration(reply) => {
                let _ = reply.send(allocate_generation(&self.next_generation));
            }
            MediaCommand::TakeAnnouncement(id, reply) => {
                let _ = reply.send(self.announcements.remove(&id));
            }
            MediaCommand::PutAnnouncement(id, active, reply) => {
                self.announcements.insert(id, active);
                let _ = reply.send(());
            }
            MediaCommand::AnnouncementGeneration(id, reply) => {
                let _ = reply.send(self.announcements.get(&id).map(|active| active.generation));
            }
            MediaCommand::AnnouncementIds(reply) => {
                let _ = reply.send(self.announcements.keys().copied().collect());
            }
            MediaCommand::DeferAnnouncement(id, generation, timer) => {
                if let Some(active) = self.announcements.get_mut(&id) {
                    if active.generation == generation {
                        active.completion = Some(timer);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::*;

    #[derive(Clone)]
    struct Effects {
        started: mpsc::SyncSender<(PbxCallId, u64)>,
        gate: Arc<Mutex<Option<oneshot::Receiver<()>>>>,
        restored: Arc<Mutex<Vec<(PbxCallId, u64)>>>,
    }

    impl MediaEffects<u64> for Effects {
        fn complete_announcement(&self, _: ConferenceId, _: AnnouncementGeneration) -> bool {
            false
        }
        fn restore_cancelled_anchor(&self, call_id: PbxCallId, restore: &u64) {
            self.started.send((call_id, *restore)).unwrap();
            if let Some(gate) = self.gate.lock().unwrap().take() {
                let _ = gate.blocking_recv();
            }
            self.restored.lock().unwrap().push((call_id, *restore));
        }
    }

    type OwnerHandle = MediaHandle<Effects, u64>;

    fn fixture(
        capacity: usize,
    ) -> (
        OwnerHandle,
        thread::JoinHandle<()>,
        Effects,
        mpsc::Receiver<(PbxCallId, u64)>,
        oneshot::Sender<()>,
    ) {
        let (started, starts) = mpsc::sync_channel(8);
        let (release, gate) = oneshot::channel();
        let effects = Effects {
            started,
            gate: Arc::new(Mutex::new(Some(gate))),
            restored: Arc::new(Mutex::new(Vec::new())),
        };
        let (handle, task) = MediaOwner::with_capacity(capacity).unwrap();
        (handle, task, effects, starts, release)
    }

    #[tokio::test]
    async fn overlapping_anchors_restore_the_first_plan_once_and_keep_other_calls_responsive() {
        let (handle, task, effects, starts, release) = fixture(32);
        let mutation = handle.try_reserve(&[PbxCallId(1)], None).unwrap();
        assert!(
            handle
                .acquire(
                    &mutation,
                    &effects,
                    PbxCallId(2),
                    MediaAnchorReason::Recording,
                    Some(22)
                )
                .is_none()
        );
        assert!(!handle.is_anchored(PbxCallId(2)));
        let recording = handle
            .acquire(
                &mutation,
                &effects,
                PbxCallId(1),
                MediaAnchorReason::Recording,
                Some(11),
            )
            .unwrap();
        let announcement = handle
            .acquire(
                &mutation,
                &effects,
                PbxCallId(1),
                MediaAnchorReason::Announcement,
                Some(99),
            )
            .unwrap();
        assert_eq!(recording.restore_call(), None);
        assert_eq!(announcement.restore_call(), None);
        drop(recording);
        assert!(starts.try_recv().is_err());
        drop(mutation);
        // This grant is also a barrier after the first reserved cancellation.
        let unrelated = handle.try_reserve(&[PbxCallId(2)], None).unwrap();
        assert_eq!(announcement.restore_call(), Some(11));
        let mutation = handle.try_reserve(&[PbxCallId(1)], None).unwrap();
        drop(announcement);
        assert!(starts.try_recv().is_err());
        drop(mutation);
        assert_eq!(
            starts.recv_timeout(Duration::from_secs(2)).unwrap(),
            (PbxCallId(1), 11)
        );
        assert!(handle.is_anchored(PbxCallId(1)));
        assert!(handle.try_reserve(&[PbxCallId(1)], None).is_none());
        let another_call = handle.try_reserve(&[PbxCallId(3)], None).unwrap();
        drop(another_call);
        let waiting = {
            let handle = handle.clone();
            tokio::spawn(async move { handle.reserve(&[PbxCallId(1)], None).await })
        };
        release.send(()).unwrap();
        let resumed = tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(!handle.is_anchored(PbxCallId(1)));
        assert_eq!(*effects.restored.lock().unwrap(), [(PbxCallId(1), 11)]);
        drop(resumed);
        drop(unrelated);
        handle.close();
        task.join().unwrap();
        assert_eq!(handle.snapshot().outstanding, 0);
    }

    #[test]
    fn saturated_admission_still_delivers_anchor_cancellation_and_shutdown_joins_native_restore() {
        let (handle, task, effects, starts, release) = fixture(8);
        let mutation = handle.try_reserve(&[PbxCallId(4)], None).unwrap();
        let anchor = handle
            .acquire(
                &mutation,
                &effects,
                PbxCallId(4),
                MediaAnchorReason::Recording,
                Some(44),
            )
            .unwrap();
        let mut saturation = Vec::new();
        while let Ok(reservation) = handle.commands.try_reserve() {
            saturation.push(reservation);
        }
        assert_eq!(handle.snapshot().outstanding, 8);
        drop(anchor);
        drop(mutation);
        assert_eq!(
            starts.recv_timeout(Duration::from_secs(2)).unwrap(),
            (PbxCallId(4), 44)
        );
        assert!(handle.is_anchored(PbxCallId(4)));
        drop(saturation);
        handle.close();
        let (joined, receipt) = mpsc::sync_channel(1);
        let joining = thread::spawn(move || {
            task.join().unwrap();
            joined.send(()).unwrap();
        });
        assert!(receipt.try_recv().is_err());
        release.send(()).unwrap();
        receipt.recv_timeout(Duration::from_secs(2)).unwrap();
        joining.join().unwrap();
        assert_eq!(*effects.restored.lock().unwrap(), [(PbxCallId(4), 44)]);
        assert_eq!(handle.snapshot().outstanding, 0);
        assert_eq!(handle.snapshot().high_water, 8);
    }

    #[test]
    fn late_announcement_retry_cannot_overwrite_a_replacement_generation() {
        let (handle, task, effects, _, release) = fixture(16);
        let conference = ConferenceId(10);
        let old_generation = handle.next_generation().unwrap();
        let new_generation = handle.next_generation().unwrap();
        let mutation = handle.try_reserve(&[], Some(conference)).unwrap();
        mutation.commit_announcement(
            conference,
            ActiveConferenceAnnouncement {
                generation: new_generation,
                call_ids: Vec::new(),
                completion: None,
                anchors: Vec::new(),
                direct_calls: Vec::new(),
                restore_attempts: 0,
            },
        );
        handle.defer(
            conference,
            old_generation,
            AnnouncementTimer::new(&effects, Duration::from_secs(30)),
        );
        assert_eq!(handle.generation(conference), Some(new_generation));
        let current = handle.take_announcement(conference).unwrap();
        assert!(current.completion.is_none());
        assert_eq!(current.generation, new_generation);
        drop(mutation);
        drop(release);
        handle.close();
        task.join().unwrap();
        assert_eq!(handle.snapshot().outstanding, 0);
    }

    #[test]
    fn explicit_release_and_retired_call_cancellation_do_not_restore_again() {
        let (handle, task, effects, starts, release) = fixture(16);
        let mutation = handle.try_reserve(&[PbxCallId(5)], None).unwrap();
        let mut completed = handle
            .acquire(
                &mutation,
                &effects,
                PbxCallId(5),
                MediaAnchorReason::Recording,
                Some(55),
            )
            .unwrap();
        assert_eq!(completed.restore_call(), Some(55));
        completed.release();
        completed.release();
        drop(completed);
        let retired = handle
            .acquire(
                &mutation,
                &effects,
                PbxCallId(5),
                MediaAnchorReason::Announcement,
                Some(66),
            )
            .unwrap();
        handle.remove_call(PbxCallId(5));
        drop(retired);
        drop(mutation);
        drop(release);
        handle.close();
        task.join().unwrap();
        assert!(!handle.is_anchored(PbxCallId(5)));
        assert!(starts.try_recv().is_err());
        assert!(effects.restored.lock().unwrap().is_empty());
        assert_eq!(handle.snapshot().outstanding, 0);
    }
}
