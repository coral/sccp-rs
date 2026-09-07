//! SCCP attended and direct transfer orchestration.

use super::{
    Access, AsteriskBackend, CallId, DeferredTransferAction, DeviceId, DriverEffect, Duration,
    HandsetEffect, Instant, LogLevel, PbxAudioFormat, PbxEffect, PhoneCallState, PhoneCommand,
    PhoneCommandAction, Tone, TransferCancellationReason, TransferCompletion,
    TransferCompletionKind, TransferCompletionPlan, TransferConsultationRequest, TransferMode,
    TransferPhase, TransferRejection, TransferSetupMilestone, TransferTrigger, ast_log,
    execute_cleanup_effects, execute_handset_effect, execute_one_effect, handle_handset_hangup,
    native_channel, preferred_codec, remove_channel,
};
use crate::runtime::backend::ChannelBackend as _;

pub(super) async fn handle_transfer_soft_key(
    access: &Access,
    device_id: DeviceId,
    reported_call_id: Option<CallId>,
    line_instance: u32,
) {
    let existing = access
        .shared
        .controller
        .snapshot()
        .transfer_transaction_for_device(&device_id)
        .cloned();
    if let Some(existing) = existing {
        let feedback_call_id = reported_call_id
            .filter(|call_id| call_id.0 != 0)
            .or_else(|| existing.consultation.map(|leg| leg.handset_call_id))
            .unwrap_or(existing.source.handset_call_id);
        let plan = access
            .shared
            .controller
            .complete_device_transfer(&device_id, reported_call_id, TransferTrigger::TransferKey)
            .unwrap_or_else(|_| Err(crate::call::transfer::TransferRejection::Conflict));
        match plan {
            Ok(plan) => execute_transfer_completion(access, plan).await,
            Err(rejection) => {
                show_transfer_rejection(access, device_id, feedback_call_id, rejection).await
            }
        }
        return;
    }

    let Some(call_id) = reported_call_id.filter(|call_id| call_id.0 != 0) else {
        ast_log(
            LogLevel::Warning,
            &format!("transfer request for device {device_id} did not identify a source call"),
        );
        return;
    };

    let config = access.config();
    let binding = access.line_binding(&device_id, line_instance);
    let complete_on_hangup = config.general.transfer_on_hangup;
    drop(config);
    let codec = preferred_codec(access, &device_id, line_instance, &PbxAudioFormat::ALL);
    let Some((binding, codec)) = binding.zip(codec) else {
        return;
    };
    let consultation_call_id = access.phone.reserve_call_id();
    let result = access
        .shared
        .controller
        .prepare_transfer(TransferConsultationRequest {
            source_call_id: call_id,
            consultation_call_id,
            binding,
            codec,
            complete_on_hangup,
            now: Instant::now(),
        })
        .unwrap_or_else(|_| {
            (
                Err(crate::call::transfer::TransferRejection::Conflict),
                None,
            )
        });
    let (effects, transaction) = result;
    let effects = match effects {
        Ok(effects) => effects,
        Err(rejection) => {
            show_transfer_rejection(access, device_id, call_id, rejection).await;
            return;
        }
    };
    let Some(transaction) = transaction else {
        show_transfer_rejection(access, device_id, call_id, TransferRejection::Conflict).await;
        return;
    };
    execute_transfer_start(access, transaction, effects).await;
}

async fn show_transfer_rejection(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    rejection: TransferRejection,
) {
    ast_log(
        LogLevel::Warning,
        &format!(
            "transfer request rejected for device {device_id} call {}: {rejection:?}",
            call_id.0
        ),
    );
    let text = if rejection == TransferRejection::CompletionInProgress {
        "Transfer in progress"
    } else {
        "Can Not Complete Transfer"
    };
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::StartTone {
                call_id,
                tone: Tone::BeepBonk,
            },
        ))
        .await;
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 4,
                text: text.into(),
            },
        ))
        .await;
}

