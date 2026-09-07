//! Native codec changes are prepared without publishing provisional codec state.

use super::{
    CallId, CallState, Codec, CodecPreferenceRejection, Controller, PbxCallId, SessionGeneration,
};
use crate::media::formats::PbxAudioFormat;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CodecMutationMode {
    PreDial,
    Held,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CodecMutation {
    pbx_id: PbxCallId,
    call_id: CallId,
    generation: u64,
    pub previous: Codec,
    selected: Codec,
    mode: CodecMutationMode,
    state: CallState,
    session_generation: SessionGeneration,
}

#[derive(Clone)]
pub(super) struct PendingCodecMutation {
    token: CodecMutation,
    preferences: Option<Option<Vec<PbxAudioFormat>>>,
}

impl Controller {
    pub(crate) fn prepare_pre_dial_codec(
        &mut self,
        pbx_id: PbxCallId,
        selected: Codec,
        preferences: Option<Vec<PbxAudioFormat>>,
    ) -> Result<CodecMutation, CodecPreferenceRejection> {
        let (appearance, _) = self.validate_pre_dial_codec(pbx_id)?;
        let call_id = self
            .call_registry
            .appearances
            .get(&appearance)
            .ok_or(CodecPreferenceRejection::Unavailable)?
            .sccp_id;
        self.prepare_codec_mutation(
            pbx_id,
            call_id,
            selected,
            CodecMutationMode::PreDial,
            Some(preferences),
        )
    }

    pub(crate) fn prepare_held_codec(
        &mut self,
        pbx_id: PbxCallId,
        call_id: CallId,
        selected: Codec,
    ) -> Result<CodecMutation, CodecPreferenceRejection> {
        self.validate_held_codec(pbx_id, call_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        self.prepare_codec_mutation(pbx_id, call_id, selected, CodecMutationMode::Held, None)
    }

    fn prepare_codec_mutation(
        &mut self,
        pbx_id: PbxCallId,
        call_id: CallId,
        selected: Codec,
        mode: CodecMutationMode,
        preferences: Option<Option<Vec<PbxAudioFormat>>>,
    ) -> Result<CodecMutation, CodecPreferenceRejection> {
        let call = self
            .call(call_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        let session_generation = self
            .registered_device(&call.device_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?
            .session_generation;
        let record = self
            .call_runtime_mut(pbx_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        if record.codec_mutation.is_some() {
            return Err(CodecPreferenceRejection::Unavailable);
        }
        let generation = record
            .codec_generation
            .checked_add(1)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        let token = CodecMutation {
            pbx_id,
            call_id,
            generation,
            previous: call.codec,
            selected,
            mode,
            state: call.state,
            session_generation,
        };
        record.codec_generation = generation;
        record.codec_mutation = Some(PendingCodecMutation { token, preferences });
        Ok(token)
    }

    pub(crate) fn commit_codec_mutation(&mut self, token: CodecMutation) -> bool {
        let Some(pending) = self
            .call_runtime
            .get(&token.pbx_id)
            .and_then(|record| record.codec_mutation.as_ref())
            .filter(|pending| pending.token == token)
            .cloned()
        else {
            return false;
        };
        let Some(call) = self.call(token.call_id) else {
            return false;
        };
        if call.pbx_id != token.pbx_id
            || call.codec != token.previous
            || call.state != token.state
            || !self.session_is_current(&call.device_id, token.session_generation)
        {
            return false;
        }
        let applied = match token.mode {
            CodecMutationMode::PreDial => self
                .set_pre_dial_codec(token.pbx_id, token.selected)
                .is_ok(),
            CodecMutationMode::Held => self
                .set_held_codec(token.pbx_id, token.call_id, token.selected)
                .is_some(),
        };
        if !applied {
            return false;
        }
        if let Some(record) = self.call_runtime.get_mut(&token.pbx_id) {
            if let Some(preferences) = pending.preferences {
                record.audio_preferences = preferences;
            }
            record.codec_mutation = None;
        }
        true
    }

    /// The native thread calls abort after restoring its old format. Until then,
    /// a rejected commit retains the mutation reservation against another setter.
    pub(crate) fn abort_codec_mutation(&mut self, token: CodecMutation) {
        if let Some(record) = self.call_runtime.get_mut(&token.pbx_id)
            && record
                .codec_mutation
                .as_ref()
                .is_some_and(|pending| pending.token == token)
        {
            record.codec_mutation = None;
        }
    }
}
