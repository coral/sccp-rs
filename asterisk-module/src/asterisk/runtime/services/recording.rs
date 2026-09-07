//! Recording service transactions and anchor restoration.

use super::{
    Access, AmiRecordingCommand, AnchoredRecordingSession, Arc, AsteriskBackend, CallId, CallState,
    ConfirmedRecordingAnchor, DeviceId, LogLevel, MediaAnchorMutation, PbxServiceCapabilities as _,
    PendingRecordingAnchor, PhoneCommand, PhoneCommandAction, RecordingButtonState,
    RecordingCallback, RecordingEvent, RecordingProvider as _, RecordingRegistryError,
    RecordingServiceRequest, RecordingSessionControl as _, RecordingState, RecordingTarget,
    RecordingTogglePlan, RecordingToggleRejection, RuntimeRecordingSession,
    RuntimeRecordingTrigger, RuntimeRecordings, ServiceOutcome, ServiceProviderError, ast_log,
    enqueue_recording_session_change, ordered_recording_start, ordered_recording_stop,
    plan_recording_toggle, prepare_anchor_retarget, prepare_direct_retarget,
    send_confirmed_service,
};

fn semantic_recording_button_state(armed: bool, active: bool) -> RecordingButtonState {
    match (armed, active) {
        (false, false) => RecordingButtonState::Off,
        (true, false) => RecordingButtonState::Armed,
        (false, true) => RecordingButtonState::Active,
        (true, true) => RecordingButtonState::ArmedActive,
    }
}

pub(in super::super) fn publish_recording_button_state(
    access: &Access,
    recordings: &RuntimeRecordings,
    device_id: &DeviceId,
) {
    publish_recording_button_semantics(access, device_id, recordings.device_is_active(device_id));
}

fn publish_recording_button_semantics(access: &Access, device_id: &DeviceId, active: bool) {
    if access
        .config()
        .recording_buttons_for_device(device_id)
        .next()
        .is_none()
    {
        return;
    }
    let armed = access
        .shared
        .controller
        .snapshot()
        .feature_state(device_id)
        .is_some_and(|features| features.recording_armed);
    access.spawn_phone(PhoneCommand::new(
        device_id.clone(),
        PhoneCommandAction::SetRecordingButtonStatus {
            state: semantic_recording_button_state(armed, active),
        },
    ));
}

pub fn recording_registry_service_error<E>(
    error: RecordingRegistryError<E>,
) -> ServiceProviderError {
    match error {
        RecordingRegistryError::Exists => ServiceProviderError::RecordingExists,
        RecordingRegistryError::NotFound => ServiceProviderError::RecordingNotFound,
        RecordingRegistryError::Session(_) => ServiceProviderError::RecordingFailed,
    }
}