pub(super) async fn execute_transfer_start(
    access: &Access,
    transaction: crate::call::transfer::TransferTransaction,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if !transfer_generation_is_active(access, &transaction) {
            return;
        }
        let milestone = match &effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id })
                if *call_id == transaction.source.pbx_call_id =>
            {
                Some(TransferSetupMilestone::SourceBackendHeld)
            }
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id,
                state: PhoneCallState::Hold,
                ..
            }) if *call_id == transaction.source.handset_call_id => {
                Some(TransferSetupMilestone::SourceHandsetHeld)
            }
            DriverEffect::Backend(PbxEffect::CreateConsultationChannel { call_id, .. })
                if transaction
                    .consultation
                    .is_some_and(|leg| leg.pbx_call_id == *call_id) =>
            {
                Some(TransferSetupMilestone::ConsultationChannelCreated)
            }
            DriverEffect::Handset(HandsetEffect::BeginTransfer {
                consultation_call_id,
                ..
            }) if transaction
                .consultation
                .is_some_and(|leg| leg.handset_call_id == *consultation_call_id) =>
            {
                Some(TransferSetupMilestone::ConsultationHandsetStarted)
            }
            _ => None,
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("transfer consultation setup failed: {error}"),
            );
            if let Some(milestone) = milestone {
                compensate_unrecorded_transfer_setup(access, &backend, &transaction, milestone)
                    .await;
            }
            abort_transfer_execution(access, &transaction).await;
            return;
        }
        if let Some(milestone) = milestone
            && !record_transfer_setup_milestone(access, &transaction, milestone)
        {
            compensate_unrecorded_transfer_setup(access, &backend, &transaction, milestone).await;
            return;
        }
        if milestone.is_none() && !transfer_generation_is_active(access, &transaction) {
            close_stale_transfer_consultation(access, &transaction).await;
            return;
        }
    }
}

pub(super) fn transfer_generation_is_active(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
) -> bool {
    access
        .shared
        .controller
        .snapshot()
        .transfer_generation_is_active(&transaction.device_id, transaction.id)
}

pub(super) fn record_transfer_setup_milestone(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
    milestone: TransferSetupMilestone,
) -> bool {
    access
        .shared
        .controller
        .transfer_setup_completed(&transaction.device_id, transaction.id, milestone)
        .unwrap_or_else(|_| Err(crate::call::transfer::TransferRejection::Conflict))
        .is_ok()
}

pub(super) async fn compensate_unrecorded_transfer_setup(
    access: &Access,
    backend: &AsteriskBackend<'_>,
    transaction: &crate::call::transfer::TransferTransaction,
    milestone: TransferSetupMilestone,
) {
    match milestone {
        TransferSetupMilestone::SourceBackendHeld => {
            let _ = backend.resume(transaction.source.pbx_call_id);
        }
        TransferSetupMilestone::SourceHandsetHeld => {
            let _ = execute_handset_effect(
                access,
                HandsetEffect::SetCallState {
                    device_id: transaction.device_id.clone(),
                    call_id: transaction.source.handset_call_id,
                    state: PhoneCallState::Connected,
                    stop_media: false,
                },
            )
            .await;
        }
        TransferSetupMilestone::ConsultationChannelCreated => {
            if let Some(consultation) = transaction.consultation {
                let _ = backend.hangup(consultation.pbx_call_id);
                remove_channel(access, consultation.pbx_call_id);
            }
        }
        TransferSetupMilestone::ConsultationHandsetStarted => {
            if let Some(consultation) = transaction.consultation {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        transaction.device_id.clone(),
                        PhoneCommandAction::CloseCall {
                            call_id: consultation.handset_call_id,
                        },
                    ))
                    .await;
            }
        }
    }
}

pub(super) async fn close_stale_transfer_consultation(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
) {
    if let Some(consultation) = transaction.consultation {
        let _ = execute_handset_effect(
            access,
            HandsetEffect::SetCallState {
                device_id: transaction.device_id.clone(),
                call_id: consultation.handset_call_id,
                state: PhoneCallState::OnHook,
                stop_media: true,
            },
        )
        .await;
    }
}

