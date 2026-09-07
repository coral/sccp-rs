//! Backend-neutral transfer transaction state.
//!
//! A transfer keeps stable call identities until its native operation either
//! commits or fails. The controller owns this state; adapters only execute
//! the typed effects produced by a claimed transaction.

use std::collections::{HashMap, HashSet};

use sccp_protocol::{CallId, DeviceId};

use crate::runtime::backend::PbxCallId;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TransferId(pub u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TransferLeg {
    pub handset_call_id: CallId,
    pub pbx_call_id: PbxCallId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferSourceState {
    Connected,
    Held,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferMode {
    Consultation,
    Direct,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferPhase {
    Collecting,
    Routing,
    Ringing,
    Connected,
    Completing,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferTrigger {
    TransferKey,
    ConsultationHangup,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeferredTransferAction {
    EndCall,
    OnHook,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferCompletionKind {
    Blind,
    Attended,
    Direct,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferCompletion {
    pub transaction_id: TransferId,
    pub device_id: DeviceId,
    pub source: TransferLeg,
    pub consultation: TransferLeg,
    pub kind: TransferCompletionKind,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferCancellationReason {
    EndCall,
    SourceResume,
    ConsultationFailure,
    ConsultationHangup,
    SourceHangup,
    DeviceDisconnect,
    BackendFailure,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferSourceRecovery {
    RestoreConnected,
    RetainHeld,
    SourceGone,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferCancellation {
    pub transaction: TransferTransaction,
    pub reason: TransferCancellationReason,
    pub source_recovery: TransferSourceRecovery,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TransferExecutionProgress {
    completed: HashSet<TransferSetupMilestone>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum TransferSetupMilestone {
    SourceBackendHeld,
    SourceHandsetHeld,
    ConsultationChannelCreated,
    ConsultationHandsetStarted,
}

impl TransferExecutionProgress {
    pub fn completed(&self, milestone: TransferSetupMilestone) -> bool {
        self.completed.contains(&milestone)
    }

    fn record(&mut self, milestone: TransferSetupMilestone) -> bool {
        self.completed.insert(milestone)
    }

    #[cfg(test)]
    pub(crate) fn with_completed(
        milestones: impl IntoIterator<Item = TransferSetupMilestone>,
    ) -> Self {
        Self {
            completed: milestones.into_iter().collect(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TransferTransaction {
    pub id: TransferId,
    pub device_id: DeviceId,
    pub source: TransferLeg,
    pub source_state: TransferSourceState,
    pub consultation: Option<TransferLeg>,
    pub mode: TransferMode,
    pub phase: TransferPhase,
    pub complete_on_hangup: bool,
    pub execution_progress: TransferExecutionProgress,
    pub source_terminated: bool,
    pub consultation_terminated: bool,
    pub deferred_action: Option<DeferredTransferAction>,
}

impl TransferTransaction {
    pub fn consultation(
        id: TransferId,
        device_id: DeviceId,
        source: TransferLeg,
        source_state: TransferSourceState,
        complete_on_hangup: bool,
    ) -> Self {
        Self {
            id,
            device_id,
            source,
            source_state,
            consultation: None,
            mode: TransferMode::Consultation,
            phase: TransferPhase::Collecting,
            complete_on_hangup,
            execution_progress: TransferExecutionProgress::default(),
            source_terminated: false,
            consultation_terminated: false,
            deferred_action: None,
        }
    }

    pub fn direct(
        id: TransferId,
        device_id: DeviceId,
        source: TransferLeg,
        source_state: TransferSourceState,
        consultation: TransferLeg,
    ) -> Result<Self, TransferRejection> {
        if source == consultation {
            return Err(TransferRejection::SameCall);
        }
        Ok(Self {
            id,
            device_id,
            source,
            source_state,
            consultation: Some(consultation),
            mode: TransferMode::Direct,
            phase: TransferPhase::Connected,
            complete_on_hangup: false,
            execution_progress: TransferExecutionProgress::default(),
            source_terminated: false,
            consultation_terminated: false,
            deferred_action: None,
        })
    }

    pub fn attach_consultation(&mut self, leg: TransferLeg) -> Result<(), TransferRejection> {
        if self.mode != TransferMode::Consultation
            || self.phase != TransferPhase::Collecting
            || self.consultation.is_some()
        {
            return Err(TransferRejection::Conflict);
        }
        if self.source == leg {
            return Err(TransferRejection::SameCall);
        }
        self.consultation = Some(leg);
        Ok(())
    }

    pub fn advance_consultation(
        &mut self,
        leg: TransferLeg,
        phase: TransferPhase,
    ) -> Result<(), TransferRejection> {
        if self.consultation != Some(leg) || self.mode != TransferMode::Consultation {
            return Err(TransferRejection::WrongCall);
        }
        let valid = matches!(
            (self.phase, phase),
            (TransferPhase::Collecting, TransferPhase::Routing)
                | (TransferPhase::Routing, TransferPhase::Ringing)
                | (TransferPhase::Routing, TransferPhase::Connected)
                | (TransferPhase::Ringing, TransferPhase::Connected)
        );
        if !valid {
            return Err(TransferRejection::InvalidPhase);
        }
        self.phase = phase;
        Ok(())
    }

    pub fn begin_completion(
        &mut self,
        trigger: TransferTrigger,
        triggering_leg: TransferLeg,
    ) -> Result<TransferCompletion, TransferRejection> {
        if self.phase == TransferPhase::Completing {
            return Err(TransferRejection::CompletionInProgress);
        }
        let consultation = self
            .consultation
            .ok_or(TransferRejection::ConsultationMissing)?;
        if consultation != triggering_leg {
            return Err(TransferRejection::WrongCall);
        }
        if trigger == TransferTrigger::ConsultationHangup && !self.complete_on_hangup {
            return Err(TransferRejection::HangupCompletionDisabled);
        }
        let kind = match (self.mode, self.phase) {
            (TransferMode::Direct, TransferPhase::Connected) => TransferCompletionKind::Direct,
            (TransferMode::Consultation, TransferPhase::Ringing) => TransferCompletionKind::Blind,
            (TransferMode::Consultation, TransferPhase::Connected) => {
                TransferCompletionKind::Attended
            }
            _ => return Err(TransferRejection::InvalidPhase),
        };
        self.phase = TransferPhase::Completing;
        Ok(TransferCompletion {
            transaction_id: self.id,
            device_id: self.device_id.clone(),
            source: self.source,
            consultation,
            kind,
        })
    }

    fn cancellation(
        &self,
        reason: TransferCancellationReason,
        triggering_leg: Option<TransferLeg>,
    ) -> Result<TransferCancellation, TransferRejection> {
        if self.phase == TransferPhase::Completing
            && !matches!(
                reason,
                TransferCancellationReason::BackendFailure
                    | TransferCancellationReason::DeviceDisconnect
            )
        {
            return Err(TransferRejection::CompletionInProgress);
        }
        if let Some(triggering_leg) = triggering_leg
            && !self.contains(triggering_leg)
        {
            return Err(TransferRejection::WrongCall);
        }
        let source_recovery = if self.source_terminated
            || matches!(
                reason,
                TransferCancellationReason::SourceHangup
                    | TransferCancellationReason::DeviceDisconnect
            ) {
            TransferSourceRecovery::SourceGone
        } else {
            match self.source_state {
                TransferSourceState::Connected => TransferSourceRecovery::RestoreConnected,
                TransferSourceState::Held => TransferSourceRecovery::RetainHeld,
            }
        };
        Ok(TransferCancellation {
            transaction: self.clone(),
            reason,
            source_recovery,
        })
    }

    pub fn contains(&self, leg: TransferLeg) -> bool {
        self.source == leg || self.consultation == Some(leg)
    }

    fn note_hangup(&mut self, leg: TransferLeg) -> Result<(), TransferRejection> {
        if leg == self.source {
            self.source_terminated = true;
        } else if self.consultation == Some(leg) {
            self.consultation_terminated = true;
        } else {
            return Err(TransferRejection::WrongCall);
        }
        Ok(())
    }

    pub fn defer_action(
        &mut self,
        action: DeferredTransferAction,
    ) -> Result<(), TransferRejection> {
        if self.phase != TransferPhase::Completing {
            return Err(TransferRejection::InvalidPhase);
        }
        self.deferred_action = Some(match (self.deferred_action, action) {
            (Some(DeferredTransferAction::OnHook), _) | (_, DeferredTransferAction::OnHook) => {
                DeferredTransferAction::OnHook
            }
            _ => DeferredTransferAction::EndCall,
        });
        Ok(())
    }

    fn record_setup_milestone(
        &mut self,
        milestone: TransferSetupMilestone,
    ) -> Result<(), TransferRejection> {
        if self.mode != TransferMode::Consultation || self.phase == TransferPhase::Completing {
            return Err(TransferRejection::InvalidPhase);
        }
        self.execution_progress
            .record(milestone)
            .then_some(())
            .ok_or(TransferRejection::Conflict)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferRejection {
    Conflict,
    SameCall,
    WrongCall,
    ConsultationMissing,
    InvalidPhase,
    InvalidSelection,
    HangupCompletionDisabled,
    CompletionInProgress,
}

#[derive(Clone, Debug, Default)]
pub struct TransferRegistry {
    next_id: u64,
    by_device: HashMap<DeviceId, TransferTransaction>,
}

impl TransferRegistry {
    pub fn allocate_id(&mut self) -> TransferId {
        loop {
            let id = TransferId(self.next_id.max(1));
            self.next_id = id.0.wrapping_add(1).max(1);
            if self
                .by_device
                .values()
                .all(|transaction| transaction.id != id)
            {
                return id;
            }
        }
    }

    pub fn insert(&mut self, transaction: TransferTransaction) -> Result<(), TransferRejection> {
        if transaction.consultation.is_none() {
            return Err(TransferRejection::ConsultationMissing);
        }
        if self.by_device.contains_key(&transaction.device_id)
            || self.by_device.values().any(|existing| {
                existing.contains(transaction.source)
                    || transaction
                        .consultation
                        .is_some_and(|leg| existing.contains(leg))
            })
        {
            return Err(TransferRejection::Conflict);
        }
        self.by_device
            .insert(transaction.device_id.clone(), transaction);
        Ok(())
    }

    pub fn get(&self, device_id: &DeviceId) -> Option<&TransferTransaction> {
        self.by_device.get(device_id)
    }

    pub fn get_mut(&mut self, device_id: &DeviceId) -> Option<&mut TransferTransaction> {
        self.by_device.get_mut(device_id)
    }

    pub fn transactions(&self) -> impl Iterator<Item = &TransferTransaction> {
        self.by_device.values()
    }

    pub fn for_leg(&self, leg: TransferLeg) -> Option<&TransferTransaction> {
        self.by_device
            .values()
            .find(|transaction| transaction.contains(leg))
    }

    pub fn begin_completion(
        &mut self,
        device_id: &DeviceId,
        trigger: TransferTrigger,
        triggering_leg: TransferLeg,
    ) -> Result<TransferCompletion, TransferRejection> {
        self.by_device
            .get_mut(device_id)
            .ok_or(TransferRejection::Conflict)?
            .begin_completion(trigger, triggering_leg)
    }

    pub fn commit(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
    ) -> Option<TransferTransaction> {
        if self.by_device.get(device_id).is_none_or(|transaction| {
            transaction.id != transaction_id || transaction.phase != TransferPhase::Completing
        }) {
            return None;
        }
        self.by_device.remove(device_id)
    }

    pub fn cancel(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        reason: TransferCancellationReason,
        triggering_leg: Option<TransferLeg>,
    ) -> Result<TransferCancellation, TransferRejection> {
        let transaction = self
            .by_device
            .get(device_id)
            .filter(|transaction| transaction.id == transaction_id)
            .ok_or(TransferRejection::Conflict)?;
        let cancellation = transaction.cancellation(reason, triggering_leg)?;
        self.by_device.remove(device_id);
        Ok(cancellation)
    }

    pub fn cancel_for_leg(
        &mut self,
        leg: TransferLeg,
        reason: TransferCancellationReason,
    ) -> Result<TransferCancellation, TransferRejection> {
        let (device_id, transaction_id) = self
            .by_device
            .iter()
            .find_map(|(device_id, transaction)| {
                transaction
                    .contains(leg)
                    .then(|| (device_id.clone(), transaction.id))
            })
            .ok_or(TransferRejection::Conflict)?;
        self.cancel(&device_id, transaction_id, reason, Some(leg))
    }

    pub fn note_hangup(&mut self, leg: TransferLeg) -> Result<(), TransferRejection> {
        self.by_device
            .values_mut()
            .find(|transaction| transaction.contains(leg))
            .ok_or(TransferRejection::Conflict)?
            .note_hangup(leg)
    }

    pub fn defer_action(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        action: DeferredTransferAction,
    ) -> Result<(), TransferRejection> {
        self.by_device
            .get_mut(device_id)
            .filter(|transaction| transaction.id == transaction_id)
            .ok_or(TransferRejection::Conflict)?
            .defer_action(action)
    }

    pub fn record_setup_milestone(
        &mut self,
        device_id: &DeviceId,
        transaction_id: TransferId,
        milestone: TransferSetupMilestone,
    ) -> Result<(), TransferRejection> {
        self.by_device
            .get_mut(device_id)
            .filter(|transaction| transaction.id == transaction_id)
            .ok_or(TransferRejection::Conflict)?
            .record_setup_milestone(milestone)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(number: u8) -> DeviceId {
        DeviceId::new(format!("SEP0011223344{number:02}")).unwrap()
    }

    fn leg(number: u64) -> TransferLeg {
        TransferLeg {
            handset_call_id: CallId(number),
            pbx_call_id: number.into(),
        }
    }

    #[test]
    fn consultation_phases_allow_blind_then_attended_completion_once() {
        let mut transaction = TransferTransaction::consultation(
            TransferId(1),
            device(1),
            leg(10),
            TransferSourceState::Connected,
            false,
        );
        transaction.attach_consultation(leg(20)).unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Routing)
            .unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Ringing)
            .unwrap();
        assert_eq!(
            transaction
                .clone()
                .begin_completion(TransferTrigger::TransferKey, leg(20))
                .unwrap()
                .kind,
            TransferCompletionKind::Blind
        );
        transaction
            .advance_consultation(leg(20), TransferPhase::Connected)
            .unwrap();
        assert_eq!(
            transaction
                .begin_completion(TransferTrigger::TransferKey, leg(20))
                .unwrap()
                .kind,
            TransferCompletionKind::Attended
        );
        assert_eq!(
            transaction.begin_completion(TransferTrigger::TransferKey, leg(20)),
            Err(TransferRejection::CompletionInProgress)
        );
    }

    #[test]
    fn hangup_completion_is_snapshotted_and_requires_the_consultation_leg() {
        let mut disabled = TransferTransaction::consultation(
            TransferId(1),
            device(1),
            leg(10),
            TransferSourceState::Connected,
            false,
        );
        disabled.attach_consultation(leg(20)).unwrap();
        disabled
            .advance_consultation(leg(20), TransferPhase::Routing)
            .unwrap();
        disabled
            .advance_consultation(leg(20), TransferPhase::Ringing)
            .unwrap();
        assert_eq!(
            disabled.begin_completion(TransferTrigger::ConsultationHangup, leg(20)),
            Err(TransferRejection::HangupCompletionDisabled)
        );

        let mut enabled = TransferTransaction::consultation(
            TransferId(2),
            device(2),
            leg(30),
            TransferSourceState::Connected,
            true,
        );
        enabled.attach_consultation(leg(40)).unwrap();
        enabled
            .advance_consultation(leg(40), TransferPhase::Routing)
            .unwrap();
        enabled
            .advance_consultation(leg(40), TransferPhase::Connected)
            .unwrap();
        assert_eq!(
            enabled.begin_completion(TransferTrigger::ConsultationHangup, leg(30)),
            Err(TransferRejection::WrongCall)
        );
        assert_eq!(
            enabled
                .begin_completion(TransferTrigger::ConsultationHangup, leg(40))
                .unwrap()
                .kind,
            TransferCompletionKind::Attended
        );
    }

    #[test]
    fn invalid_phase_wrong_call_and_duplicate_consultation_are_rejected_without_mutation() {
        let mut transaction = TransferTransaction::consultation(
            TransferId(1),
            device(1),
            leg(10),
            TransferSourceState::Held,
            false,
        );
        assert_eq!(
            transaction.attach_consultation(leg(10)),
            Err(TransferRejection::SameCall)
        );
        transaction.attach_consultation(leg(20)).unwrap();
        let expected = transaction.clone();
        assert_eq!(
            transaction.attach_consultation(leg(30)),
            Err(TransferRejection::Conflict)
        );
        assert_eq!(transaction, expected);
        assert_eq!(
            transaction.advance_consultation(leg(30), TransferPhase::Routing),
            Err(TransferRejection::WrongCall)
        );
        assert_eq!(
            transaction.advance_consultation(leg(20), TransferPhase::Connected),
            Err(TransferRejection::InvalidPhase)
        );
        assert_eq!(transaction, expected);
    }

    #[test]
    fn direct_transfer_requires_distinct_calls_and_has_one_completion_phase() {
        assert_eq!(
            TransferTransaction::direct(
                TransferId(1),
                device(1),
                leg(10),
                TransferSourceState::Held,
                leg(10),
            ),
            Err(TransferRejection::SameCall)
        );
        let mut transaction = TransferTransaction::direct(
            TransferId(2),
            device(1),
            leg(10),
            TransferSourceState::Held,
            leg(20),
        )
        .unwrap();
        assert_eq!(
            transaction
                .begin_completion(TransferTrigger::TransferKey, leg(20))
                .unwrap()
                .kind,
            TransferCompletionKind::Direct
        );
        assert_eq!(
            transaction.begin_completion(TransferTrigger::TransferKey, leg(20)),
            Err(TransferRejection::CompletionInProgress)
        );
    }

    #[test]
    fn registry_serializes_devices_calls_and_generation_checked_removal() {
        let mut registry = TransferRegistry::default();
        let first_id = registry.allocate_id();
        let mut first = TransferTransaction::consultation(
            first_id,
            device(1),
            leg(10),
            TransferSourceState::Connected,
            false,
        );
        first.attach_consultation(leg(11)).unwrap();
        registry.insert(first.clone()).unwrap();
        assert_eq!(registry.get(&device(1)), Some(&first));
        assert_eq!(registry.for_leg(leg(10)), Some(&first));
        let mut same_device = TransferTransaction::consultation(
            TransferId(90),
            device(1),
            leg(20),
            TransferSourceState::Connected,
            false,
        );
        same_device.attach_consultation(leg(21)).unwrap();
        assert_eq!(
            registry.insert(same_device),
            Err(TransferRejection::Conflict)
        );
        let mut same_call = TransferTransaction::consultation(
            TransferId(91),
            device(2),
            leg(10),
            TransferSourceState::Connected,
            false,
        );
        same_call.attach_consultation(leg(22)).unwrap();
        assert_eq!(registry.insert(same_call), Err(TransferRejection::Conflict));
        assert!(registry.commit(&device(1), TransferId(999)).is_none());
        assert!(registry.commit(&device(1), first_id).is_none());
        assert_eq!(
            registry
                .cancel(
                    &device(1),
                    first_id,
                    TransferCancellationReason::EndCall,
                    Some(leg(10)),
                )
                .unwrap()
                .source_recovery,
            TransferSourceRecovery::RestoreConnected
        );
        assert!(registry.get(&device(1)).is_none());
        assert!(registry.allocate_id() > first_id);
    }

    #[test]
    fn setup_milestones_are_exact_once_and_stale_generations_cannot_mutate_a_retry() {
        let mut registry = TransferRegistry::default();
        let mut first = TransferTransaction::consultation(
            TransferId(1),
            device(1),
            leg(10),
            TransferSourceState::Connected,
            false,
        );
        first.attach_consultation(leg(11)).unwrap();
        registry.insert(first).unwrap();
        registry
            .record_setup_milestone(
                &device(1),
                TransferId(1),
                TransferSetupMilestone::SourceBackendHeld,
            )
            .unwrap();
        assert_eq!(
            registry.record_setup_milestone(
                &device(1),
                TransferId(1),
                TransferSetupMilestone::SourceBackendHeld,
            ),
            Err(TransferRejection::Conflict)
        );
        let cancellation = registry
            .cancel(
                &device(1),
                TransferId(1),
                TransferCancellationReason::ConsultationFailure,
                None,
            )
            .unwrap();
        assert!(
            cancellation
                .transaction
                .execution_progress
                .completed(TransferSetupMilestone::SourceBackendHeld)
        );

        let mut retry = TransferTransaction::consultation(
            TransferId(2),
            device(1),
            leg(20),
            TransferSourceState::Connected,
            false,
        );
        retry.attach_consultation(leg(21)).unwrap();
        registry.insert(retry).unwrap();
        assert_eq!(
            registry.record_setup_milestone(
                &device(1),
                TransferId(1),
                TransferSetupMilestone::SourceHandsetHeld,
            ),
            Err(TransferRejection::Conflict)
        );
        assert_eq!(
            registry.get(&device(1)).unwrap().execution_progress,
            TransferExecutionProgress::default()
        );
    }

    #[test]
    fn cancellation_by_either_leg_is_exact_and_does_not_touch_other_devices() {
        let mut registry = TransferRegistry::default();
        let mut first = TransferTransaction::consultation(
            TransferId(1),
            device(1),
            leg(10),
            TransferSourceState::Connected,
            false,
        );
        first.attach_consultation(leg(11)).unwrap();
        let second = TransferTransaction::direct(
            TransferId(2),
            device(2),
            leg(20),
            TransferSourceState::Held,
            leg(21),
        )
        .unwrap();
        registry.insert(first.clone()).unwrap();
        registry.insert(second.clone()).unwrap();

        let cancellation = registry
            .cancel_for_leg(leg(11), TransferCancellationReason::ConsultationHangup)
            .unwrap();
        assert_eq!(cancellation.transaction, first);
        assert_eq!(
            registry.cancel_for_leg(leg(11), TransferCancellationReason::ConsultationHangup),
            Err(TransferRejection::Conflict)
        );
        assert_eq!(registry.get(&device(2)), Some(&second));
        assert_eq!(
            registry
                .cancel_for_leg(leg(20), TransferCancellationReason::SourceHangup)
                .unwrap()
                .source_recovery,
            TransferSourceRecovery::SourceGone
        );
    }

    #[test]
    fn completion_claim_wins_over_late_hangup_and_only_exact_generation_commits() {
        let mut registry = TransferRegistry::default();
        let mut transaction = TransferTransaction::consultation(
            TransferId(41),
            device(1),
            leg(10),
            TransferSourceState::Connected,
            true,
        );
        transaction.attach_consultation(leg(20)).unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Routing)
            .unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Ringing)
            .unwrap();
        registry.insert(transaction).unwrap();

        let completion = registry
            .begin_completion(&device(1), TransferTrigger::TransferKey, leg(20))
            .unwrap();
        assert_eq!(completion.transaction_id, TransferId(41));
        assert_eq!(completion.source, leg(10));
        assert_eq!(completion.consultation, leg(20));
        assert_eq!(completion.kind, TransferCompletionKind::Blind);
        assert_eq!(
            registry.cancel_for_leg(leg(20), TransferCancellationReason::ConsultationHangup),
            Err(TransferRejection::CompletionInProgress)
        );
        assert!(registry.commit(&device(1), TransferId(40)).is_none());
        assert_eq!(
            registry
                .commit(&device(1), completion.transaction_id)
                .unwrap()
                .phase,
            TransferPhase::Completing
        );
        assert!(
            registry
                .commit(&device(1), completion.transaction_id)
                .is_none()
        );
    }

    #[test]
    fn backend_failure_can_rollback_a_claimed_completion_once() {
        let mut registry = TransferRegistry::default();
        let mut transaction = TransferTransaction::consultation(
            TransferId(9),
            device(1),
            leg(10),
            TransferSourceState::Held,
            false,
        );
        transaction.attach_consultation(leg(20)).unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Routing)
            .unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Connected)
            .unwrap();
        registry.insert(transaction).unwrap();
        registry
            .begin_completion(&device(1), TransferTrigger::TransferKey, leg(20))
            .unwrap();

        let cancellation = registry
            .cancel(
                &device(1),
                TransferId(9),
                TransferCancellationReason::BackendFailure,
                None,
            )
            .unwrap();
        assert_eq!(
            cancellation.source_recovery,
            TransferSourceRecovery::RetainHeld
        );
        assert_eq!(
            registry.cancel(
                &device(1),
                TransferId(9),
                TransferCancellationReason::BackendFailure,
                None,
            ),
            Err(TransferRejection::Conflict)
        );
    }

    #[test]
    fn non_backend_cancellation_cannot_steal_a_claimed_completion() {
        let mut registry = TransferRegistry::default();
        let transaction = TransferTransaction::direct(
            TransferId(7),
            device(1),
            leg(10),
            TransferSourceState::Held,
            leg(20),
        )
        .unwrap();
        registry.insert(transaction).unwrap();
        registry
            .begin_completion(&device(1), TransferTrigger::TransferKey, leg(20))
            .unwrap();

        for reason in [
            TransferCancellationReason::EndCall,
            TransferCancellationReason::SourceResume,
            TransferCancellationReason::ConsultationFailure,
            TransferCancellationReason::ConsultationHangup,
            TransferCancellationReason::SourceHangup,
        ] {
            assert_eq!(
                registry.cancel(&device(1), TransferId(7), reason, None),
                Err(TransferRejection::CompletionInProgress)
            );
        }
        assert_eq!(
            registry
                .cancel(
                    &device(1),
                    TransferId(7),
                    TransferCancellationReason::DeviceDisconnect,
                    None,
                )
                .unwrap()
                .reason,
            TransferCancellationReason::DeviceDisconnect
        );
    }

    #[test]
    fn completing_transfer_defers_user_actions_and_onhook_has_priority() {
        let mut transaction = TransferTransaction::consultation(
            TransferId(12),
            device(1),
            leg(10),
            TransferSourceState::Connected,
            false,
        );
        transaction.attach_consultation(leg(20)).unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Routing)
            .unwrap();
        transaction
            .advance_consultation(leg(20), TransferPhase::Connected)
            .unwrap();
        transaction
            .begin_completion(TransferTrigger::TransferKey, leg(20))
            .unwrap();

        transaction
            .defer_action(DeferredTransferAction::EndCall)
            .unwrap();
        transaction
            .defer_action(DeferredTransferAction::OnHook)
            .unwrap();
        transaction
            .defer_action(DeferredTransferAction::EndCall)
            .unwrap();
        assert_eq!(
            transaction.deferred_action,
            Some(DeferredTransferAction::OnHook)
        );
    }
}