pub async fn recording_service_operation(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    request: RecordingServiceRequest,
) -> Result<ServiceOutcome, ServiceProviderError> {
    let RecordingServiceRequest {
        command,
        call_id,
        target,
        append,
        bridged_only,
        direction,
    } = request;
    let current_owner = access
        .shared
        .controller
        .snapshot()
        .active_or_primary_call_by_pbx(call_id)
        .map(|call| super::RuntimeRecordingOwner {
            device_id: call.device_id.clone(),
            handset_call_id: call.sccp_id,
        });
    let remembered_owner = recordings.owner(call_id).cloned();
    let owner = match command {
        AmiRecordingCommand::Start => current_owner,
        AmiRecordingCommand::Stop | AmiRecordingCommand::Mute | AmiRecordingCommand::Unmute => {
            remembered_owner.or(current_owner)
        }
    }
    .ok_or(ServiceProviderError::CallNotFound)?;
    let device_id = owner.device_id.clone();
    let handset_call_id = owner.handset_call_id;
    match command {
        AmiRecordingCommand::Start => {
            if recordings.sessions.contains(call_id) {
                return Err(ServiceProviderError::RecordingExists);
            }
            let mut options = String::new();
            if append {
                options.push('a');
            }
            if bridged_only {
                options.push('b');
            }
            let target = target.ok_or(ServiceProviderError::RecordingFailed)?;
            let callback_shared = Arc::downgrade(&access.shared);
            let callback: RecordingCallback = Arc::new(move |event| {
                if event == RecordingEvent::Stopped {
                    let Some(shared) = callback_shared.upgrade() else {
                        return;
                    };
                    enqueue_recording_session_change(&shared, call_id);
                }
            });
            let mutation = MediaAnchorMutation::acquire(access, call_id)
                .await
                .ok_or(ServiceProviderError::RecordingFailed)?;
            let pending = PendingRecordingAnchor::acquire(access, call_id, &mutation)
                .map_err(|_| ServiceProviderError::RecordingFailed)?;
            let mutation_ref = &mutation;
            let (inner, anchor) = ordered_recording_start(
                confirm_recording_anchor(access, pending, mutation_ref),
                || {
                    AsteriskBackend::new(access)
                        .recordings()
                        .start_recording(call_id, target, &options, callback)
                        .map_err(|_| ServiceProviderError::RecordingFailed)
                },
                |mut anchor| async move {
                    let result = restore_recording_anchor(access, &mut anchor, mutation_ref).await;
                    if result.is_err() {
                        ast_log(
                            LogLevel::Warning,
                            &format!(
                                "unable to restore direct media after recording start failed for PBX call {}",
                                call_id.0
                            ),
                        );
                    }
                },
            )
            .await?;
            let session = RuntimeRecordingSession::new(
                AnchoredRecordingSession::new(inner, anchor),
                owner.device_id,
                owner.handset_call_id,
            );
            if let Err((error, mut session)) = recordings.sessions.insert_owned(call_id, session) {
                let _ = session.stop_native();
                let _ = restore_recording_session(access, session, &mutation).await;
                return Err(recording_registry_service_error(error));
            }
            drop(mutation);
            if let Err(error) = send_confirmed_service(
                access,
                PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::SetRecordingStatus {
                        call_id: handset_call_id,
                        active: true,
                    },
                ),
            )
            .await
            {
                if let Ok(session) = recordings.sessions.take(call_id) {
                    let mutation = MediaAnchorMutation::acquire(access, call_id)
                        .await
                        .ok_or(ServiceProviderError::RecordingFailed)?;
                    if let Err((_, session)) =
                        stop_and_restore_recording(access, session, &mutation).await
                    {
                        let _ = recordings.sessions.insert(call_id, session);
                    }
                }
                publish_recording_button_state(access, recordings, &device_id);
                return Err(error);
            }
            publish_recording_button_state(access, recordings, &device_id);
            Ok(ServiceOutcome::Recording {
                command,
                call_id,
                active: true,
                muted: false,
                affected: 0,
            })
        }
        AmiRecordingCommand::Stop => {
            let session = recordings
                .sessions
                .take(call_id)
                .map_err(recording_registry_service_error)?;
            recordings.suppress_automatic_start(call_id);
            if let Err(error) = send_confirmed_service(
                access,
                PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::SetRecordingStatus {
                        call_id: handset_call_id,
                        active: false,
                    },
                ),
            )
            .await
            {
                let _ = recordings.sessions.insert(call_id, session);
                return Err(error);
            }
            publish_recording_button_state(access, recordings, &device_id);
            let mutation = MediaAnchorMutation::acquire(access, call_id)
                .await
                .ok_or(ServiceProviderError::RecordingFailed)?;
            if let Err((error, session)) =
                stop_and_restore_recording(access, session, &mutation).await
            {
                let recording_still_active =
                    !matches!(session.state(), Ok(RecordingState::Stopped));
                let _ = recordings.sessions.insert(call_id, session);
                if recording_still_active {
                    let _ = send_confirmed_service(
                        access,
                        PhoneCommand::new(
                            device_id.clone(),
                            PhoneCommandAction::SetRecordingStatus {
                                call_id: handset_call_id,
                                active: true,
                            },
                        ),
                    )
                    .await;
                }
                publish_recording_button_state(access, recordings, &device_id);
                return Err(error);
            }
            publish_recording_button_state(access, recordings, &device_id);
            Ok(ServiceOutcome::Recording {
                command,
                call_id,
                active: false,
                muted: false,
                affected: 0,
            })
        }
        AmiRecordingCommand::Mute | AmiRecordingCommand::Unmute => {
            let muted = command == AmiRecordingCommand::Mute;
            let affected = recordings
                .sessions
                .set_muted(
                    call_id,
                    direction.ok_or(ServiceProviderError::RecordingFailed)?,
                    muted,
                )
                .map_err(recording_registry_service_error)?;
            Ok(ServiceOutcome::Recording {
                command,
                call_id,
                active: true,
                muted,
                affected,
            })
        }
    }
}

async fn confirm_recording_anchor(
    access: &Access,
    pending: PendingRecordingAnchor,
    _mutation: &MediaAnchorMutation,
) -> Result<ConfirmedRecordingAnchor, ServiceProviderError> {
    let Some(call) = pending.direct_call() else {
        return Ok(pending.confirm());
    };
    let retarget =
        prepare_anchor_retarget(access, call).ok_or(ServiceProviderError::RecordingFailed)?;
    if let Err(error) = send_confirmed_service(access, retarget.command()).await {
        retarget.rollback(access);
        return Err(error);
    }
    retarget.confirm();
    Ok(pending.confirm())
}

