use super::super::*;

impl Controller {
    pub fn with_digit_timeouts(first_digit: Duration, interdigit: Duration) -> Self {
        Self {
            next_pbx_id: 1,
            next_appearance_id: 1,
            next_bridge_id: 1,
            next_conference_id: 1,
            next_participant_id: 1,
            next_conference_mutation_generation: 1,
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
            next_call_transition_id: 1,
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
            next_auto_answer_generation: 1,
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
            next_remote_hangup_generation: 1,
            first_digit,
            interdigit,
            dial_terminator: '#',
            simulate_enbloc: true,
            overlap_devices: HashSet::new(),
            line_dial_tones: HashMap::new(),
            line_incoming_limits: HashMap::new(),
            call_waiting_tones: HashMap::new(),
            pending_phone_answers: HashMap::new(),
            pending_route_media: HashSet::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
            pending_call_transitions: HashMap::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
            auto_answer_requests: HashMap::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
            pending_auto_answers: HashMap::new(),
            #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
            pending_remote_hangups: HashMap::new(),
            devices: HashMap::new(),
            features: HashMap::new(),
            call_registry: CallRegistry::default(),
            shared_control_claims: HashMap::new(),
            barges: BargeRegistry::default(),
            conferences: ConferenceRegistry::default(),
            conference_mutations: HashMap::new(),
            transfers: TransferRegistry::default(),
            voicemail: VoicemailRegistry::default(),
            redirect_claims: HashSet::new(),
        }
    }

    /// Applies to future digit collection updates. Existing absolute
    /// deadlines and active call state are intentionally left unchanged.
    pub fn set_interdigit_timeout(&mut self, interdigit: Duration) {
        self.interdigit = interdigit;
    }

    /// Applies to future calls that have not collected their first digit.
    /// Existing absolute deadlines and active call state are unchanged.
    pub fn set_first_digit_timeout(&mut self, first_digit: Duration) {
        self.first_digit = first_digit;
    }

    /// Applies to future digits. A terminator already collected into a call is
    /// not rewritten when this policy changes.
    pub fn set_dial_terminator(&mut self, character: char) {
        self.dial_terminator = character;
    }

    pub fn set_overlap_devices(&mut self, devices: impl IntoIterator<Item = DeviceId>) {
        self.overlap_devices = devices.into_iter().collect();
    }

    /// Replaces the normalized logical-line dial-tone policy used by future
    /// off-hook and digit-collection transitions.
    pub fn set_line_dial_tones(
        &mut self,
        lines: impl IntoIterator<Item = (String, LineDialToneConfig)>,
    ) {
        self.line_dial_tones = lines.into_iter().collect();
    }

    /// Installs a newly accepted station session.
    ///
    /// A newer session atomically retires the prior session's call state. Its
    /// cleanup is returned to the adapter. Handset effects for the replaced
    /// connection are discarded, while effects for surviving devices remain.
    pub fn register_session(
        &mut self,
        session_generation: SessionGeneration,
        registration: DeviceRegistration,
    ) -> Option<RegisterSessionOutcome> {
        let device = registration.id.clone();
        if self
            .devices
            .get(&device)
            .is_some_and(|current| session_generation <= current.session_generation)
        {
            return None;
        }

        let replaced = self.devices.contains_key(&device);
        let mut cleanup = if replaced {
            self.disconnected(&device)
        } else {
            Vec::new()
        };
        cleanup.retain(|effect| {
            !matches!(effect, DriverEffect::Handset(effect) if effect.device_id() == &device)
        });
        self.devices.insert(
            device,
            RegisteredDevice {
                registration,
                session_generation,
                capabilities: None,
                audio_encryption: StationEncryptionCapabilities::default(),
                selected_line: None,
                active_call: None,
                selected_calls: HashSet::new(),
            },
        );
        Some(RegisterSessionOutcome { cleanup, replaced })
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

    pub fn registered_device(&self, device: &DeviceId) -> Option<&RegisteredDevice> {
        self.devices.get(device)
    }

    pub fn registered_devices(&self) -> impl Iterator<Item = (&DeviceId, &RegisteredDevice)> {
        self.devices.iter()
    }

    pub fn feature_state(&self, device: &DeviceId) -> Option<&DeviceFeatureState> {
        self.features.get(device)
    }

    pub fn feature_state_mut(&mut self, device: &DeviceId) -> &mut DeviceFeatureState {
        self.features.entry(device.clone()).or_default()
    }

    pub fn set_feature_state(&mut self, device: &DeviceId, state: DeviceFeatureState) {
        self.features.insert(device.clone(), state);
    }

    pub fn replace_feature_states(&mut self, states: HashMap<DeviceId, DeviceFeatureState>) {
        self.features = states;
    }

    pub fn set_dnd(&mut self, device: &DeviceId, mode: DndMode) {
        self.feature_state_mut(device).dnd = mode;
    }

    pub fn set_forwarding(&mut self, device: &DeviceId, forwarding: ForwardingState) {
        self.feature_state_mut(device).forwarding = forwarding;
    }

    pub fn set_feature_button(&mut self, device: &DeviceId, instance: u32, enabled: bool) {
        self.feature_state_mut(device)
            .buttons
            .insert(instance, enabled);
    }

    /// Change privacy for the call owned by `call_id`. Remote appearances
    /// cannot change another handset's privacy policy.
    pub fn set_call_privacy(&mut self, call_id: CallId, enabled: bool) -> bool {
        let Some(appearance) = self.appearance_for_call(call_id) else {
            return false;
        };
        let pbx_id = appearance.pbx_id;
        let appearance_id = appearance.id;
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return false;
        };
        if call.active_appearance != Some(appearance_id) {
            return false;
        }
        call.privacy = enabled;
        self.refresh_conference_participant_identity(pbx_id);
        true
    }

    pub fn call_privacy(&self, call_id: CallId) -> Option<bool> {
        let appearance = self.appearance_for_call(call_id)?;
        self.call_registry
            .pbx
            .get(&appearance.pbx_id)
            .map(|call| call.privacy)
    }

    pub(in crate::runtime::controller) fn set_active_call(
        &mut self,
        device: &DeviceId,
        call_id: Option<CallId>,
    ) -> bool {
        if call_id.is_some_and(|call_id| {
            self.appearance_for_call(call_id)
                .is_none_or(|appearance| &appearance.device_id != device)
        }) {
            return false;
        }
        let Some(state) = self.devices.get_mut(device) else {
            return false;
        };
        state.active_call = call_id;
        true
    }

