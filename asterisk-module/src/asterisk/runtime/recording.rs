//! Serialized recording-session ownership and trigger admission.
//!
//! Each live session owns the handset identity needed for teardown
//! publication. Eligibility attempts remain consumed until the associated PBX
//! call disappears, independently of whether its recording session stops.

use std::collections::{HashSet, VecDeque};

use sccp_protocol::{CallId, DeviceId};

use crate::ami::services::OwnedRecordingSessions;
use crate::media::recording::{
    RecordingDirection, RecordingError, RecordingSessionControl, RecordingState,
};
use crate::runtime::backend::PbxCallId;

use super::{Access, LogLevel, MutexExt as _, Shared, ast_log, backend, mpsc};

pub(super) const RECORDING_TRIGGER_WAKE_CAPACITY: usize = 1;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) enum RuntimeRecordingTrigger {
    Eligible { pbx_id: PbxCallId },
    SessionChanged { pbx_id: PbxCallId },
}

#[derive(Default)]
pub(super) struct RuntimeRecordingTriggerQueue {
    order: VecDeque<RuntimeRecordingTrigger>,
    pending: HashSet<RuntimeRecordingTrigger>,
}

impl RuntimeRecordingTriggerQueue {
    pub(super) fn push(&mut self, trigger: RuntimeRecordingTrigger) -> bool {
        if !self.pending.insert(trigger) {
            return false;
        }
        self.order.push_back(trigger);
        true
    }

    pub(super) fn drain(&mut self) -> Vec<RuntimeRecordingTrigger> {
        self.pending.clear();
        self.order.drain(..).collect()
    }

    pub(super) fn clear(&mut self) {
        self.pending.clear();
        self.order.clear();
    }
}

#[derive(Default)]
pub(in super::super) struct RuntimeRecordings {
    pub(super) sessions: OwnedRecordingSessions<RuntimeRecordingSession>,
    automatic_attempts: HashSet<PbxCallId>,
}

#[derive(Clone)]
pub(super) struct RuntimeRecordingOwner {
    pub(super) device_id: DeviceId,
    pub(super) handset_call_id: CallId,
}

pub(super) struct RuntimeRecordingSession {
    inner: backend::AnchoredRecordingSession,
    owner: RuntimeRecordingOwner,
}

impl RuntimeRecordingSession {
    pub(super) fn new(
        inner: backend::AnchoredRecordingSession,
        device_id: DeviceId,
        handset_call_id: CallId,
    ) -> Self {
        Self {
            inner,
            owner: RuntimeRecordingOwner {
                device_id,
                handset_call_id,
            },
        }
    }

    pub(super) fn owner(&self) -> &RuntimeRecordingOwner {
        &self.owner
    }

    pub(super) fn is_active(&self) -> bool {
        !matches!(self.state(), Ok(RecordingState::Stopped))
    }

    pub(super) fn release_anchor(&mut self) {
        self.inner.release_anchor();
    }

    pub(super) fn anchor_mut(&mut self) -> &mut backend::ConfirmedRecordingAnchor {
        self.inner.anchor_mut()
    }

    pub(super) fn stop_native(&mut self) -> Result<(), RecordingError> {
        self.inner.stop_native()
    }
}

impl RecordingSessionControl for RuntimeRecordingSession {
    type Error = RecordingError;

    fn id(&self) -> Result<String, Self::Error> {
        self.inner.id()
    }

    fn state(&self) -> Result<RecordingState, Self::Error> {
        self.inner.state()
    }

    fn stop(&mut self) -> Result<(), Self::Error> {
        self.stop_native()
    }

    fn set_muted(
        &mut self,
        direction: RecordingDirection,
        muted: bool,
    ) -> Result<usize, Self::Error> {
        self.inner.set_muted(direction, muted)
    }
}

impl RuntimeRecordings {
    pub(super) fn extract_calls(&mut self, call_ids: &HashSet<PbxCallId>) -> Self {
        let mut extracted = Self::default();
        for (call_id, session) in self
            .sessions
            .extract_if(|call_id, _| call_ids.contains(&call_id))
        {
            let _ = extracted.sessions.insert(call_id, session);
        }
        for call_id in call_ids {
            if self.automatic_attempts.remove(call_id) {
                extracted.automatic_attempts.insert(*call_id);
            }
        }
        extracted
    }

