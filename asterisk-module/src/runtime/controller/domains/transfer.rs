use super::super::*;

impl Controller {
    pub fn begin_selected_voicemail_transfer(
        &mut self,
        device_id: &DeviceId,
        target: VoicemailTarget,
    ) -> Result<VoicemailPlan, VoicemailRejection> {
        let selected = self
            .devices
            .get(device_id)
            .ok_or(VoicemailRejection::Conflict)?
            .selected_calls
            .iter()
            .copied()
            .collect::<Vec<_>>();
        if selected.len() != 1 {
            return Err(VoicemailRejection::Conflict);
        }
        let appearance = self
            .appearance_for_call(selected[0])
            .filter(|appearance| &appearance.device_id == device_id)
            .cloned()
            .ok_or(VoicemailRejection::Conflict)?;
        let call = self
            .call_registry
            .pbx
            .get(&appearance.pbx_id)
            .ok_or(VoicemailRejection::Conflict)?;
        let eligible = match appearance.state {
            CallState::Connected => {
                call.state == CallState::Connected && call.active_appearance == Some(appearance.id)
            }
            CallState::Held => {
                call.state == CallState::Held && call.active_appearance == Some(appearance.id)
            }
            _ => false,
        };
        if !eligible {
            return Err(VoicemailRejection::InvalidPhase);
        }
        self.begin_voicemail_claim(&appearance, VoicemailAction::TransferSelected, target)
    }

