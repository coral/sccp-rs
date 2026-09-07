//! Recording provider, anchor ownership, and session translation.

use super::{
    Access, AsteriskChannel, AsteriskRecording, DirectMediaCall, MediaAnchorReason, NonNull,
    PbxCallId, RecordingCallback, RecordingDirection, RecordingError, RecordingProvider,
    RecordingSession, RecordingSessionControl, RecordingState, RecordingTarget, direct_media_call,
    with_channel,
};
use super::{MediaAnchorLease, MediaAnchorMutation};

pub struct AsteriskRecordingService<'a> {
    pub access: &'a Access,
}

pub(in super::super) struct AnchoredRecordingSession {
    inner: RecordingSession,
    anchor: ConfirmedRecordingAnchor,
    stopped: bool,
}

pub(in super::super) struct PendingRecordingAnchor {
    lease: MediaAnchorLease,
    retarget_call: Option<DirectMediaCall>,
}

pub(in super::super) struct ConfirmedRecordingAnchor {
    lease: MediaAnchorLease,
}

#[derive(Debug, thiserror::Error)]
pub enum AsteriskRecordingServiceError {
    #[error("PBX call {} is unavailable for recording", .0.0)]
    CallUnavailable(PbxCallId),
    #[error(transparent)]
    Recording(RecordingError),
}

impl PendingRecordingAnchor {
    pub(in super::super) fn acquire(
        access: &Access,
        call_id: PbxCallId,
        mutation: &MediaAnchorMutation,
    ) -> Result<Self, AsteriskRecordingServiceError> {
        with_channel(access, call_id, |channel| {
            let channel = NonNull::new(channel)
                .ok_or(AsteriskRecordingServiceError::CallUnavailable(call_id))?;
            let retarget_call = direct_media_call(access, channel.as_ptr());
            Ok(Self {
                lease: access
                    .shared
                    .media_runtime
                    .acquire(
                        &mutation._reservation,
                        access,
                        call_id,
                        MediaAnchorReason::Recording,
                        retarget_call.clone(),
                    )
                    .ok_or(AsteriskRecordingServiceError::CallUnavailable(call_id))?,
                retarget_call,
            })
        })
        .unwrap_or(Err(AsteriskRecordingServiceError::CallUnavailable(call_id)))
    }

    pub(in super::super) fn direct_call(&self) -> Option<&DirectMediaCall> {
        self.retarget_call.as_ref()
    }

    pub(in super::super) fn confirm(self) -> ConfirmedRecordingAnchor {
        ConfirmedRecordingAnchor { lease: self.lease }
    }
}

impl ConfirmedRecordingAnchor {
    pub(in super::super) fn restore_call(&self) -> Option<DirectMediaCall> {
        self.lease.restore_call()
    }

    pub(in super::super) fn release(&mut self) {
        self.lease.release();
    }
}

impl AnchoredRecordingSession {
    pub(in super::super) fn new(inner: RecordingSession, anchor: ConfirmedRecordingAnchor) -> Self {
        Self {
            inner,
            anchor,
            stopped: false,
        }
    }

    pub(in super::super) fn release_anchor(&mut self) {
        self.anchor.release();
    }

    pub(in super::super) fn anchor_mut(&mut self) -> &mut ConfirmedRecordingAnchor {
        &mut self.anchor
    }

    pub(in super::super) fn stop_native(&mut self) -> Result<(), RecordingError> {
        if self.stopped || matches!(self.inner.state(), Ok(RecordingState::Stopped)) {
            self.stopped = true;
            return Ok(());
        }
        self.inner.stop()?;
        self.stopped = true;
        Ok(())
    }
}

impl RecordingSessionControl for AnchoredRecordingSession {
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

impl RecordingProvider for AsteriskRecordingService<'_> {
    type Session = RecordingSession;
    type StartError = AsteriskRecordingServiceError;

    fn start_recording(
        &self,
        call_id: PbxCallId,
        target: RecordingTarget,
        options: &str,
        callback: RecordingCallback,
    ) -> Result<Self::Session, Self::StartError> {
        with_channel(self.access, call_id, |channel| {
            let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }
                .map_err(|_| AsteriskRecordingServiceError::CallUnavailable(call_id))?;
            AsteriskRecording::new()
                .start(&channel, target, options, move |event| callback(event))
                .map_err(AsteriskRecordingServiceError::Recording)
        })
        .unwrap_or(Err(AsteriskRecordingServiceError::CallUnavailable(call_id)))
    }
}