pub(super) async fn abort_transfer_execution(
    access: &Access,
    transaction: &crate::call::transfer::TransferTransaction,
) {
    let outcome = access
        .shared
        .controller
        .abort_transfer(
            &transaction.device_id,
            transaction.id,
            TransferCancellationReason::ConsultationFailure,
        )
        .unwrap_or_else(|_| Err(crate::call::transfer::TransferRejection::Conflict));
    if let Ok(outcome) = outcome {
        let consultation_created = outcome
            .transaction
            .execution_progress
            .completed(crate::call::transfer::TransferSetupMilestone::ConsultationChannelCreated);
        execute_cleanup_effects(access, outcome.effects).await;
        if consultation_created && let Some(consultation) = transaction.consultation {
            remove_channel(access, consultation.pbx_call_id);
        }
    }
}

pub(super) async fn handle_direct_transfer(access: &Access, device_id: DeviceId) {
    let (plan, active_call) = access
        .shared
        .controller
        .prepare_direct_transfer(device_id.clone())
        .unwrap_or_else(|_| {
            (
                Err(crate::call::transfer::TransferRejection::Conflict),
                None,
            )
        });
    match plan {
        Ok(plan) => execute_transfer_completion(access, plan).await,
        Err(rejection) => {
            if let Some(call_id) = active_call {
                show_transfer_rejection(access, device_id, call_id, rejection).await;
            }
        }
    }
}

pub(super) async fn execute_transfer_completion(access: &Access, plan: TransferCompletionPlan) {
    let completion = plan.completion;
    if !access
        .shared
        .controller
        .snapshot()
        .transfer_generation_is_active(&completion.device_id, completion.transaction_id)
    {
        return;
    }
    debug_assert!(matches!(
        plan.effects.as_slice(),
        [DriverEffect::Backend(PbxEffect::Transfer { operation })] if operation == &completion
    ));
    let _ = access
        .phone
        .send(PhoneCommand::new(
            completion.device_id.clone(),
            PhoneCommandAction::DisplayPrompt {
                call_id: completion.consultation.handset_call_id,
                timeout_seconds: 0,
                text: "Completing transfer".into(),
            },
        ))
        .await;
    ast_log(
        LogLevel::Notice,
        &format!(
            "starting {:?} transfer {} for device {} between PBX calls {} and {}",
            completion.kind,
            completion.transaction_id.0,
            completion.device_id,
            completion.source.pbx_call_id.0,
            completion.consultation.pbx_call_id.0,
        ),
    );

    let worker = match access.shared.workers.try_reserve() {
        Ok(worker) => worker,
        Err(_) => {
            finish_transfer_completion(
                access,
                completion,
                native_channel::AttendedTransferResult::Failed,
                Duration::ZERO,
            )
            .await;
            return;
        }
    };
    let started = Instant::now();
    let task_access = access.clone();
    worker.spawn(async move {
        let backend = AsteriskBackend::new(&task_access);
        let native = execute_one_effect(
            &task_access,
            &backend,
            0,
            DriverEffect::Backend(PbxEffect::Transfer { operation: completion.clone() }),
        );
        tokio::pin!(native);
        let outcome = tokio::select! {
            result = &mut native => result,
            _ = tokio::time::sleep(Duration::from_secs(5)) => {
                ast_log(LogLevel::Warning, &format!("transfer {} for device {} is still pending in Asterisk after 5 seconds", completion.transaction_id.0, completion.device_id));
                native.await
            }
        };
        let result = if outcome.is_ok() {
            native_channel::AttendedTransferResult::Success
        } else {
            native_channel::AttendedTransferResult::Failed
        };
        finish_transfer_completion(&task_access, completion, result, started.elapsed()).await;
    });
}