    pub(super) fn return_sessions(&mut self, mut completed: Self) {
        for (call_id, session) in completed.sessions.extract_if(|_, _| true) {
            let _ = self.sessions.insert(call_id, session);
        }
        self.automatic_attempts.extend(completed.automatic_attempts);
    }

    pub(in super::super) fn is_active_call(&self, pbx_id: PbxCallId) -> bool {
        self.sessions
            .get(pbx_id)
            .is_some_and(RuntimeRecordingSession::is_active)
    }

    pub(super) fn owner(&self, pbx_id: PbxCallId) -> Option<&RuntimeRecordingOwner> {
        self.sessions
            .get(pbx_id)
            .map(RuntimeRecordingSession::owner)
    }

    pub(super) fn device_is_active(&self, device_id: &DeviceId) -> bool {
        self.sessions
            .iter()
            .any(|(_, session)| &session.owner().device_id == device_id && session.is_active())
    }

    pub(super) fn claim_automatic_start(&mut self, pbx_id: PbxCallId) -> bool {
        self.automatic_attempts.insert(pbx_id)
    }

    pub(super) fn suppress_automatic_start(&mut self, pbx_id: PbxCallId) {
        self.automatic_attempts.insert(pbx_id);
    }

    pub(super) fn forget_call(&mut self, pbx_id: PbxCallId) {
        self.automatic_attempts.remove(&pbx_id);
    }

    pub(super) fn retain_live_calls(&mut self, live: &HashSet<PbxCallId>) {
        self.automatic_attempts
            .retain(|pbx_id| live.contains(pbx_id));
    }
}

pub(super) fn enqueue_recording_trigger(shared: &Shared, trigger: RuntimeRecordingTrigger) {
    let mut pending = shared.pending_recording_triggers.lock_unpoisoned();
    if !pending.push(trigger) {
        return;
    }
    match shared.recording_trigger_wake.try_send(()) {
        Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
        Err(mpsc::error::TrySendError::Closed(())) => {
            pending.clear();
            ast_log(LogLevel::Warning, "unable to enqueue SCCP recording update");
        }
    }
}

pub(super) fn take_recording_triggers(access: &Access) -> Vec<RuntimeRecordingTrigger> {
    access
        .shared
        .pending_recording_triggers
        .lock_unpoisoned()
        .drain()
}

impl Access {
    pub(in super::super) fn enqueue_recording_eligibility(&self, pbx_id: PbxCallId) {
        enqueue_recording_trigger(&self.shared, RuntimeRecordingTrigger::Eligible { pbx_id });
    }

    pub(super) fn take_recording_triggers(&self) -> Vec<RuntimeRecordingTrigger> {
        take_recording_triggers(self)
    }
}

pub(super) fn enqueue_recording_session_change(shared: &Shared, pbx_id: PbxCallId) {
    enqueue_recording_trigger(shared, RuntimeRecordingTrigger::SessionChanged { pbx_id });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn automatic_attempts_are_one_shot_until_the_call_is_reaped() {
        let call = PbxCallId(42);
        let stopped_call = PbxCallId(43);
        let mut recordings = RuntimeRecordings::default();

        assert!(recordings.claim_automatic_start(call));
        assert!(!recordings.claim_automatic_start(call));
        recordings.suppress_automatic_start(stopped_call);
        assert!(!recordings.claim_automatic_start(stopped_call));
        recordings.retain_live_calls(&HashSet::new());
        assert!(recordings.claim_automatic_start(call));
        assert!(recordings.claim_automatic_start(stopped_call));
    }

    #[test]
    fn trigger_queue_deduplicates_without_reordering_distinct_work() {
        let call = PbxCallId(42);
        let eligible = RuntimeRecordingTrigger::Eligible { pbx_id: call };
        let changed = RuntimeRecordingTrigger::SessionChanged { pbx_id: call };
        let mut queue = RuntimeRecordingTriggerQueue::default();

        assert!(queue.push(eligible));
        assert!(!queue.push(eligible));
        assert!(queue.push(changed));
        assert_eq!(queue.drain(), [eligible, changed]);
    }
}
