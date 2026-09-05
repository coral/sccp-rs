use super::super::*;

impl Controller {
    /// Commit a configured destination-based conference request for an
    /// existing pre-dial handset call. Any ordinary connected call on the same
    /// handset is held first; an active ad-hoc conference is never modified.
    pub fn begin_conference_destination(
        &mut self,
        request: ConferenceDestinationRequest,
    ) -> Result<Vec<DriverEffect>, ConferenceDestinationRejection> {
        if request.destination.trim().is_empty() {
            return Err(ConferenceDestinationRejection::Unavailable);
        }
        let target = self
            .appearance_for_call(request.handset_call_id)
            .cloned()
            .ok_or(ConferenceDestinationRejection::Unavailable)?;
        if target.device_id != request.device_id {
            return Err(ConferenceDestinationRejection::Unavailable);
        }
        let target_call = self
            .call_registry
            .pbx
            .get(&target.pbx_id)
            .ok_or(ConferenceDestinationRejection::Unavailable)?;
        if target.state != CallState::Collecting
            || target_call.state != CallState::Collecting
            || !target_call.digits.is_empty()
            || target_call.active_appearance != Some(target.id)
        {
            return Err(ConferenceDestinationRejection::Conflict);
        }
        if self.conferences.by_consultation.values().any(|session| {
            session
                .participants
                .iter()
                .any(|participant| participant.device_id == target.device_id)
        }) {
            return Err(ConferenceDestinationRejection::Conflict);
        }

        let mut ordinary_connected = self
            .call_registry
            .by_device
            .get(&target.device_id)
            .into_iter()
            .flatten()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .filter(|appearance| appearance.id != target.id)
            .filter(|appearance| appearance.state == CallState::Connected)
            .filter(|appearance| !self.conferences.by_pbx.contains_key(&appearance.pbx_id))
            .filter(|appearance| {
                self.call_registry
                    .pbx
                    .get(&appearance.pbx_id)
                    .is_some_and(|call| call.active_appearance == Some(appearance.id))
            })
            .map(|appearance| (appearance.pbx_id, appearance.sccp_id))
            .collect::<Vec<_>>();
        ordinary_connected.sort_by_key(|(pbx_id, call_id)| (pbx_id.0, call_id.0));
        ordinary_connected.dedup_by_key(|(pbx_id, _)| *pbx_id);
        if ordinary_connected
            .iter()
            .any(|(pbx_id, _)| self.redirect_claims.contains(pbx_id))
        {
            return Err(ConferenceDestinationRejection::Conflict);
        }

        let mutation = self
            .allocate_conference_mutation(ConferenceMutationOwner::Destination(target.pbx_id))
            .ok_or(ConferenceDestinationRejection::Conflict)?;

        let held_calls = ordinary_connected
            .iter()
            .map(|(pbx_id, _)| *pbx_id)
            .collect::<Vec<_>>();
        let mut effects = Vec::new();
        for (_, call_id) in ordinary_connected {
            effects.extend(self.hold(call_id));
        }

        let info = {
            let appearance = self
                .call_registry
                .appearances
                .get_mut(&target.id)
                .ok_or(ConferenceDestinationRejection::Unavailable)?;
            appearance.state = CallState::Calling;
            appearance.info.called_name = "Conference".into();
            appearance
                .info
                .called_number
                .clone_from(&request.destination);
            appearance.info.clone()
        };
        let call = self
            .call_registry
            .pbx
            .get_mut(&target.pbx_id)
            .ok_or(ConferenceDestinationRejection::Unavailable)?;
        call.state = CallState::Calling;
        call.digit_deadline = None;
        call.last_digit_at = None;
        debug_assert!(self.invariant_error().is_none());

        effects.extend([
            HandsetEffect::SetCallInfo {
                device_id: target.device_id.clone(),
                call_id: request.handset_call_id,
                info,
            }
            .into(),
            HandsetEffect::StartTone {
                device_id: target.device_id.clone(),
                call_id: request.handset_call_id,
                tone: Tone::Silence,
            }
            .into(),
            HandsetEffect::SetCallState {
                device_id: target.device_id,
                call_id: request.handset_call_id,
                state: HandsetCallState::Proceed,
                stop_media: false,
            }
            .into(),
            PbxEffect::StartConferenceDestination {
                operation: ConferenceDestinationOperation {
                    call_id: target.pbx_id,
                    destination: request.destination,
                    application_options: request.application_options,
                    handset_call_id: request.handset_call_id,
                    held_calls,
                    mutation,
                },
            }
            .into(),
        ]);
        Ok(effects)
    }