async fn restore_recording_anchor(
    access: &Access,
    anchor: &mut ConfirmedRecordingAnchor,
    _mutation: &MediaAnchorMutation,
) -> Result<(), ServiceProviderError> {
    if let Some(call) = anchor.restore_call() {
        let retarget =
            prepare_direct_retarget(access, &call).ok_or(ServiceProviderError::RecordingFailed)?;
        if let Err(error) = send_confirmed_service(access, retarget.command()).await {
            retarget.rollback(access);
            return Err(error);
        }
        retarget.confirm();
    }
    anchor.release();
    Ok(())
}

pub(super) async fn restore_recording_session(
    access: &Access,
    mut session: RuntimeRecordingSession,
    mutation: &MediaAnchorMutation,
) -> Result<(), (ServiceProviderError, RuntimeRecordingSession)> {
    if let Err(error) = restore_recording_anchor(access, session.anchor_mut(), mutation).await {
        return Err((error, session));
    }
    Ok(())
}

async fn stop_and_restore_recording(
    access: &Access,
    session: RuntimeRecordingSession,
    mutation: &MediaAnchorMutation,
) -> Result<(), (ServiceProviderError, RuntimeRecordingSession)> {
    ordered_recording_stop(
        session,
        |session| {
            session
                .stop_native()
                .map_err(|_| ServiceProviderError::RecordingFailed)
        },
        |session| restore_recording_session(access, session, mutation),
    )
    .await
}

pub(super) async fn handle_recording_trigger(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    trigger: RuntimeRecordingTrigger,
) {
    let pbx_id = match trigger {
        RuntimeRecordingTrigger::Eligible { pbx_id } => pbx_id,
        RuntimeRecordingTrigger::SessionChanged { pbx_id } => {
            super::prune_recording_session(access, recordings, pbx_id).await;
            return;
        }
    };
    let call = access
        .shared
        .controller
        .snapshot()
        .active_or_primary_call_by_pbx(pbx_id);
    let Some(call) = call.filter(|call| {
        matches!(call.state, CallState::Connected | CallState::Barged)
            && access
                .shared
                .controller
                .snapshot()
                .feature_state(&call.device_id)
                .is_some_and(|features| features.recording_armed)
    }) else {
        return;
    };
    if !recordings.claim_automatic_start(pbx_id) || recordings.sessions.contains(pbx_id) {
        return;
    }
    let device_id = call.device_id.clone();
    let handset_call_id = call.sccp_id;
    if let Err(error) = recording_service_operation(
        access,
        recordings,
        RecordingServiceRequest {
            command: AmiRecordingCommand::Start,
            call_id: pbx_id,
            target: Some(RecordingTarget::Automatic),
            append: false,
            bridged_only: false,
            direction: None,
        },
    )
    .await
    {
        ast_log(
            LogLevel::Warning,
            &format!("unable to start armed SCCP recording: {error}"),
        );
        publish_recording_button_state(access, recordings, &device_id);
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::DisplayPrompt {
                    call_id: handset_call_id,
                    timeout_seconds: 4,
                    text: "Recording unavailable".into(),
                },
            ))
            .await;
    }
}

pub async fn toggle_monitor_recording(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    device_id: &DeviceId,
    handset_call_id: CallId,
) -> Result<ServiceOutcome, ServiceProviderError> {
    let call = access
        .shared
        .controller
        .snapshot()
        .call(handset_call_id)
        .ok_or(ServiceProviderError::CallNotFound)?;
    let active = recordings.sessions.contains(call.pbx_id);
    let plan = plan_recording_toggle(
        device_id,
        &call.device_id,
        matches!(call.state, CallState::Connected | CallState::Barged),
        active,
    )
    .map_err(|error| match error {
        RecordingToggleRejection::Ownership => ServiceProviderError::CallOwnership,
        RecordingToggleRejection::CallState => ServiceProviderError::CallState,
    })?;
    let (command, target) = match plan {
        RecordingTogglePlan::Start => {
            (AmiRecordingCommand::Start, Some(RecordingTarget::Automatic))
        }
        RecordingTogglePlan::Stop => (AmiRecordingCommand::Stop, None),
    };
    recording_service_operation(
        access,
        recordings,
        RecordingServiceRequest {
            command,
            call_id: call.pbx_id,
            target,
            append: false,
            bridged_only: false,
            direction: None,
        },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_button_state_covers_armed_and_active_independently() {
        assert_eq!(
            semantic_recording_button_state(false, false),
            RecordingButtonState::Off
        );
        assert_eq!(
            semantic_recording_button_state(true, false),
            RecordingButtonState::Armed
        );
        assert_eq!(
            semantic_recording_button_state(false, true),
            RecordingButtonState::Active
        );
        assert_eq!(
            semantic_recording_button_state(true, true),
            RecordingButtonState::ArmedActive
        );
    }
}
