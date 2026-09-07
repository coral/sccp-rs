//! Shared read-only controller queries for live state and immutable snapshots.

macro_rules! controller_queries {
    ($state:ty) => {
        impl $state {
    pub fn active_call_by_pbx(&self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        let appearance_id = self.call_registry.pbx.get(&pbx_id)?.active_appearance?;
        self.call_snapshot(appearance_id)
    }

    pub fn active_call_id(&self, pbx_id: PbxCallId) -> Option<CallId> {
        self.active_call_by_pbx(pbx_id).map(|call| call.sccp_id)
    }

    pub fn active_or_primary_call_by_pbx(&self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        self.active_call_by_pbx(pbx_id)
            .or_else(|| self.primary_call_by_pbx(pbx_id))
    }

    pub fn appearance_for_call(&self, call_id: CallId) -> Option<&CallAppearance> {
        self.call_registry
            .by_sccp
            .get(&call_id)
            .and_then(|appearance_id| self.call_registry.appearances.get(appearance_id))
    }

    pub fn appearances_for_device(
        &self,
        device: &DeviceId,
    ) -> impl Iterator<Item = &CallAppearance> {
        self.call_registry
            .by_device
            .get(device)
            .into_iter()
            .flatten()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
    }

    pub fn appearances_for_pbx(&self, pbx_id: PbxCallId) -> impl Iterator<Item = &CallAppearance> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .into_iter()
            .flat_map(|call| call.appearance_ids.iter())
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
    }



    pub fn barge_session_by_pbx(&self, pbx_id: PbxCallId) -> Option<&BargeSession> {
        self.barges
            .by_pbx
            .get(&pbx_id)
            .and_then(|call_id| self.barges.by_handset.get(call_id))
    }

    pub fn call(&self, call_id: CallId) -> Option<CallSnapshot> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id)?;
        self.call_snapshot(*appearance_id)
    }

    pub fn call_appearance(&self, appearance_id: CallAppearanceId) -> Option<&CallAppearance> {
        self.call_registry.appearances.get(&appearance_id)
    }



    pub fn call_info(&self, call_id: CallId) -> Option<&CallInfo> {
        self.appearance_for_call(call_id)
            .map(|appearance| &appearance.info)
    }

    pub fn call_line_instance(&self, call_id: CallId) -> Option<u32> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.line_instance)
    }

    pub fn call_metadata(&self, pbx_id: PbxCallId) -> Option<&CallMetadata> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .map(|call| &call.metadata)
    }

    pub fn call_pbx_id(&self, call_id: CallId) -> Option<PbxCallId> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.pbx_id)
    }

    pub fn call_privacy(&self, call_id: CallId) -> Option<bool> {
        let appearance = self.appearance_for_call(call_id)?;
        self.call_registry
            .pbx
            .get(&appearance.pbx_id)
            .map(|call| call.privacy)
    }

    pub(in crate::runtime::controller) fn call_snapshot(
        &self,
        appearance_id: CallAppearanceId,
    ) -> Option<CallSnapshot> {
        let appearance = self.call_registry.appearances.get(&appearance_id)?;
        let call = self.call_registry.pbx.get(&appearance.pbx_id)?;
        Some(CallSnapshot {
            sccp_id: appearance.sccp_id,
            pbx_id: call.id,
            device_id: appearance.device_id.clone(),
            line_instance: appearance.line_instance,
            line: call.line.clone(),
            direction: call.direction,
            state: appearance.state,
            digits: call.digits.clone(),
            info: appearance.info.clone(),
            metadata: call.metadata.clone(),
            codec: appearance.codec,
            audio: appearance.audio,
            audio_transmit: appearance.audio_transmit,
            video: appearance.video.clone(),
            digit_deadline: call.digit_deadline,
        })
    }

    pub fn call_state(&self, call_id: CallId) -> Option<CallState> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.state)
    }

    pub fn calls(&self) -> impl Iterator<Item = CallSnapshot> + '_ {
        self.call_registry
            .appearances
            .keys()
            .filter_map(|appearance_id| self.call_snapshot(*appearance_id))
    }

    /// Build one typed PBX announcement from committed conference state.
    /// Callers invoke this only after the associated bridge mutation succeeds.
    pub fn conference_announcement_effects(
        &self,
        conference_id: ConferenceId,
        announcement: ConferenceAnnouncement,
    ) -> Vec<DriverEffect> {
        let Some(session) = self.conference_session_by_id(conference_id) else {
            return Vec::new();
        };
        Controller::conference_announcement_effects_for_session(session, announcement)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn conference_mutation_is_active(&self, token: ConferenceMutationToken) -> bool {
        if self.conference_mutations.get(&token.owner) != Some(&token.generation) {
            return false;
        }
        match token.owner {
            ConferenceMutationOwner::Session(conference_id) => {
                self.conference_session_by_id(conference_id).is_some()
            }
            ConferenceMutationOwner::Destination(call_id) => {
                self.call_registry.pbx.contains_key(&call_id)
            }
        }
    }

    pub fn conference_session(&self, call_id: CallId) -> Option<&ConferenceSession> {
        let pbx_id = self.appearance_for_call(call_id)?.pbx_id;
        self.conference_session_by_pbx(pbx_id)
    }

    pub fn conference_session_by_id(
        &self,
        conference_id: ConferenceId,
    ) -> Option<&ConferenceSession> {
        self.conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
    }

    pub fn conference_session_by_pbx(&self, pbx_id: PbxCallId) -> Option<&ConferenceSession> {
        let consultation = self.conferences.by_pbx.get(&pbx_id)?;
        self.conferences.by_consultation.get(consultation)
    }



    pub(in crate::runtime::controller) fn device_has_active_call(
        &self,
        device_id: &DeviceId,
    ) -> bool {
        self.devices
            .get(device_id)
            .and_then(|device| device.active_call)
            .and_then(|call_id| self.appearance_for_call(call_id))
            .is_some_and(|appearance| {
                matches!(
                    appearance.state,
                    CallState::Collecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::TransferCollecting
                )
            })
    }

    pub fn feature_state(&self, device: &DeviceId) -> Option<&DeviceFeatureState> {
        self.features.get(device)
    }



    /// Resolve a hook flash against the exact active handset identity without
    /// mutating call state. A waiting inbound call takes precedence over
    /// starting a consultation transfer; existing transfer consultations use
    /// the same action so a second flash can complete that transaction.
    pub fn hook_flash_action(&self, device_id: &DeviceId, call_id: CallId) -> HookFlashAction {
        let Some(device) = self.devices.get(device_id) else {
            return HookFlashAction::Ignore;
        };
        if device.active_call != Some(call_id) {
            return HookFlashAction::Ignore;
        }
        if let Some(transfer) = self.transfers.get(device_id) {
            return if transfer
                .consultation
                .is_some_and(|leg| leg.handset_call_id == call_id)
            {
                HookFlashAction::Transfer
            } else {
                HookFlashAction::Ignore
            };
        }
        let Some(active) = self.appearance_for_call(call_id) else {
            return HookFlashAction::Ignore;
        };
        if active.state != CallState::Connected
            || self.conferences.by_pbx.contains_key(&active.pbx_id)
            || self.barges.by_handset.contains_key(&call_id)
        {
            return HookFlashAction::Ignore;
        }
        let mut waiting = self
            .appearances_for_device(device_id)
            .filter(|appearance| {
                appearance.sccp_id != call_id && appearance.state == CallState::Ringing
            })
            .map(|appearance| appearance.sccp_id)
            .collect::<Vec<_>>();
        waiting.sort_by_key(|waiting_call_id| waiting_call_id.0);
        waiting
            .first()
            .copied()
            .map_or(HookFlashAction::Transfer, HookFlashAction::AnswerWaiting)
    }

    pub(in crate::runtime::controller) fn inbound_offer_for_appearance(
        &self,
        appearance: &CallAppearance,
    ) -> InboundOffer {
        InboundOffer {
            device_id: appearance.device_id.clone(),
            line_instance: appearance.line_instance,
            call_id: appearance.sccp_id,
            ring_mode: appearance.ring_mode,
            state: if self.device_has_active_call(&appearance.device_id) {
                HandsetCallState::CallWaiting
            } else {
                HandsetCallState::RingIn
            },
        }
    }

    pub fn inbound_offers_for_pbx(&self, pbx_id: PbxCallId) -> Vec<InboundOffer> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .filter(|call| call.direction == CallDirection::Inbound)
            .into_iter()
            .flat_map(|call| call.appearance_ids.iter())
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .filter(|appearance| appearance.state == CallState::Ringing)
            .map(|appearance| self.inbound_offer_for_appearance(appearance))
            .collect()
    }

    pub fn is_registered(&self, device: &DeviceId) -> bool {
        self.devices.contains_key(device)
    }

    pub fn pbx_call(&self, pbx_id: PbxCallId) -> Option<&PbxCall> {
        self.call_registry.pbx.get(&pbx_id)
    }

    pub fn primary_call_by_pbx(&self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .and_then(|call| call.appearance_ids.first())
            .and_then(|appearance_id| self.call_snapshot(*appearance_id))
    }

    pub fn refresh_video_for_pbx(&self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(call) = self.active_call_by_pbx(pbx_id) else {
            return Vec::new();
        };
        let Some(appearance) = self.appearance_for_call(call.sccp_id) else {
            return Vec::new();
        };
        let VideoMediaState::Ready {
            plan,
            transmit: VideoStreamState::Open { .. },
            transmit_token: Some(passthrough_party_id),
            ..
        } = &appearance.video
        else {
            return Vec::new();
        };
        if appearance.state != CallState::Connected
            || self
                .devices
                .get(&appearance.device_id)
                .is_none_or(|device| {
                    device.session_generation != plan.session_generation
                        || device.active_call != Some(call.sccp_id)
                })
        {
            return Vec::new();
        }
        vec![
            HandsetEffect::RefreshVideo {
                device_id: appearance.device_id.clone(),
                call_id: call.sccp_id,
                session_generation: plan.session_generation,
                passthrough_party_id: *passthrough_party_id,
            }
            .into(),
        ]
    }

    pub fn registered_device(&self, device: &DeviceId) -> Option<&RegisteredDevice> {
        self.devices.get(device)
    }

    pub fn registered_devices(&self) -> impl Iterator<Item = (&DeviceId, &RegisteredDevice)> {
        self.devices.iter()
    }

    pub fn session_is_current(
        &self,
        device: &DeviceId,
        session_generation: SessionGeneration,
    ) -> bool {
        self.devices
            .get(device)
            .is_some_and(|state| state.session_generation == session_generation)
    }

    pub fn transfer_generation_is_active(
        &self,
        device_id: &DeviceId,
        transaction_id: TransferId,
    ) -> bool {
        self.transfers
            .get(device_id)
            .is_some_and(|transaction| transaction.id == transaction_id)
    }

    pub fn transfer_transaction(&self, call_id: CallId) -> Option<&TransferTransaction> {
        let appearance = self.appearance_for_call(call_id)?;
        self.transfers.for_leg(TransferLeg {
            handset_call_id: call_id,
            pbx_call_id: appearance.pbx_id,
        })
    }

    pub fn transfer_transaction_for_device(
        &self,
        device_id: &DeviceId,
    ) -> Option<&TransferTransaction> {
        self.transfers.get(device_id)
    }

    pub(in crate::runtime::controller) fn video_plan_for_device_matching(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        state_matches: impl FnOnce(&VideoMediaState) -> bool,
    ) -> Option<&VideoPlan> {
        let device = self.devices.get(device_id)?;
        if device.session_generation != session_generation {
            return None;
        }
        let appearance = self.appearance_for_call(call_id)?;
        if &appearance.device_id != device_id || appearance.state != CallState::Connected {
            return None;
        }
        if !state_matches(&appearance.video) {
            return None;
        }
        appearance
            .video
            .plan()
            .filter(|plan| plan.session_generation == session_generation)
    }

    pub fn video_refresh_is_current(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
        passthrough_party_id: PassthroughPartyId,
    ) -> bool {
        self.video_plan_for_device_matching(device_id, session_generation, call_id, |video| {
            matches!(
                video,
                VideoMediaState::Ready {
                    transmit: VideoStreamState::Open { .. },
                    transmit_token: Some(token),
                    ..
                } if *token == passthrough_party_id
            )
        })
        .is_some()
    }

    pub fn voicemail_generation_is_active(
        &self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> bool {
        self.voicemail
            .get(device_id)
            .is_some_and(|transaction| transaction.id == transaction_id)
    }
pub fn opening_video_receive_plan_for_device(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
    ) -> Option<&VideoPlan> {
        self.video_plan_for_device_matching(device_id, session_generation, call_id, |video| {
            video.receive() == VideoStreamState::Opening
        })
    }
pub fn opening_video_transmit_plan_for_device(
        &self,
        device_id: &DeviceId,
        session_generation: SessionGeneration,
        call_id: CallId,
    ) -> Option<&VideoPlan> {
        self.video_plan_for_device_matching(device_id, session_generation, call_id, |video| {
            video.transmit() == VideoStreamState::Opening
        })
    }
        }
    };
}
pub(super) use controller_queries;