async fn finish_transfer_completion(
    access: &Access,
    completion: TransferCompletion,
    result: native_channel::AttendedTransferResult,
    elapsed: Duration,
) {
    let active = access
        .shared
        .controller
        .snapshot()
        .transfer_generation_is_active(&completion.device_id, completion.transaction_id);
    if !active {
        return;
    }
    ast_log(
        if result == native_channel::AttendedTransferResult::Success {
            LogLevel::Notice
        } else {
            LogLevel::Warning
        },
        &format!(
            "transfer {} for device {} completed as {result:?} after {} ms",
            completion.transaction_id.0,
            completion.device_id,
            elapsed.as_millis(),
        ),
    );

    if result == native_channel::AttendedTransferResult::Success {
        let outcome = access
            .shared
            .controller
            .transfer_succeeded(&completion.device_id, completion.transaction_id)
            .unwrap_or_else(|_| None);
        if let Some(outcome) = outcome {
            execute_cleanup_effects(access, outcome.effects).await;
        }
        remove_channel(access, completion.source.pbx_call_id);
        remove_channel(access, completion.consultation.pbx_call_id);
        return;
    }

    let outcome = access
        .shared
        .controller
        .abort_transfer(
            &completion.device_id,
            completion.transaction_id,
            TransferCancellationReason::BackendFailure,
        )
        .unwrap_or_else(|_| Err(crate::call::transfer::TransferRejection::Conflict));
    if let Ok(outcome) = outcome {
        let deferred = outcome.transaction.deferred_action;
        execute_cleanup_effects(access, outcome.effects).await;
        if completion.kind != TransferCompletionKind::Direct {
            remove_channel(access, completion.consultation.pbx_call_id);
        }
        show_transfer_rejection(
            access,
            completion.device_id.clone(),
            completion.source.handset_call_id,
            TransferRejection::Conflict,
        )
        .await;
        if deferred == Some(DeferredTransferAction::OnHook) {
            handle_handset_hangup(access, completion.source.handset_call_id, true).await;
        }
    }
}

pub(super) async fn cancel_transfer(
    access: &Access,
    transaction: crate::call::transfer::TransferTransaction,
    reason: TransferCancellationReason,
) -> bool {
    let outcome = access
        .shared
        .controller
        .abort_transfer(&transaction.device_id, transaction.id, reason)
        .unwrap_or_else(|_| Err(crate::call::transfer::TransferRejection::Conflict));
    let Ok(outcome) = outcome else {
        return false;
    };
    execute_cleanup_effects(access, outcome.effects).await;
    if transaction.mode == TransferMode::Consultation
        && let Some(consultation) = transaction.consultation
    {
        remove_channel(access, consultation.pbx_call_id);
    }
    true
}

pub(super) async fn handle_transfer_hangup(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    physical: bool,
) -> bool {
    let transaction = access
        .shared
        .controller
        .snapshot()
        .transfer_transaction(call_id)
        .cloned();
    let Some(transaction) = transaction else {
        return false;
    };
    if transaction.phase == TransferPhase::Completing {
        let action = if physical {
            DeferredTransferAction::OnHook
        } else {
            DeferredTransferAction::EndCall
        };
        let deferred = access
            .shared
            .controller
            .defer_transfer_action(&transaction.device_id, transaction.id, action)
            .unwrap_or_else(|_| Err(crate::call::transfer::TransferRejection::Conflict));
        if deferred.is_ok() {
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::DisplayPrompt {
                        call_id,
                        timeout_seconds: 4,
                        text: "Transfer in progress".into(),
                    },
                ))
                .await;
        }
        return true;
    }
    if physical
        && transaction
            .consultation
            .is_some_and(|leg| leg.handset_call_id == call_id)
    {
        let plan = access
            .shared
            .controller
            .complete_transfer(&device_id, call_id, TransferTrigger::ConsultationHangup)
            .unwrap_or_else(|_| Err(crate::call::transfer::TransferRejection::Conflict));
        if let Ok(plan) = plan {
            execute_transfer_completion(access, plan).await;
            return true;
        }
    }
    let source_hung_up = transaction.source.handset_call_id == call_id;
    let reason = if physical && source_hung_up {
        TransferCancellationReason::SourceHangup
    } else if physical {
        TransferCancellationReason::ConsultationHangup
    } else {
        TransferCancellationReason::EndCall
    };
    let cancelled = cancel_transfer(access, transaction, reason).await;
    if physical && source_hung_up {
        !cancelled
    } else {
        true
    }
}