    /// Start an isolated consultation call for one connected source. The
    /// caller executes the returned hold/create/handset effects in order and
    /// records each completed setup milestone before it can continue.
    pub fn begin_transfer(
        &mut self,
        request: TransferConsultationRequest,
    ) -> Result<Vec<DriverEffect>, TransferRejection> {
        if request.source_call_id == request.consultation_call_id
            || request.binding.device_id
                != self
                    .appearance_for_call(request.source_call_id)
                    .map(|appearance| appearance.device_id.clone())
                    .ok_or(TransferRejection::WrongCall)?
        {
            return Err(TransferRejection::WrongCall);
        }
        let source = self
            .appearance_for_call(request.source_call_id)
            .cloned()
            .ok_or(TransferRejection::WrongCall)?;
        let source_call = self
            .call_registry
            .pbx
            .get(&source.pbx_id)
            .ok_or(TransferRejection::WrongCall)?;
        if source.state != CallState::Connected
            || source_call.state != CallState::Connected
            || source_call.active_appearance != Some(source.id)
            || self
                .devices
                .get(&source.device_id)
                .is_none_or(|device| device.active_call != Some(request.source_call_id))
        {
            return Err(TransferRejection::InvalidPhase);
        }
        if self.redirect_claims.contains(&source.pbx_id)
            || self.conferences.by_pbx.contains_key(&source.pbx_id)
            || self.barges.by_handset.contains_key(&request.source_call_id)
            || self.transfers.get(&source.device_id).is_some()
            || self
                .call_registry
                .by_sccp
                .contains_key(&request.consultation_call_id)
        {
            return Err(TransferRejection::Conflict);
        }

        let transaction_id = self.transfers.allocate_id();
        let mut effects = self
            .begin_additional_phone_call(
                request.consultation_call_id,
                request.binding,
                request.codec,
                request.now,
            )
            .map_err(|_| TransferRejection::Conflict)?;
        let consultation = self
            .appearance_for_call(request.consultation_call_id)
            .cloned()
            .ok_or(TransferRejection::Conflict)?;
        if let Some(appearance_id) = self
            .call_registry
            .by_sccp
            .get(&request.consultation_call_id)
            .copied()
            && let Some(appearance) = self.call_registry.appearances.get_mut(&appearance_id)
        {
            appearance.state = CallState::TransferCollecting;
        }
        if let Some(call) = self.call_registry.pbx.get_mut(&consultation.pbx_id) {
            call.state = CallState::TransferCollecting;
        }
        for effect in &mut effects {
            if let DriverEffect::Backend(PbxEffect::CreateChannel {
                handset_call_id,
                call_id,
                binding,
                codec,
            }) = effect
                && *call_id == consultation.pbx_id
            {
                *effect = PbxEffect::CreateConsultationChannel {
                    source_call_id: source.pbx_id,
                    handset_call_id: *handset_call_id,
                    call_id: *call_id,
                    binding: binding.clone(),
                    codec: *codec,
                }
                .into();
            }
        }
        let mut transaction = TransferTransaction::consultation(
            transaction_id,
            source.device_id.clone(),
            TransferLeg {
                handset_call_id: source.sccp_id,
                pbx_call_id: source.pbx_id,
            },
            TransferSourceState::Connected,
            request.complete_on_hangup,
        );
        transaction.attach_consultation(TransferLeg {
            handset_call_id: consultation.sccp_id,
            pbx_call_id: consultation.pbx_id,
        })?;
        if let Err(error) = self.transfers.insert(transaction) {
            self.remove_pbx_call(consultation.pbx_id);
            let _ = self.resume(source.sccp_id);
            return Err(error);
        }
        if !effects.iter().any(|effect| {
            matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hold { call_id }) if *call_id == source.pbx_id
            )
        }) {
            let _ = self.transfers.cancel(
                &source.device_id,
                transaction_id,
                TransferCancellationReason::ConsultationFailure,
                None,
            );
            self.remove_pbx_call(consultation.pbx_id);
            let _ = self.resume(source.sccp_id);
            return Err(TransferRejection::Conflict);
        }
        self.set_call_selected(&source.device_id, request.source_call_id, true);
        self.set_call_selected(&source.device_id, request.consultation_call_id, true);
        effects.push(
            HandsetEffect::BeginTransfer {
                device_id: source.device_id,
                source_call_id: request.source_call_id,
                consultation_call_id: request.consultation_call_id,
                consultation_line_instance: consultation.line_instance,
                codec: request.codec,
            }
            .into(),
        );
        debug_assert!(self.invariant_error().is_none());
        Ok(effects)
    }

    pub fn transfer_setup_completed(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        milestone: TransferSetupMilestone,
    ) -> Result<(), TransferRejection> {
        self.transfers
            .record_setup_milestone(device_id, transaction_id, milestone)
    }

    pub fn defer_transfer_action(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        action: DeferredTransferAction,
    ) -> Result<(), TransferRejection> {
        self.transfers
            .defer_action(device_id, transaction_id, action)
    }

    pub(in crate::runtime::controller) fn advance_transfer_for_pbx(
        &mut self,
        pbx_id: PbxCallId,
        phase: TransferPhase,
    ) {
        let Some(appearance) = self
            .call_registry
            .pbx
            .get(&pbx_id)
            .and_then(|call| call.appearance_ids.first())
            .and_then(|id| self.call_registry.appearances.get(id))
            .cloned()
        else {
            return;
        };
        let leg = TransferLeg {
            handset_call_id: appearance.sccp_id,
            pbx_call_id: pbx_id,
        };
        let Some(device_id) = self
            .transfers
            .for_leg(leg)
            .filter(|transaction| transaction.consultation == Some(leg))
            .map(|transaction| transaction.device_id.clone())
        else {
            return;
        };
        let _ = self
            .transfers
            .get_mut(&device_id)
            .expect("transfer found by device")
            .advance_consultation(leg, phase);
    }

    pub fn complete_transfer(
        &mut self,
        device_id: &DeviceId,
        consultation_call_id: CallId,
        trigger: TransferTrigger,
    ) -> Result<TransferCompletionPlan, TransferRejection> {
        let consultation = self
            .appearance_for_call(consultation_call_id)
            .filter(|appearance| &appearance.device_id == device_id)
            .ok_or(TransferRejection::WrongCall)?;
        let completion = self.transfers.begin_completion(
            device_id,
            trigger,
            TransferLeg {
                handset_call_id: consultation.sccp_id,
                pbx_call_id: consultation.pbx_id,
            },
        )?;
        Ok(TransferCompletionPlan {
            effects: vec![
                PbxEffect::Transfer {
                    operation: completion.clone(),
                }
                .into(),
            ],
            completion,
        })
    }

    pub fn complete_device_transfer(
        &mut self,
        device_id: &DeviceId,
        reported_call_id: Option<CallId>,
        trigger: TransferTrigger,
    ) -> Result<TransferCompletionPlan, TransferRejection> {
        let transaction = self
            .transfers
            .get(device_id)
            .cloned()
            .ok_or(TransferRejection::WrongCall)?;
        if let Some(reported_call_id) = reported_call_id.filter(|call_id| call_id.0 != 0) {
            let reported = self
                .appearance_for_call(reported_call_id)
                .filter(|appearance| &appearance.device_id == device_id)
                .map(|appearance| TransferLeg {
                    handset_call_id: appearance.sccp_id,
                    pbx_call_id: appearance.pbx_id,
                })
                .ok_or(TransferRejection::WrongCall)?;
            if !transaction.contains(reported) {
                return Err(TransferRejection::WrongCall);
            }
        }
        let consultation = transaction
            .consultation
            .ok_or(TransferRejection::ConsultationMissing)?;
        self.complete_transfer(device_id, consultation.handset_call_id, trigger)
    }

    /// Claim one native transfer for exactly two selected local calls. A held
    /// call is always the source; otherwise the active call is the target and
    /// call identifiers provide a stable fallback order.
    pub fn direct_transfer(
        &mut self,
        device_id: &DeviceId,
    ) -> Result<TransferCompletionPlan, TransferRejection> {
        if self.transfers.get(device_id).is_some() {
            return Err(TransferRejection::Conflict);
        }
        let device = self
            .devices
            .get(device_id)
            .ok_or(TransferRejection::WrongCall)?;
        if device.selected_calls.len() != 2 {
            return Err(TransferRejection::InvalidSelection);
        }
        let active_call = device.active_call;
        let mut selected = device.selected_calls.iter().copied().collect::<Vec<_>>();
        selected.sort_by_key(|call_id| call_id.0);
        let mut appearances = selected
            .iter()
            .map(|call_id| {
                self.appearance_for_call(*call_id)
                    .filter(|appearance| &appearance.device_id == device_id)
                    .cloned()
                    .ok_or(TransferRejection::WrongCall)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if appearances.iter().any(|appearance| {
            !matches!(appearance.state, CallState::Connected | CallState::Held)
                || self.redirect_claims.contains(&appearance.pbx_id)
                || self.conferences.by_pbx.contains_key(&appearance.pbx_id)
                || self.barges.by_handset.contains_key(&appearance.sccp_id)
        }) || appearances[0].pbx_id == appearances[1].pbx_id
        {
            return Err(TransferRejection::InvalidSelection);
        }
        appearances.sort_by_key(|appearance| {
            let held_rank = u8::from(appearance.state != CallState::Held);
            let active_rank = u8::from(Some(appearance.sccp_id) == active_call);
            (held_rank, active_rank, appearance.sccp_id.0)
        });
        let source = appearances.remove(0);
        let consultation = appearances.remove(0);
        let source_state = match source.state {
            CallState::Connected => TransferSourceState::Connected,
            CallState::Held => TransferSourceState::Held,
            _ => return Err(TransferRejection::InvalidSelection),
        };
        let transaction_id = self.transfers.allocate_id();
        let transaction = TransferTransaction::direct(
            transaction_id,
            device_id.clone(),
            TransferLeg {
                handset_call_id: source.sccp_id,
                pbx_call_id: source.pbx_id,
            },
            source_state,
            TransferLeg {
                handset_call_id: consultation.sccp_id,
                pbx_call_id: consultation.pbx_id,
            },
        )?;
        self.transfers.insert(transaction)?;
        self.complete_transfer(
            device_id,
            consultation.sccp_id,
            TransferTrigger::TransferKey,
        )
    }

    pub fn transfer_succeeded(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
    ) -> Option<TransferTerminalOutcome> {
        let transaction = self.transfers.commit(device_id, transaction_id)?;
        let mut appearances = Vec::new();
        for pbx_id in [
            transaction.source.pbx_call_id,
            transaction.consultation?.pbx_call_id,
        ] {
            let ids = self.call_registry.pbx.get(&pbx_id)?.appearance_ids.clone();
            appearances.extend(
                ids.into_iter()
                    .filter_map(|id| self.call_registry.appearances.get(&id).cloned()),
            );
        }
        let effects = appearances
            .iter()
            .map(|appearance| appearance_state_effect(appearance, HandsetCallState::OnHook, true))
            .collect();
        self.remove_pbx_call(transaction.source.pbx_call_id);
        self.remove_pbx_call(
            transaction
                .consultation
                .expect("committed transfer has a consultation leg")
                .pbx_call_id,
        );
        debug_assert!(self.invariant_error().is_none());
        Some(TransferTerminalOutcome {
            transaction,
            effects,
        })
    }

    pub fn abort_transfer(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        reason: TransferCancellationReason,
    ) -> Result<TransferTerminalOutcome, TransferRejection> {
        let cancellation = self
            .transfers
            .cancel(device_id, transaction_id, reason, None)?;
        let transaction = cancellation.transaction;
        let progress = transaction.execution_progress.clone();
        if transaction.mode == TransferMode::Direct {
            let mut effects = Vec::new();
            for (terminated, leg) in [
                (transaction.source_terminated, transaction.source),
                (
                    transaction.consultation_terminated,
                    transaction
                        .consultation
                        .expect("direct transfer has a consultation leg"),
                ),
            ] {
                if !terminated {
                    continue;
                }
                if let Some(call) = self.call_registry.pbx.get(&leg.pbx_call_id) {
                    effects.extend(
                        call.appearance_ids
                            .iter()
                            .filter_map(|id| self.call_registry.appearances.get(id))
                            .map(|appearance| {
                                appearance_state_effect(appearance, HandsetCallState::OnHook, true)
                            }),
                    );
                }
                self.remove_pbx_call(leg.pbx_call_id);
            }
            return Ok(TransferTerminalOutcome {
                transaction,
                effects,
            });
        }
        let mut effects = Vec::new();
        if let Some(consultation) = transaction.consultation {
            if progress.completed(TransferSetupMilestone::ConsultationChannelCreated)
                && !transaction.consultation_terminated
            {
                effects.push(
                    PbxEffect::Hangup {
                        call_id: consultation.pbx_call_id,
                    }
                    .into(),
                );
            }
            if progress.completed(TransferSetupMilestone::ConsultationHandsetStarted)
                && let Some(appearance) = self
                    .appearance_for_call(consultation.handset_call_id)
                    .cloned()
            {
                effects.push(appearance_state_effect(
                    &appearance,
                    HandsetCallState::OnHook,
                    true,
                ));
            }
            self.remove_pbx_call(consultation.pbx_call_id);
        }
        if cancellation.source_recovery == TransferSourceRecovery::RestoreConnected
            && self
                .appearance_for_call(transaction.source.handset_call_id)
                .is_some()
        {
            effects.extend(
                self.resume(transaction.source.handset_call_id)
                    .into_iter()
                    .filter(|effect| match effect {
                        DriverEffect::Backend(PbxEffect::Resume { .. }) => {
                            progress.completed(TransferSetupMilestone::SourceBackendHeld)
                        }
                        DriverEffect::Handset(_) => {
                            progress.completed(TransferSetupMilestone::SourceHandsetHeld)
                        }
                        _ => true,
                    }),
            );
        }
        if cancellation.source_recovery == TransferSourceRecovery::SourceGone
            && transaction.source_terminated
        {
            if let Some(call) = self.call_registry.pbx.get(&transaction.source.pbx_call_id) {
                effects.extend(
                    call.appearance_ids
                        .iter()
                        .filter_map(|id| self.call_registry.appearances.get(id))
                        .map(|appearance| {
                            appearance_state_effect(appearance, HandsetCallState::OnHook, true)
                        }),
                );
            }
            self.remove_pbx_call(transaction.source.pbx_call_id);
        }
        debug_assert!(self.invariant_error().is_none());
        Ok(TransferTerminalOutcome {
            transaction,
            effects,
        })
    }
}