    pub fn set_call_selected(
        &mut self,
        device: &DeviceId,
        call_id: CallId,
        selected: bool,
    ) -> bool {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device)
        {
            return false;
        }
        let Some(state) = self.devices.get_mut(device) else {
            return false;
        };
        if selected {
            state.selected_calls.insert(call_id);
        } else {
            state.selected_calls.remove(&call_id);
        }
        true
    }

    /// Toggle a handset's explicit selection marker for an appearance owned
    /// by that handset. The returned value is the new selection state.
    pub fn toggle_call_selected(&mut self, device: &DeviceId, call_id: CallId) -> Option<bool> {
        if self
            .appearance_for_call(call_id)
            .is_none_or(|appearance| &appearance.device_id != device)
        {
            return None;
        }
        let state = self.devices.get_mut(device)?;
        let selected = !state.selected_calls.contains(&call_id);
        if selected {
            state.selected_calls.insert(call_id);
        } else {
            state.selected_calls.remove(&call_id);
        }
        Some(selected)
    }

    /// Move one handset's active call plane to an exact local presentation.
    ///
    /// The previous connected leg is held before the target is answered or
    /// resumed. Validation is completed before either transition so a stale,
    /// remote, or conference-owned target cannot disturb the current call.
    pub fn switch_active_call(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> Result<Vec<DriverEffect>, CallSwitchRejection> {
        let target = self
            .appearance_for_call(call_id)
            .filter(|appearance| &appearance.device_id == device_id)
            .cloned()
            .ok_or(CallSwitchRejection::Unavailable)?;
        if self.conferences.by_pbx.contains_key(&target.pbx_id) {
            return Err(CallSwitchRejection::Conflict);
        }
        if !matches!(
            target.state,
            CallState::Ringing | CallState::Connected | CallState::Held | CallState::SharedHeld
        ) {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous = self
            .devices
            .get(device_id)
            .ok_or(CallSwitchRejection::Unavailable)?
            .active_call;
        if previous == Some(call_id) {
            return Ok(Vec::new());
        }
        if let Some(previous) = previous
            && let Some(previous_state) = self.call_state(previous)
            && matches!(
                previous_state,
                CallState::Collecting
                    | CallState::Calling
                    | CallState::Connected
                    | CallState::TransferCollecting
            )
            && (self.conference_session(previous).is_some()
                || self.barges.by_handset.contains_key(&previous))
        {
            return Err(CallSwitchRejection::Conflict);
        }

        let mut effects = Vec::new();
        if let Some(previous) = previous
            && let Some(previous_state) = self.call_state(previous)
            && matches!(
                previous_state,
                CallState::Collecting
                    | CallState::Calling
                    | CallState::Connected
                    | CallState::TransferCollecting
            )
        {
            let hold = self.hold(previous);
            if hold.is_empty() {
                return Err(CallSwitchRejection::Conflict);
            }
            effects.extend(hold);
        }
        let activate = match target.state {
            CallState::Ringing => self.phone_answer(call_id),
            CallState::Held | CallState::SharedHeld => self.resume(call_id),
            CallState::Connected => {
                self.set_active_call(device_id, Some(call_id));
                self.select_line(device_id, target.line_instance);
                vec![
                    HandsetEffect::SetCallState {
                        device_id: device_id.clone(),
                        call_id,
                        state: HandsetCallState::Connected,
                        stop_media: false,
                    }
                    .into(),
                ]
            }
            _ => Vec::new(),
        };
        if activate.is_empty() && target.state != CallState::Connected {
            return Err(CallSwitchRejection::Conflict);
        }
        effects.extend(activate);
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn begin_active_call_switch_transaction(
        &mut self,
        device_id: &DeviceId,
        call_id: CallId,
    ) -> Result<CallTransition, CallSwitchRejection> {
        if self
            .pending_call_transitions
            .values()
            .any(|pending| &pending.transition.device_id == device_id)
        {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous_call_id = self
            .devices
            .get(device_id)
            .and_then(|device| device.active_call);
        let previous_pbx_id = previous_call_id
            .and_then(|previous| self.appearance_for_call(previous))
            .map(|appearance| appearance.pbx_id);
        let target = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(CallSwitchRejection::Unavailable)?;
        let snapshot = self.call_domain_snapshot();
        let effects = self.switch_active_call(device_id, call_id)?;
        let transition = CallTransition {
            id: self.allocate_call_transition_id(),
            effects,
            device_id: device_id.clone(),
            target_call_id: call_id,
            target_pbx_id: target.pbx_id,
            previous_call_id,
            previous_pbx_id,
            kind: CallTransitionKind::Switch(target.state),
            auto_answer_mode: None,
        };
        self.pending_call_transitions.insert(
            transition.id,
            PendingCallTransition {
                transition: transition.clone(),
                snapshot,
                progress: CallTransitionProgress::default(),
            },
        );
        Ok(transition)
    }

    pub fn begin_phone_call(
        &mut self,
        sccp_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Vec<DriverEffect> {
        if self.call_registry.by_sccp.contains_key(&sccp_id) {
            return Vec::new();
        }
        let device_id = binding.device_id.clone();
        let line_instance = binding.line_instance;
        let pbx_id = self.allocate_pbx_id();
        let privacy = binding.appearance.privacy
            || self
                .features
                .get(&device_id)
                .is_some_and(|state| state.privacy);
        let appearance_id = self.allocate_appearance_id();
        let info = CallInfo {
            direction: CallDirection::Outbound,
            calling_name: binding.line.caller_name.clone(),
            calling_number: binding.line.caller_number.clone(),
            ..CallInfo::default()
        };
        let pbx_call = PbxCall {
            id: pbx_id,
            line: binding.line.number.clone(),
            context: binding.line.context.clone(),
            direction: CallDirection::Outbound,
            state: CallState::Collecting,
            outbound_phase: Some(OutboundCallPhase::Collecting),
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: Some(appearance_id),
            digit_deadline: Some(now + self.first_digit),
            last_digit_at: None,
            simulated_enbloc_eligible: self.simulate_enbloc,
            overlap_enabled: self.overlap_devices.contains(&device_id),
        };
        let appearance = CallAppearance {
            id: appearance_id,
            sccp_id,
            pbx_id,
            device_id: device_id.clone(),
            line_instance: binding.line_instance,
            state: CallState::Collecting,
            ring_mode: binding.appearance.ring_mode,
            privacy: binding.appearance.privacy,
            info,
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        if !self.insert_pbx_call(pbx_call, appearance) {
            return Vec::new();
        }
        self.select_line(&device_id, line_instance);
        self.set_active_call(&device_id, Some(sccp_id));
        self.set_call_selected(&device_id, sccp_id, true);
        debug_assert!(self.invariant_error().is_none());
        vec![
            PbxEffect::CreateChannel {
                handset_call_id: sccp_id,
                call_id: pbx_id,
                binding: Box::new(binding),
                codec,
            }
            .into(),
        ]
    }

    /// Begin an additional handset call, holding the exact active ordinary
    /// call first. Conference and barge legs reject the transition so a new
    /// call cannot silently detach their active presentation.
    pub fn begin_additional_phone_call(
        &mut self,
        sccp_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Result<Vec<DriverEffect>, CallSwitchRejection> {
        if self.call_registry.by_sccp.contains_key(&sccp_id)
            || !self.devices.contains_key(&binding.device_id)
        {
            return Err(CallSwitchRejection::Unavailable);
        }
        let active = self
            .devices
            .get(&binding.device_id)
            .and_then(|device| device.active_call);
        if active.is_some_and(|call_id| {
            self.conference_session(call_id).is_some()
                || self.barges.by_handset.contains_key(&call_id)
        }) {
            return Err(CallSwitchRejection::Conflict);
        }
        let mut effects = active
            .filter(|call_id| {
                self.call_state(*call_id).is_some_and(|state| {
                    matches!(
                        state,
                        CallState::Collecting
                            | CallState::Calling
                            | CallState::Connected
                            | CallState::TransferCollecting
                    )
                })
            })
            .map_or_else(Vec::new, |call_id| self.hold(call_id));
        let created = self.begin_phone_call(sccp_id, binding, codec, now);
        if created.is_empty() {
            return Err(CallSwitchRejection::Unavailable);
        }
        effects.extend(created);
        Ok(effects)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn begin_additional_phone_call_transaction(
        &mut self,
        sccp_id: CallId,
        binding: LineBinding,
        codec: Codec,
        now: Instant,
    ) -> Result<CallTransition, CallSwitchRejection> {
        if self
            .pending_call_transitions
            .values()
            .any(|pending| pending.transition.device_id == binding.device_id)
        {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous_call_id = self
            .devices
            .get(&binding.device_id)
            .and_then(|device| device.active_call);
        if previous_call_id.is_some_and(|previous| {
            self.call_state(previous)
                .is_none_or(|state| state != CallState::Connected)
        }) {
            return Err(CallSwitchRejection::Conflict);
        }
        let previous_pbx_id = previous_call_id
            .and_then(|previous| self.appearance_for_call(previous))
            .map(|appearance| appearance.pbx_id);
        let device_id = binding.device_id.clone();
        let snapshot = self.call_domain_snapshot();
        let effects = self.begin_additional_phone_call(sccp_id, binding, codec, now)?;
        let target_pbx_id = self
            .appearance_for_call(sccp_id)
            .map(|appearance| appearance.pbx_id)
            .ok_or(CallSwitchRejection::Unavailable)?;
        let transition = CallTransition {
            id: self.allocate_call_transition_id(),
            effects,
            device_id,
            target_call_id: sccp_id,
            target_pbx_id,
            previous_call_id,
            previous_pbx_id,
            kind: CallTransitionKind::Additional,
            auto_answer_mode: None,
        };
        self.pending_call_transitions.insert(
            transition.id,
            PendingCallTransition {
                transition: transition.clone(),
                snapshot,
                progress: CallTransitionProgress::default(),
            },
        );
        Ok(transition)
    }

    /// Begin a configured PLAR/hotline call as the same rollback-safe
    /// additional-call transaction used by ordinary NewCall, but route the
    /// captured destination immediately without exposing digit collection or
    /// a dial tone to the handset.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn begin_hotline_call_transaction(
        &mut self,
        request: HotlineCallRequest,
    ) -> Result<CallTransition, CallSwitchRejection> {
        let destination = request.destination.as_str().to_owned();
        let mut transition = self.begin_additional_phone_call_transaction(
            request.handset_call_id,
            request.binding,
            request.codec,
            request.now,
        )?;
        let routing = self.enbloc(request.handset_call_id, destination);
        if !matches!(
            routing.last(),
            Some(DriverEffect::Backend(PbxEffect::StartRouting { .. }))
        ) {
            let _ = self.abort_call_transition(transition.id, &CallTransitionProgress::default());
            return Err(CallSwitchRejection::Conflict);
        }
        transition.effects.retain(|effect| {
            !matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::StartTone { call_id, .. })
                    if *call_id == request.handset_call_id
            )
        });
        transition.effects.extend(routing);
        let Some(pending) = self.pending_call_transitions.get_mut(&transition.id) else {
            return Err(CallSwitchRejection::Conflict);
        };
        pending.transition.effects.clone_from(&transition.effects);
        debug_assert!(self.invariant_error().is_none());
        Ok(transition)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn commit_call_transition(&mut self, id: CallTransitionId) -> bool {
        let Some(pending) = self.pending_call_transitions.remove(&id) else {
            return false;
        };
        if let Some(mode) = pending.transition.auto_answer_mode
            && let Some(appearance_id) = self
                .call_registry
                .by_sccp
                .get(&pending.transition.target_call_id)
                .copied()
            && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
        {
            appearance.auto_answer_mode = Some(mode);
        }
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn record_call_transition_success(
        &mut self,
        id: CallTransitionId,
        effect: &DriverEffect,
    ) -> bool {
        let Some(pending) = self.pending_call_transitions.get_mut(&id) else {
            return false;
        };
        pending.progress.record_success(&pending.transition, effect);
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn abort_call_transition(
        &mut self,
        id: CallTransitionId,
        progress: &CallTransitionProgress,
    ) -> Vec<DriverEffect> {
        let Some(pending) = self.pending_call_transitions.remove(&id) else {
            return Vec::new();
        };
        let transition = pending.transition;
        if !self.restore_call_domain(&pending.snapshot, &transition) {
            return Vec::new();
        }
        let mut effects = Vec::new();
        match transition.kind {
            CallTransitionKind::Additional => {
                if progress.completed(CallTransitionMilestone::TargetBackendStarted) {
                    effects.push(
                        PbxEffect::Hangup {
                            call_id: transition.target_pbx_id,
                        }
                        .into(),
                    );
                }
                effects.push(
                    HandsetEffect::SetCallState {
                        device_id: transition.device_id.clone(),
                        call_id: transition.target_call_id,
                        state: HandsetCallState::OnHook,
                        stop_media: true,
                    }
                    .into(),
                );
            }
            CallTransitionKind::Switch(CallState::Ringing)
                if progress.completed(CallTransitionMilestone::TargetBackendStarted) =>
            {
                effects.push(
                    PbxEffect::Hangup {
                        call_id: transition.target_pbx_id,
                    }
                    .into(),
                );
                if let Some(mut outcome) = self.pbx_hangup_with_effects(transition.target_pbx_id) {
                    effects.append(&mut outcome.effects);
                }
            }
            CallTransitionKind::Switch(CallState::Held | CallState::SharedHeld)
                if progress.completed(CallTransitionMilestone::TargetBackendStarted) =>
            {
                effects.push(
                    PbxEffect::Hold {
                        call_id: transition.target_pbx_id,
                    }
                    .into(),
                );
                if progress.completed(CallTransitionMilestone::TargetHandsetChanged) {
                    effects.push(
                        HandsetEffect::SetCallState {
                            device_id: transition.device_id.clone(),
                            call_id: transition.target_call_id,
                            state: HandsetCallState::Hold,
                            stop_media: true,
                        }
                        .into(),
                    );
                }
            }
            CallTransitionKind::Switch(_) => {}
        }
        if progress.completed(CallTransitionMilestone::TargetMicrophoneDisabled) {
            effects.push(
                HandsetEffect::SetMicrophoneMode {
                    device_id: transition.device_id.clone(),
                    call_id: transition.target_call_id,
                    enabled: true,
                }
                .into(),
            );
        }
        if progress.completed(CallTransitionMilestone::PreviousBackendHeld)
            && let Some(call_id) = transition.previous_pbx_id
        {
            effects.push(PbxEffect::Resume { call_id }.into());
        }
        if progress.completed(CallTransitionMilestone::PreviousHandsetHeld)
            && let Some(previous_call_id) = transition.previous_call_id
            && let Some(previous) = self.appearance_for_call(previous_call_id).cloned()
        {
            effects.push(appearance_state_effect(
                &previous,
                HandsetCallState::Connected,
                false,
            ));
            if previous.state == CallState::Connected {
                effects.push(
                    HandsetEffect::BeginMedia {
                        device_id: previous.device_id,
                        call_id: previous.sccp_id,
                        codec: previous.codec,
                    }
                    .into(),
                );
            }
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Compensate one external effect that completed after a lifecycle event
    /// had already cancelled its transition. The controller has already
    /// restored or removed the affected calls, so this applies only the exact
    /// inverse still meaningful against the surviving call domain.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn compensate_unrecorded_call_transition_effect(
        &mut self,
        transition: &CallTransition,
        effect: &DriverEffect,
    ) -> CallTransitionCompensation {
        if self.pending_call_transitions.contains_key(&transition.id) {
            return CallTransitionCompensation::default();
        }

        let mut compensation = CallTransitionCompensation::default();
        match effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id })
                if Some(*call_id) == transition.previous_pbx_id
                    && self.call_registry.pbx.contains_key(call_id) =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Resume { call_id: *call_id }.into());
            }
            DriverEffect::Backend(PbxEffect::CreateChannel { call_id, .. })
                if *call_id == transition.target_pbx_id =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hangup { call_id: *call_id }.into());
                compensation.remove_target_channel = true;
            }
            DriverEffect::Backend(PbxEffect::StartRouting { call_id, .. })
                if *call_id == transition.target_pbx_id =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hangup { call_id: *call_id }.into());
                compensation.remove_target_channel = true;
            }
            DriverEffect::Backend(PbxEffect::Answer { call_id })
                if *call_id == transition.target_pbx_id
                    && self.call_registry.pbx.contains_key(call_id) =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hangup { call_id: *call_id }.into());
                if let Some(mut outcome) = self.pbx_hangup_with_effects(*call_id) {
                    compensation.effects.append(&mut outcome.effects);
                }
                compensation.remove_target_channel = true;
            }
            DriverEffect::Backend(PbxEffect::Resume { call_id })
                if *call_id == transition.target_pbx_id
                    && self.call_registry.pbx.contains_key(call_id) =>
            {
                compensation
                    .effects
                    .push(PbxEffect::Hold { call_id: *call_id }.into());
            }
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id,
                state: HandsetCallState::Hold,
                ..
            }) if Some(*call_id) == transition.previous_call_id => {
                if let Some(previous) = self.appearance_for_call(*call_id).cloned() {
                    compensation.effects.push(appearance_state_effect(
                        &previous,
                        HandsetCallState::Connected,
                        false,
                    ));
                    if previous.state == CallState::Connected {
                        compensation.effects.push(
                            HandsetEffect::BeginMedia {
                                device_id: previous.device_id,
                                call_id: previous.sccp_id,
                                codec: previous.codec,
                            }
                            .into(),
                        );
                    }
                }
            }
            DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                device_id,
                call_id,
                enabled: false,
            }) if *call_id == transition.target_call_id => {
                compensation.effects.push(
                    HandsetEffect::SetMicrophoneMode {
                        device_id: device_id.clone(),
                        call_id: *call_id,
                        enabled: true,
                    }
                    .into(),
                );
            }
            DriverEffect::Handset(handset)
                if handset.transition_call_id() == Some(transition.target_call_id) =>
            {
                compensation
                    .effects
                    .extend(self.restored_target_handset_effects(transition));
            }
            _ => {}
        }
        compensation
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn abort_call_transitions_for_pbx(
        &mut self,
        pbx_id: PbxCallId,
    ) -> Vec<DriverEffect> {
        let pending = self
            .pending_call_transitions
            .iter()
            .filter(|(_, pending)| {
                pending.transition.target_pbx_id == pbx_id
                    || pending.transition.previous_pbx_id == Some(pbx_id)
            })
            .map(|(id, pending)| (*id, pending.progress.clone()))
            .collect::<Vec<_>>();
        let mut effects = Vec::new();
        for (id, progress) in pending {
            effects.extend(
                self.abort_call_transition(id, &progress)
                    .into_iter()
                    .filter(|effect| {
                        !matches!(
                            effect,
                            DriverEffect::Backend(
                                PbxEffect::Hold { call_id }
                                    | PbxEffect::Resume { call_id }
                                    | PbxEffect::Answer { call_id }
                                    | PbxEffect::Hangup { call_id }
                            )
                                if *call_id == pbx_id
                        )
                    }),
            );
        }
        effects
    }

    pub fn begin_asterisk_call(
        &mut self,
        sccp_id: CallId,
        pbx_id: PbxCallId,
        binding: &LineBinding,
        codec: Codec,
    ) {
        if self.call_registry.by_sccp.contains_key(&sccp_id)
            || self.call_registry.pbx.contains_key(&pbx_id)
        {
            return;
        }
        self.next_pbx_id = self.next_pbx_id.max(pbx_id.0.saturating_add(1));
        let appearance_id = self.allocate_appearance_id();
        let info = CallInfo {
            direction: CallDirection::Inbound,
            called_name: binding.appearance.display_label().to_owned(),
            called_number: binding.line.number.clone(),
            ..CallInfo::default()
        };
        let pbx_call = PbxCall {
            id: pbx_id,
            line: binding.line.number.clone(),
            context: binding.line.context.clone(),
            direction: CallDirection::Inbound,
            state: CallState::Ringing,
            outbound_phase: None,
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy: false,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: None,
            digit_deadline: None,
            last_digit_at: None,
            simulated_enbloc_eligible: false,
            overlap_enabled: false,
        };
        let appearance = CallAppearance {
            id: appearance_id,
            sccp_id,
            pbx_id,
            device_id: binding.device_id.clone(),
            line_instance: binding.line_instance,
            state: CallState::Ringing,
            ring_mode: binding.appearance.ring_mode,
            privacy: binding.appearance.privacy,
            info,
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        let inserted = self.insert_pbx_call(pbx_call, appearance);
        debug_assert!(inserted);
        debug_assert!(self.invariant_error().is_none());
    }

    /// Build every currently eligible handset presentation for one inbound
    /// PBX call. Candidate order is preserved so presentation and fallback
    /// ownership are stable across runs.
    pub fn offer_inbound_call(
        &mut self,
        pbx_id: PbxCallId,
        candidates: impl IntoIterator<Item = InboundAppearance>,
    ) -> Vec<InboundOffer> {
        match self.offer_inbound_call_with_policy(pbx_id, candidates) {
            InboundCallDisposition::Offer(offers) => offers,
            InboundCallDisposition::Forward { .. } | InboundCallDisposition::Unavailable(_) => {
                Vec::new()
            }
        }
    }

    /// Apply per-device DND and forwarding before creating the shared handset
    /// presentations for an inbound PBX call.
    ///
    /// A forwarding or rejecting appearance never suppresses a different
    /// appearance that can still ring. A PBX-level forward is selected only
    /// when no handset remains and every forwarding appearance agrees on the
    /// destination.
    pub fn offer_inbound_call_with_policy(
        &mut self,
        pbx_id: PbxCallId,
        candidates: impl IntoIterator<Item = InboundAppearance>,
    ) -> InboundCallDisposition {
        if self.call_registry.pbx.contains_key(&pbx_id) {
            return InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict);
        }
        let candidates: Vec<_> = candidates.into_iter().collect();
        let Some(line) = candidates
            .first()
            .map(|candidate| candidate.binding.line.number.clone())
        else {
            return InboundCallDisposition::Unavailable(
                InboundUnavailableReason::NoEligibleAppearance,
            );
        };
        let mut seen_calls = HashSet::new();
        let mut seen_buttons = HashSet::new();
        let mut structural_exclusions = 0_usize;
        let mut eligible = Vec::new();
        for candidate in candidates {
            let button = (
                candidate.binding.device_id.clone(),
                candidate.binding.line_instance,
            );
            if candidate.binding.line.number != line
                || !self.devices.contains_key(&candidate.binding.device_id)
                || candidate.binding.appearance.ring_mode == AppearanceRingMode::Disabled
                || self.call_registry.by_sccp.contains_key(&candidate.call_id)
                || !seen_calls.insert(candidate.call_id)
                || !seen_buttons.insert(button)
            {
                structural_exclusions += 1;
                continue;
            }
            eligible.push(candidate);
        }
        let mut ringable = Vec::new();
        let mut forwarded = Vec::new();
        let eligible_count = eligible.len();
        let mut dnd_rejected = 0_usize;
        for mut candidate in eligible {
            let features = self
                .features
                .get(&candidate.binding.device_id)
                .cloned()
                .unwrap_or_default();
            match features.dnd {
                DndMode::Reject => {
                    dnd_rejected += 1;
                    continue;
                }
                DndMode::Silent => {
                    candidate.binding.appearance.ring_mode = AppearanceRingMode::Silent;
                }
                DndMode::Off => {}
            }
            let route = features
                .forwarding
                .all
                .map(|destination| (destination, ForwardingRouteReason::Unconditional))
                .or_else(|| {
                    self.device_is_busy(&candidate.binding.device_id)
                        .then_some(features.forwarding.busy)
                        .flatten()
                        .map(|destination| (destination, ForwardingRouteReason::Busy))
                });
            if let Some((destination, reason)) = route {
                forwarded.push((candidate, destination, reason));
            } else {
                ringable.push(candidate);
            }
        }

        let incoming_limit = self.line_incoming_limits.get(&line).copied().unwrap_or(6);
        let incoming_calls = self
            .call_registry
            .pbx
            .values()
            .filter(|call| call.direction == CallDirection::Inbound && call.line == line)
            .count();

        if ringable.is_empty() {
            let Some((first, destination, reason)) = forwarded.first() else {
                let dnd_only = structural_exclusions == 0
                    && eligible_count != 0
                    && dnd_rejected == eligible_count;
                let reason = if dnd_only && incoming_calls >= incoming_limit as usize {
                    InboundUnavailableReason::IncomingLimit
                } else if dnd_only {
                    InboundUnavailableReason::DoNotDisturb
                } else {
                    InboundUnavailableReason::NoEligibleAppearance
                };
                return InboundCallDisposition::Unavailable(reason);
            };
            if forwarded
                .iter()
                .any(|(_, candidate_destination, candidate_reason)| {
                    candidate_destination != destination || candidate_reason != reason
                })
            {
                return InboundCallDisposition::Unavailable(
                    InboundUnavailableReason::ForwardingConflict,
                );
            }
            return InboundCallDisposition::Forward {
                binding: Box::new(first.binding.clone()),
                destination: destination.clone(),
                reason: *reason,
            };
        }

        if incoming_calls >= incoming_limit as usize {
            return InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit);
        }

        let Some(first) = ringable.first() else {
            return InboundCallDisposition::Unavailable(
                InboundUnavailableReason::NoEligibleAppearance,
            );
        };

        self.next_pbx_id = self.next_pbx_id.max(pbx_id.0.saturating_add(1));
        let first_appearance_id = self.allocate_appearance_id();
        let call = PbxCall {
            id: pbx_id,
            line,
            context: first.binding.line.context.clone(),
            direction: CallDirection::Inbound,
            state: CallState::Ringing,
            outbound_phase: None,
            outbound_identity_stage: OutboundIdentityStage::Awaiting,
            digits: String::new(),
            privacy: false,
            metadata: CallMetadata::default(),
            pending_pickup: None,
            appearance_ids: Vec::new(),
            active_appearance: None,
            digit_deadline: None,
            last_digit_at: None,
            simulated_enbloc_eligible: false,
            overlap_enabled: false,
        };
        let first_appearance = inbound_call_appearance(first_appearance_id, pbx_id, first);
        if !self.insert_pbx_call(call, first_appearance) {
            return InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict);
        }

        let mut offers = vec![self.inbound_offer(first)];
        for candidate in &ringable[1..] {
            let appearance_id = self.allocate_appearance_id();
            if self.attach_appearance(inbound_call_appearance(appearance_id, pbx_id, candidate)) {
                offers.push(self.inbound_offer(candidate));
            }
        }
        debug_assert!(self.invariant_error().is_none());
        InboundCallDisposition::Offer(offers)
    }

    /// Starts the configured call-waiting tone for one successfully queued
    /// inbound presentation. Repeats retain the policy captured here so a
    /// reload affects only later waiting calls.
    pub fn start_call_waiting_tone(
        &mut self,
        waiting_call_id: CallId,
        tone: Option<Tone>,
        interval: Duration,
        now: Instant,
    ) -> Vec<DriverEffect> {
        self.call_waiting_tones.remove(&waiting_call_id);
        let Some(tone) = tone else {
            return Vec::new();
        };
        let Some(waiting) = self.appearance_for_call(waiting_call_id) else {
            return Vec::new();
        };
        if waiting.state != CallState::Ringing || waiting.ring_mode != AppearanceRingMode::Normal {
            return Vec::new();
        }
        let device_id = waiting.device_id.clone();
        let Some(active_call_id) = self
            .devices
            .get(&device_id)
            .and_then(|device| device.active_call)
            .filter(|active| *active != waiting_call_id)
        else {
            return Vec::new();
        };
        if !self
            .appearance_for_call(active_call_id)
            .is_some_and(|active| {
                matches!(
                    active.state,
                    CallState::Collecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::TransferCollecting
                )
            })
        {
            return Vec::new();
        }
        if !interval.is_zero() {
            self.call_waiting_tones.insert(
                waiting_call_id,
                CallWaitingToneSchedule {
                    device_id: device_id.clone(),
                    waiting_call_id,
                    active_call_id,
                    tone,
                    interval,
                    next_at: now + interval,
                },
            );
        }
        vec![
            HandsetEffect::StartTone {
                device_id,
                call_id: active_call_id,
                tone,
            }
            .into(),
        ]
    }

    /// Emits every due repeat in deterministic waiting-call order. Invalid or
    /// completed schedules are discarded before any handset effect escapes.
    pub fn expire_call_waiting_tones(&mut self, now: Instant) -> Vec<DriverEffect> {
        let mut call_ids = self.call_waiting_tones.keys().copied().collect::<Vec<_>>();
        call_ids.sort_by_key(|call_id| call_id.0);
        let mut effects = Vec::new();
        for waiting_call_id in call_ids {
            let Some(schedule) = self.call_waiting_tones.get(&waiting_call_id).cloned() else {
                continue;
            };
            let valid = self
                .appearance_for_call(schedule.waiting_call_id)
                .is_some_and(|appearance| appearance.state == CallState::Ringing)
                && self
                    .devices
                    .get(&schedule.device_id)
                    .is_some_and(|device| device.active_call == Some(schedule.active_call_id))
                && self
                    .appearance_for_call(schedule.active_call_id)
                    .is_some_and(|appearance| {
                        matches!(
                            appearance.state,
                            CallState::Collecting
                                | CallState::Calling
                                | CallState::Connected
                                | CallState::TransferCollecting
                        )
                    });
            if !valid {
                self.call_waiting_tones.remove(&waiting_call_id);
                continue;
            }
            if schedule.next_at > now {
                continue;
            }
            effects.push(
                HandsetEffect::StartTone {
                    device_id: schedule.device_id,
                    call_id: schedule.active_call_id,
                    tone: schedule.tone,
                }
                .into(),
            );
            if let Some(active) = self.call_waiting_tones.get_mut(&waiting_call_id) {
                active.next_at = now + active.interval;
            }
        }
        effects
    }

    pub fn cancel_call_waiting_tone(&mut self, waiting_call_id: CallId) -> bool {
        self.call_waiting_tones.remove(&waiting_call_id).is_some()
    }

    /// Attach another handset presentation to an existing PBX call.
    ///
    /// Current call setup creates one appearance. Shared-line routing can use
    /// this operation to fan the same PBX identity out to additional devices.
    pub fn add_call_appearance(
        &mut self,
        pbx_id: PbxCallId,
        sccp_id: CallId,
        binding: &LineBinding,
        codec: Codec,
    ) -> Option<CallAppearanceId> {
        let (state, active_appearance, direction, mut info) = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .filter(|call| call.line == binding.line.number)
            .and_then(|call| {
                let info = call
                    .appearance_ids
                    .first()
                    .and_then(|id| self.call_registry.appearances.get(id))?
                    .info
                    .clone();
                Some((call.state, call.active_appearance, call.direction, info))
            })?;
        if self.call_registry.by_sccp.contains_key(&sccp_id) {
            return None;
        }
        let id = self.allocate_appearance_id();
        let state = shared_appearance_state(state, active_appearance.is_some());
        if direction == CallDirection::Inbound {
            info.called_name = binding.appearance.display_label().to_owned();
            info.called_number.clone_from(&binding.line.number);
        }
        let appearance = CallAppearance {
            id,
            sccp_id,
            pbx_id,
            device_id: binding.device_id.clone(),
            line_instance: binding.line_instance,
            state,
            ring_mode: binding.appearance.ring_mode,
            privacy: binding.appearance.privacy,
            info,
            codec,
            audio: MediaStreamState::Closed,
            audio_transmit: MediaStreamState::Closed,
            video: VideoMediaState::default(),
            auto_answer_mode: None,
        };
        self.attach_appearance(appearance).then(|| {
            debug_assert!(self.invariant_error().is_none());
            id
        })
    }

    pub fn digit(&mut self, call_id: CallId, digit: Digit, now: Instant) -> Vec<DriverEffect> {
        let Some(character) = digit_character(digit) else {
            return Vec::new();
        };
        let Some((appearance_state, appearance_pbx_id, device_id)) =
            self.appearance_for_call(call_id).map(|appearance| {
                (
                    appearance.state,
                    appearance.pbx_id,
                    appearance.device_id.clone(),
                )
            })
        else {
            return Vec::new();
        };
        let pbx_id = self
            .barges
            .by_handset
            .get(&call_id)
            .map_or(appearance_pbx_id, |barge| barge.barger_call_id);
        match appearance_state {
            CallState::Collecting | CallState::PickupCollecting | CallState::TransferCollecting => {
                if character == self.dial_terminator {
                    return self.finish_digits(call_id);
                }
                let (overlap, secondary_tone) = {
                    let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
                        return Vec::new();
                    };
                    if let Some(previous) = call.last_digit_at
                        && now.saturating_duration_since(previous) > Duration::from_millis(400)
                    {
                        call.simulated_enbloc_eligible = false;
                    }
                    call.last_digit_at = Some(now);
                    call.digits.push(character);
                    let overlap = if appearance_state == CallState::Collecting
                        && call.overlap_enabled
                        && call.digits.len() == 1
                    {
                        call.state = CallState::Calling;
                        call.digit_deadline = None;
                        Some((call.context.clone(), call.digits.clone()))
                    } else {
                        let timeout = if call.simulated_enbloc_eligible && call.digits.len() >= 4 {
                            self.interdigit.min(Duration::from_secs(2))
                        } else {
                            self.interdigit
                        };
                        call.digit_deadline = (appearance_state != CallState::PickupCollecting)
                            .then_some(now + timeout);
                        None
                    };
                    let secondary_tone = self
                        .line_dial_tones
                        .get(&call.line)
                        .filter(|dial_tones| {
                            dial_tones.secondary_prefix.as_deref() == Some(call.digits.as_str())
                        })
                        .map(|dial_tones| dial_tones.secondary);
                    (overlap, secondary_tone)
                };
                if let Some((context, destination)) = overlap {
                    if let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied()
                        && let Some(appearance) =
                            self.call_registry.appearances.get_mut(&appearance_id)
                    {
                        appearance.state = CallState::Calling;
                    }
                    debug_assert!(self.invariant_error().is_none());
                    let mut effects = Vec::new();
                    if let Some(tone) = secondary_tone {
                        effects.push(
                            HandsetEffect::StartTone {
                                device_id,
                                call_id,
                                tone,
                            }
                            .into(),
                        );
                    }
                    effects.extend(self.outbound_route_presentation(pbx_id, &destination));
                    effects.push(
                        PbxEffect::StartRouting {
                            call_id: pbx_id,
                            context,
                            destination,
                        }
                        .into(),
                    );
                    return effects;
                }
                debug_assert!(self.invariant_error().is_none());
                secondary_tone
                    .map(|tone| {
                        vec![
                            HandsetEffect::StartTone {
                                device_id,
                                call_id,
                                tone,
                            }
                            .into(),
                        ]
                    })
                    .unwrap_or_default()
            }
            CallState::Calling
                if self
                    .call_registry
                    .pbx
                    .get(&pbx_id)
                    .is_some_and(|call| call.overlap_enabled) =>
            {
                vec![
                    PbxEffect::SendDigit {
                        call_id: pbx_id,
                        digit: character,
                    }
                    .into(),
                ]
            }
            CallState::Connected | CallState::Held => vec![
                PbxEffect::SendDigit {
                    call_id: pbx_id,
                    digit: character,
                }
                .into(),
            ],
            _ => Vec::new(),
        }
    }

    /// Applies a configured speed-dial destination to an outbound digit
    /// collector. Immediate mode follows the ordinary en-bloc path. Awaiting
    /// mode seeds the collector without triggering overlap routing and uses
    /// the normal interdigit deadline for any manually appended digits.
    pub fn speed_dial(
        &mut self,
        call_id: CallId,
        number: String,
        await_further_digits: bool,
        now: Instant,
    ) -> Vec<DriverEffect> {
        if !await_further_digits {
            return self.enbloc(call_id, number);
        }
        let Some((pbx_id, state)) = self
            .appearance_for_call(call_id)
            .map(|appearance| (appearance.pbx_id, appearance.state))
        else {
            return Vec::new();
        };
        if !matches!(state, CallState::Collecting | CallState::TransferCollecting) {
            return Vec::new();
        }
        let Some(mut digits) = number
            .chars()
            .map(|character| match character {
                '0'..='9' | '*' | '#' | 'A'..='D' => Some(character),
                'a'..='d' => Some(character.to_ascii_uppercase()),
                _ => None,
            })
            .collect::<Option<String>>()
        else {
            return Vec::new();
        };
        let terminated = digits.ends_with(self.dial_terminator);
        if terminated {
            digits.pop();
        }
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return Vec::new();
        };
        call.digits = digits;
        call.last_digit_at = Some(now);
        call.simulated_enbloc_eligible = false;
        call.digit_deadline = Some(now + self.interdigit);
        if terminated {
            self.finish_digits(call_id)
        } else {
            debug_assert!(self.invariant_error().is_none());
            Vec::new()
        }
    }

    pub fn expire_digits(&mut self, now: Instant) -> Vec<DriverEffect> {
        let expired: Vec<_> = self
            .call_registry
            .pbx
            .values()
            .filter(|call| call.digit_deadline.is_some_and(|deadline| deadline <= now))
            .filter_map(|call| {
                call.appearance_ids
                    .first()
                    .and_then(|id| self.call_registry.appearances.get(id))
                    .map(|appearance| appearance.sccp_id)
            })
            .collect();
        expired
            .into_iter()
            .flat_map(|call| self.finish_digits(call))
            .collect()
    }

    pub fn phone_answer(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(winner) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&winner.pbx_id) else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&winner.pbx_id)
            || call.state != CallState::Ringing
            || call.active_appearance.is_some()
            || winner.state != CallState::Ringing
        {
            return Vec::new();
        }
        let appearance_ids = call.appearance_ids.clone();
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        self.cancel_auto_answers_for_pbx(winner.pbx_id);
        self.call_waiting_tones.remove(&call_id);
        let previous_active = self
            .devices
            .get(&winner.device_id)
            .and_then(|device| device.active_call)
            .filter(|active| *active != call_id);
        if previous_active.is_some_and(|active| {
            self.call_state(active).is_some_and(|state| {
                matches!(
                    state,
                    CallState::Collecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::TransferCollecting
                )
            }) && (self.conference_session(active).is_some()
                || self.barges.by_handset.contains_key(&active))
        }) {
            return Vec::new();
        }
        let winner_privacy = winner.privacy
            || self
                .features
                .get(&winner.device_id)
                .is_some_and(|state| state.privacy);
        let mut effects = previous_active.map_or_else(Vec::new, |active| self.hold(active));
        if let Some(call) = self.call_registry.pbx.get_mut(&winner.pbx_id) {
            call.state = CallState::Connected;
            call.active_appearance = Some(winner.id);
            call.privacy |= winner_privacy;
        }
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == winner.id {
                appearance.state = CallState::Connected;
            } else {
                appearance.state = CallState::RemoteInUse;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::RemoteMultiline,
                    false,
                ));
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        effects.push(
            HandsetEffect::BeginAnswerMedia {
                device_id: winner.device_id.clone(),
                call_id: winner.sccp_id,
                codec: winner.codec,
            }
            .into(),
        );
        self.pending_phone_answers
            .insert(winner.sccp_id, winner.pbx_id);
        self.select_line(&winner.device_id, winner.line_instance);
        self.set_active_call(&winner.device_id, Some(call_id));
        self.set_call_selected(&winner.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn pbx_answer(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        if self.redirect_claims.contains(&pbx_id) {
            return Vec::new();
        }
        let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
            return Vec::new();
        };
        if call.state == CallState::Connected && call.active_appearance.is_some()
            || call
                .outbound_phase
                .is_some_and(|phase| phase >= OutboundCallPhase::Answered)
        {
            return Vec::new();
        }
        let appearance_ids = call.appearance_ids.clone();
        let Some(winner_id) = call
            .active_appearance
            .or_else(|| appearance_ids.first().copied())
        else {
            return Vec::new();
        };
        let winner_privacy =
            self.call_registry
                .appearances
                .get(&winner_id)
                .is_some_and(|appearance| {
                    appearance.privacy
                        || self
                            .features
                            .get(&appearance.device_id)
                            .is_some_and(|state| state.privacy)
                });
        let winner_direction = self
            .call_registry
            .appearances
            .get(&winner_id)
            .map(|appearance| appearance.info.direction);
        let media_state = self.call_registry.appearances.get(&winner_id).map_or(
            MediaStreamState::Closed,
            |appearance| {
                if self.pending_route_media.contains(&appearance.sccp_id) {
                    MediaStreamState::Opening
                } else {
                    appearance.audio
                }
            },
        );
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.state = CallState::Connected;
            call.active_appearance = Some(winner_id);
            call.privacy |= winner_privacy;
            if call.direction == CallDirection::Outbound && call.outbound_phase.is_some() {
                call.outbound_phase = Some(OutboundCallPhase::Answered);
            }
        }
        let mut effects = Vec::new();
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == winner_id {
                appearance.state = CallState::Connected;
                if appearance.audio == MediaStreamState::Closed {
                    appearance.audio = MediaStreamState::Opening;
                }
            } else {
                appearance.state = CallState::RemoteInUse;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::RemoteMultiline,
                    false,
                ));
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        let Some(winner) = self.call_registry.appearances.get(&winner_id).cloned() else {
            return Vec::new();
        };
        self.select_line(&winner.device_id, winner.line_instance);
        self.set_active_call(&winner.device_id, Some(winner.sccp_id));
        self.set_call_selected(&winner.device_id, winner.sccp_id, true);
        self.advance_transfer_for_pbx(pbx_id, TransferPhase::Connected);
        debug_assert!(self.invariant_error().is_none());
        match media_state {
            MediaStreamState::Open(_) => effects.push(
                HandsetEffect::SetCallState {
                    device_id: winner.device_id,
                    call_id: winner.sccp_id,
                    state: HandsetCallState::Connected,
                    stop_media: false,
                }
                .into(),
            ),
            MediaStreamState::Opening => {
                if winner_direction == Some(CallDirection::Outbound) {
                    effects.push(
                        HandsetEffect::SetCallState {
                            device_id: winner.device_id,
                            call_id: winner.sccp_id,
                            state: HandsetCallState::Connected,
                            stop_media: false,
                        }
                        .into(),
                    );
                }
            }
            MediaStreamState::Closed => effects.push(
                if winner_direction == Some(CallDirection::Outbound) {
                    HandsetEffect::BeginMedia {
                        device_id: winner.device_id,
                        call_id: winner.sccp_id,
                        codec: winner.codec,
                    }
                } else {
                    HandsetEffect::BeginAnswerMedia {
                        device_id: winner.device_id,
                        call_id: winner.sccp_id,
                        codec: winner.codec,
                    }
                }
                .into(),
            ),
        }
        effects
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn set_auto_answer_request(
        &mut self,
        pbx_id: PbxCallId,
        request: AutoAnswerRequest,
    ) -> bool {
        if self
            .call_registry
            .pbx
            .get(&pbx_id)
            .is_none_or(|call| call.state != CallState::Ringing)
        {
            return false;
        }
        self.auto_answer_requests.insert(pbx_id, request);
        true
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn has_auto_answer_request(&self, pbx_id: PbxCallId) -> bool {
        self.auto_answer_requests.contains_key(&pbx_id)
    }

    /// Capture the current normalized delay/tone only after the adapter has
    /// successfully queued the inbound presentation. Each eligible shared
    /// appearance receives an independent generation; the first valid due
    /// generation claims the PBX call and cancels its peers.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn schedule_auto_answers(
        &mut self,
        pbx_id: PbxCallId,
        policy: AutoAnswerPolicy,
        now: Instant,
    ) -> Result<usize, AutoAnswerScheduleRejection> {
        let Some(request) = self.auto_answer_requests.remove(&pbx_id) else {
            return Err(AutoAnswerScheduleRejection::Unavailable);
        };
        self.cancel_auto_answers_for_pbx(pbx_id);
        let mut call_ids = self
            .appearances_for_pbx(pbx_id)
            .filter(|appearance| appearance.state == CallState::Ringing)
            .filter(|appearance| {
                self.device_can_auto_answer(&appearance.device_id, appearance.sccp_id)
            })
            .map(|appearance| appearance.sccp_id)
            .collect::<Vec<_>>();
        call_ids.sort_by_key(|call_id| call_id.0);
        let count = u64::try_from(call_ids.len())
            .map_err(|_| AutoAnswerScheduleRejection::GenerationExhausted)?;
        let next_generation = self
            .next_auto_answer_generation
            .checked_add(count)
            .ok_or(AutoAnswerScheduleRejection::GenerationExhausted)?;
        for call_id in &call_ids {
            let generation = self.next_auto_answer_generation;
            self.next_auto_answer_generation += 1;
            self.pending_auto_answers.insert(
                *call_id,
                PendingAutoAnswer {
                    generation,
                    pbx_id,
                    call_id: *call_id,
                    deadline: now + policy.delay,
                    request,
                    tone: policy.tone,
                },
            );
        }
        debug_assert_eq!(self.next_auto_answer_generation, next_generation);
        Ok(call_ids.len())
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn expire_auto_answers(&mut self, now: Instant) -> Vec<CallTransition> {
        let mut due = self
            .pending_auto_answers
            .values()
            .copied()
            .filter(|pending| pending.deadline <= now)
            .collect::<Vec<_>>();
        due.sort_by_key(|pending| (pending.deadline, pending.generation, pending.call_id.0));
        let mut transitions = Vec::new();
        for pending in due {
            if self
                .pending_auto_answers
                .get(&pending.call_id)
                .is_none_or(|current| current.generation != pending.generation)
            {
                continue;
            }
            self.pending_auto_answers.remove(&pending.call_id);
            let Ok(mut transition) =
                self.begin_active_call_switch_transaction_for_auto_answer(pending)
            else {
                continue;
            };
            transition.effects.push(
                HandsetEffect::StartTone {
                    device_id: transition.device_id.clone(),
                    call_id: transition.target_call_id,
                    tone: pending.tone,
                }
                .into(),
            );
            if transition.auto_answer_mode == Some(AutoAnswerMode::OneWay) {
                transition.effects.push(
                    HandsetEffect::SetMicrophoneMode {
                        device_id: transition.device_id.clone(),
                        call_id: transition.target_call_id,
                        enabled: false,
                    }
                    .into(),
                );
            }
            if let Some(stored) = self.pending_call_transitions.get_mut(&transition.id) {
                stored.transition.clone_from(&transition);
            }
            transitions.push(transition);
        }
        transitions
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn begin_active_call_switch_transaction_for_auto_answer(
        &mut self,
        pending: PendingAutoAnswer,
    ) -> Result<CallTransition, CallSwitchRejection> {
        let appearance = self
            .appearance_for_call(pending.call_id)
            .ok_or(CallSwitchRejection::Unavailable)?;
        if appearance.pbx_id != pending.pbx_id || appearance.state != CallState::Ringing {
            return Err(CallSwitchRejection::Unavailable);
        }
        let device_id = appearance.device_id.clone();
        if !self.device_can_auto_answer(&device_id, pending.call_id) {
            return Err(CallSwitchRejection::Conflict);
        }
        let mut transition =
            self.begin_active_call_switch_transaction(&device_id, pending.call_id)?;
        transition.auto_answer_mode = Some(pending.request.mode);
        for effect in &mut transition.effects {
            if let DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
                device_id,
                call_id,
                codec,
            }) = effect
                && *call_id == pending.call_id
            {
                *effect = if pending.request.mode == AutoAnswerMode::OneWay {
                    HandsetEffect::BeginOneWayMedia {
                        device_id: device_id.clone(),
                        call_id: *call_id,
                        codec: *codec,
                    }
                    .into()
                } else {
                    HandsetEffect::BeginMedia {
                        device_id: device_id.clone(),
                        call_id: *call_id,
                        codec: *codec,
                    }
                    .into()
                };
            }
        }
        Ok(transition)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn device_can_auto_answer(
        &self,
        device_id: &DeviceId,
        target: CallId,
    ) -> bool {
        self.devices
            .get(device_id)
            .is_some_and(|device| device.active_call.is_none())
            && !self
                .pending_call_transitions
                .values()
                .any(|pending| &pending.transition.device_id == device_id)
            && self.transfers.get(device_id).is_none()
            && !self.conferences.by_consultation.values().any(|session| {
                session
                    .participants
                    .iter()
                    .any(|participant| &participant.device_id == device_id)
            })
            && !self
                .appearances_for_device(device_id)
                .filter(|appearance| appearance.sccp_id != target)
                .any(|appearance| {
                    self.barges.by_handset.contains_key(&appearance.sccp_id)
                        || matches!(
                            appearance.state,
                            CallState::Collecting
                                | CallState::PickupCollecting
                                | CallState::Calling
                                | CallState::Connected
                                | CallState::Parking
                                | CallState::Retrieving
                                | CallState::Barged
                                | CallState::TransferCollecting
                        )
                })
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn cancel_auto_answers_for_pbx(
        &mut self,
        pbx_id: PbxCallId,
    ) {
        self.auto_answer_requests.remove(&pbx_id);
        self.pending_auto_answers
            .retain(|_, pending| pending.pbx_id != pbx_id);
    }

    pub fn pbx_ringing(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(appearance) = self.advance_outbound_call_phase(pbx_id, OutboundCallPhase::Ringing)
        else {
            return Vec::new();
        };
        self.advance_transfer_for_pbx(pbx_id, TransferPhase::Ringing);
        let mut effects = vec![
            HandsetEffect::PresentOutboundRinging {
                device_id: appearance.device_id.clone(),
                call_id: appearance.sccp_id,
                info: appearance.info,
            }
            .into(),
        ];
        effects.extend(self.publish_outbound_ring_out(pbx_id));
        effects
    }

    pub(in crate::runtime::controller) fn advance_outbound_call_phase(
        &mut self,
        pbx_id: PbxCallId,
        next: OutboundCallPhase,
    ) -> Option<CallAppearance> {
        let appearance_id = {
            let call = self.call_registry.pbx.get(&pbx_id)?;
            if call.direction != CallDirection::Outbound
                || call.state != CallState::Calling
                || call.outbound_phase.is_none_or(|current| current >= next)
            {
                return None;
            }
            call.active_appearance
                .or_else(|| call.appearance_ids.first().copied())?
        };
        let appearance = self.call_registry.appearances.get(&appearance_id)?.clone();
        self.call_registry.pbx.get_mut(&pbx_id)?.outbound_phase = Some(next);
        Some(appearance)
    }

    pub fn hold(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(owner) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&owner.pbx_id) else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&owner.pbx_id)
            || self.barges.groups.contains_key(&owner.pbx_id)
            || self.pending_phone_answers.contains_key(&call_id)
        {
            return Vec::new();
        }
        if !matches!(
            call.state,
            CallState::Collecting
                | CallState::Calling
                | CallState::Connected
                | CallState::TransferCollecting
        ) || call.active_appearance != Some(owner.id)
        {
            return Vec::new();
        }
        let appearance_ids = call.appearance_ids.clone();
        if let Some(call) = self.call_registry.pbx.get_mut(&owner.pbx_id) {
            call.state = CallState::Held;
        }
        self.shared_control_claims.remove(&owner.pbx_id);
        let mut effects = vec![
            PbxEffect::Hold {
                call_id: owner.pbx_id,
            }
            .into(),
        ];
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            appearance.audio = MediaStreamState::Closed;
            appearance.audio_transmit = MediaStreamState::Closed;
            effects.extend(
                appearance
                    .video
                    .cleanup(&appearance.device_id, appearance.sccp_id)
                    .map(HandsetEffect::from)
                    .map(DriverEffect::from),
            );
            appearance.video.close_streams();
            if appearance_id == owner.id {
                appearance.state = CallState::Held;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::Hold,
                    true,
                ));
            } else {
                appearance.state = CallState::SharedHeld;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::HoldRed,
                    false,
                ));
            }
        }
        if self
            .devices
            .get(&owner.device_id)
            .is_some_and(|device| device.active_call == Some(call_id))
        {
            self.set_active_call(&owner.device_id, None);
        }
        self.set_call_selected(&owner.device_id, call_id, false);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn resume(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        let Some(requester) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        if self.redirect_claims.contains(&requester.pbx_id)
            || self.conferences.by_pbx.contains_key(&requester.pbx_id)
        {
            return Vec::new();
        }
        let Some(call) = self.call_registry.pbx.get(&requester.pbx_id) else {
            return Vec::new();
        };
        if call.state != CallState::Held
            || !matches!(requester.state, CallState::Held | CallState::SharedHeld)
            || (call.active_appearance != Some(requester.id)
                && !self.shared_control_eligible(&requester))
        {
            return Vec::new();
        }
        let previous_owner = call.active_appearance;
        let appearance_ids = call.appearance_ids.clone();
        let requester_privacy = requester.privacy
            || self
                .features
                .get(&requester.device_id)
                .is_some_and(|state| state.privacy);
        if let Some(call) = self.call_registry.pbx.get_mut(&requester.pbx_id) {
            call.state = CallState::Connected;
            call.active_appearance = Some(requester.id);
            call.privacy |= requester_privacy;
        }
        if previous_owner != Some(requester.id) {
            self.shared_control_claims
                .insert(requester.pbx_id, SharedControlClaim::Steal(requester.id));
        }
        let mut effects = vec![
            PbxEffect::Resume {
                call_id: requester.pbx_id,
            }
            .into(),
        ];
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            if appearance_id == requester.id {
                appearance.state = CallState::Connected;
                appearance.audio = MediaStreamState::Opening;
                appearance.audio_transmit = MediaStreamState::Closed;
            } else {
                appearance.state = CallState::RemoteInUse;
                effects.push(appearance_state_effect(
                    appearance,
                    HandsetCallState::RemoteMultiline,
                    previous_owner == Some(appearance_id),
                ));
                if let Some(device) = self.devices.get_mut(&appearance.device_id) {
                    device.selected_calls.remove(&appearance.sccp_id);
                }
            }
        }
        effects.push(
            HandsetEffect::BeginMedia {
                device_id: requester.device_id.clone(),
                call_id: requester.sccp_id,
                codec: requester.codec,
            }
            .into(),
        );
        self.select_line(&requester.device_id, requester.line_instance);
        self.set_active_call(&requester.device_id, Some(call_id));
        self.set_call_selected(&requester.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Attach a remote shared-line appearance to the target call through a
    /// separate PBX media channel. The first serialized steal or barge claim
    /// wins; conference barges may add more participants to the winning
    /// conference bridge.
    pub fn barge(
        &mut self,
        call_id: CallId,
        binding: LineBinding,
        codec: Codec,
        mode: BargeMode,
    ) -> Result<Vec<DriverEffect>, BargeRejection> {
        let requester = self
            .appearance_for_call(call_id)
            .cloned()
            .ok_or(BargeRejection::Unavailable)?;
        let target = self
            .call_registry
            .pbx
            .get(&requester.pbx_id)
            .cloned()
            .ok_or(BargeRejection::Unavailable)?;
        if requester.state != CallState::RemoteInUse
            || target.state != CallState::Connected
            || target.active_appearance == Some(requester.id)
        {
            return Err(BargeRejection::NotRemote);
        }
        if target.privacy {
            return Err(BargeRejection::Private);
        }
        if requester.ring_mode == AppearanceRingMode::Disabled
            || binding.device_id != requester.device_id
            || binding.line_instance != requester.line_instance
            || binding.line.number != target.line
            || !self.devices.contains_key(&requester.device_id)
        {
            return Err(BargeRejection::Unavailable);
        }
        let owner = target
            .active_appearance
            .and_then(|id| self.call_registry.appearances.get(&id))
            .ok_or(BargeRejection::Unavailable)?;
        if !self.device_supports_codec(&requester.device_id, codec)
            || !self.device_supports_codec(&owner.device_id, owner.codec)
        {
            return Err(BargeRejection::Capability);
        }
        if self.barges.by_handset.contains_key(&call_id) {
            return Err(BargeRejection::AlreadyBarged);
        }
        if self.redirect_claims.contains(&requester.pbx_id) {
            return Err(BargeRejection::Conflict);
        }

        let (bridge_id, first_participant) =
            match self.shared_control_claims.get(&target.id).copied() {
                Some(SharedControlClaim::Steal(_)) => return Err(BargeRejection::Conflict),
                Some(SharedControlClaim::Barge(bridge_id)) => {
                    let group = self
                        .barges
                        .groups
                        .get(&target.id)
                        .ok_or(BargeRejection::Conflict)?;
                    if mode != BargeMode::Conference || group.mode != BargeMode::Conference {
                        return Err(BargeRejection::AlreadyBarged);
                    }
                    (bridge_id, false)
                }
                None => (self.allocate_bridge_id(), true),
            };

        let barger_call_id = self.allocate_pbx_id();
        self.call_registry.pbx.insert(
            barger_call_id,
            PbxCall {
                id: barger_call_id,
                line: target.line.clone(),
                context: target.context.clone(),
                direction: CallDirection::Outbound,
                state: CallState::Connected,
                outbound_phase: None,
                outbound_identity_stage: OutboundIdentityStage::Awaiting,
                digits: String::new(),
                privacy: true,
                metadata: CallMetadata::default(),
                pending_pickup: None,
                appearance_ids: Vec::new(),
                active_appearance: None,
                digit_deadline: None,
                last_digit_at: None,
                simulated_enbloc_eligible: false,
                overlap_enabled: false,
            },
        );
        if let Some(appearance) = self.call_registry.appearances.get_mut(&requester.id) {
            appearance.state = CallState::Barged;
            appearance.codec = codec;
            appearance.audio = MediaStreamState::Opening;
            appearance.video.close_streams();
        }
        let session = BargeSession {
            target_call_id: target.id,
            barger_call_id,
            bridge_id,
            handset_call_id: call_id,
            mode,
        };
        self.barges.by_handset.insert(call_id, session);
        self.barges.by_pbx.insert(barger_call_id, call_id);
        if first_participant {
            self.shared_control_claims
                .insert(target.id, SharedControlClaim::Barge(bridge_id));
            self.barges.groups.insert(
                target.id,
                BargeGroup {
                    bridge_id,
                    mode,
                    members: vec![call_id],
                },
            );
        } else if let Some(group) = self.barges.groups.get_mut(&target.id) {
            group.members.push(call_id);
        }
        self.select_line(&requester.device_id, requester.line_instance);
        self.set_call_selected(&requester.device_id, call_id, true);
        debug_assert!(self.invariant_error().is_none());

        Ok(vec![
            PbxEffect::CreateChannel {
                handset_call_id: call_id,
                call_id: barger_call_id,
                binding: Box::new(binding),
                codec,
            }
            .into(),
            PbxEffect::Barge {
                operation: BargeOperation::Join {
                    bridge_id,
                    target_call_id: target.id,
                    barger_call_id,
                },
            }
            .into(),
            HandsetEffect::BeginMedia {
                device_id: requester.device_id,
                call_id,
                codec,
            }
            .into(),
        ])
    }

    pub fn barge_session(&self, call_id: CallId) -> Option<&BargeSession> {
        self.barges.by_handset.get(&call_id)
    }

    pub fn barge_session_by_pbx(&self, pbx_id: PbxCallId) -> Option<&BargeSession> {
        self.barges
            .by_pbx
            .get(&pbx_id)
            .and_then(|call_id| self.barges.by_handset.get(call_id))
    }

    /// Roll back a failed adapter operation. `bridge_joined` and
    /// `channel_created` describe which preceding effects completed.
    pub fn abort_barge(
        &mut self,
        call_id: CallId,
        bridge_joined: bool,
        channel_created: bool,
    ) -> Vec<DriverEffect> {
        self.end_barge(call_id, bridge_joined, channel_created)
    }

    pub fn hangup(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        // The protocol's immediate OnHook presentation is not sufficient to
        // retire its indexed SessionCall. Emit the idempotent terminal effect
        // as well so CloseCall removes wire/controller ownership together.
        self.hangup_internal(call_id, true)
    }

    /// Terminate a call whose failure did not originate from a physical
    /// handset OnHook. The active appearance still needs explicit terminal UI
    /// and media cleanup before controller ownership is removed.
    pub fn terminate(&mut self, call_id: CallId) -> Vec<DriverEffect> {
        self.hangup_internal(call_id, true)
    }

    pub(in crate::runtime::controller) fn hangup_internal(
        &mut self,
        call_id: CallId,
        cleanup_current_appearance: bool,
    ) -> Vec<DriverEffect> {
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        if let Some(effect) = self.complete_remote_hangup(call_id) {
            return vec![effect];
        }
        if let Some(session) = self.conference_session(call_id).cloned() {
            if session
                .pending_invite
                .as_ref()
                .is_some_and(|invite| invite.participant.handset_call_id == call_id)
            {
                return self.abort_conference_invite(call_id, true, true, true);
            }
            if session.phase == ConferencePhase::Consultation
                && session.consultation_handset_call_id == call_id
            {
                return self.cancel_conference(call_id);
            }
            if session.phase == ConferencePhase::Active
                || (session.phase == ConferencePhase::Merging
                    && session.origin == ConferenceOrigin::Selection)
            {
                if session.phase == ConferencePhase::Active {
                    let Some(pbx_id) = self
                        .appearance_for_call(call_id)
                        .map(|appearance| appearance.pbx_id)
                    else {
                        return Vec::new();
                    };
                    return self.active_conference_departure(session, pbx_id, None, true);
                }
                return self.end_conference_internal(session, true, None);
            }
        }
        if self.barges.by_handset.contains_key(&call_id) {
            return self.end_barge(call_id, true, true);
        }
        let Some(appearance) = self.appearance_for_call(call_id).cloned() else {
            return Vec::new();
        };
        let Some(call) = self.call_registry.pbx.get(&appearance.pbx_id) else {
            return Vec::new();
        };
        let appearance_ids = call.appearance_ids.clone();
        if call
            .active_appearance
            .is_some_and(|owner| owner != appearance.id)
        {
            return Vec::new();
        }
        if call.active_appearance.is_none()
            && call.state == CallState::Ringing
            && appearance_ids.len() > 1
        {
            self.remove_appearance(appearance.id);
            debug_assert!(self.invariant_error().is_none());
            return Vec::new();
        }
        let mut effects = vec![
            PbxEffect::Hangup {
                call_id: appearance.pbx_id,
            }
            .into(),
        ];
        if appearance.auto_answer_mode == Some(AutoAnswerMode::OneWay) {
            effects.push(
                HandsetEffect::SetMicrophoneMode {
                    device_id: appearance.device_id.clone(),
                    call_id: appearance.sccp_id,
                    enabled: true,
                }
                .into(),
            );
        }
        effects.extend(
            appearance_ids
                .into_iter()
                .filter(|id| cleanup_current_appearance || *id != appearance.id)
                .filter_map(|id| self.call_registry.appearances.get(&id))
                .flat_map(appearance_terminal_effects),
        );
        self.remove_pbx_call(appearance.pbx_id);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub fn pbx_hangup(&mut self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        self.pbx_hangup_with_effects(pbx_id)?.primary
    }

    /// Detach one PBX call immediately while optionally leaving the exact
    /// active handset presentation up for a bounded remote-hangup tone.
    /// Conference, transfer, barge, held/ringing and in-flight switch state
    /// always take the ordinary immediate cleanup path.
    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn begin_remote_hangup(
        &mut self,
        pbx_id: PbxCallId,
        tone: Option<Tone>,
        delay: Duration,
        now: Instant,
    ) -> Option<RemoteHangupPlan> {
        let eligible =
            tone.is_some() && !delay.is_zero() && self.remote_hangup_owner(pbx_id).is_some();
        let pending = eligible
            .then(|| self.allocate_remote_hangup_token())
            .flatten();
        let owner = pending.zip(self.remote_hangup_owner(pbx_id));
        let mut outcome = self.pbx_hangup_with_effects(pbx_id)?;
        if let (Some(tone), Some((token, owner))) = (tone, owner) {
            outcome.effects.retain(|effect| {
                !matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::SetCallState {
                        device_id,
                        call_id,
                        ..
                    }) if device_id == &owner.device_id && *call_id == owner.sccp_id
                )
            });
            outcome.effects.push(
                HandsetEffect::SetCallState {
                    device_id: owner.device_id.clone(),
                    call_id: owner.sccp_id,
                    state: HandsetCallState::Connected,
                    stop_media: true,
                }
                .into(),
            );
            outcome.effects.push(
                HandsetEffect::StartTone {
                    device_id: owner.device_id.clone(),
                    call_id: owner.sccp_id,
                    tone,
                }
                .into(),
            );
            self.pending_remote_hangups.insert(
                owner.sccp_id,
                PendingRemoteHangup {
                    token,
                    device_id: owner.device_id,
                    call_id: owner.sccp_id,
                    deadline: now + delay,
                },
            );
            return Some(RemoteHangupPlan {
                outcome,
                pending: Some(token),
            });
        }
        Some(RemoteHangupPlan {
            outcome,
            pending: None,
        })
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn expire_remote_hangups(&mut self, now: Instant) -> Vec<DriverEffect> {
        let mut due = self
            .pending_remote_hangups
            .values()
            .filter(|pending| pending.deadline <= now)
            .map(|pending| (pending.deadline, pending.token, pending.call_id))
            .collect::<Vec<_>>();
        due.sort_by_key(|(deadline, token, call_id)| (*deadline, token.0, call_id.0));
        due.into_iter()
            .filter_map(|(_, token, _)| self.complete_remote_hangup_token(token))
            .collect()
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn complete_remote_hangup_token(
        &mut self,
        token: RemoteHangupToken,
    ) -> Option<DriverEffect> {
        let call_id = self
            .pending_remote_hangups
            .iter()
            .find_map(|(call_id, pending)| (pending.token == token).then_some(*call_id))?;
        self.complete_remote_hangup(call_id)
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn complete_remote_hangup(&mut self, call_id: CallId) -> Option<DriverEffect> {
        let pending = self.pending_remote_hangups.remove(&call_id)?;
        Some(
            HandsetEffect::SetCallState {
                device_id: pending.device_id,
                call_id: pending.call_id,
                state: HandsetCallState::OnHook,
                stop_media: true,
            }
            .into(),
        )
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(crate) fn drain_remote_hangups(&mut self) -> Vec<DriverEffect> {
        let mut pending = self
            .pending_remote_hangups
            .values()
            .map(|pending| (pending.token.0, pending.call_id))
            .collect::<Vec<_>>();
        pending.sort_by_key(|(generation, call_id)| (*generation, call_id.0));
        pending
            .into_iter()
            .filter_map(|(_, call_id)| self.complete_remote_hangup(call_id))
            .collect()
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn remote_hangup_owner(
        &self,
        pbx_id: PbxCallId,
    ) -> Option<CallAppearance> {
        let call = self.call_registry.pbx.get(&pbx_id)?;
        let owner_id = call.active_appearance?;
        let owner = self.call_registry.appearances.get(&owner_id)?;
        let transfer_owned = self.transfers.transactions().any(|transaction| {
            transaction.source.pbx_call_id == pbx_id
                || transaction
                    .consultation
                    .is_some_and(|leg| leg.pbx_call_id == pbx_id)
        });
        let transition_owned = self.pending_call_transitions.values().any(|pending| {
            pending.transition.target_pbx_id == pbx_id
                || pending.transition.previous_pbx_id == Some(pbx_id)
        });
        if call.state != CallState::Connected
            || owner.state != CallState::Connected
            || self
                .devices
                .get(&owner.device_id)
                .is_none_or(|device| device.active_call != Some(owner.sccp_id))
            || transfer_owned
            || transition_owned
            || self.conferences.by_pbx.contains_key(&pbx_id)
            || self.barges.by_pbx.contains_key(&pbx_id)
            || self.barges.groups.contains_key(&pbx_id)
        {
            return None;
        }
        Some(owner.clone())
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn allocate_remote_hangup_token(
        &mut self,
    ) -> Option<RemoteHangupToken> {
        let next = self.next_remote_hangup_generation.checked_add(1)?;
        let token = RemoteHangupToken(self.next_remote_hangup_generation);
        self.next_remote_hangup_generation = next;
        Some(token)
    }

    pub fn pbx_hangup_with_effects(&mut self, pbx_id: PbxCallId) -> Option<PbxHangupOutcome> {
        let transition_primary = self.primary_call_by_pbx(pbx_id);
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        let transition_effects = self.abort_call_transitions_for_pbx(pbx_id);
        #[cfg(not(any(test, feature = "asterisk-22", feature = "asterisk-latest")))]
        let transition_effects = Vec::new();
        if !transition_effects.is_empty() && !self.call_registry.pbx.contains_key(&pbx_id) {
            return Some(PbxHangupOutcome {
                primary: transition_primary,
                effects: transition_effects,
            });
        }
        let transfer = {
            let mut transactions = self.transfers.transactions();
            transactions
                .find(|transaction| {
                    transaction.source.pbx_call_id == pbx_id
                        || transaction
                            .consultation
                            .is_some_and(|leg| leg.pbx_call_id == pbx_id)
                })
                .cloned()
        };
        if let Some(transaction) = transfer {
            let primary = self.primary_call_by_pbx(pbx_id);
            let terminated_leg = if transaction.source.pbx_call_id == pbx_id {
                transaction.source
            } else {
                transaction
                    .consultation
                    .expect("indexed transfer has a consultation leg")
            };
            let _ = self.transfers.note_hangup(terminated_leg);
            if transaction.phase == TransferPhase::Completing {
                return Some(PbxHangupOutcome {
                    primary,
                    effects: Vec::new(),
                });
            }
            let source_hung_up = transaction.source.pbx_call_id == pbx_id;
            let reason = if source_hung_up {
                TransferCancellationReason::SourceHangup
            } else {
                TransferCancellationReason::ConsultationHangup
            };
            let transfer = self
                .abort_transfer(&transaction.device_id, transaction.id, reason)
                .ok()?;
            return Some(PbxHangupOutcome {
                primary,
                effects: transfer.effects,
            });
        }
        if let Some(session) = self.conference_session_by_pbx(pbx_id).cloned() {
            self.conference_mutations
                .remove(&ConferenceMutationOwner::Session(session.id));
            let primary = self.primary_call_by_pbx(pbx_id);
            if let Some(pending) = session.pending_participant_mutation
                && pending.kind == ConferenceParticipantMutationKind::Remove
                && pending.call_id == pbx_id
            {
                let mut effects = self
                    .conference_participant_removed(session.id, pending.participant_id)
                    .unwrap_or_default();
                effects.extend(self.conference_announcement_effects(
                    session.id,
                    ConferenceAnnouncement::ParticipantRemoved(pending.participant_id),
                ));
                debug_assert!(self.invariant_error().is_none());
                return Some(PbxHangupOutcome { primary, effects });
            }
            if session
                .pending_invite
                .as_ref()
                .is_some_and(|invite| invite.participant.pbx_call_id == pbx_id)
            {
                let effects = self.abort_conference_invite(
                    session
                        .pending_invite
                        .as_ref()
                        .expect("checked pending invite")
                        .participant
                        .handset_call_id,
                    false,
                    true,
                    true,
                );
                debug_assert!(self.invariant_error().is_none());
                return Some(PbxHangupOutcome { primary, effects });
            }
            let effects = match session.phase {
                ConferencePhase::Consultation if pbx_id == session.consultation_call_id => self
                    .abort_conference(
                        session.consultation_handset_call_id,
                        false,
                        false,
                        true,
                        true,
                    ),
                ConferencePhase::Consultation => {
                    self.end_conference_internal(session, false, Some(pbx_id))
                }
                ConferencePhase::Merging => {
                    self.end_conference_internal(session, true, Some(pbx_id))
                }
                ConferencePhase::Active => {
                    self.active_conference_departure(session, pbx_id, Some(pbx_id), true)
                }
            };
            debug_assert!(self.invariant_error().is_none());
            return Some(PbxHangupOutcome { primary, effects });
        }
        if let Some(handset_call_id) = self.barges.by_pbx.get(&pbx_id).copied() {
            let primary = self.call(handset_call_id);
            let effects = self.end_barge_internal(handset_call_id, true, false, true);
            debug_assert!(self.invariant_error().is_none());
            return Some(PbxHangupOutcome { primary, effects });
        }
        let mut effects = transition_effects;
        effects.extend(self.end_barges_for_target(pbx_id));
        effects.extend(
            self.call_registry
                .pbx
                .get(&pbx_id)?
                .appearance_ids
                .iter()
                .filter_map(|id| self.call_registry.appearances.get(id))
                .flat_map(appearance_terminal_effects),
        );
        let (_, primary) = self.remove_pbx_call(pbx_id)?;
        debug_assert!(self.invariant_error().is_none());
        Some(PbxHangupOutcome { primary, effects })
    }

    pub fn call(&self, call_id: CallId) -> Option<CallSnapshot> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id)?;
        self.call_snapshot(*appearance_id)
    }

    pub fn call_state(&self, call_id: CallId) -> Option<CallState> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.state)
    }

    pub fn call_pbx_id(&self, call_id: CallId) -> Option<PbxCallId> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.pbx_id)
    }

    pub fn call_device_id(&self, call_id: CallId) -> Option<&DeviceId> {
        self.appearance_for_call(call_id)
            .map(|appearance| &appearance.device_id)
    }

    pub fn call_line_instance(&self, call_id: CallId) -> Option<u32> {
        self.appearance_for_call(call_id)
            .map(|appearance| appearance.line_instance)
    }

    pub fn primary_call_by_pbx(&self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .and_then(|call| call.appearance_ids.first())
            .and_then(|appearance_id| self.call_snapshot(*appearance_id))
    }

    pub fn active_call_by_pbx(&self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        let appearance_id = self.call_registry.pbx.get(&pbx_id)?.active_appearance?;
        self.call_snapshot(appearance_id)
    }

    pub fn active_or_primary_call_by_pbx(&self, pbx_id: PbxCallId) -> Option<CallSnapshot> {
        self.active_call_by_pbx(pbx_id)
            .or_else(|| self.primary_call_by_pbx(pbx_id))
    }

    pub fn calls(&self) -> impl Iterator<Item = CallSnapshot> + '_ {
        self.call_registry
            .appearances
            .keys()
            .filter_map(|appearance_id| self.call_snapshot(*appearance_id))
    }

    pub fn pbx_call(&self, pbx_id: PbxCallId) -> Option<&PbxCall> {
        self.call_registry.pbx.get(&pbx_id)
    }

    pub fn call_metadata(&self, pbx_id: PbxCallId) -> Option<&CallMetadata> {
        self.call_registry
            .pbx
            .get(&pbx_id)
            .map(|call| &call.metadata)
    }

    /// Atomically replaces PBX-owned channel metadata after the complete value
    /// validates.
    pub fn set_call_metadata(
        &mut self,
        pbx_id: PbxCallId,
        metadata: CallMetadata,
    ) -> Result<bool, MetadataError> {
        metadata.validate()?;
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return Ok(false);
        };
        if call.metadata == metadata {
            return Ok(true);
        }
        call.metadata = metadata;
        Ok(true)
    }

    pub fn active_call_id(&self, pbx_id: PbxCallId) -> Option<CallId> {
        self.active_call_by_pbx(pbx_id).map(|call| call.sccp_id)
    }

    pub fn call_appearance(&self, appearance_id: CallAppearanceId) -> Option<&CallAppearance> {
        self.call_registry.appearances.get(&appearance_id)
    }

    pub fn appearance_for_call(&self, call_id: CallId) -> Option<&CallAppearance> {
        self.call_registry
            .by_sccp
            .get(&call_id)
            .and_then(|appearance_id| self.call_registry.appearances.get(appearance_id))
    }

    pub(in crate::runtime::controller) fn appearance_for_call_mut(
        &mut self,
        call_id: CallId,
    ) -> Option<&mut CallAppearance> {
        let appearance_id = self.call_registry.by_sccp.get(&call_id)?;
        self.call_registry.appearances.get_mut(appearance_id)
    }

    pub fn call_info(&self, call_id: CallId) -> Option<&CallInfo> {
        self.appearance_for_call(call_id)
            .map(|appearance| &appearance.info)
    }

    /// Replaces one appearance's party metadata and returns its handset update.
    pub fn set_call_info(&mut self, call_id: CallId, info: CallInfo) -> Vec<DriverEffect> {
        let Some(appearance_id) = self.call_registry.by_sccp.get(&call_id).copied() else {
            return Vec::new();
        };
        let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
            return Vec::new();
        };
        if appearance.info == info {
            return Vec::new();
        }
        appearance.info = info.clone();
        let device_id = appearance.device_id.clone();
        let pbx_id = appearance.pbx_id;
        self.refresh_conference_participant_identity(pbx_id);
        vec![
            HandsetEffect::SetCallInfo {
                device_id,
                call_id,
                info,
            }
            .into(),
        ]
    }

    /// Updates every presentation of one PBX call in stable appearance order.
    pub fn update_call_info_by_pbx(
        &mut self,
        pbx_id: PbxCallId,
        mut update: impl FnMut(&CallInfo) -> CallInfo,
    ) -> Vec<DriverEffect> {
        let appearance_ids = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .map(|call| call.appearance_ids.clone())
            .unwrap_or_default();
        let mut effects = Vec::new();
        for appearance_id in appearance_ids {
            let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) else {
                continue;
            };
            let info = update(&appearance.info);
            if appearance.info == info {
                continue;
            }
            appearance.info = info.clone();
            effects.push(
                HandsetEffect::SetCallInfo {
                    device_id: appearance.device_id.clone(),
                    call_id: appearance.sccp_id,
                    info,
                }
                .into(),
            );
        }
        self.refresh_conference_participant_identity(pbx_id);
        effects
    }

    pub(in crate::runtime::controller) fn publish_outbound_ring_out(
        &mut self,
        pbx_id: PbxCallId,
    ) -> Vec<DriverEffect> {
        let appearance_id = {
            let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
                return Vec::new();
            };
            if call.direction != CallDirection::Outbound
                || call.state != CallState::Calling
                || call.outbound_identity_stage != OutboundIdentityStage::Ready
                || call.outbound_phase != Some(OutboundCallPhase::Ringing)
            {
                return Vec::new();
            }
            let Some(appearance_id) = call
                .active_appearance
                .or_else(|| call.appearance_ids.first().copied())
            else {
                return Vec::new();
            };
            appearance_id
        };
        let Some(appearance) = self.call_registry.appearances.get(&appearance_id).cloned() else {
            return Vec::new();
        };
        if let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) {
            call.outbound_identity_stage = OutboundIdentityStage::RingOutPublished;
        }
        vec![
            HandsetEffect::SetCallState {
                device_id: appearance.device_id.clone(),
                call_id: appearance.sccp_id,
                state: HandsetCallState::RingOut,
                stop_media: false,
            }
            .into(),
            HandsetEffect::SetCallInfo {
                device_id: appearance.device_id,
                call_id: appearance.sccp_id,
                info: appearance.info,
            }
            .into(),
        ]
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

    /// Remove a still-ringing PBX call from every handset before the adapter
    /// continues that same PBX channel at a forwarding destination.
    pub fn forward_ringing_call(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        let Some(call) = self.call_registry.pbx.get(&pbx_id) else {
            return Vec::new();
        };
        if call.state != CallState::Ringing || call.active_appearance.is_some() {
            return Vec::new();
        }
        let effects = call
            .appearance_ids
            .iter()
            .filter_map(|id| self.call_registry.appearances.get(id))
            .map(|appearance| appearance_state_effect(appearance, HandsetCallState::OnHook, false))
            .collect();
        self.remove_pbx_call(pbx_id);
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    /// Reserve one still-ringing logical call for a no-answer redirect without
    /// mutating handset-visible state. Answer/hold/steal transitions reject the
    /// call until the exact adapter claim completes or rolls back.
    pub fn claim_ringing_forward(&mut self, pbx_id: PbxCallId) -> bool {
        if self.redirect_claims.contains(&pbx_id)
            || self.call_registry.pbx.get(&pbx_id).is_none_or(|call| {
                call.state != CallState::Ringing || call.active_appearance.is_some()
            })
        {
            return false;
        }
        self.redirect_claims.insert(pbx_id)
    }

    pub fn complete_ringing_forward(&mut self, pbx_id: PbxCallId) -> Vec<DriverEffect> {
        if !self.redirect_claims.remove(&pbx_id) {
            return Vec::new();
        }
        self.forward_ringing_call(pbx_id)
    }

    pub fn rollback_ringing_forward(&mut self, pbx_id: PbxCallId) -> bool {
        self.redirect_claims.remove(&pbx_id)
    }

    pub(in crate::runtime::controller) fn device_is_busy(&self, device: &DeviceId) -> bool {
        self.appearances_for_device(device).any(|appearance| {
            matches!(
                appearance.state,
                CallState::Collecting
                    | CallState::PickupCollecting
                    | CallState::Calling
                    | CallState::Connected
                    | CallState::Parking
                    | CallState::Retrieving
                    | CallState::Held
                    | CallState::TransferCollecting
            )
        })
    }

    pub(in crate::runtime::controller) fn shared_control_eligible(
        &self,
        appearance: &CallAppearance,
    ) -> bool {
        appearance.ring_mode != AppearanceRingMode::Disabled
            && self.devices.contains_key(&appearance.device_id)
            && !self.shared_control_claims.contains_key(&appearance.pbx_id)
            && self
                .call_registry
                .pbx
                .get(&appearance.pbx_id)
                .is_some_and(|call| !call.privacy)
    }

    pub(in crate::runtime::controller) fn finish_digits(
        &mut self,
        call_id: CallId,
    ) -> Vec<DriverEffect> {
        let Some((appearance_id, pbx_id, appearance_state, device_id, codec)) =
            self.appearance_for_call(call_id).map(|appearance| {
                (
                    appearance.id,
                    appearance.pbx_id,
                    appearance.state,
                    appearance.device_id.clone(),
                    appearance.codec,
                )
            })
        else {
            return Vec::new();
        };
        if !matches!(
            appearance_state,
            CallState::Collecting | CallState::PickupCollecting | CallState::TransferCollecting
        ) {
            return Vec::new();
        }
        let Some(call) = self.call_registry.pbx.get_mut(&pbx_id) else {
            return Vec::new();
        };
        if call.digits.is_empty() {
            call.digit_deadline = None;
            return Vec::new();
        }
        call.digit_deadline = None;
        let destination = call.digits.clone();
        if let Some(pickup) = call.pending_pickup.take() {
            let next_state = if pickup.answer {
                CallState::Connected
            } else {
                CallState::Ringing
            };
            call.state = next_state;
            call.active_appearance = pickup.answer.then_some(appearance_id);
            if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) {
                appearance.state = next_state;
                appearance.audio = if pickup.answer {
                    MediaStreamState::Opening
                } else {
                    MediaStreamState::Closed
                };
            }
            debug_assert!(self.invariant_error().is_none());
            return vec![
                PbxEffect::Pickup {
                    operation: PickupOperation::Directed {
                        call_id: pbx_id,
                        device_id,
                        handset_call_id: call_id,
                        codec,
                        extension: destination,
                        context: pickup.context,
                        answer: pickup.answer,
                    },
                }
                .into(),
            ];
        }
        let context = call.context.clone();
        let consultation_transfer = self
            .transfers
            .for_leg(TransferLeg {
                handset_call_id: call_id,
                pbx_call_id: pbx_id,
            })
            .is_some_and(|transaction| {
                transaction.consultation
                    == Some(TransferLeg {
                        handset_call_id: call_id,
                        pbx_call_id: pbx_id,
                    })
            });
        let next_state = CallState::Calling;
        call.state = next_state;
        if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) {
            appearance.state = next_state;
        }
        if consultation_transfer {
            self.advance_transfer_for_pbx(pbx_id, TransferPhase::Routing);
        }
        debug_assert!(self.invariant_error().is_none());
        let mut effects = self.outbound_route_presentation(pbx_id, &destination);
        effects.push(
            PbxEffect::StartRouting {
                call_id: pbx_id,
                context,
                destination,
            }
            .into(),
        );
        effects
    }

    pub(in crate::runtime::controller) fn end_barge(
        &mut self,
        call_id: CallId,
        bridge_joined: bool,
        channel_created: bool,
    ) -> Vec<DriverEffect> {
        self.end_barge_internal(call_id, bridge_joined, channel_created, true)
    }

    pub(in crate::runtime::controller) fn end_barge_internal(
        &mut self,
        call_id: CallId,
        bridge_joined: bool,
        channel_created: bool,
        restore_handset: bool,
    ) -> Vec<DriverEffect> {
        let Some(session) = self.barges.by_handset.remove(&call_id) else {
            return Vec::new();
        };
        self.barges.by_pbx.remove(&session.barger_call_id);
        self.call_registry.pbx.remove(&session.barger_call_id);

        let last_participant =
            if let Some(group) = self.barges.groups.get_mut(&session.target_call_id) {
                group.members.retain(|member| *member != call_id);
                group.members.is_empty()
            } else {
                true
            };
        if last_participant {
            self.barges.groups.remove(&session.target_call_id);
            if self.shared_control_claims.get(&session.target_call_id)
                == Some(&SharedControlClaim::Barge(session.bridge_id))
            {
                self.shared_control_claims.remove(&session.target_call_id);
            }
        }

        let mut effects = Vec::new();
        if bridge_joined {
            effects.push(
                PbxEffect::Barge {
                    operation: BargeOperation::Leave {
                        bridge_id: session.bridge_id,
                        barger_call_id: session.barger_call_id,
                        last_participant,
                    },
                }
                .into(),
            );
        }
        if channel_created {
            effects.push(
                PbxEffect::Hangup {
                    call_id: session.barger_call_id,
                }
                .into(),
            );
        }

        let target_exists = self.call_registry.pbx.contains_key(&session.target_call_id);
        let appearance_id = self.call_registry.by_sccp.get(&call_id).copied();
        if let Some(appearance_id) = appearance_id {
            if target_exists {
                if let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id) {
                    appearance.state = CallState::RemoteInUse;
                    appearance.audio = MediaStreamState::Closed;
                    appearance.audio_transmit = MediaStreamState::Closed;
                    appearance.video.close_streams();
                }
                if restore_handset
                    && let Some(appearance) = self.call_registry.appearances.get(&appearance_id)
                {
                    effects.push(appearance_state_effect(
                        appearance,
                        HandsetCallState::RemoteMultiline,
                        true,
                    ));
                }
            } else if let Some(appearance) =
                self.call_registry.appearances.get(&appearance_id).cloned()
            {
                if restore_handset {
                    effects.push(appearance_state_effect(
                        &appearance,
                        HandsetCallState::OnHook,
                        true,
                    ));
                }
                self.remove_appearance(appearance_id);
            }
        }
        debug_assert!(self.invariant_error().is_none());
        effects
    }

    pub(in crate::runtime::controller) fn end_barges_for_target(
        &mut self,
        target: PbxCallId,
    ) -> Vec<DriverEffect> {
        let members = self
            .barges
            .groups
            .get(&target)
            .map(|group| group.members.clone())
            .unwrap_or_default();
        let mut effects = Vec::new();
        for call_id in members {
            effects.extend(self.end_barge_internal(call_id, true, true, false));
        }
        effects
    }

    pub(in crate::runtime::controller) fn allocate_appearance_id(&mut self) -> CallAppearanceId {
        loop {
            let id = CallAppearanceId(self.next_appearance_id);
            self.next_appearance_id = self.next_appearance_id.wrapping_add(1).max(1);
            if !self.call_registry.appearances.contains_key(&id) {
                return id;
            }
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn allocate_call_transition_id(
        &mut self,
    ) -> CallTransitionId {
        loop {
            let id = CallTransitionId(self.next_call_transition_id);
            self.next_call_transition_id = self.next_call_transition_id.wrapping_add(1).max(1);
            if !self.pending_call_transitions.contains_key(&id) {
                return id;
            }
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn call_domain_snapshot(&self) -> CallDomainSnapshot {
        CallDomainSnapshot {
            devices: self.devices.clone(),
            pbx_calls: self.call_registry.pbx.clone(),
            appearances: self.call_registry.appearances.clone(),
            appearance_by_sccp: self.call_registry.by_sccp.clone(),
            shared_control_claims: self.shared_control_claims.clone(),
            call_waiting_tones: self.call_waiting_tones.clone(),
            pending_phone_answers: self.pending_phone_answers.clone(),
            pending_route_media: self.pending_route_media.clone(),
        }
    }

    #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
    pub(in crate::runtime::controller) fn restore_call_domain(
        &mut self,
        snapshot: &CallDomainSnapshot,
        transition: &CallTransition,
    ) -> bool {
        let affected_pbx = [transition.previous_pbx_id, Some(transition.target_pbx_id)]
            .into_iter()
            .flatten()
            .collect::<HashSet<_>>();
        let current_active = self
            .devices
            .get(&transition.device_id)
            .and_then(|device| device.active_call);
        if current_active != Some(transition.target_call_id)
            || self
                .appearance_for_call(transition.target_call_id)
                .is_none_or(|appearance| appearance.pbx_id != transition.target_pbx_id)
            || transition.previous_call_id.is_some_and(|previous| {
                self.appearance_for_call(previous)
                    .is_none_or(|appearance| Some(appearance.pbx_id) != transition.previous_pbx_id)
            })
        {
            return false;
        }

        let mut affected_calls = self
            .call_registry
            .appearances
            .values()
            .filter(|appearance| affected_pbx.contains(&appearance.pbx_id))
            .map(|appearance| appearance.sccp_id)
            .collect::<HashSet<_>>();
        affected_calls.extend(
            snapshot
                .appearances
                .values()
                .filter(|appearance| affected_pbx.contains(&appearance.pbx_id))
                .map(|appearance| appearance.sccp_id),
        );
        affected_calls.insert(transition.target_call_id);
        if let Some(previous) = transition.previous_call_id {
            affected_calls.insert(previous);
        }

        self.call_registry
            .pbx
            .retain(|pbx_id, _| !affected_pbx.contains(pbx_id));
        self.call_registry.pbx.extend(
            snapshot
                .pbx_calls
                .iter()
                .filter(|(pbx_id, _)| affected_pbx.contains(pbx_id))
                .map(|(pbx_id, call)| (*pbx_id, call.clone())),
        );
        self.call_registry
            .appearances
            .retain(|_, appearance| !affected_pbx.contains(&appearance.pbx_id));
        self.call_registry.appearances.extend(
            snapshot
                .appearances
                .iter()
                .filter(|(_, appearance)| affected_pbx.contains(&appearance.pbx_id))
                .map(|(id, appearance)| (*id, appearance.clone())),
        );
        self.call_registry
            .by_sccp
            .retain(|call_id, _| !affected_calls.contains(call_id));
        self.call_registry.by_sccp.extend(
            snapshot
                .appearance_by_sccp
                .iter()
                .filter(|(call_id, _)| affected_calls.contains(call_id))
                .map(|(call_id, appearance_id)| (*call_id, *appearance_id)),
        );
        self.call_registry.by_device.clear();
        for (appearance_id, appearance) in &self.call_registry.appearances {
            self.call_registry
                .by_device
                .entry(appearance.device_id.clone())
                .or_default()
                .insert(*appearance_id);
        }
        for (device_id, before) in &snapshot.devices {
            let Some(device) = self.devices.get_mut(device_id) else {
                continue;
            };
            if device_id == &transition.device_id {
                device.active_call = before.active_call;
            }
            for call_id in &affected_calls {
                if before.selected_calls.contains(call_id) {
                    device.selected_calls.insert(*call_id);
                } else {
                    device.selected_calls.remove(call_id);
                }
            }
        }
        for pbx_id in &affected_pbx {
            match snapshot.shared_control_claims.get(pbx_id) {
                Some(claim) => {
                    self.shared_control_claims.insert(*pbx_id, *claim);
                }
                None => {
                    self.shared_control_claims.remove(pbx_id);
                }
            }
        }
        self.call_waiting_tones.retain(|call_id, schedule| {
            !affected_calls.contains(call_id) && !affected_calls.contains(&schedule.active_call_id)
        });
        self.call_waiting_tones.extend(
            snapshot
                .call_waiting_tones
                .iter()
                .filter(|(call_id, schedule)| {
                    affected_calls.contains(call_id)
                        || affected_calls.contains(&schedule.active_call_id)
                })
                .map(|(call_id, schedule)| (*call_id, schedule.clone())),
        );
        self.pending_phone_answers
            .retain(|call_id, _| !affected_calls.contains(call_id));
        self.pending_phone_answers.extend(
            snapshot
                .pending_phone_answers
                .iter()
                .filter(|(call_id, _)| affected_calls.contains(call_id))
                .map(|(call_id, pbx_id)| (*call_id, *pbx_id)),
        );
        self.pending_route_media
            .retain(|call_id| !affected_calls.contains(call_id));
        self.pending_route_media.extend(
            snapshot
                .pending_route_media
                .iter()
                .filter(|call_id| affected_calls.contains(call_id))
                .copied(),
        );
        true
    }

    pub(in crate::runtime::controller) fn insert_pbx_call(
        &mut self,
        call: PbxCall,
        appearance: CallAppearance,
    ) -> bool {
        if call.id != appearance.pbx_id
            || !call.appearance_ids.is_empty()
            || self.call_registry.pbx.contains_key(&call.id)
        {
            return false;
        }
        let pbx_id = call.id;
        self.call_registry.pbx.insert(pbx_id, call);
        if self.attach_appearance(appearance) {
            true
        } else {
            self.call_registry.pbx.remove(&pbx_id);
            false
        }
    }

    pub(in crate::runtime::controller) fn attach_appearance(
        &mut self,
        appearance: CallAppearance,
    ) -> bool {
        if !self.call_registry.pbx.contains_key(&appearance.pbx_id)
            || self.call_registry.appearances.contains_key(&appearance.id)
            || self.call_registry.by_sccp.contains_key(&appearance.sccp_id)
        {
            return false;
        }
        let appearance_id = appearance.id;
        let pbx_id = appearance.pbx_id;
        let call_id = appearance.sccp_id;
        let device_id = appearance.device_id.clone();
        self.call_registry
            .appearances
            .insert(appearance_id, appearance);
        self.call_registry.by_sccp.insert(call_id, appearance_id);
        self.call_registry
            .by_device
            .entry(device_id)
            .or_default()
            .insert(appearance_id);
        self.call_registry
            .pbx
            .get_mut(&pbx_id)
            .expect("PBX call checked above")
            .appearance_ids
            .push(appearance_id);
        true
    }

    pub(in crate::runtime::controller) fn remove_appearance(
        &mut self,
        appearance_id: CallAppearanceId,
    ) -> Option<CallAppearance> {
        let appearance = self.call_registry.appearances.remove(&appearance_id)?;
        self.call_waiting_tones.retain(|_, schedule| {
            schedule.waiting_call_id != appearance.sccp_id
                && schedule.active_call_id != appearance.sccp_id
        });
        self.pending_phone_answers.remove(&appearance.sccp_id);
        self.pending_route_media.remove(&appearance.sccp_id);
        self.call_registry.by_sccp.remove(&appearance.sccp_id);
        if let Some(device_appearances) =
            self.call_registry.by_device.get_mut(&appearance.device_id)
        {
            device_appearances.remove(&appearance_id);
            if device_appearances.is_empty() {
                self.call_registry.by_device.remove(&appearance.device_id);
            }
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&appearance.pbx_id) {
            call.appearance_ids.retain(|id| *id != appearance_id);
            if call.active_appearance == Some(appearance_id) {
                call.active_appearance = None;
            }
        }
        if self.shared_control_claims.get(&appearance.pbx_id)
            == Some(&SharedControlClaim::Steal(appearance_id))
        {
            self.shared_control_claims.remove(&appearance.pbx_id);
        }
        if let Some(device) = self.devices.get_mut(&appearance.device_id) {
            device.selected_calls.remove(&appearance.sccp_id);
            if device.active_call == Some(appearance.sccp_id) {
                device.active_call = None;
            }
        }
        Some(appearance)
    }

    pub(in crate::runtime::controller) fn remove_pbx_call(
        &mut self,
        pbx_id: PbxCallId,
    ) -> Option<(PbxCall, Option<CallSnapshot>)> {
        self.conference_mutations
            .remove(&ConferenceMutationOwner::Destination(pbx_id));
        if let Some(transaction) = self.voicemail.for_pbx(pbx_id).cloned() {
            let _ = self
                .voicemail
                .cancel(&transaction.device_id, transaction.id);
        }
        self.redirect_claims.remove(&pbx_id);
        self.shared_control_claims.remove(&pbx_id);
        #[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
        self.cancel_auto_answers_for_pbx(pbx_id);
        let appearance_ids = self.call_registry.pbx.get(&pbx_id)?.appearance_ids.clone();
        let mut primary = appearance_ids
            .first()
            .and_then(|appearance_id| self.call_snapshot(*appearance_id));
        for appearance_id in appearance_ids {
            self.remove_appearance(appearance_id);
        }
        let mut call = self.call_registry.pbx.remove(&pbx_id)?;
        call.state = CallState::Ended;
        call.digit_deadline = None;
        if let Some(primary) = &mut primary {
            primary.state = CallState::Ended;
            primary.digit_deadline = None;
        }
        Some((call, primary))
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
}