    /// Roll back a destination-conference launch after effect execution
    /// failed. Calls whose PBX hold completed are resumed externally; calls
    /// whose hold was never executed are restored only in controller state.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn conference_destination_failed(
        &mut self,
        mutation: ConferenceMutationToken,
        handset_call_id: CallId,
        held_calls: &[PbxCallId],
        completed_holds: &[PbxCallId],
    ) -> Vec<DriverEffect> {
        if !self.complete_conference_mutation(mutation) {
            return Vec::new();
        }
        let Some(target) = self.appearance_for_call(handset_call_id).cloned() else {
            return Vec::new();
        };
        if target.state != CallState::Calling {
            return Vec::new();
        }
        let mut effects = self.hangup(handset_call_id);
        let completed = completed_holds.iter().copied().collect::<HashSet<_>>();
        for pbx_id in held_calls {
            let handset_call_id = self.call_registry.pbx.get(pbx_id).and_then(|call| {
                call.active_appearance
                    .and_then(|id| self.call_registry.appearances.get(&id))
                    .map(|appearance| appearance.sccp_id)
            });
            let Some(handset_call_id) = handset_call_id else {
                continue;
            };
            let resume = self.resume(handset_call_id);
            if completed.contains(pbx_id) {
                effects.extend(resume);
            }
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Create a conference from locally eligible calls. Two or more selected
    /// calls are an exact set; otherwise every eligible call on the initiating
    /// handset is used. The initiating call is always the moderator.
    pub fn join_calls_with_media(
        &mut self,
        device_id: &DeviceId,
        initiating_call_id: CallId,
        permitted: bool,
        media_policy: ConferenceMediaPolicy,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let mut effects = self.join_calls(device_id, initiating_call_id, permitted)?;
        if !self.configure_conference_media(initiating_call_id, media_policy) {
            self.abort_join_conference(initiating_call_id, false, &[]);
            return Err(ConferenceRejection::Conflict);
        }
        if let Some(session) = self.conference_session(initiating_call_id) {
            effects.extend(Self::conference_mute_on_entry_effects(
                session,
                session.participants.iter(),
            ));
        }
        Ok(effects)
    }

    pub fn join_calls(
        &mut self,
        device_id: &DeviceId,
        initiating_call_id: CallId,
        permitted: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        if !permitted {
            return Err(ConferenceRejection::Disabled);
        }
        let Some(device) = self.devices.get(device_id) else {
            return Err(ConferenceRejection::Unavailable);
        };
        let selected = device.selected_calls.clone();
        let mut eligible: Vec<_> = self
            .call_registry
            .appearances
            .values()
            .filter(|appearance| {
                &appearance.device_id == device_id
                    && matches!(appearance.state, CallState::Connected | CallState::Held)
                    && self
                        .call_registry
                        .pbx
                        .get(&appearance.pbx_id)
                        .is_some_and(|call| {
                            matches!(call.state, CallState::Connected | CallState::Held)
                                && call.active_appearance == Some(appearance.id)
                        })
                    && !self.conferences.by_pbx.contains_key(&appearance.pbx_id)
            })
            .cloned()
            .collect();
        eligible.sort_by_key(|appearance| appearance.sccp_id.0);
        if !eligible
            .iter()
            .any(|appearance| appearance.sccp_id == initiating_call_id)
        {
            return Err(ConferenceRejection::NotConnected);
        }

        let selected_eligible: Vec<_> = eligible
            .iter()
            .filter(|appearance| selected.contains(&appearance.sccp_id))
            .cloned()
            .collect();
        let mut chosen = if selected_eligible.len() >= 2 {
            selected_eligible
        } else {
            eligible
        };
        if chosen.len() < 2 {
            return Err(ConferenceRejection::NotConnected);
        }
        if chosen.len() > MAX_CONFERENCE_PARTICIPANTS {
            return Err(ConferenceRejection::Conflict);
        }
        let Some(moderator_index) = chosen
            .iter()
            .position(|appearance| appearance.sccp_id == initiating_call_id)
        else {
            return Err(ConferenceRejection::Conflict);
        };
        let moderator = chosen.remove(moderator_index);
        chosen.insert(0, moderator);

        let participants = ConferenceParticipantRegistry::new(
            chosen
                .iter()
                .enumerate()
                .map(|(index, appearance)| self.conference_participant(appearance, index == 0))
                .collect::<Vec<_>>(),
        )
        .expect("eligible conference calls have unique identities");
        let original = &chosen[0];
        let consultation = &chosen[1];
        let session = ConferenceSession {
            id: self.allocate_conference_id(),
            bridge_id: self.allocate_bridge_id(),
            device_id: device_id.clone(),
            original_handset_call_id: original.sccp_id,
            original_call_id: original.pbx_id,
            consultation_handset_call_id: consultation.sccp_id,
            consultation_call_id: consultation.pbx_id,
            phase: ConferencePhase::Merging,
            origin: ConferenceOrigin::Selection,
            participants,
            media_policy: ConferenceMediaPolicy::default(),
            pending_invite: None,
            pending_participant_mutation: None,
        };
        let key = session.consultation_handset_call_id;
        for participant in session.participants.iter() {
            self.conferences.by_pbx.insert(participant.pbx_call_id, key);
        }
        let resumed: Vec<_> = chosen
            .iter()
            .filter(|appearance| appearance.state == CallState::Held)
            .map(|appearance| appearance.pbx_id)
            .collect();
        let call_ids = session
            .participants
            .iter()
            .map(|participant| participant.pbx_call_id)
            .collect();
        let bridge_id = session.bridge_id;
        self.conferences.by_consultation.insert(key, session);

        let mut effects = vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Create { bridge_id },
            }
            .into(),
        ];
        effects.extend(
            resumed
                .into_iter()
                .map(|call_id| PbxEffect::Resume { call_id }.into()),
        );
        effects.push(
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeCalls {
                    bridge_id,
                    call_ids,
                },
            }
            .into(),
        );
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    /// Start an outbound consultation from the active moderator leg while the
    /// existing conference bridge remains live for its other participants.
    pub fn begin_conference_invite(
        &mut self,
        moderator_call_id: CallId,
        invite_call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        if self.call_registry.by_sccp.contains_key(&invite_call_id) {
            return Err(ConferenceRejection::Conflict);
        }
        let session = self
            .conference_session(moderator_call_id)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active
            || session.pending_invite.is_some()
            || session.pending_participant_mutation.is_some()
            || session.participants.iter().len() >= MAX_CONFERENCE_PARTICIPANTS
        {
            return Err(ConferenceRejection::Conflict);
        }
        let moderator = session
            .participants
            .iter()
            .find(|participant| {
                participant.moderator && participant.handset_call_id == moderator_call_id
            })
            .ok_or(ConferenceRejection::Disabled)?;
        let moderator_appearance = self
            .appearance_for_call(moderator_call_id)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        if self
            .call_registry
            .pbx
            .get(&moderator.pbx_call_id)
            .is_none_or(|call| call.state != CallState::Connected)
            || moderator_appearance.state != CallState::Connected
            || binding.device_id != session.device_id
            || binding.line_instance != moderator_appearance.line_instance
        {
            return Err(ConferenceRejection::NotConnected);
        }

        let music_started = session.participants.active_moderator_count() == 1;
        let moderator_id = moderator.id;
        let moderator_pbx_call_id = moderator.pbx_call_id;
        let mut effects = self.hold(moderator_call_id);
        if effects.is_empty() {
            return Err(ConferenceRejection::Conflict);
        }
        if music_started {
            effects.extend(Self::conference_music_effects(&session, true));
        }
        let invite_device_id = binding.device_id.clone();
        let invite_line_instance = binding.line_instance;
        let mut invite_effects = self.begin_phone_call(invite_call_id, binding, codec, now);
        invite_effects.insert(
            0,
            HandsetEffect::BeginCall {
                device_id: invite_device_id,
                line_instance: invite_line_instance,
                call_id: invite_call_id,
                codec,
            }
            .into(),
        );
        effects.extend(invite_effects);
        let Some(invite) = self.appearance_for_call(invite_call_id).cloned() else {
            let _ = self.resume(moderator_call_id);
            return Err(ConferenceRejection::Conflict);
        };
        let participant = self.conference_participant(&invite, false);
        let key = session.consultation_handset_call_id;
        let Some(stored) = self.conferences.by_consultation.get_mut(&key) else {
            let _ = self.resume(moderator_call_id);
            self.remove_pbx_call(invite.pbx_id);
            return Err(ConferenceRejection::Conflict);
        };
        stored.pending_invite = Some(ConferenceInvite {
            moderator_id,
            moderator_call_id: moderator_pbx_call_id,
            music_started,
            participant: participant.clone(),
        });
        self.conferences.by_pbx.insert(participant.pbx_call_id, key);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn confirm_conference_invite(
        &self,
        invite_call_id: CallId,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let session = self
            .conference_session(invite_call_id)
            .ok_or(ConferenceRejection::Unavailable)?;
        let invite = session
            .pending_invite
            .as_ref()
            .filter(|invite| invite.participant.handset_call_id == invite_call_id)
            .ok_or(ConferenceRejection::Conflict)?;
        let moderator = session
            .participants
            .get(invite.moderator_id)
            .filter(|moderator| {
                moderator.moderator && moderator.pbx_call_id == invite.moderator_call_id
            })
            .ok_or(ConferenceRejection::Unavailable)?;
        if self
            .call_registry
            .pbx
            .get(&invite.participant.pbx_call_id)
            .is_none_or(|call| call.state != CallState::Connected)
            || self
                .call_registry
                .pbx
                .get(&moderator.pbx_call_id)
                .is_none_or(|call| call.state != CallState::Held)
        {
            return Err(ConferenceRejection::NotConnected);
        }
        let mut effects = if invite.music_started {
            Self::conference_music_effects(session, false)
        } else {
            Vec::new()
        };
        effects.extend([
            PbxEffect::Resume {
                call_id: moderator.pbx_call_id,
            }
            .into(),
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                    bridge_id: session.bridge_id,
                    call_id: invite.participant.pbx_call_id,
                },
            }
            .into(),
        ]);
        effects.extend(Self::conference_mute_on_entry_effects(
            session,
            std::iter::once(&invite.participant),
        ));
        Ok(effects)
    }

    pub fn conference_invite_merged(&mut self, invite_call_id: CallId) -> bool {
        let Some(key) = self
            .conference_session(invite_call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&key) else {
            return false;
        };
        let Some(invite) = session.pending_invite.take() else {
            return false;
        };
        let participant_id = invite.participant.id;
        if invite.participant.handset_call_id != invite_call_id
            || session
                .participants
                .insert(invite.participant.clone())
                .is_err()
        {
            session.pending_invite = Some(invite);
            return false;
        }
        if session.media_policy.mute_on_entry
            && !session.participants.set_muted(participant_id, true)
        {
            return false;
        }
        let participant_calls: Vec<_> = session
            .participants
            .iter()
            .map(|participant| (participant.pbx_call_id, participant.handset_call_id))
            .collect();
        for (pbx_id, handset_call_id) in participant_calls {
            if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
                call.state = CallState::Connected;
            }
            if let Some(appearance_id) = self.call_registry.by_sccp.get(&handset_call_id).copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.state = CallState::Connected;
            }
        }
        debug_assert!(self.invariant_error().is_none());
        true
    }

    pub fn abort_conference_invite(
        &mut self,
        invite_call_id: CallId,
        invite_channel_created: bool,
        moderator_needs_resume: bool,
        restore_moderator_media: bool,
    ) -> Vec<DriverEffect> {
        let Some(key) = self
            .conference_session(invite_call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return Vec::new();
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&key) else {
            return Vec::new();
        };
        let Some(invite) = session.pending_invite.take() else {
            return Vec::new();
        };
        if invite.participant.handset_call_id != invite_call_id {
            session.pending_invite = Some(invite);
            return Vec::new();
        }
        let moderator = session
            .participants
            .get(invite.moderator_id)
            .filter(|moderator| {
                moderator.moderator && moderator.pbx_call_id == invite.moderator_call_id
            })
            .cloned();
        let music_effects = if invite.music_started {
            Self::conference_music_effects(session, false)
        } else {
            Vec::new()
        };
        self.conferences
            .by_pbx
            .remove(&invite.participant.pbx_call_id);
        self.remove_pbx_call(invite.participant.pbx_call_id);
        if let Some(moderator) = moderator.as_ref() {
            if let Some(call) = self.call_registry.pbx.get_mut(&moderator.pbx_call_id) {
                call.state = CallState::Connected;
            }
            if let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&moderator.handset_call_id)
                .copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.state = CallState::Connected;
                if restore_moderator_media {
                    appearance.audio = MediaStreamState::Opening;
                }
            }
        }

        let mut effects = music_effects;
        if invite_channel_created {
            effects.push(
                PbxEffect::Hangup {
                    call_id: invite.participant.pbx_call_id,
                }
                .into(),
            );
        }
        if moderator_needs_resume && let Some(moderator) = moderator.as_ref() {
            effects.push(
                PbxEffect::Resume {
                    call_id: moderator.pbx_call_id,
                }
                .into(),
            );
        }
        effects.push(
            HandsetEffect::SetCallState {
                device_id: invite.participant.device_id,
                call_id: invite.participant.handset_call_id,
                state: HandsetCallState::OnHook,
                stop_media: true,
            }
            .into(),
        );
        if restore_moderator_media && let Some(moderator) = moderator {
            let codec = self
                .appearance_for_call(moderator.handset_call_id)
                .map_or(Codec::Pcmu, |appearance| appearance.codec);
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: moderator.device_id,
                    call_id: moderator.handset_call_id,
                    codec,
                }
                .into(),
            );
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Hold an active call and create a second outbound call for an attended
    /// conference consultation. The handset call identifier is reserved by
    /// the session adapter before this transition.
    pub fn begin_conference_with_media(
        &mut self,
        request: ConferenceConsultationRequest,
        media_policy: ConferenceMediaPolicy,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let effects = self.begin_conference(
            request.original_call_id,
            request.consultation_call_id,
            request.binding,
            request.codec,
            request.now,
            request.permitted,
        )?;
        if !self.configure_conference_media(request.consultation_call_id, media_policy) {
            self.abort_conference(request.consultation_call_id, false, false, false, false);
            return Err(ConferenceRejection::Conflict);
        }
        Ok(effects)
    }

    pub fn begin_conference(
        &mut self,
        original_call_id: CallId,
        consultation_call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
        permitted: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        if !permitted {
            return Err(ConferenceRejection::Disabled);
        }
        if self
            .call_registry
            .by_sccp
            .contains_key(&consultation_call_id)
        {
            return Err(ConferenceRejection::Conflict);
        }
        let original = self
            .appearance_for_call(original_call_id)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        let original_pbx = self
            .call_registry
            .pbx
            .get(&original.pbx_id)
            .ok_or(ConferenceRejection::Unavailable)?;
        if original_pbx.state != CallState::Connected
            || original.state != CallState::Connected
            || original_pbx.active_appearance != Some(original.id)
        {
            return Err(ConferenceRejection::NotConnected);
        }
        if binding.device_id != original.device_id
            || binding.line_instance != original.line_instance
            || binding.line.number != original_pbx.line
            || !self.devices.contains_key(&original.device_id)
        {
            return Err(ConferenceRejection::Unavailable);
        }
        if self.redirect_claims.contains(&original.pbx_id)
            || self.conferences.by_pbx.contains_key(&original.pbx_id)
        {
            return Err(ConferenceRejection::Conflict);
        }

        let mut effects = self.hold(original_call_id);
        if effects.is_empty() {
            return Err(ConferenceRejection::Conflict);
        }
        let consultation_device_id = binding.device_id.clone();
        let consultation_line_instance = binding.line_instance;
        let mut consultation_effects =
            self.begin_phone_call(consultation_call_id, binding, codec, now);
        consultation_effects.insert(
            0,
            HandsetEffect::BeginCall {
                device_id: consultation_device_id,
                line_instance: consultation_line_instance,
                call_id: consultation_call_id,
                codec,
            }
            .into(),
        );
        let Some(consultation) = self.appearance_for_call(consultation_call_id).cloned() else {
            let _ = self.resume(original_call_id);
            return Err(ConferenceRejection::Conflict);
        };
        let participants = ConferenceParticipantRegistry::new([
            self.conference_participant(&original, true),
            self.conference_participant(&consultation, false),
        ])
        .expect("fresh conference participant identities are unique");
        effects.extend(consultation_effects);
        let session = ConferenceSession {
            id: self.allocate_conference_id(),
            bridge_id: self.allocate_bridge_id(),
            device_id: original.device_id,
            original_handset_call_id: original_call_id,
            original_call_id: original.pbx_id,
            consultation_handset_call_id: consultation_call_id,
            consultation_call_id: consultation.pbx_id,
            phase: ConferencePhase::Consultation,
            origin: ConferenceOrigin::Consultation,
            participants,
            media_policy: ConferenceMediaPolicy::default(),
            pending_invite: None,
            pending_participant_mutation: None,
        };
        self.conferences
            .by_pbx
            .insert(session.original_call_id, consultation_call_id);
        self.conferences
            .by_pbx
            .insert(session.consultation_call_id, consultation_call_id);
        self.conferences
            .by_consultation
            .insert(consultation_call_id, session);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn conference_session(&self, call_id: CallId) -> Option<&ConferenceSession> {
        let pbx_id = self.appearance_for_call(call_id)?.pbx_id;
        self.conference_session_by_pbx(pbx_id)
    }

    pub fn conference_session_by_pbx(&self, pbx_id: PbxCallId) -> Option<&ConferenceSession> {
        let consultation = self.conferences.by_pbx.get(&pbx_id)?;
        self.conferences.by_consultation.get(consultation)
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

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn claim_conference_mutation(
        &mut self,
        call_id: CallId,
    ) -> Option<ConferenceMutationToken> {
        let owner = ConferenceMutationOwner::Session(self.conference_session(call_id)?.id);
        self.allocate_conference_mutation(owner)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn claim_conference_mutation_by_id(
        &mut self,
        conference_id: ConferenceId,
    ) -> Option<ConferenceMutationToken> {
        self.conference_session_by_id(conference_id)?;
        self.allocate_conference_mutation(ConferenceMutationOwner::Session(conference_id))
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

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn complete_conference_mutation(&mut self, token: ConferenceMutationToken) -> bool {
        if self.conference_mutations.get(&token.owner) != Some(&token.generation) {
            return false;
        }
        self.conference_mutations.remove(&token.owner);
        true
    }

    #[cfg(test)]
    pub(in crate::runtime::controller) fn conference_destination_mutation(
        &self,
        handset_call_id: CallId,
    ) -> Option<ConferenceMutationToken> {
        let owner =
            ConferenceMutationOwner::Destination(self.appearance_for_call(handset_call_id)?.pbx_id);
        self.conference_mutations
            .get(&owner)
            .copied()
            .map(|generation| ConferenceMutationToken { owner, generation })
    }

    pub(in crate::runtime::controller) fn allocate_conference_mutation(
        &mut self,
        owner: ConferenceMutationOwner,
    ) -> Option<ConferenceMutationToken> {
        if self.conference_mutations.contains_key(&owner) {
            return None;
        }
        let generation = self.next_conference_mutation_generation;
        self.next_conference_mutation_generation = generation.checked_add(1)?;
        self.conference_mutations.insert(owner, generation);
        Some(ConferenceMutationToken { owner, generation })
    }

    /// Bind normalized media policy before a conference becomes active. An
    /// active session keeps its captured policy across configuration reloads.
    pub fn configure_conference_media(
        &mut self,
        call_id: CallId,
        policy: ConferenceMediaPolicy,
    ) -> bool {
        let Some(key) = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&key) else {
            return false;
        };
        if session.phase == ConferencePhase::Active {
            return false;
        }
        session.media_policy = policy;
        true
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
        Self::conference_announcement_effects_for_session(session, announcement)
    }

    pub(in crate::runtime::controller) fn conference_announcement_effects_for_session(
        session: &ConferenceSession,
        announcement: ConferenceAnnouncement,
    ) -> Vec<DriverEffect> {
        if session.phase != ConferencePhase::Active {
            return Vec::new();
        }
        let enabled = match announcement {
            ConferenceAnnouncement::Connected
            | ConferenceAnnouncement::ParticipantJoined(_)
            | ConferenceAnnouncement::ParticipantRemoved(_)
            | ConferenceAnnouncement::ModeratorDeparted(_) => {
                session.media_policy.play_general_announcements
            }
            ConferenceAnnouncement::ParticipantMuted(_)
            | ConferenceAnnouncement::ParticipantUnmuted(_) => {
                session.media_policy.play_participant_announcements
            }
        };
        if !enabled {
            return Vec::new();
        }
        let participant_ids = match announcement {
            ConferenceAnnouncement::ParticipantMuted(participant_id)
            | ConferenceAnnouncement::ParticipantUnmuted(participant_id) => session
                .participants
                .get(participant_id)
                .map(|_| vec![participant_id])
                .unwrap_or_default(),
            ConferenceAnnouncement::Connected
            | ConferenceAnnouncement::ParticipantJoined(_)
            | ConferenceAnnouncement::ParticipantRemoved(_) => session
                .participants
                .iter()
                .map(|participant| participant.id)
                .collect(),
            ConferenceAnnouncement::ModeratorDeparted(participant_id) => session
                .participants
                .iter()
                .filter(|participant| participant.id != participant_id)
                .map(|participant| participant.id)
                .collect(),
        };
        if participant_ids.is_empty() {
            return Vec::new();
        }
        let targets = participant_ids
            .iter()
            .filter_map(|participant_id| {
                session
                    .participants
                    .get(*participant_id)
                    .map(|participant| ConferenceAnnouncementTarget {
                        participant_id: *participant_id,
                        call_id: participant.pbx_call_id,
                    })
            })
            .collect();
        vec![
            PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: session.id,
                    targets,
                    announcement,
                },
            }
            .into(),
        ]
    }

    pub(in crate::runtime::controller) fn conference_music_effects(
        session: &ConferenceSession,
        enabled: bool,
    ) -> Vec<DriverEffect> {
        let Some(class) = session.media_policy.music_on_hold_class.as_ref() else {
            return Vec::new();
        };
        session
            .participants
            .iter()
            .filter(|participant| !participant.moderator)
            .map(|participant| {
                PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            bridge_id: session.bridge_id,
                            participant_id: participant.id,
                            call_id: participant.pbx_call_id,
                            class: class.clone(),
                            enabled,
                        },
                }
                .into()
            })
            .collect()
    }

    pub(in crate::runtime::controller) fn conference_mute_on_entry_effects<'a>(
        session: &ConferenceSession,
        participants: impl IntoIterator<Item = &'a ConferenceParticipant>,
    ) -> Vec<DriverEffect> {
        if !session.media_policy.mute_on_entry {
            return Vec::new();
        }
        participants
            .into_iter()
            .filter(|participant| !participant.moderator)
            .map(|participant| {
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                        bridge_id: session.bridge_id,
                        participant_id: participant.id,
                        call_id: participant.pbx_call_id,
                        muted: true,
                    },
                }
                .into()
            })
            .collect()
    }

    /// Plan the atomic native merge after the consultation party answers.
    pub fn confirm_conference(
        &mut self,
        call_id: CallId,
    ) -> Result<Vec<DriverEffect>, ConferenceRejection> {
        let consultation = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
            .ok_or(ConferenceRejection::Unavailable)?;
        if consultation != call_id {
            return Err(ConferenceRejection::Conflict);
        }
        let session = self
            .conferences
            .by_consultation
            .get(&consultation)
            .cloned()
            .ok_or(ConferenceRejection::Unavailable)?;
        if session.phase != ConferencePhase::Consultation {
            return Err(ConferenceRejection::Conflict);
        }
        if self
            .call_registry
            .pbx
            .get(&session.consultation_call_id)
            .is_none_or(|call| call.state != CallState::Connected)
            || self
                .call_registry
                .pbx
                .get(&session.original_call_id)
                .is_none_or(|call| call.state != CallState::Held)
        {
            return Err(ConferenceRejection::NotConnected);
        }
        if let Some(stored) = self.conferences.by_consultation.get_mut(&consultation) {
            stored.phase = ConferencePhase::Merging;
        }
        debug_assert!(self.invariant_error().is_none());
        // Preserve both live two-party bridges until the atomic merge owns
        // them. Queueing Unhold first lets Asterisk transiently reconfigure
        // the original bridge before the immediately following lookup.
        let mut effects = vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Create {
                    bridge_id: session.bridge_id,
                },
            }
            .into(),
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeConsultation {
                    bridge_id: session.bridge_id,
                    original_call_id: session.original_call_id,
                    consultation_call_id: session.consultation_call_id,
                },
            }
            .into(),
            PbxEffect::Resume {
                call_id: session.original_call_id,
            }
            .into(),
        ];
        effects.extend(Self::conference_mute_on_entry_effects(
            &session,
            session.participants.iter(),
        ));
        Ok(effects)
    }

    pub fn conference_merged(&mut self, call_id: CallId) -> bool {
        let Some(consultation) = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&consultation) else {
            return false;
        };
        if session.phase != ConferencePhase::Merging {
            return false;
        }
        session.phase = ConferencePhase::Active;
        if session.media_policy.mute_on_entry {
            let participant_ids = session
                .participants
                .iter()
                .filter(|participant| !participant.moderator)
                .map(|participant| participant.id)
                .collect::<Vec<_>>();
            for participant_id in participant_ids {
                if !session.participants.set_muted(participant_id, true) {
                    return false;
                }
            }
        }
        let participant_calls: Vec<_> = session
            .participants
            .iter()
            .map(|participant| (participant.pbx_call_id, participant.handset_call_id))
            .collect();
        for (pbx_id, handset_call_id) in participant_calls {
            if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
                call.state = CallState::Connected;
            }
            if let Some(appearance_id) = self.call_registry.by_sccp.get(&handset_call_id).copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.state = CallState::Connected;
            }
        }
        debug_assert!(self.invariant_error().is_none());
        true
    }

    pub fn conference_json(&self, call_id: CallId) -> Option<String> {
        let session = self.conference_session(call_id)?;
        session.participants.to_json(session.id).ok()
    }

    /// Reserve a moderator-leg hold or resume while leaving the PBX channel
    /// and live conference bridge connected. Music changes only when this
    /// transition crosses the boundary between at least one listening
    /// moderator and none.
    pub fn begin_conference_moderator_leg_transition(
        &mut self,
        call_id: CallId,
        held: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conference_session(call_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .iter()
            .find(|participant| participant.handset_call_id == call_id)
            .filter(|participant| participant.moderator)
            .cloned()
            .ok_or(ConferenceParticipantRejection::NotModerator)?;
        if participant.held == held {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let appearance = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        let expected_state = if held {
            CallState::Connected
        } else {
            CallState::Held
        };
        if appearance.state != expected_state
            || self
                .call_registry
                .pbx
                .get(&participant.pbx_call_id)
                .is_none_or(|call| call.state != CallState::Connected)
        {
            return Err(ConferenceParticipantRejection::Conflict);
        }

        let change_music = if held {
            session.participants.active_moderator_count() == 1
        } else {
            session.participants.active_moderator_count() == 0
        };
        let mut effects = Vec::new();
        if held {
            effects.push(appearance_state_effect(
                &appearance,
                HandsetCallState::Hold,
                true,
            ));
        }
        if change_music {
            effects.extend(Self::conference_music_effects(session, held));
        }
        if !held {
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: participant.device_id.clone(),
                    call_id: participant.handset_call_id,
                    codec: appearance.codec,
                }
                .into(),
            );
        }

        let conference_id = session.id;
        let mutation = ConferenceParticipantMutation {
            participant_id: participant.id,
            call_id: participant.pbx_call_id,
            kind: ConferenceParticipantMutationKind::Hold(held),
        };
        self.conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above")
            .pending_participant_mutation = Some(mutation);
        if !held && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance.id) {
            appearance.audio = MediaStreamState::Opening;
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    /// Commit a handset/native-confirmed moderator-leg transition without
    /// changing the participant, PBX-call, or bridge identities.
    pub fn conference_moderator_leg_transitioned(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        held: bool,
    ) -> bool {
        let Some(key) = self
            .conferences
            .by_consultation
            .iter()
            .find_map(|(key, session)| (session.id == conference_id).then_some(*key))
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get(&key) else {
            return false;
        };
        let Some(pending) = session.pending_participant_mutation else {
            return false;
        };
        let Some(participant) = session.participants.get(participant_id).cloned() else {
            return false;
        };
        if pending.participant_id != participant_id
            || pending.call_id != participant.pbx_call_id
            || pending.kind != ConferenceParticipantMutationKind::Hold(held)
            || participant.held == held
        {
            return false;
        }
        let Some(appearance_id) = self
            .call_registry
            .by_sccp
            .get(&participant.handset_call_id)
            .copied()
            .filter(|appearance_id| self.call_registry.appearances.contains_key(appearance_id))
        else {
            return false;
        };
        let session = self
            .conferences
            .by_consultation
            .get_mut(&key)
            .expect("conference key validated above");
        if !session.participants.set_held(participant_id, held) {
            return false;
        }
        session.pending_participant_mutation = None;

        let appearance = self
            .call_registry
            .appearances
            .get_mut(&appearance_id)
            .expect("appearance validated above");
        appearance.state = if held {
            appearance.audio = MediaStreamState::Closed;
            appearance.audio_transmit = MediaStreamState::Closed;
            appearance.video.close_streams();
            CallState::Held
        } else {
            appearance.audio = MediaStreamState::Opening;
            CallState::Connected
        };
        let device_id = appearance.device_id.clone();
        let line_instance = appearance.line_instance;
        let handset_call_id = appearance.sccp_id;
        self.select_line(&device_id, line_instance);
        self.set_call_selected(&device_id, handset_call_id, !held);
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Release a failed transition and describe only the inverse operations
    /// required for handset/native work that may already have completed.
    pub fn abort_conference_moderator_leg_transition(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        held: bool,
        completed_music: &[ParticipantId],
        handset_attempted: bool,
    ) -> Vec<DriverEffect> {
        let Some(session) = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .cloned()
        else {
            return Vec::new();
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Hold(held)
        }) {
            return Vec::new();
        }
        let Some(participant) = session.participants.get(participant_id).cloned() else {
            return Vec::new();
        };
        if let Some(stored) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|stored| stored.id == conference_id)
        {
            stored.pending_participant_mutation = None;
        }

        let music = session.media_policy.music_on_hold_class.as_ref();
        let mut effects = Vec::new();
        if !held {
            if let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&participant.handset_call_id)
                .copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.audio = MediaStreamState::Closed;
            }
            if handset_attempted {
                effects.push(
                    HandsetEffect::SetCallState {
                        device_id: participant.device_id.clone(),
                        call_id: participant.handset_call_id,
                        state: HandsetCallState::Hold,
                        stop_media: true,
                    }
                    .into(),
                );
            }
        }
        if let Some(class) = music {
            effects.extend(completed_music.iter().filter_map(|completed| {
                session.participants.get(*completed).map(|target| {
                    PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                bridge_id: session.bridge_id,
                                participant_id: target.id,
                                call_id: target.pbx_call_id,
                                class: class.clone(),
                                enabled: !held,
                            },
                    }
                    .into()
                })
            }));
        }
        if held && handset_attempted {
            if let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&participant.handset_call_id)
                .copied()
                && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
            {
                appearance.audio = MediaStreamState::Opening;
            }
            let codec = self
                .appearance_for_call(participant.handset_call_id)
                .map_or(Codec::Pcmu, |appearance| appearance.codec);
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: participant.device_id,
                    call_id: participant.handset_call_id,
                    codec,
                }
                .into(),
            );
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Reserve a moderator-authorized participant mute transition. Participant
    /// state is committed only after the backend confirms that the live bridge
    /// channel was updated.
    pub fn begin_conference_participant_mute(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if &session.device_id != requester
            || session
                .participants
                .moderator()
                .is_none_or(|moderator| &moderator.device_id != requester)
        {
            return Err(ConferenceParticipantRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .get(participant_id)
            .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
        if participant.moderator {
            return Err(ConferenceParticipantRejection::Moderator);
        }
        if participant.muted == muted {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let mutation = ConferenceParticipantMutation {
            participant_id,
            call_id: participant.pbx_call_id,
            kind: ConferenceParticipantMutationKind::Mute(muted),
        };
        let bridge_id = session.bridge_id;
        let session = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above");
        session.pending_participant_mutation = Some(mutation);
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    bridge_id,
                    participant_id,
                    call_id: mutation.call_id,
                    muted,
                },
            }
            .into(),
        ])
    }

    /// Commit a participant mute transition after the backend succeeds.
    pub fn conference_participant_muted(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        let Some(pending) = session.pending_participant_mutation else {
            return false;
        };
        if pending.participant_id != participant_id
            || pending.kind != ConferenceParticipantMutationKind::Mute(muted)
        {
            return false;
        }
        let Some(participant) = session.participants.get(participant_id) else {
            return false;
        };
        if participant.pbx_call_id != pending.call_id
            || participant.moderator
            || participant.muted == muted
        {
            return false;
        }
        if !session.participants.set_muted(participant_id, muted) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Release a reserved mute transition after any backend failure. The
    /// published participant state remains unchanged.
    pub fn abort_conference_participant_mute(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        muted: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Mute(muted)
        }) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Reserve removal of one non-moderator while retaining at least two live
    /// conference members. Registry and UI state remain unchanged until the
    /// backend validates and clears the exact bridge member.
    pub fn begin_conference_participant_removal(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if &session.device_id != requester
            || session
                .participants
                .moderator()
                .is_none_or(|moderator| &moderator.device_id != requester)
        {
            return Err(ConferenceParticipantRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .get(participant_id)
            .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
        if participant.moderator {
            return Err(ConferenceParticipantRejection::Moderator);
        }
        if session.participants.iter().len() <= 2 {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let mutation = ConferenceParticipantMutation {
            participant_id,
            call_id: participant.pbx_call_id,
            kind: ConferenceParticipantMutationKind::Remove,
        };
        let bridge_id = session.bridge_id;
        self.conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above")
            .pending_participant_mutation = Some(mutation);
        debug_assert!(self.invariant_error().is_none());
        Ok(vec![
            PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::RemoveConferenceParticipant {
                    bridge_id,
                    participant_id,
                    call_id: mutation.call_id,
                },
            }
            .into(),
        ])
    }

    /// Commit a backend-confirmed participant removal, re-keying the internal
    /// conference session if its historical consultation leg was removed.
    pub fn conference_participant_removed(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> Option<Vec<DriverEffect>> {
        let key = self
            .conferences
            .by_consultation
            .iter()
            .find_map(|(key, session)| (session.id == conference_id).then_some(*key))?;
        let mut session = self.conferences.by_consultation.remove(&key)?;
        let Some(pending) = session.pending_participant_mutation else {
            self.conferences.by_consultation.insert(key, session);
            return None;
        };
        if pending.participant_id != participant_id
            || pending.kind != ConferenceParticipantMutationKind::Remove
            || session.participants.iter().len() <= 2
            || session
                .participants
                .get(participant_id)
                .is_none_or(|participant| {
                    participant.moderator || participant.pbx_call_id != pending.call_id
                })
        {
            self.conferences.by_consultation.insert(key, session);
            return None;
        }

        let removed = session
            .participants
            .remove(participant_id)
            .expect("participant validated above");
        session.pending_participant_mutation = None;
        self.conferences.by_pbx.remove(&removed.pbx_call_id);
        if session.consultation_call_id == removed.pbx_call_id {
            let replacement = session
                .participants
                .iter()
                .find(|participant| participant.pbx_call_id != session.original_call_id)
                .expect("a removable conference retains a secondary participant");
            session.consultation_call_id = replacement.pbx_call_id;
            session.consultation_handset_call_id = replacement.handset_call_id;
        }
        let new_key = session.consultation_handset_call_id;
        for participant in session.participants.iter() {
            self.conferences
                .by_pbx
                .insert(participant.pbx_call_id, new_key);
        }
        let appearance = self.appearance_for_call(removed.handset_call_id).cloned();
        self.remove_pbx_call(removed.pbx_call_id);
        self.conferences.by_consultation.insert(new_key, session);

        let effects = appearance
            .as_ref()
            .map(|appearance| {
                vec![appearance_state_effect(
                    appearance,
                    HandsetCallState::OnHook,
                    true,
                )]
            })
            .unwrap_or_default();
        debug_assert!(self.invariant_error().is_none());
        Some(effects)
    }

    pub fn abort_conference_participant_removal(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Remove
        }) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Reserve a moderator role transition. When the role change crosses the
    /// boundary between a listening moderator and no listening moderators,
    /// conference music is changed before the new role is committed.
    pub fn begin_conference_participant_role_change(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .ok_or(ConferenceParticipantRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceParticipantRejection::Unavailable);
        }
        if &session.device_id != requester
            || !session
                .participants
                .iter()
                .any(|participant| participant.moderator && &participant.device_id == requester)
        {
            return Err(ConferenceParticipantRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        let participant = session
            .participants
            .get(participant_id)
            .ok_or(ConferenceParticipantRejection::InvalidParticipant)?;
        if participant.moderator == moderator {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        if participant.held || (moderator && participant.muted) {
            return Err(ConferenceParticipantRejection::Conflict);
        }
        if !moderator && session.participants.moderator_count() == 1 {
            return Err(ConferenceParticipantRejection::LastModerator);
        }
        let effects = if moderator && session.participants.active_moderator_count() == 0 {
            Self::conference_music_effects(session, false)
        } else if !moderator && session.participants.active_moderator_count() == 1 {
            let Some(class) = session.media_policy.music_on_hold_class.as_ref() else {
                return self.reserve_conference_participant_role_change(
                    conference_id,
                    participant_id,
                    participant.pbx_call_id,
                    moderator,
                    Vec::new(),
                );
            };
            session
                .participants
                .iter()
                .filter(|candidate| !candidate.moderator || candidate.id == participant_id)
                .map(|candidate| {
                    PbxEffect::Bridge {
                        operation:
                            crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                                bridge_id: session.bridge_id,
                                participant_id: candidate.id,
                                call_id: candidate.pbx_call_id,
                                class: class.clone(),
                                enabled: true,
                            },
                    }
                    .into()
                })
                .collect()
        } else {
            Vec::new()
        };
        self.reserve_conference_participant_role_change(
            conference_id,
            participant_id,
            participant.pbx_call_id,
            moderator,
            effects,
        )
    }

    pub(in crate::runtime::controller) fn reserve_conference_participant_role_change(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        call_id: PbxCallId,
        moderator: bool,
        effects: Vec<DriverEffect>,
    ) -> Result<Vec<DriverEffect>, ConferenceParticipantRejection> {
        let mutation = ConferenceParticipantMutation {
            participant_id,
            call_id,
            kind: ConferenceParticipantMutationKind::Moderator(moderator),
        };
        self.conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
            .expect("conference validated above")
            .pending_participant_mutation = Some(mutation);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn conference_participant_role_changed(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        let Some(pending) = session.pending_participant_mutation else {
            return false;
        };
        if pending.participant_id != participant_id
            || pending.kind != ConferenceParticipantMutationKind::Moderator(moderator)
            || session
                .participants
                .get(participant_id)
                .is_none_or(|participant| {
                    participant.pbx_call_id != pending.call_id
                        || participant.moderator == moderator
                        || participant.held
                        || (moderator && participant.muted)
                })
        {
            return false;
        }
        if session
            .participants
            .set_moderator(participant_id, moderator)
            .is_err()
        {
            session.pending_participant_mutation = None;
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    pub fn abort_conference_participant_role_change(
        &mut self,
        conference_id: ConferenceId,
        participant_id: ParticipantId,
        moderator: bool,
    ) -> bool {
        let Some(session) = self
            .conferences
            .by_consultation
            .values_mut()
            .find(|session| session.id == conference_id)
        else {
            return false;
        };
        if session.pending_participant_mutation.is_none_or(|pending| {
            pending.participant_id != participant_id
                || pending.kind != ConferenceParticipantMutationKind::Moderator(moderator)
        }) {
            return false;
        }
        session.pending_participant_mutation = None;
        debug_assert!(self.invariant_error().is_none());
        true
    }

    /// Restore selected calls after a native multi-call merge fails. Native
    /// merge is atomic, so only calls resumed before the merge need re-hold.
    pub fn abort_join_conference(
        &mut self,
        call_id: CallId,
        bridge_created: bool,
        resumed_call_ids: &[PbxCallId],
    ) -> Vec<DriverEffect> {
        let Some(key) = self
            .conference_session(call_id)
            .filter(|session| session.origin == ConferenceOrigin::Selection)
            .map(|session| session.consultation_handset_call_id)
        else {
            return Vec::new();
        };
        let Some(session) = self.conferences.by_consultation.remove(&key) else {
            return Vec::new();
        };
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Session(session.id));
        for participant in session.participants.iter() {
            self.conferences.by_pbx.remove(&participant.pbx_call_id);
        }
        let mut effects = Vec::new();
        if bridge_created {
            effects.push(
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy {
                        bridge_id: session.bridge_id,
                    },
                }
                .into(),
            );
        }
        effects.extend(
            resumed_call_ids
                .iter()
                .copied()
                .map(|call_id| PbxEffect::Hold { call_id }.into()),
        );
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn cancel_conference(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(session) = self.conference_session(call_id) else {
            return Vec::new();
        };
        if session.phase != ConferencePhase::Consultation
            || session.origin != ConferenceOrigin::Consultation
        {
            return Vec::new();
        }
        self.abort_conference(call_id, false, true, true, true)
    }

    pub fn end_conference(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(session) = self.conference_session(call_id).cloned() else {
            return Vec::new();
        };
        if session.phase != ConferencePhase::Active {
            return Vec::new();
        }
        self.end_conference_internal(session, true, None)
    }

    /// Commit the loss of one handset conference presentation under the same
    /// controller lock as every other conference mutation. Pending work makes
    /// the failure terminal; otherwise the normal departure policy may retain
    /// the remaining participants and their stable identities.
    pub fn conference_participant_failed(
        &mut self,
        call_id: CallId,
    ) -> Option<ConferenceParticipantFailureOutcome> {
        let session = self.conference_session(call_id)?.clone();
        let failed_call_id = self.appearance_for_call(call_id)?.pbx_id;
        let mut owned_call_ids = session
            .participants
            .iter()
            .map(|participant| participant.pbx_call_id)
            .collect::<Vec<_>>();
        if let Some(invite) = &session.pending_invite {
            owned_call_ids.push(invite.participant.pbx_call_id);
        }
        let effects = self.hangup(call_id);
        let surviving_session = self.conference_session_by_id(session.id).cloned();
        let call_ids = if surviving_session.is_some() {
            vec![failed_call_id]
        } else {
            owned_call_ids
        };
        debug_assert!(self.invariant_error().is_none());
        Some(ConferenceParticipantFailureOutcome {
            conference_id: session.id,
            failed_call_id,
            call_ids,
            surviving_session,
            effects,
        })
    }

    /// Atomically detach every conference before module shutdown. The
    /// deterministic plans remain valid after the controller lock is released
    /// and a second drain is an explicit no-op.
    pub fn drain_conferences_for_shutdown(&mut self) -> Vec<ConferenceCleanupPlan> {
        let mut sessions = self
            .conferences
            .by_consultation
            .values()
            .cloned()
            .collect::<Vec<_>>();
        sessions.sort_by_key(|session| session.id);

        let plans = sessions
            .into_iter()
            .map(|session| {
                debug_assert!(self.conference_session_by_id(session.id).is_some());
                let mut call_ids = session
                    .participants
                    .iter()
                    .map(|participant| participant.pbx_call_id)
                    .collect::<Vec<_>>();
                if let Some(invite) = &session.pending_invite {
                    call_ids.push(invite.participant.pbx_call_id);
                }
                let bridge_created = session.phase != ConferencePhase::Consultation;
                let conference_id = session.id;
                let effects = self.end_conference_internal(session, bridge_created, None);
                ConferenceCleanupPlan {
                    conference_id,
                    call_ids,
                    effects,
                }
            })
            .collect();
        debug_assert!(self.invariant_error().is_none());
        plans
    }

    /// Authorize and claim an explicit handset conference termination. The
    /// controller removes the complete conference atomically so a concurrent
    /// action or PBX callback cannot schedule a second cleanup sequence.
    pub fn end_conference_by_moderator(
        &mut self,
        requester: &DeviceId,
        conference_id: ConferenceId,
    ) -> Result<Vec<DriverEffect>, ConferenceEndRejection> {
        let session = self
            .conferences
            .by_consultation
            .values()
            .find(|session| session.id == conference_id)
            .cloned()
            .ok_or(ConferenceEndRejection::Unavailable)?;
        if session.phase != ConferencePhase::Active {
            return Err(ConferenceEndRejection::Unavailable);
        }
        if &session.device_id != requester
            || !session
                .participants
                .iter()
                .any(|participant| participant.moderator && &participant.device_id == requester)
        {
            return Err(ConferenceEndRejection::NotModerator);
        }
        if session.pending_participant_mutation.is_some() || session.pending_invite.is_some() {
            return Err(ConferenceEndRejection::Conflict);
        }
        Ok(self.end_conference_internal(session, true, None))
    }

    /// Roll back a failed consultation start or bridge merge. The flags
    /// describe backend/handset work that completed before the failure.
    pub fn abort_conference(
        &mut self,
        call_id: CallId,
        bridge_created: bool,
        consultation_channel_created: bool,
        original_needs_resume: bool,
        restore_original_media: bool,
    ) -> Vec<DriverEffect> {
        let Some(consultation) = self
            .conference_session(call_id)
            .map(|session| session.consultation_handset_call_id)
        else {
            return Vec::new();
        };
        let Some(session) = self.conferences.by_consultation.remove(&consultation) else {
            return Vec::new();
        };
        if session.origin != ConferenceOrigin::Consultation {
            self.conferences
                .by_consultation
                .insert(consultation, session);
            return Vec::new();
        }
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Session(session.id));
        for participant in session.participants.iter() {
            self.conferences.by_pbx.remove(&participant.pbx_call_id);
        }
        self.remove_pbx_call(session.consultation_call_id);

        let original_appearance = self
            .appearance_for_call(session.original_handset_call_id)
            .cloned();
        if let Some(call) = self.call_registry.pbx.get_mut(&session.original_call_id) {
            call.state = CallState::Connected;
        }
        if let Some(appearance) = original_appearance.as_ref()
            && let Some(stored) = self.call_registry.appearances.get_mut(&appearance.id)
        {
            stored.state = CallState::Connected;
            stored.audio = if restore_original_media {
                MediaStreamState::Opening
            } else {
                appearance.audio
            };
        }
        self.select_line(
            &session.device_id,
            original_appearance
                .as_ref()
                .map_or(0, |call| call.line_instance),
        );
        self.set_call_selected(&session.device_id, session.original_handset_call_id, true);

        let mut effects = Vec::new();
        if bridge_created {
            effects.push(
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy {
                        bridge_id: session.bridge_id,
                    },
                }
                .into(),
            );
        }
        if consultation_channel_created {
            effects.push(
                PbxEffect::Hangup {
                    call_id: session.consultation_call_id,
                }
                .into(),
            );
        }
        if original_needs_resume {
            effects.push(
                PbxEffect::Resume {
                    call_id: session.original_call_id,
                }
                .into(),
            );
        }
        effects.push(
            HandsetEffect::SetCallState {
                device_id: session.device_id.clone(),
                call_id: session.consultation_handset_call_id,
                state: HandsetCallState::OnHook,
                stop_media: true,
            }
            .into(),
        );
        if restore_original_media && let Some(original) = original_appearance {
            effects.push(
                HandsetEffect::BeginMedia {
                    device_id: original.device_id,
                    call_id: original.sccp_id,
                    codec: original.codec,
                }
                .into(),
            );
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub(in crate::runtime::controller) fn active_conference_departure(
        &mut self,
        session: ConferenceSession,
        pbx_id: PbxCallId,
        already_hung_up: Option<PbxCallId>,
        announce: bool,
    ) -> Vec<DriverEffect> {
        let Some(departing) = session
            .participants
            .iter()
            .find(|participant| participant.pbx_call_id == pbx_id)
            .cloned()
        else {
            return self.end_conference_internal(session, true, already_hung_up);
        };
        let remaining_participants = session.participants.iter().len().saturating_sub(1);
        let remaining_moderators = session
            .participants
            .moderator_count()
            .saturating_sub(usize::from(departing.moderator));
        let must_end = remaining_participants < 2
            || remaining_moderators == 0
            || session.pending_invite.is_some()
            || session.pending_participant_mutation.is_some();
        if must_end {
            let announcement = if departing.moderator {
                ConferenceAnnouncement::ModeratorDeparted(departing.id)
            } else {
                ConferenceAnnouncement::ParticipantRemoved(departing.id)
            };
            let mut effects = if announce {
                Self::conference_announcement_effects_for_session(&session, announcement)
            } else {
                Vec::new()
            };
            effects.extend(self.end_conference_internal(session, true, already_hung_up));
            return effects;
        }

        let old_key = session.consultation_handset_call_id;
        let mut session = self
            .conferences
            .by_consultation
            .remove(&old_key)
            .expect("active conference departure has a live session");
        let removed = session
            .participants
            .remove(departing.id)
            .expect("departing participant was validated above");
        self.conferences.by_pbx.remove(&removed.pbx_call_id);
        let appearance = self.appearance_for_call(removed.handset_call_id).cloned();
        self.remove_pbx_call(removed.pbx_call_id);

        if session.original_call_id == removed.pbx_call_id {
            let moderator = session
                .participants
                .moderator()
                .expect("a preserved conference retains a moderator");
            session.original_call_id = moderator.pbx_call_id;
            session.original_handset_call_id = moderator.handset_call_id;
            session.device_id = moderator.device_id.clone();
        }
        if session.consultation_call_id == removed.pbx_call_id
            || session.consultation_call_id == session.original_call_id
        {
            let replacement = session
                .participants
                .iter()
                .find(|participant| participant.pbx_call_id != session.original_call_id)
                .expect("a preserved conference retains a secondary participant");
            session.consultation_call_id = replacement.pbx_call_id;
            session.consultation_handset_call_id = replacement.handset_call_id;
        }
        let new_key = session.consultation_handset_call_id;
        for participant in session.participants.iter() {
            self.conferences
                .by_pbx
                .insert(participant.pbx_call_id, new_key);
        }
        let announcement = if announce {
            Self::conference_announcement_effects_for_session(
                &session,
                if departing.moderator {
                    ConferenceAnnouncement::ModeratorDeparted(departing.id)
                } else {
                    ConferenceAnnouncement::ParticipantRemoved(departing.id)
                },
            )
        } else {
            Vec::new()
        };
        self.conferences.by_consultation.insert(new_key, session);

        let mut effects = already_hung_up
            .is_none()
            .then_some(
                PbxEffect::Hangup {
                    call_id: removed.pbx_call_id,
                }
                .into(),
            )
            .into_iter()
            .collect::<Vec<_>>();
        effects.extend(
            appearance
                .as_ref()
                .map(|appearance| {
                    vec![appearance_state_effect(
                        appearance,
                        HandsetCallState::OnHook,
                        true,
                    )]
                })
                .unwrap_or_default(),
        );
        effects.extend(announcement);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub(in crate::runtime::controller) fn end_conference_internal(
        &mut self,
        session: ConferenceSession,
        bridge_created: bool,
        already_hung_up: Option<PbxCallId>,
    ) -> Vec<DriverEffect> {
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Session(session.id));
        self.conferences
            .by_consultation
            .remove(&session.consultation_handset_call_id);
        let mut participants: Vec<_> = session.participants.iter().cloned().collect();
        if let Some(invite) = session.pending_invite {
            participants.push(invite.participant);
        }
        for participant in &participants {
            self.conferences.by_pbx.remove(&participant.pbx_call_id);
        }
        let appearances: Vec<_> = participants
            .iter()
            .filter_map(|participant| {
                self.appearance_for_call(participant.handset_call_id)
                    .cloned()
            })
            .collect();
        for participant in &participants {
            self.remove_pbx_call(participant.pbx_call_id);
        }

        let mut effects = Vec::new();
        if bridge_created {
            effects.push(
                PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy {
                        bridge_id: session.bridge_id,
                    },
                }
                .into(),
            );
        }
        for participant in &participants {
            if already_hung_up != Some(participant.pbx_call_id) {
                effects.push(
                    PbxEffect::Hangup {
                        call_id: participant.pbx_call_id,
                    }
                    .into(),
                );
            }
        }
        for appearance in appearances {
            effects.push(appearance_state_effect(
                &appearance,
                HandsetCallState::OnHook,
                true,
            ));
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub(in crate::runtime::controller) fn allocate_conference_id(&mut self) -> ConferenceId {
        loop {
            let id = ConferenceId::new(self.next_conference_id);
            self.next_conference_id = self.next_conference_id.wrapping_add(1).max(1);
            if !self
                .conferences
                .by_consultation
                .values()
                .any(|conference| conference.id == id)
            {
                return id;
            }
        }
    }

    pub(in crate::runtime::controller) fn allocate_participant_id(&mut self) -> ParticipantId {
        loop {
            let id = ParticipantId::new(self.next_participant_id);
            self.next_participant_id = self.next_participant_id.wrapping_add(1).max(1);
            if !self
                .conferences
                .by_consultation
                .values()
                .any(|conference| conference.participants.get(id).is_some())
            {
                return id;
            }
        }
    }

    pub(in crate::runtime::controller) fn conference_participant(
        &mut self,
        appearance: &CallAppearance,
        moderator: bool,
    ) -> ConferenceParticipant {
        let identity = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .map(|call| conference_participant_identity(call, appearance))
            .unwrap_or_default();
        ConferenceParticipant {
            id: self.allocate_participant_id(),
            pbx_call_id: appearance.pbx_id,
            handset_call_id: appearance.sccp_id,
            device_id: appearance.device_id.clone(),
            display_name: identity.display_name,
            number: identity.number,
            moderator,
            muted: false,
            held: false,
        }
    }

    pub(in crate::runtime::controller) fn refresh_conference_participant_identity(
        &mut self,
        pbx_id: PbxCallId,
    ) -> bool {
        let Some(conference_key) = self.conferences.by_pbx.get(&pbx_id).copied() else {
            return false;
        };
        let Some(handset_call_id) = self
            .conferences
            .by_consultation
            .get(&conference_key)
            .and_then(|session| {
                session
                    .participants
                    .by_pbx(pbx_id)
                    .map(|participant| participant.handset_call_id)
                    .or_else(|| {
                        session
                            .pending_invite
                            .as_ref()
                            .filter(|invite| invite.participant.pbx_call_id == pbx_id)
                            .map(|invite| invite.participant.handset_call_id)
                    })
            })
        else {
            return false;
        };
        let Some(identity) = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .zip(self.appearance_for_call(handset_call_id))
            .map(|(call, appearance)| conference_participant_identity(call, appearance))
        else {
            return false;
        };
        let Some(session) = self.conferences.by_consultation.get_mut(&conference_key) else {
            return false;
        };
        if session
            .participants
            .update_identity(pbx_id, identity.clone())
        {
            return true;
        }
        let Some(invite) = session
            .pending_invite
            .as_mut()
            .filter(|invite| invite.participant.pbx_call_id == pbx_id)
        else {
            return false;
        };
        invite.participant.display_name = identity.display_name;
        invite.participant.number = identity.number;
        true
    }
}
