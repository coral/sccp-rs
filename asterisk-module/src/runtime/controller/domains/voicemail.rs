use super::super::*;

impl Controller {
    pub(in crate::runtime::controller) fn begin_voicemail_claim(
        &mut self,
        appearance: &CallAppearance,
        action: VoicemailAction,
        target: VoicemailTarget,
    ) -> Result<VoicemailPlan, VoicemailRejection> {
        if self.redirect_claims.contains(&appearance.pbx_id)
            || self.conferences.by_pbx.contains_key(&appearance.pbx_id)
            || self.barges.by_handset.contains_key(&appearance.sccp_id)
            || self
                .transfers
                .for_leg(TransferLeg {
                    handset_call_id: appearance.sccp_id,
                    pbx_call_id: appearance.pbx_id,
                })
                .is_some()
        {
            return Err(VoicemailRejection::Conflict);
        }
        let mut transaction = self.voicemail.claim(
            appearance.device_id.clone(),
            appearance.sccp_id,
            appearance.pbx_id,
            action,
            target,
        )?;
        if !self.redirect_claims.insert(appearance.pbx_id) {
            let _ = self.voicemail.cancel(&appearance.device_id, transaction.id);
            return Err(VoicemailRejection::Conflict);
        }
        let operation = match self
            .voicemail
            .begin_execution(&appearance.device_id, transaction.id)
        {
            Ok(operation) => operation,
            Err(error) => {
                self.redirect_claims.remove(&appearance.pbx_id);
                let _ = self.voicemail.cancel(&appearance.device_id, transaction.id);
                return Err(error);
            }
        };
        transaction.phase = VoicemailPhase::Executing;
        debug_assert!(self.invariant_error().is_none());
        Ok(VoicemailPlan {
            transaction,
            effects: vec![PbxEffect::Voicemail { operation }.into()],
        })
    }

    pub fn abort_voicemail(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<VoicemailTransaction, VoicemailRejection> {
        let transaction = self.voicemail.cancel(device_id, transaction_id)?;
        self.redirect_claims.remove(&transaction.pbx_call_id);
        debug_assert!(self.invariant_error().is_none());
        Ok(transaction)
    }

    pub fn voicemail_succeeded(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
    ) -> Result<VoicemailTerminalOutcome, VoicemailRejection> {
        let transaction = self
            .voicemail
            .get(device_id)
            .filter(|transaction| transaction.id == transaction_id)
            .cloned()
            .ok_or(VoicemailRejection::Conflict)?;
        let appearance_is_owned = self
            .appearance_for_call(transaction.handset_call_id)
            .is_some_and(|appearance| appearance.pbx_id == transaction.pbx_call_id);
        let owner_disconnected = !self.devices.contains_key(&transaction.device_id);
        if !self.redirect_claims.contains(&transaction.pbx_call_id)
            || (!appearance_is_owned && !owner_disconnected)
        {
            return Err(VoicemailRejection::Conflict);
        }
        let effects = self
            .call_registry
            .pbx
            .get(&transaction.pbx_call_id)
            .ok_or(VoicemailRejection::Conflict)?
            .appearance_ids
            .iter()
            .filter_map(|appearance_id| self.call_registry.appearances.get(appearance_id))
            .map(|appearance| appearance_state_effect(appearance, HandsetCallState::OnHook, true))
            .collect();
        let transaction = self.voicemail.commit(device_id, transaction_id)?;
        self.redirect_claims.remove(&transaction.pbx_call_id);
        self.remove_pbx_call(transaction.pbx_call_id);
        debug_assert!(self.invariant_error().is_none());
        Ok(VoicemailTerminalOutcome {
            transaction,
            effects,
        })
    }

    pub fn complete_voicemail_native(
        &mut self,
        device_id: &DeviceId,
        transaction_id: VoicemailTransactionId,
        pbx_call_id: PbxCallId,
    ) -> Result<VoicemailNativeOutcome, VoicemailRejection> {
        if self.voicemail.get(device_id).is_some_and(|transaction| {
            transaction.id == transaction_id && transaction.pbx_call_id == pbx_call_id
        }) {
            return self
                .voicemail_succeeded(device_id, transaction_id)
                .map(VoicemailNativeOutcome::Committed);
        }
        if !self.call_registry.pbx.contains_key(&pbx_call_id)
            && self.voicemail.for_pbx(pbx_call_id).is_none()
            && !self.redirect_claims.contains(&pbx_call_id)
        {
            return Ok(VoicemailNativeOutcome::CallAlreadyEnded);
        }
        Err(VoicemailRejection::Conflict)
    }
}
