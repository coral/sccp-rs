//! Per-call metadata and forwarding transactions owned with controller state.

use super::*;
use crate::call::forwarding::{
    ForwardingContext, ForwardingOperation, ForwardingRejection, NoAnswerTimer, NoAnswerTimerId,
};
use crate::call::shared_lines::SharedNoAnswerRoute;
use crate::media::formats::PbxAudioFormat;

#[derive(Clone, Default)]
pub(crate) struct CallRuntimeRecord {
    pub audio_encryption: Option<crate::media::encryption::AudioEncryptionAdmission>,
    pub assigned_channel_id: Option<String>,
    pub audio_packet_ms: Option<u32>,
    pub audio_preferences: Option<Vec<PbxAudioFormat>>,
    pub forwarding: Option<ForwardingOperation>,
    pub no_answer: Option<SharedNoAnswerRoute>,
    pub(super) codec_mutation: Option<super::codec_mutation::PendingCodecMutation>,
    pub(super) codec_generation: u64,
    controller_owned: bool,
}

impl Controller {
    pub(super) fn call_runtime_mut(&mut self, pbx_id: PbxCallId) -> Option<&mut CallRuntimeRecord> {
        if self.pbx_call(pbx_id).is_some() {
            return Some(
                self.call_runtime
                    .entry(pbx_id)
                    .or_insert_with(|| CallRuntimeRecord {
                        controller_owned: true,
                        ..CallRuntimeRecord::default()
                    }),
            );
        }
        self.call_runtime.get_mut(&pbx_id)
    }

    pub(crate) fn retire_call_runtime(&mut self, pbx_id: PbxCallId) {
        self.call_runtime.remove(&pbx_id);
        self.clear_no_answer_route(pbx_id);
    }

