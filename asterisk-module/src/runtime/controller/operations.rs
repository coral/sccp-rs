//! Typed compound transitions used at asynchronous and native boundaries.

use super::*;

#[cfg_attr(
    all(test, feature = "development"),
    expect(
        dead_code,
        reason = "native hold effect executors consume these payloads"
    )
)]
pub(crate) enum HoldPlan {
    Missing,
    Regular(Vec<DriverEffect>),
    Conference {
        device_id: DeviceId,
        result: Result<
            (
                ConferenceId,
                ParticipantId,
                ConferenceMutationToken,
                Vec<DriverEffect>,
            ),
            ConferenceParticipantRejection,
        >,
    },
}

impl Controller {
    pub(crate) fn cancel_failed_inbound_offer(
        &mut self,
        call_id: CallId,
        pbx_id: PbxCallId,
    ) -> bool {
        let removed = self.cancel_inbound_offer(call_id);
        self.cancel_call_waiting_tone(call_id);
        removed && self.pbx_call(pbx_id).is_none()
    }
    pub(crate) fn schedule_ready_auto_answers(
        &mut self,
        pbx_id: PbxCallId,
        auto_answer: AutoAnswerPolicy,
        now: Instant,
    ) -> (
        Option<Result<usize, AutoAnswerScheduleRejection>>,
        Vec<CallTransition>,
    ) {
        if self.has_auto_answer_request(pbx_id) {
            let scheduled = self.schedule_auto_answers(pbx_id, auto_answer, now);
            (Some(scheduled), self.expire_auto_answers(now))
        } else {
            (None, Vec::new())
        }
    }
    pub(crate) fn abort_present_barge(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        if self.barge_session(call_id).is_some() {
            self.abort_barge(call_id, true, true)
        } else {
            Vec::new()
        }
    }
    pub(crate) fn reject_native_call(
        &mut self,
        handset_call_id: CallId,
    ) -> (bool, Vec<DriverEffect>) {
        if self.barge_session(handset_call_id).is_some() {
            (true, self.abort_barge(handset_call_id, false, false))
        } else {
            (false, self.hangup(handset_call_id))
        }
    }
    pub(crate) fn abort_native_barge(&mut self, barger_call_id: PbxCallId) -> Vec<DriverEffect> {
        self.barge_session_by_pbx(barger_call_id)
            .map(|session| session.handset_call_id)
            .map(|call_id| self.abort_barge(call_id, false, true))
            .unwrap_or_default()
    }
    pub(crate) fn expire_call_deadlines(
        &mut self,
        now: Instant,
    ) -> (Vec<DriverEffect>, Vec<CallTransition>) {
        let mut effects = self.expire_digits(now);
        effects.extend(self.expire_call_waiting_tones(now));
        (effects, self.expire_auto_answers(now))
    }
    pub(crate) fn apply_party_snapshot(
        &mut self,
        pbx_id: PbxCallId,
        snapshot: crate::pbx::party::PartySnapshot,
    ) -> Vec<DriverEffect> {
        let mut effects =
            self.update_call_info_by_pbx(pbx_id, |current| snapshot.apply_to_call_info(current));
        effects.extend(self.pbx_remote_identity_ready(pbx_id));
        effects
    }
    pub(crate) fn prepare_remote_hangup(
        &mut self,
        pbx_id: PbxCallId,
        remote_hangup_tone: Option<Tone>,
        presentation: Duration,
        now: Instant,
    ) -> (
        Option<ConferenceId>,
        Option<RemoteHangupPlan>,
        Option<ConferenceSession>,
    ) {
        let conference_id = self
            .conference_session_by_pbx(pbx_id)
            .map(|session| session.id);
        let plan = self.begin_remote_hangup(pbx_id, remote_hangup_tone, presentation, now);
        if let Some(token) = plan.as_ref().and_then(|plan| plan.pending) {
            if let Some(pending) = self
                .pending_remote_hangups
                .values_mut()
                .find(|pending| pending.token == token)
            {
                pending.deferred = true;
            }
        }
        let surviving = conference_id
            .and_then(|conference_id| self.conference_session_by_id(conference_id))
            .cloned();
        (conference_id, plan, surviving)
    }
    pub(crate) fn activate_remote_hangup(
        &mut self,
        token: RemoteHangupToken,
        presentation: Duration,
        now: Instant,
    ) -> bool {
        let Some(pending) = self
            .pending_remote_hangups
            .values_mut()
            .find(|pending| pending.token == token)
        else {
            return false;
        };
        if pending.deferred {
            pending.deadline = now + presentation;
            pending.deferred = false;
        }
        true
    }

