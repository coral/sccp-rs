use super::super::*;

impl Controller {
    /// Publish outbound progress with the station's resolved media-opening
    /// strategy. Coupling is a NAT compatibility operation, not the default
    /// early-media transaction.
    pub fn pbx_progress_with_media_mode(
        &mut self,
        pbx_id: PbxCallId,
        early_media: bool,
        outbound_media_mode: OutboundMediaMode,
    ) -> Vec<DriverEffect> {
        let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
            return Vec::new();
        };
        if call.direction != CallDirection::Outbound
            || call.state != CallState::Calling
            || call
                .outbound_phase
                .is_some_and(|phase| phase > OutboundCallPhase::Progress)
        {
            return Vec::new();
        }
        let publish_proceed = call
            .outbound_phase
            .is_none_or(|phase| phase < OutboundCallPhase::Routing);
        let Some(appearance_id) = call
            .active_appearance
            .or_else(|| call.appearance_ids.first().copied())
        else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        let device_id = appearance.device_id.clone();
        let call_id = appearance.sccp_id;
        let codec = appearance.codec;
        let begin_media = early_media && appearance.audio == MediaStreamState::Closed;
        let coupled = begin_media && outbound_media_mode == OutboundMediaMode::Coupled;
        if begin_media {
            appearance.audio = MediaStreamState::Opening;
            if coupled {
                appearance.audio_transmit = MediaStreamState::Opening;
                self.pending_route_media.insert(call_id);
            }
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.outbound_phase = Some(OutboundCallPhase::Progress);
        }
        self.advance_transfer_for_pbx(pbx_id, TransferPhase::Ringing);
        debug_assert!(self.invariant_error().is_none());
        let mut effects = Vec::new();
        if publish_proceed {
            effects.push(
                HandsetEffect::SetCallState {
                    device_id: device_id.clone(),
                    call_id,
                    state: HandsetCallState::Proceed,
                    stop_media: false,
                }
                .into(),
            );
        }
        if coupled {
            effects.push(
                HandsetEffect::BeginOutboundMedia {
                    device_id,
                    call_id,
                    codec,
                }
                .into(),
            );
        } else if begin_media {
            effects.push(
                HandsetEffect::BeginEarlyMedia {
                    device_id,
                    call_id,
                    codec,
                }
                .into(),
            );
        }
        effects
    }

    /// True only while an exact coupled ORC/SMT generation is awaiting its
    /// receive acknowledgement. Protocol state uses this provenance to settle
    /// the transmit side explicitly for firmware which omits a separate SMT
    /// acknowledgement.
    pub fn coupled_outbound_media_pending(&self, call_id: CallId) -> bool {
        self.pending_route_media.contains(&call_id)
    }

    /// Commit a receive acknowledgement only when it came from the device
    /// that owns the call appearance. Session-local call IDs are validated at
    /// the protocol boundary too; retaining the identity check here keeps a
    /// stale or misrouted runtime event from mutating another appearance.
    pub fn media_opened_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> Vec<DriverEffect> {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device_id)
        {
            return Vec::new();
        }
        self.media_opened(call_id, endpoint)
    }

    pub fn media_opened(&mut self, call_id: CallId, endpoint: MediaEndpoint) -> Vec<DriverEffect> {
        let outbound_hole_punch = self.pending_route_media.remove(&call_id);
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        match endpoint.codec.kind() {
            CodecKind::Audio => {
                appearance.audio = MediaStreamState::Open(endpoint);
                if !matches!(appearance.audio_transmit, MediaStreamState::Open(_)) {
                    appearance.audio_transmit = MediaStreamState::Opening;
                }
            }
            CodecKind::Video => return Vec::new(),
            CodecKind::Text | CodecKind::Data | CodecKind::TelephoneEvent | CodecKind::Unknown => {
                return Vec::new();
            }
        }
        let pbx_id = self
            .barges
            .by_handset
            .get(&call_id)
            .map_or(appearance.pbx_id, |barge| barge.barger_call_id);
        let device_id = appearance.device_id.clone();
        let handset_call_id = appearance.sccp_id;
        let codec = appearance.codec;
        let presentation = appearance.clone();
        let pending_answer = self
            .pending_phone_answers
            .remove(&call_id)
            .filter(|pending_pbx_id| *pending_pbx_id == appearance.pbx_id);
        debug_assert!(self.invariant_error().is_none());
        let configure = if outbound_hole_punch {
            PbxEffect::ConfigureMediaOnly {
                call_id: pbx_id,
                device_id,
                codec,
                remote: endpoint,
            }
        } else {
            PbxEffect::ConfigureMedia {
                call_id: pbx_id,
                device_id,
                handset_call_id,
                codec,
                remote: endpoint,
            }
        };
        let mut effects = Vec::new();
        if presentation.info.direction == CallDirection::Outbound {
            if outbound_hole_punch {
                effects.push(
                    HandsetEffect::StartTone {
                        device_id: presentation.device_id.clone(),
                        call_id: presentation.sccp_id,
                        tone: Tone::Silence,
                    }
                    .into(),
                );
            }
            effects.push(
                HandsetEffect::SetCallInfo {
                    device_id: presentation.device_id.clone(),
                    call_id: presentation.sccp_id,
                    info: presentation.info.clone(),
                }
                .into(),
            );
        }
        effects.push(configure.into());
        if let Some(pbx_id) = pending_answer {
            // ConfigureMedia's immediate handset follow-up sends
            // StartMediaTransmission. Only after that succeeds may Asterisk
            // be answered and the full Connected presentation be published.
            effects.push(PbxEffect::Answer { call_id: pbx_id }.into());
        }
        if presentation.state == CallState::Connected
            && presentation.info.direction == CallDirection::Inbound
        {
            effects.push(appearance_state_effect(
                &presentation,
                HandsetCallState::Connected,
                false,
            ));
        }
        effects.extend(self.begin_auto_video(call_id));
        effects
    }

    /// Commit a transmit acknowledgement only for its owning handset.
    pub fn media_transmission_started_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> Vec<DriverEffect> {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device_id)
        {
            return Vec::new();
        }
        self.media_transmission_started(call_id, endpoint)
    }

    pub fn media_transmission_started(
        &mut self,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> Vec<DriverEffect> {
        if endpoint.codec.kind() != CodecKind::Audio {
            return Vec::new();
        }
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        if appearance.audio_transmit != MediaStreamState::Opening {
            return Vec::new();
        }
        appearance.audio_transmit = MediaStreamState::Open(endpoint);
        debug_assert!(self.invariant_error().is_none());
        Vec::new()
    }

    pub(in crate::runtime::controller) fn begin_auto_video(
        &mut self,
        call_id: CallId,
    ) -> Vec<DriverEffect> {
        let Some(appearance) = self.appearance_for_call(call_id) else {
            return Vec::new();
        };
        let device_id = appearance.device_id.clone();
        let Some(current_generation) = self
            .devices
            .get(&device_id)
            .filter(|device| device.active_call == Some(call_id))
            .map(|device| device.session_generation)
        else {
            return Vec::new();
        };
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return Vec::new();
        };
        let Some(plan) = appearance.video.plan() else {
            return Vec::new();
        };
        if appearance.state != CallState::Connected
            || plan.mode != VideoMode::Auto
            || plan.session_generation != current_generation
            || !appearance.video.begin_receive()
        {
            return Vec::new();
        }
        vec![
            HandsetEffect::OpenVideoReceive {
                device_id,
                call_id,
                session_generation: current_generation,
            }
            .into(),
        ]
    }

    pub fn install_video_plan_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        plan: VideoPlan,
        readiness: VideoPlanReadiness,
    ) -> bool {
        if self
            .devices
            .get(device_id)
            .is_none_or(|device| device.session_generation != plan.session_generation)
        {
            return false;
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        if &appearance.device_id != device_id {
            return false;
        }
        appearance.video = match readiness {
            VideoPlanReadiness::Ready => VideoMediaState::ready(plan),
            VideoPlanReadiness::Blocked(reason) => VideoMediaState::blocked(plan, reason),
        };
        true
    }

    pub fn set_video_audio_only_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        reason: VideoFallbackReason,
    ) -> bool {
        if self
            .devices
            .get(device_id)
            .is_none_or(|device| device.session_generation != session_generation)
        {
            return false;
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        if &appearance.device_id != device_id {
            return false;
        }
        appearance.video = VideoMediaState::audio_only(reason);
        true
    }

    pub fn video_mode_for_device(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> Vec<DriverEffect> {
        let Some(device) = self.devices.get(device_id) else {
            return Vec::new();
        };
        let current_generation = device.session_generation;
        if device.active_call != Some(call_id) {
            return Vec::new();
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return Vec::new();
        };
        if &appearance.device_id != device_id
            || appearance.state != CallState::Connected
            || appearance.video.plan().is_none_or(|plan| {
                plan.mode != VideoMode::User || plan.session_generation != current_generation
            })
            || appearance.video.fallback_reason().is_some()
        {
            return Vec::new();
        }
        let Some(session_generation) = appearance.video.plan().map(|plan| plan.session_generation)
        else {
            return Vec::new();
        };
        let effect = if appearance.video.is_idle() {
            if !appearance.video.begin_receive() {
                return Vec::new();
            }
            HandsetEffect::OpenVideoReceive {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }
        } else {
            appearance.video.close_streams();
            HandsetEffect::StopVideo {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }
        };
        vec![effect.into()]
    }

    /// Returns the plan only while its exact receive-open command is pending.

    /// Returns the plan only while its exact transmit-start command is pending.

    pub fn video_receive_opened_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
    ) -> bool {
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        &appearance.device_id == device_id
            && appearance
                .video
                .plan()
                .is_some_and(|plan| plan.session_generation == session_generation)
            && appearance.video.opened_receive(codec, endpoint)
    }

    pub fn video_transmit_opened_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        codec: Codec,
        endpoint: MediaEndpointAddress,
        passthrough_party_id: PassthroughPartyId,
    ) -> bool {
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return false;
        };
        &appearance.device_id == device_id
            && appearance
                .video
                .plan()
                .is_some_and(|plan| plan.session_generation == session_generation)
            && appearance
                .video
                .opened_transmit(codec, endpoint, passthrough_party_id)
    }

    pub fn begin_video_transmit_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
    ) -> Vec<DriverEffect> {
        if self
            .devices
            .get(device_id)
            .is_none_or(|device| device.session_generation != session_generation)
        {
            return Vec::new();
        }
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return Vec::new();
        };
        if &appearance.device_id != device_id
            || appearance.state != CallState::Connected
            || appearance
                .video
                .plan()
                .is_none_or(|plan| plan.session_generation != session_generation)
            || !appearance.video.begin_transmit()
        {
            return Vec::new();
        }
        vec![
            HandsetEffect::StartVideoTransmit {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }
            .into(),
        ]
    }

    pub fn video_fallback_for_device(
        &mut self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        reason: VideoFallbackReason,
    ) -> VideoFallbackOutcome {
        let Some(appearance) = self.appearance_for_call_mut(call_id) else {
            return VideoFallbackOutcome::Ignored;
        };
        if &appearance.device_id != device_id
            || appearance
                .video
                .plan()
                .is_none_or(|plan| plan.session_generation != session_generation)
            || !appearance.video.accepts_failure(reason)
        {
            return VideoFallbackOutcome::Ignored;
        }
        let was_active = !appearance.video.is_idle();
        appearance.video = VideoMediaState::audio_only(reason);
        VideoFallbackOutcome::Applied {
            cleanup: was_active.then(|| VideoCleanup {
                device_id: device_id.clone(),
                call_id,
                session_generation,
            }),
        }
    }

    pub fn recover_optional_video_effect_failure(
        &mut self,
        effect: &HandsetEffect,
    ) -> Option<Vec<DriverEffect>> {
        match effect {
            HandsetEffect::OpenVideoReceive {
                device_id,
                call_id,
                session_generation,
            } => Some(
                self.video_fallback_for_device(
                    device_id,
                    *session_generation,
                    *call_id,
                    VideoFallbackReason::ReceiveFailed,
                )
                .into_effects(),
            ),
            HandsetEffect::StartVideoTransmit {
                device_id,
                call_id,
                session_generation,
            } => Some(
                self.video_fallback_for_device(
                    device_id,
                    *session_generation,
                    *call_id,
                    VideoFallbackReason::TransmitFailed,
                )
                .into_effects(),
            ),
            HandsetEffect::StopVideo {
                device_id,
                call_id,
                session_generation,
            } => {
                let Some(appearance) = self.appearance_for_call_mut(*call_id) else {
                    return Some(Vec::new());
                };
                if &appearance.device_id == device_id
                    && appearance
                        .video
                        .plan()
                        .is_some_and(|plan| plan.session_generation == *session_generation)
                {
                    appearance.video =
                        VideoMediaState::audio_only(VideoFallbackReason::TransmitFailed);
                }
                Some(Vec::new())
            }
            _ => None,
        }
    }

    /// Marks an acknowledged handset transmit stream as awaiting a peer
    /// retarget acknowledgement. The prior endpoint is returned so an adapter
    /// can restore it if command enqueueing fails.
    pub fn media_retarget_started(&mut self, call_id: CallId) -> Option<MediaEndpoint> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id).copied()?;
        let appearance = self.call_registry.appearances.get_mut(&appearance_id)?;
        let MediaStreamState::Open(previous) = appearance.audio_transmit else {
            return None;
        };
        appearance.audio_transmit = MediaStreamState::Opening;
        debug_assert!(self.invariant_error().is_none());
        Some(previous)
    }

    pub fn media_retarget_enqueue_failed(
        &mut self,
        call_id: CallId,
        previous: MediaEndpoint,
    ) -> bool {
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return false;
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return false;
        };
        if appearance.audio_transmit != MediaStreamState::Opening {
            return false;
        }
        appearance.audio_transmit = MediaStreamState::Open(previous);
        debug_assert!(self.invariant_error().is_none());
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn media_retarget_compensation_started(
        &mut self,
        call_id: CallId,
    ) -> Option<MediaStreamState> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id).copied()?;
        let appearance = self.call_registry.appearances.get_mut(&appearance_id)?;
        let previous = appearance.audio_transmit;
        if matches!(previous, MediaStreamState::Closed) {
            return None;
        }
        appearance.audio_transmit = MediaStreamState::Opening;
        Some(previous)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn media_retarget_compensation_enqueue_failed(
        &mut self,
        call_id: CallId,
        previous: MediaStreamState,
    ) -> bool {
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return false;
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return false;
        };
        if appearance.audio_transmit != MediaStreamState::Opening {
            return false;
        }
        appearance.audio_transmit = previous;
        true
    }

    pub fn begin_immediate_divert(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        target: VoicemailTarget,
    ) -> Result<VoicemailPlan, VoicemailRejection> {
        let appearance = self
            .appearance_for_call(call_id)
            .filter(|appearance| &appearance.device_id == device_id)
            .cloned()
            .ok_or(VoicemailRejection::Conflict)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(VoicemailRejection::Conflict)?;
        if appearance.state != CallState::Ringing
            || call.state != CallState::Ringing
            || call.active_appearance.is_some()
        {
            return Err(VoicemailRejection::InvalidPhase);
        }
        self.begin_voicemail_claim(&appearance, VoicemailAction::ImmediateDivert, target)
    }

    pub(in crate::runtime::controller) fn validate_pre_dial_codec(
        &self,
        pbx_id: PbxCallId,
    ) -> Result<(CallAppearanceId, Codec), CodecPreferenceRejection> {
        let call = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        if call.direction != CallDirection::Outbound
            || !matches!(call.state, CallState::Collecting | CallState::Calling)
        {
            return Err(CodecPreferenceRejection::NotPreDial);
        }
        let [appearance_id] = call.appearance_ids.as_slice() else {
            return Err(CodecPreferenceRejection::Ambiguous);
        };
        let appearance = self
            .call_registry
            .appearances
            .get(appearance_id)
            .ok_or(CodecPreferenceRejection::Unavailable)?;
        if appearance.audio != MediaStreamState::Closed
            || appearance.audio_transmit != MediaStreamState::Closed
            || !appearance.video.is_idle()
        {
            return Err(CodecPreferenceRejection::NotPreDial);
        }
        Ok((*appearance_id, appearance.codec))
    }

    pub fn set_pre_dial_codec(
        &mut self,
        pbx_id: PbxCallId,
        codec: Codec,
    ) -> Result<Codec, CodecPreferenceRejection> {
        let (appearance_id, previous) = self.validate_pre_dial_codec(pbx_id)?;
        self.call_registry
            .appearances
            .get_mut(&appearance_id)
            .expect("validated call appearance")
            .codec = codec;
        debug_assert!(self.invariant_error().is_none());
        Ok(previous)
    }

    pub(in crate::runtime::controller) fn validate_held_codec(
        &self,
        pbx_id: PbxCallId,
        call_id: CallId,
    ) -> Option<(CallAppearanceId, Codec)> {
        let call = self.call_registry.pbx.get(&pbx_id)?;
        if call.state != CallState::Held {
            return None;
        }
        let appearance_id = self.call_registry.by_sccp.get(&call_id).copied()?;
        if call.active_appearance != Some(appearance_id) {
            return None;
        }
        let appearance = self.call_registry.appearances.get(&appearance_id)?;
        if appearance.pbx_id != pbx_id
            || appearance.state != CallState::Held
            || appearance.audio != MediaStreamState::Closed
            || appearance.audio_transmit != MediaStreamState::Closed
        {
            return None;
        }
        Some((appearance_id, appearance.codec))
    }

    pub fn set_held_codec(
        &mut self,
        pbx_id: PbxCallId,
        call_id: CallId,
        codec: Codec,
    ) -> Option<Codec> {
        let (appearance_id, previous) = self.validate_held_codec(pbx_id, call_id)?;
        self.call_registry
            .appearances
            .get_mut(&appearance_id)?
            .codec = codec;
        debug_assert!(self.invariant_error().is_none());
        Some(previous)
    }

    pub(in crate::runtime::controller) fn device_supports_codec(
        &self,
        device: &DeviceId,
        codec: Codec,
    ) -> bool {
        codec.kind() == CodecKind::Audio
            && self.devices.get(device).is_some_and(|state| {
                state.capabilities.as_ref().is_some_and(|capabilities| {
                    capabilities
                        .audio()
                        .iter()
                        .any(|capability| capability.codec == codec)
                })
            })
    }
}