    pub(super) fn retire_ended_call_records(&mut self) {
        self.retire_parking_attempts();
        let ended = self
            .call_runtime
            .iter()
            .filter_map(|(id, record)| {
                (record.controller_owned && self.pbx_call(*id).is_none()).then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in ended {
            self.retire_call_runtime(id);
        }
    }

    pub(crate) fn retain_audio_encryption(
        &mut self,
        call_id: CallId,
        device_id: DeviceId,
        generation: SessionGeneration,
        admission: crate::media::encryption::AudioEncryptionAdmission,
    ) -> Option<crate::media::encryption::AudioEncryptionAdmission> {
        let call = self.call(call_id)?;
        if call.device_id != device_id
            || self.registered_device(&device_id)?.session_generation != generation
        {
            return None;
        }
        let record = self.call_runtime_mut(call.pbx_id)?;
        Some(record.audio_encryption.get_or_insert(admission).clone())
    }

    pub(crate) fn register_forwarded_call(&mut self, operation: ForwardingOperation) -> bool {
        if self.call_runtime.contains_key(&operation.call_id) {
            return false;
        }
        self.call_runtime.insert(
            operation.call_id,
            CallRuntimeRecord {
                forwarding: Some(operation),
                ..CallRuntimeRecord::default()
            },
        );
        true
    }

    pub(crate) fn set_assigned_channel_id(
        &mut self,
        pbx_id: PbxCallId,
        assigned: Option<String>,
    ) -> bool {
        let Some(record) = self.call_runtime_mut(pbx_id) else {
            return false;
        };
        record.assigned_channel_id = assigned;
        true
    }

    pub(crate) fn set_audio_packet_ms(&mut self, pbx_id: PbxCallId, packet_ms: u32) -> bool {
        let Some(record) = self.call_runtime_mut(pbx_id) else {
            return false;
        };
        record.audio_packet_ms = Some(packet_ms);
        true
    }

    pub(crate) fn set_no_answer_plan(
        &mut self,
        pbx_id: PbxCallId,
        route: SharedNoAnswerRoute,
    ) -> bool {
        let Some(record) = self.call_runtime_mut(pbx_id) else {
            return false;
        };
        record.no_answer = Some(route);
        true
    }

    pub(crate) fn take_no_answer_plan(&mut self, pbx_id: PbxCallId) -> Option<SharedNoAnswerRoute> {
        self.call_runtime_mut(pbx_id)?.no_answer.take()
    }

    pub(crate) fn schedule_no_answer_timer(
        &mut self,
        pbx_id: PbxCallId,
        deadline: Instant,
        context: ForwardingContext,
        destination: ForwardingDestination,
    ) -> Result<NoAnswerTimer, ForwardingRejection> {
        self.call_runtime_mut(pbx_id)
            .ok_or(ForwardingRejection::Conflict)?;
        self.no_answer_timers
            .schedule(pbx_id, deadline, context, destination)
    }

    pub(crate) fn claim_no_answer_routes(
        &mut self,
        now: Instant,
    ) -> Vec<(NoAnswerTimer, Option<String>)> {
        let expired = self.no_answer_timers.claim_expired(now);
        expired
            .into_iter()
            .filter_map(|timer| {
                let line = self.pbx_call(timer.call_id).map(|call| call.line.clone());
                if !self.claim_ringing_forward(timer.call_id) {
                    let _ = self.no_answer_timers.cancel(timer.call_id, timer.id);
                    return None;
                }
                Some((timer, line))
            })
            .collect()
    }

    pub(crate) fn finish_no_answer_route(
        &mut self,
        pbx_id: PbxCallId,
        timer_id: NoAnswerTimerId,
        succeeded: bool,
    ) -> Vec<DriverEffect> {
        if !succeeded {
            self.rollback_ringing_forward(pbx_id);
            let _ = self.no_answer_timers.cancel(pbx_id, timer_id);
            return Vec::new();
        }
        if self.no_answer_timers.commit(pbx_id, timer_id).is_err() {
            self.rollback_ringing_forward(pbx_id);
            return Vec::new();
        }
        self.complete_ringing_forward(pbx_id)
    }

    pub(crate) fn cancel_no_answer_timer(&mut self, pbx_id: PbxCallId) -> bool {
        let Some(timer) = self.no_answer_timers.get(pbx_id) else {
            return false;
        };
        self.no_answer_timers
            .cancel_pending(pbx_id, timer.id)
            .is_ok()
    }

    pub(crate) fn clear_no_answer_route(&mut self, pbx_id: PbxCallId) {
        if let Some(record) = self.call_runtime.get_mut(&pbx_id) {
            record.no_answer = None;
        }
        if let Some(timer) = self.no_answer_timers.get(pbx_id) {
            let _ = self.no_answer_timers.cancel(pbx_id, timer.id);
        }
    }
}

impl ControllerSnapshot {
    pub fn call_runtime_record(&self, pbx_id: PbxCallId) -> Option<&CallRuntimeRecord> {
        self.call_runtime.get(&pbx_id)
    }

    #[cfg(any(test, feature = "telemetry"))]
    pub fn call_runtime_records(&self) -> impl Iterator<Item = (&PbxCallId, &CallRuntimeRecord)> {
        self.call_runtime.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::encryption::{
        AudioEncryptionAdmission, LocalEncryptionCapabilities, MediaEncryptionDecision,
        MediaEncryptionPolicy, StationEncryptionCapabilities,
    };

    #[test]
    fn encryption_admission_is_retained_only_for_the_live_call_and_station_generation() {
        let mut controller = super::super::tests::shared_inbound_controller();
        let device = DeviceId::new("SEP001122334455").unwrap();
        let generation = controller
            .registered_device(&device)
            .unwrap()
            .session_generation;
        let admission = AudioEncryptionAdmission::new(
            MediaEncryptionPolicy::default(),
            StationEncryptionCapabilities::NotReported,
            LocalEncryptionCapabilities::default(),
        );
        let first = controller
            .retain_audio_encryption(CallId(2), device.clone(), generation, admission.clone())
            .unwrap();
        assert_eq!(first.decide(), Ok(MediaEncryptionDecision::Clear));
        let retained = controller
            .retain_audio_encryption(CallId(2), device.clone(), generation, admission.clone())
            .unwrap();
        assert_eq!(retained, first);
        controller.hangup(CallId(2));
        controller.retire_ended_call_records();
        assert!(
            controller
                .retain_audio_encryption(CallId(2), device, generation, admission)
                .is_none()
        );
        controller.hangup(CallId(3));
        controller.retire_ended_call_records();
        assert!(
            controller
                .snapshot()
                .call_runtime_record(PbxCallId(8))
                .is_none()
        );
    }
}