    pub(crate) fn reload_policy(
        &mut self,
        next: crate::config::ModuleConfig,
        feature_states: HashMap<DeviceId, DeviceFeatureState>,
        registered_after: HashSet<DeviceId>,
    ) -> (
        Vec<DeviceId>,
        std::collections::BTreeMap<DeviceId, DeviceFeatureState>,
    ) {
        let previous_feature_states = registered_after
            .iter()
            .filter_map(|device| {
                self.feature_state(device)
                    .cloned()
                    .map(|state| (device.clone(), state))
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        self.set_interdigit_timeout(Duration::from_millis(next.general.interdigit_timeout_ms));
        self.set_first_digit_timeout(Duration::from_millis(next.general.first_digit_timeout_ms));
        self.set_simulated_enbloc(next.general.simulate_enbloc);
        self.set_overlap_devices(
            next.devices
                .values()
                .filter(|device| device.allow_overlap)
                .map(|device| device.id.clone()),
        );
        self.set_line_dial_tones(
            next.line_features
                .iter()
                .map(|(line, features)| (line.clone(), features.dial_tones.clone())),
        );
        self.set_line_incoming_limits(
            next.line_features
                .iter()
                .map(|(line, features)| (line.clone(), features.incoming_limit)),
        );
        self.replace_feature_states(feature_states.clone());
        let registered = self
            .registered_devices()
            .filter(|(device, _)| registered_after.contains(*device))
            .map(|(device, _)| device.clone())
            .collect::<Vec<_>>();
        (registered, previous_feature_states)
    }
    pub(crate) fn set_called_party(
        &mut self,
        pbx_id: PbxCallId,
        name: Option<String>,
        number: String,
    ) -> Vec<DriverEffect> {
        self.update_call_info_by_pbx(pbx_id, |current| {
            let mut info = current.clone();
            info.called_name = name.clone().unwrap_or_default();
            info.called_number.clone_from(&number);
            info
        })
    }
    pub(crate) fn toggle_call_privacy(
        &mut self,
        call_id: CallId,
    ) -> Option<(CallSnapshot, bool, bool)> {
        let call = self.call(call_id)?;
        let previous = self.call_privacy(call_id)?;
        let enabled = !previous;
        self.set_call_privacy(call_id, enabled)
            .then_some((call, previous, enabled))
    }
    pub(crate) fn prepare_parking(
        &mut self,
        call_id: CallId,
        enabled: bool,
        lot: Option<String>,
        deadline: Instant,
    ) -> (
        Option<PbxCallId>,
        Result<Vec<DriverEffect>, ParkingRejection>,
    ) {
        if !self.parking.has_capacity() || deadline <= Instant::now() {
            return (None, Err(ParkingRejection::Unavailable));
        }
        let pbx_id = self.call_pbx_id(call_id);
        let result = self.park(call_id, enabled, lot.clone());
        if result.is_ok() {
            if let Some(call) = self.call(call_id) {
                self.parking.parks.insert(
                    call_id,
                    parking::PendingPark {
                        pbx_id: call.pbx_id,
                        device_id: call.device_id,
                        requested_lot: lot,
                        parkee_unique_id: None,
                        deadline,
                    },
                );
            }
        }
        (pbx_id, result)
    }
    pub(crate) fn prepare_parking_retrieval(
        &mut self,
        call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        lot: String,
        slot: u32,
        info: CallInfo,
        deadline: Instant,
    ) -> (
        Option<PbxCallId>,
        Result<Vec<DriverEffect>, ParkingRejection>,
    ) {
        if !self.parking.commit_claim(call_id, &lot, slot) {
            return (None, Err(ParkingRejection::Conflict));
        }
        let device_id = binding.device_id.clone();
        let effects =
            self.begin_parking_retrieval(call_id, binding, codec, Some(lot.clone()), slot, info);
        let pbx_id = self.call_pbx_id(call_id);
        if effects.is_ok() {
            if let Some(pbx_id) = pbx_id {
                self.parking.retrievals.insert(
                    call_id,
                    parking::PendingRetrieval {
                        pbx_id,
                        device_id,
                        lot: lot.clone(),
                        slot,
                        deadline,
                    },
                );
            }
        }
        if effects.is_err() {
            self.parking.registry.release_claim(&lot, slot, call_id);
        }
        (pbx_id, effects)
    }
    pub(crate) fn prepare_join_calls(
        &mut self,
        device_id: DeviceId,
        call_id: CallId,
        permitted: bool,
        media_policy: ConferenceMediaPolicy,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection> {
        self.join_calls_with_media(&device_id, call_id, permitted, media_policy)
            .and_then(|effects| {
                self.claim_conference_mutation(call_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceRejection::Conflict)
            })
    }
    pub(crate) fn prepare_conference_removal(
        &mut self,
        device_id: DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceParticipantRejection> {
        self.begin_conference_participant_removal(&device_id, conference_id, participant_id)
            .and_then(|effects| {
                self.claim_conference_mutation_by_id(conference_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceParticipantRejection::Conflict)
            })
    }
    pub(crate) fn prepare_conference_mute(
        &mut self,
        device_id: DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceParticipantRejection> {
        self.begin_conference_participant_mute(&device_id, conference_id, participant_id, muted)
            .and_then(|effects| {
                self.claim_conference_mutation_by_id(conference_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceParticipantRejection::Conflict)
            })
    }
    pub(crate) fn prepare_conference_role_change(
        &mut self,
        device_id: DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceParticipantRejection> {
        self.begin_conference_participant_role_change(
            &device_id,
            conference_id,
            participant_id,
            moderator,
        )
        .and_then(|effects| {
            self.claim_conference_mutation_by_id(conference_id)
                .map(|mutation| (mutation, effects))
                .ok_or(ConferenceParticipantRejection::Conflict)
        })
    }
    pub(crate) fn abort_reserved_conference_removal(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> bool {
        if !self.conference_mutation_is_active(mutation) {
            return false;
        }
        let aborted = self.abort_conference_participant_removal(conference_id, participant_id);
        self.complete_conference_mutation(mutation);
        aborted
    }
    pub(crate) fn commit_reserved_conference_removal(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> Option<Vec<DriverEffect>> {
        if !self.conference_mutation_is_active(mutation) {
            return None;
        }
        let cleanup = self.conference_participant_removed(conference_id, participant_id);
        self.complete_conference_mutation(mutation);
        cleanup
    }
    pub(crate) fn abort_reserved_conference_mute(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> () {
        if self.conference_mutation_is_active(mutation) {
            self.abort_conference_participant_mute(conference_id, participant_id, muted);
            self.complete_conference_mutation(mutation);
        }
    }
    pub(crate) fn commit_reserved_conference_mute(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> bool {
        if !self.conference_mutation_is_active(mutation) {
            return false;
        }
        let committed = self.conference_participant_muted(conference_id, participant_id, muted);
        self.complete_conference_mutation(mutation);
        committed
    }
    pub(crate) fn abort_reserved_conference_role_change(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> () {
        if self.conference_mutation_is_active(mutation) {
            self.abort_conference_participant_role_change(conference_id, participant_id, moderator);
            self.complete_conference_mutation(mutation);
        }
    }
    pub(crate) fn commit_reserved_conference_role_change(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> bool {
        if !self.conference_mutation_is_active(mutation) {
            return false;
        }
        let committed =
            self.conference_participant_role_changed(conference_id, participant_id, moderator);
        self.complete_conference_mutation(mutation);
        committed
    }
    pub(crate) fn prepare_conference_invite(
        &mut self,
        moderator_call_id: CallId,
        invite_call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection> {
        self.begin_conference_invite(moderator_call_id, invite_call_id, binding, codec, now)
            .and_then(|effects| {
                self.claim_conference_mutation(invite_call_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceRejection::Conflict)
            })
    }
    pub(crate) fn prepare_confirm_conference_invite(
        &mut self,
        call_id: CallId,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection> {
        self.confirm_conference_invite(call_id).and_then(|effects| {
            self.claim_conference_mutation(call_id)
                .map(|mutation| (mutation, effects))
                .ok_or(ConferenceRejection::Conflict)
        })
    }
    pub(crate) fn prepare_confirm_conference(
        &mut self,
        call_id: CallId,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection> {
        self.confirm_conference(call_id).and_then(|effects| {
            self.claim_conference_mutation(call_id)
                .map(|mutation| (mutation, effects))
                .ok_or(ConferenceRejection::Conflict)
        })
    }
    pub(crate) fn prepare_conference(
        &mut self,
        request: ConferenceConsultationRequest,
        media_policy: ConferenceMediaPolicy,
    ) -> Result<(ConferenceMutationToken, Vec<DriverEffect>), ConferenceRejection> {
        let consultation_call_id = request.consultation_call_id;
        self.begin_conference_with_media(request, media_policy)
            .and_then(|effects| {
                self.claim_conference_mutation(consultation_call_id)
                    .map(|mutation| (mutation, effects))
                    .ok_or(ConferenceRejection::Conflict)
            })
    }
    pub(crate) fn abort_reserved_conference(
        &mut self,
        mutation: ConferenceMutationToken,
        call_id: CallId,
        bridge_created: bool,
        channel_created: bool,
        original_needs_resume: bool,
        restore_original_media: bool,
    ) -> Vec<DriverEffect> {
        if !self.conference_mutation_is_active(mutation) {
            return Vec::new();
        }
        let cleanup = self.abort_conference(
            call_id,
            bridge_created,
            channel_created,
            original_needs_resume,
            restore_original_media,
        );
        self.complete_conference_mutation(mutation);
        cleanup
    }
    pub(crate) fn abort_reserved_conference_invite(
        &mut self,
        mutation: ConferenceMutationToken,
        invite_call_id: CallId,
        invite_channel_created: bool,
        moderator_needs_resume: bool,
        restore_moderator_media: bool,
    ) -> Vec<DriverEffect> {
        if !self.conference_mutation_is_active(mutation) {
            return Vec::new();
        }
        let cleanup = self.abort_conference_invite(
            invite_call_id,
            invite_channel_created,
            moderator_needs_resume,
            restore_moderator_media,
        );
        self.complete_conference_mutation(mutation);
        cleanup
    }
    pub(crate) fn commit_reserved_conference(
        &mut self,
        mutation: ConferenceMutationToken,
        call_id: CallId,
        conference_id: ConferenceId,
    ) -> (bool, Option<Vec<DriverEffect>>) {
        if !self.conference_mutation_is_active(mutation) {
            return (false, None);
        }
        let committed = self.conference_merged(call_id);
        let announcement = committed.then(|| {
            self.conference_announcement_effects(conference_id, ConferenceAnnouncement::Connected)
        });
        self.complete_conference_mutation(mutation);
        (committed, announcement)
    }
    pub(crate) fn abort_reserved_join(
        &mut self,
        mutation: ConferenceMutationToken,
        call_id: CallId,
        bridge_created: bool,
        resumed_call_ids: Vec<PbxCallId>,
    ) -> Vec<DriverEffect> {
        if !self.conference_mutation_is_active(mutation) {
            return Vec::new();
        }
        let cleanup = self.abort_join_conference(call_id, bridge_created, &resumed_call_ids);
        self.complete_conference_mutation(mutation);
        cleanup
    }
    pub(crate) fn commit_reserved_conference_invite(
        &mut self,
        mutation: ConferenceMutationToken,
        invite_call_id: CallId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> (bool, Option<Vec<DriverEffect>>) {
        if !self.conference_mutation_is_active(mutation) {
            return (false, None);
        }
        let committed = self.conference_invite_merged(invite_call_id);
        let announcement = committed.then(|| {
            self.conference_announcement_effects(
                conference_id,
                ConferenceAnnouncement::ParticipantJoined(participant_id),
            )
        });
        self.complete_conference_mutation(mutation);
        (committed, announcement)
    }
    pub(crate) fn prepare_phone_hangup(
        &mut self,
        call_id: CallId,
        physical_on_hook: bool,
    ) -> (
        Option<ConferenceId>,
        Vec<DriverEffect>,
        Option<ConferenceSession>,
    ) {
        let conference_id = self.conference_session(call_id).map(|session| session.id);
        let effects = if physical_on_hook {
            self.hangup(call_id)
        } else {
            self.terminate(call_id)
        };
        let surviving = conference_id
            .and_then(|conference_id| self.conference_session_by_id(conference_id))
            .cloned();
        (conference_id, effects, surviving)
    }
    pub(crate) fn prepare_hold(&mut self, call_id: CallId, held: bool) -> HoldPlan {
        let Some(device_id) = self.call_device_id(call_id).cloned() else {
            return HoldPlan::Missing;
        };
        if self.conference_session(call_id).is_none() {
            return HoldPlan::Regular(if held {
                self.hold(call_id)
            } else {
                self.resume(call_id)
            });
        }
        let result = (|| {
            let effects = self.begin_conference_moderator_leg_transition(call_id, held)?;
            let session = self
                .conference_session(call_id)
                .ok_or(ConferenceParticipantRejection::Unavailable)?;
            let participant = session
                .participants
                .iter()
                .find(|participant| participant.handset_call_id == call_id)
                .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
            let conference_id = session.id;
            let participant_id = participant.id;
            let mutation = self
                .claim_conference_mutation_by_id(conference_id)
                .ok_or(ConferenceParticipantRejection::Conflict)?;
            Ok((conference_id, participant_id, mutation, effects))
        })();
        HoldPlan::Conference { device_id, result }
    }
    pub(crate) fn abort_reserved_hold(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        held: bool,
        completed_music: Vec<ParticipantId>,
        handset_attempted: bool,
    ) -> Vec<DriverEffect> {
        if !self.conference_mutation_is_active(mutation) {
            return Vec::new();
        }
        let rollback = self.abort_conference_moderator_leg_transition(
            conference_id,
            participant_id,
            held,
            &completed_music,
            handset_attempted,
        );
        self.complete_conference_mutation(mutation);
        rollback
    }
    pub(crate) fn commit_reserved_hold(
        &mut self,
        mutation: ConferenceMutationToken,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        held: bool,
        completed_music: Vec<ParticipantId>,
        handset_attempted: bool,
    ) -> (bool, Vec<DriverEffect>) {
        if !self.conference_mutation_is_active(mutation) {
            return (false, Vec::new());
        }
        let committed =
            self.conference_moderator_leg_transitioned(conference_id, participant_id, held);
        let rollback = if committed {
            Vec::new()
        } else {
            self.abort_conference_moderator_leg_transition(
                conference_id,
                participant_id,
                held,
                &completed_music,
                handset_attempted,
            )
        };
        self.complete_conference_mutation(mutation);
        (committed, rollback)
    }
    pub(crate) fn prepare_transfer(
        &mut self,
        request: TransferConsultationRequest,
    ) -> (
        Result<Vec<DriverEffect>, TransferRejection>,
        Option<TransferTransaction>,
    ) {
        let consultation_call_id = request.consultation_call_id;
        let effects = self.begin_transfer(request);
        let transaction = self.transfer_transaction(consultation_call_id).cloned();
        (effects, transaction)
    }
    pub(crate) fn prepare_direct_transfer(
        &mut self,
        device_id: DeviceId,
    ) -> (
        Result<TransferCompletionPlan, TransferRejection>,
        Option<CallId>,
    ) {
        (
            self.direct_transfer(&device_id),
            self.registered_device(&device_id)
                .and_then(|device| device.active_call()),
        )
    }
    pub(crate) fn prepare_register_session(
        &mut self,
        session_generation: SessionGeneration,
        registration: DeviceRegistration,
    ) -> Option<(
        RegisterSessionOutcome,
        Vec<ConferenceId>,
        Vec<ConferenceSession>,
    )> {
        let device = registration.id.clone();
        let mut affected = self
            .calls()
            .filter(|call| call.device_id == device)
            .filter_map(|call| {
                self.conference_session(call.sccp_id)
                    .map(|conference| conference.id)
            })
            .collect::<Vec<_>>();
        affected.sort_unstable();
        affected.dedup();
        let session = self.register_session(session_generation, registration)?;
        self.clear_mobility_prompts(&device);
        if !session.replaced {
            affected.clear();
        } else {
            self.cancel_forwarding_for_device(&device);
        }
        let surviving = affected
            .iter()
            .filter_map(|conference_id| self.conference_session_by_id(*conference_id).cloned())
            .collect::<Vec<_>>();
        Some((session, affected, surviving))
    }
    pub(crate) fn prepare_disconnect(
        &mut self,
        device_id: DeviceId,
        generation: SessionGeneration,
    ) -> Option<(Vec<DriverEffect>, Vec<ConferenceSession>, Vec<ConferenceId>)> {
        if !self.session_is_current(&device_id, generation) {
            return None;
        }
        let mut affected = self
            .calls()
            .filter(|call| call.device_id == device_id)
            .filter_map(|call| {
                self.conference_session(call.sccp_id)
                    .map(|session| session.id)
            })
            .collect::<Vec<_>>();
        affected.sort_unstable();
        affected.dedup();
        let actions = self.disconnected(&device_id);
        self.clear_mobility_prompts(&device_id);
        self.cancel_forwarding_for_device(&device_id);
        let surviving = affected
            .iter()
            .filter_map(|conference_id| self.conference_session_by_id(*conference_id).cloned())
            .collect::<Vec<_>>();
        Some((actions, surviving, affected))
    }
    pub(crate) fn accept_media_transmission(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> (Vec<DriverEffect>, bool) {
        let actions = self.media_transmission_started_for_device(device_id, call_id, endpoint);
        let accepted = self.call(call_id).is_some_and(|call| {
            call.device_id == *device_id && call.audio_transmit == MediaStreamState::Open(endpoint)
        });
        (actions, accepted)
    }
    pub(crate) fn accept_media_receive(
        &mut self,
        device_id: DeviceId,
        call_id: CallId,
        endpoint: MediaEndpoint,
    ) -> (Vec<DriverEffect>, bool) {
        let actions = self.media_opened_for_device(&device_id, call_id, endpoint);
        let accepted = self.call(call_id).is_some_and(|call| {
            call.device_id == device_id && call.audio == MediaStreamState::Open(endpoint)
        });
        (actions, accepted)
    }
    pub(crate) fn apply_pickup_identity(
        &mut self,
        call_id: CallId,
        parties: crate::runtime::backend::PickupOutcome,
    ) -> CallInfo {
        let mut info = self.call_info(call_id).cloned().unwrap_or(CallInfo {
            direction: CallDirection::Inbound,
            ..CallInfo::default()
        });
        info.direction = CallDirection::Inbound;
        info.calling_name = parties.calling_name;
        info.calling_number = parties.calling_number;
        info.called_name = parties.connected_name;
        info.called_number = parties.connected_number;
        info.last_redirecting_name = parties.redirecting_name;
        info.last_redirecting_number = parties.redirecting_number;
        let _ = self.set_call_info(call_id, info.clone());
        info
    }
    pub(crate) fn prepare_phone_call(
        &mut self,
        call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> (Option<PbxCallId>, Vec<DriverEffect>) {
        let effects = self.begin_phone_call(call_id, binding.clone(), codec, now);
        let pbx_id = self.call_pbx_id(call_id);
        (pbx_id, effects)
    }
}

impl Controller {
    pub(crate) fn seed_inbound_identity(
        &mut self,
        pbx_id: PbxCallId,
        snapshot: crate::pbx::party::PartySnapshot,
    ) -> Vec<DriverEffect> {
        self.update_call_info_by_pbx(pbx_id, |current| {
            snapshot.apply_initial_inbound_to_call_info(current)
        })
    }
}

impl Controller {
    pub(crate) fn commit_device_features(
        &mut self,
        device_id: &DeviceId,
        expected: Option<DeviceFeatureState>,
        next: DeviceFeatureState,
    ) -> bool {
        if self.feature_state(device_id) != expected.as_ref() {
            return false;
        }
        self.set_feature_state(device_id, next);
        true
    }
}

impl Controller {
    pub(crate) fn prepare_device_features(
        &mut self,
        device_id: &DeviceId,
        defaults: DeviceFeatureState,
        mutation: crate::state::features::FeatureMutation,
    ) -> crate::state::features::DeviceFeaturePlan {
        let expected = self.feature_state(device_id).cloned();
        let previous = expected.clone().unwrap_or(defaults);
        let mut next = previous.clone();
        mutation.apply(&mut next);
        crate::state::features::DeviceFeaturePlan {
            expected,
            previous,
            next,
        }
    }
    pub(crate) fn commit_registered_features(
        &mut self,
        device_id: &DeviceId,
        generation: SessionGeneration,
        features: DeviceFeatureState,
    ) -> bool {
        if !self.session_is_current(device_id, generation) {
            return false;
        }
        self.set_feature_state(device_id, features);
        true
    }
}
