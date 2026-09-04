use super::backend::{
    AnchoredRecordingSession, ConfirmedRecordingAnchor, MediaAnchorMutation, PendingRecordingAnchor,
};
use super::media::{prepare_anchor_retarget, prepare_direct_retarget};
use super::recording::enqueue_recording_session_change;
use super::{
    Access, AsteriskBackend, BlfEvent, CallId, CallState, ControlProviderError, DeviceId,
    DriverEffect, Duration, EffectExecutionError, HandsetEffect, HashMap, HashSet, Instant,
    LogLevel, MANAGER_CONTROL_DELIVERY_TIMEOUT, MutexExt, ParkingEvent, PbxCallId, PhoneCallState,
    PhoneCommand, PhoneCommandAction, PhoneEvent, REMOTE_HANGUP_PRESENTATION_TIME, RecordingState,
    RuntimeCallSignal, RuntimeCallSignalKind, RuntimeControlRequest, RuntimeServiceRequest,
    ServiceOperation, ServiceOutcome, ServiceProviderError, Tone, ast_log, c_string,
    cancel_conference_announcement, cancel_no_answer_timer, configured_early_media,
    controller_step, execute_answer_call_transition, execute_backend_cleanup_effects,
    execute_cleanup_effects, execute_effects, execute_effects_confirmed, execute_handset_effect,
    execute_one_effect, execute_remote_hangup_plan, expire_forwarding_entries,
    expire_no_answer_routes, expire_parking_attempts, handle_blf_event, handle_effect_error,
    handle_parking_event, handle_phone_event, mpsc, native_channel, outbound_media_mode,
    publish_line, remove_channel, retry_blf, run_dnd_schedule_tick, send_handset_call_state,
    show_conference_list, take_pending_retrieval_by_pbx,
};
use super::{
    ActiveSystemMessage, AmiConferenceCommand, AmiParkingCommand, AmiRecordingCommand, Arc,
    ConferenceEndRejection, ConferenceId, ConferenceParticipantRejection, ConferencePhase,
    ControlOperation, ControlOutcome, LineInstance, MessageTarget, PARKING_CONFIRM_TIMEOUT,
    ParkingRejection, ParticipantId, PbxAudioFormat, PbxServiceCapabilities, PendingPark,
    RecordingButtonState, RecordingCallback, RecordingDirection, RecordingEvent, RecordingProvider,
    RecordingRegistryError, RecordingSessionControl, RecordingTarget, RecordingTogglePlan,
    RecordingToggleRejection, ResetMode, ResetTarget, ResetType, RuntimeRecordingOwner,
    RuntimeRecordingSession, RuntimeRecordingTrigger, RuntimeRecordings, begin_parking_retrieval,
    execute_call_transition_result, ordered_recording_start, ordered_recording_stop,
    plan_recording_toggle, preferred_codec, registered_device_ids, remove_conference_participant,
    set_conference_participant_moderator, set_conference_participant_muted,
};

mod conference;
mod control;
mod parking;
mod recording;

struct RecordingServiceRequest {
    command: AmiRecordingCommand,
    call_id: PbxCallId,
    target: Option<RecordingTarget>,
    append: bool,
    bridged_only: bool,
    direction: Option<RecordingDirection>,
}

pub use conference::{conference_participant_service_error, conference_service_operation};
pub use control::{handle_control_operation, restore_system_message};
pub use parking::{parking_service_error, parking_service_operation};
pub(super) use recording::publish_recording_button_state;
pub use recording::toggle_monitor_recording;
use recording::{handle_recording_trigger, recording_service_operation, restore_recording_session};

pub async fn run_events(
    access: Access,
    mut events: mpsc::Receiver<PhoneEvent>,
    mut blf_events: mpsc::UnboundedReceiver<BlfEvent>,
    mut parking_events: mpsc::UnboundedReceiver<ParkingEvent>,
    mut control_requests: mpsc::UnboundedReceiver<RuntimeControlRequest>,
    mut service_requests: mpsc::UnboundedReceiver<RuntimeServiceRequest>,
    mut recording_triggers: mpsc::Receiver<()>,
) {
    let mut recording_sessions = RuntimeRecordings::default();
    let mut deadlines = tokio::time::interval(Duration::from_millis(100));
    deadlines.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dnd_schedule = tokio::time::interval(Duration::from_secs(1));
    dnd_schedule.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            event = events.recv() => {
                let Some(event) = event else { return; };
                handle_phone_event(&access, &mut recording_sessions, event).await;
                prune_recording_sessions(&access, &mut recording_sessions).await;
            }
            event = blf_events.recv() => {
                let Some(event) = event else { return; };
                handle_blf_event(&access, event);
            }
            event = parking_events.recv() => {
                let Some(event) = event else { return; };
                handle_parking_event(&access, event).await;
            }
            request = control_requests.recv() => {
                let Some(request) = request else { return; };
                let result = handle_control_operation(&access, request.operation).await;
                let _ = request.response.send(result);
            }
            request = service_requests.recv() => {
                let Some(request) = request else { return; };
                let result = handle_service_operation(
                    &access,
                    &mut recording_sessions,
                    request.operation,
                ).await;
                let _ = request.response.send(result);
            }
            wake = recording_triggers.recv() => {
                let Some(()) = wake else { return; };
                for trigger in access.take_recording_triggers() {
                    handle_recording_trigger(&access, &mut recording_sessions, trigger).await;
                }
            }
            _ = deadlines.tick() => {
                retry_blf(&access, Instant::now());
                let (actions, auto_answers) = controller_step(&access.shared.controller, |controller| {
                    let now = Instant::now();
                    let mut effects = controller.expire_digits(now);
                    effects.extend(controller.expire_call_waiting_tones(now));
                    (effects, controller.expire_auto_answers(now))
                });
                execute_effects(&access, actions).await;
                for transition in auto_answers {
                    execute_answer_call_transition(&access, transition).await;
                }
                let remote_hangups = controller_step(&access.shared.controller, |controller| {
                    controller.expire_remote_hangups(Instant::now())
                });
                execute_cleanup_effects(&access, remote_hangups).await;
                expire_forwarding_entries(&access, Instant::now()).await;
                expire_no_answer_routes(&access, Instant::now()).await;
                expire_parking_attempts(&access, Instant::now()).await;
                prune_recording_sessions(&access, &mut recording_sessions).await;
            }
            _ = dnd_schedule.tick() => {
                run_dnd_schedule_tick(&access);
            }
        }
    }
}

pub async fn run_call_signals(
    access: Access,
    mut signals: mpsc::UnboundedReceiver<RuntimeCallSignal>,
) {
    let mut last_sequence = 0;
    let mut lanes = HashMap::<PbxCallId, mpsc::UnboundedSender<RuntimeCallSignal>>::new();
    while let Some(signal) = signals.recv().await {
        if signal.sequence <= last_sequence {
            ast_log(
                LogLevel::Error,
                "discarding an out-of-order SCCP call signal",
            );
            continue;
        }
        last_sequence = signal.sequence;
        lanes.retain(|_, sender| !sender.is_closed());
        let pbx_id = signal.pbx_id;
        let sender = lanes.entry(pbx_id).or_insert_with(|| {
            let (sender, mut receiver) = mpsc::unbounded_channel::<RuntimeCallSignal>();
            let lane_access = access.clone();
            access.handle.spawn(async move {
                while let Some(signal) = receiver.recv().await {
                    let terminal = matches!(signal.kind, RuntimeCallSignalKind::Hangup { .. });
                    handle_runtime_call_signal(&lane_access, signal).await;
                    if terminal {
                        break;
                    }
                }
            });
            sender
        });
        if sender.send(signal).is_err() {
            lanes.remove(&pbx_id);
        }
    }
}

pub async fn handle_runtime_call_signal(access: &Access, signal: RuntimeCallSignal) {
    let line = controller_step(&access.shared.controller, |controller| {
        controller
            .active_or_primary_call_by_pbx(signal.pbx_id)
            .map(|call| call.line)
    });
    match signal.kind {
        RuntimeCallSignalKind::StopTone => {
            let effects = controller_step(&access.shared.controller, |controller| {
                controller
                    .active_or_primary_call_by_pbx(signal.pbx_id)
                    .map(|call| {
                        HandsetEffect::StartTone {
                            device_id: call.device_id,
                            call_id: call.sccp_id,
                            tone: Tone::Silence,
                        }
                        .into()
                    })
                    .into_iter()
                    .collect()
            });
            execute_effects(access, effects).await;
        }
        RuntimeCallSignalKind::Answer { completion } => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_answer(signal.pbx_id)
            });
            if !actions.is_empty() {
                cancel_no_answer_timer(access, signal.pbx_id);
            }
            let delivered = execute_effects_confirmed(access, actions).await;
            if delivered.is_ok() {
                access.enqueue_recording_eligibility(signal.pbx_id);
            }
            let _ = completion.send(delivered);
        }
        RuntimeCallSignalKind::Hangup { handset_call_id } => {
            handle_runtime_hangup_signal(access, signal.pbx_id, handset_call_id).await;
        }
        RuntimeCallSignalKind::Proceeding => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_proceeding(signal.pbx_id)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Ringing => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_ringing(signal.pbx_id)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Progress => {
            let Some(call) = controller_step(&access.shared.controller, |controller| {
                controller.active_or_primary_call_by_pbx(signal.pbx_id)
            }) else {
                return;
            };
            let early_media = configured_early_media(access, &call.device_id, call.sccp_id);
            let media_mode = outbound_media_mode(access, &call.device_id);
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.pbx_progress_with_media_mode(signal.pbx_id, early_media, media_mode)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Busy | RuntimeCallSignalKind::Congestion => {
            let Some(call) = controller_step(&access.shared.controller, |controller| {
                controller
                    .active_or_primary_call_by_pbx(signal.pbx_id)
                    .filter(|call| call.state == CallState::Calling)
            }) else {
                return;
            };
            let state = if matches!(signal.kind, RuntimeCallSignalKind::Busy) {
                PhoneCallState::Busy
            } else {
                PhoneCallState::Congestion
            };
            if let Err(error) =
                send_handset_call_state(access, call.device_id, call.sccp_id, state).await
            {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to publish terminal handset state: {error}"),
                );
            }
        }
        RuntimeCallSignalKind::VideoUpdate => {
            let actions = controller_step(&access.shared.controller, |controller| {
                controller.refresh_video_for_pbx(signal.pbx_id)
            });
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::PartyUpdate(snapshot) => {
            let actions = controller_step(&access.shared.controller, |controller| {
                let mut effects = controller.update_call_info_by_pbx(signal.pbx_id, |current| {
                    snapshot.apply_to_call_info(current)
                });
                effects.extend(controller.pbx_remote_identity_ready(signal.pbx_id));
                effects
            });
            execute_effects(access, actions).await;
        }
    }
    if let Some(line) = line {
        publish_line(access, &line);
    }
}

pub async fn handle_runtime_hangup_signal(
    access: &Access,
    pbx_id: PbxCallId,
    handset_call_id: CallId,
) {
    if let Some(pending) = take_pending_retrieval_by_pbx(access, pbx_id) {
        access
            .shared
            .parking_registry
            .lock_unpoisoned()
            .release_claim(&pending.lot, pending.slot, handset_call_id);
    }
    let remote_hangup_tone = access.config().general.remote_hangup_tone;
    let (conference_id, plan, surviving_conference) =
        controller_step(&access.shared.controller, |controller| {
            let conference_id = controller
                .conference_session_by_pbx(pbx_id)
                .map(|session| session.id);
            let plan = controller.begin_remote_hangup(
                pbx_id,
                remote_hangup_tone,
                REMOTE_HANGUP_PRESENTATION_TIME,
                Instant::now(),
            );
            let surviving = conference_id
                .and_then(|conference_id| controller.conference_session_by_id(conference_id))
                .cloned();
            (conference_id, plan, surviving)
        });
    remove_channel(access, pbx_id);
    if let Some(plan) = plan {
        if let Some(call) = plan.outcome.primary.as_ref() {
            publish_line(access, &call.line);
        }
        if let Some(session) = surviving_conference {
            execute_cleanup_effects(access, plan.outcome.effects).await;
            let show_list = access
                .config()
                .conference_for_device(&session.device_id)
                .is_some_and(|conference| conference.show_conference_list);
            if show_list {
                show_conference_list(access, session.device_id, session.original_handset_call_id)
                    .await;
            }
        } else if let Some(conference_id) = conference_id {
            execute_cleanup_effects(access, plan.outcome.effects).await;
            cancel_conference_announcement(access, conference_id);
        } else if plan.pending.is_some() {
            execute_remote_hangup_plan(access, plan).await;
        } else {
            execute_effects(access, plan.outcome.effects).await;
        }
    } else if let Some(conference_id) = conference_id {
        cancel_conference_announcement(access, conference_id);
    }
}

pub async fn prune_recording_sessions(access: &Access, recordings: &mut RuntimeRecordings) {
    let live = controller_step(&access.shared.controller, |controller| {
        controller
            .calls()
            .map(|call| call.pbx_id)
            .collect::<HashSet<_>>()
    });
    recordings.retain_live_calls(&live);
    let finished = recordings.sessions.extract_if(|call_id, session| {
        !live.contains(&call_id) || matches!(session.state(), Ok(RecordingState::Stopped))
    });
    for (call_id, session) in finished {
        finalize_recording_session(
            access,
            recordings,
            call_id,
            session,
            live.contains(&call_id),
        )
        .await;
    }
}

async fn prune_recording_session(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    call_id: PbxCallId,
) {
    let live = controller_step(&access.shared.controller, |controller| {
        controller.calls().any(|call| call.pbx_id == call_id)
    });
    if !live {
        recordings.forget_call(call_id);
    }
    let finished = recordings.sessions.extract_if(|candidate, session| {
        candidate == call_id && (!live || matches!(session.state(), Ok(RecordingState::Stopped)))
    });
    for (call_id, session) in finished {
        finalize_recording_session(access, recordings, call_id, session, live).await;
    }
}

async fn finalize_recording_session(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    call_id: PbxCallId,
    mut session: RuntimeRecordingSession,
    live: bool,
) {
    let owner = session.owner().clone();
    if !live {
        let _ = session.stop_native();
        session.release_anchor();
        recordings.forget_call(call_id);
    } else {
        recordings.suppress_automatic_start(call_id);
        let mutation = MediaAnchorMutation::acquire(access).await;
        if let Err((_, session)) = restore_recording_session(access, session, &mutation).await {
            let _ = recordings.sessions.insert(call_id, session);
            publish_recording_button_state(access, recordings, &owner.device_id);
            return;
        }
    }
    access.spawn_phone(PhoneCommand::new(
        owner.device_id.clone(),
        PhoneCommandAction::SetRecordingStatus {
            call_id: owner.handset_call_id,
            active: false,
        },
    ));
    publish_recording_button_state(access, recordings, &owner.device_id);
}

pub async fn handle_service_operation(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    operation: ServiceOperation,
) -> Result<ServiceOutcome, ServiceProviderError> {
    match operation {
        ServiceOperation::Microphone { device_id, enabled } => {
            microphone_service_operation(access, device_id, enabled).await
        }
        ServiceOperation::Recording {
            command,
            call_id,
            filename,
            append,
            bridged_only,
            direction,
        } => {
            recording_service_operation(
                access,
                recordings,
                RecordingServiceRequest {
                    command,
                    call_id,
                    target: filename.map(RecordingTarget::ExplicitlyNamed),
                    append,
                    bridged_only,
                    direction,
                },
            )
            .await
        }
        ServiceOperation::Parking {
            command,
            device_id,
            call_id,
            line_instance,
            lot,
            slot,
        } => {
            parking_service_operation(
                access,
                command,
                device_id,
                call_id,
                line_instance,
                lot,
                slot,
            )
            .await
        }
        ServiceOperation::Conference {
            command,
            conference_id,
            participant_id,
        } => conference_service_operation(access, command, conference_id, participant_id).await,
    }
}

pub async fn microphone_service_operation(
    access: &Access,
    device_id: DeviceId,
    enabled: bool,
) -> Result<ServiceOutcome, ServiceProviderError> {
    if !access.config().devices.contains_key(&device_id) {
        return Err(ServiceProviderError::DeviceNotFound);
    }
    let call_id = controller_step(&access.shared.controller, |controller| {
        let registered = controller.registered_device(&device_id)?;
        let mut selected = registered
            .selected_calls()
            .filter(|call_id| {
                controller
                    .call(*call_id)
                    .is_some_and(|call| call.device_id == device_id)
            })
            .collect::<Vec<_>>();
        selected.sort_by_key(|call_id| call_id.0);
        if selected.len() == 1 {
            return selected.first().copied();
        }
        let mut active = controller
            .calls()
            .filter(|call| {
                call.device_id == device_id
                    && matches!(
                        call.state,
                        CallState::Connected
                            | CallState::Held
                            | CallState::SharedHeld
                            | CallState::Barged
                    )
            })
            .map(|call| call.sccp_id)
            .collect::<Vec<_>>();
        active.sort_by_key(|call_id| call_id.0);
        (active.len() == 1).then(|| active[0])
    })
    .ok_or_else(|| {
        if controller_step(&access.shared.controller, |controller| {
            controller.is_registered(&device_id)
        }) {
            ServiceProviderError::CallState
        } else {
            ServiceProviderError::DeviceNotRegistered
        }
    })?;
    send_confirmed_service(
        access,
        PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetMicrophoneMode { enabled },
        ),
    )
    .await?;
    Ok(ServiceOutcome::Microphone {
        device_id,
        call_id,
        enabled,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn send_confirmed_service(
    access: &Access,
    command: PhoneCommand,
) -> Result<(), ServiceProviderError> {
    tokio::time::timeout(
        MANAGER_CONTROL_DELIVERY_TIMEOUT,
        access.phone.send_confirmed(command),
    )
    .await
    .map_err(|_| ServiceProviderError::Delivery)?
    .map_err(|_| ServiceProviderError::Delivery)
}

pub async fn execute_service_effects(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ServiceProviderError> {
    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            handle_effect_error(access, &backend, error).await;
            return Err(ServiceProviderError::Delivery);
        }
    }
    Ok(())
}

pub async fn execute_service_cleanup(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ServiceProviderError> {
    let backend = AsteriskBackend::new(access);
    let errors = execute_backend_cleanup_effects(&backend, effects, |effect| {
        execute_handset_effect(access, effect)
    })
    .await;
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ServiceProviderError::Delivery)
    }
}

pub fn native_uniqueid_in_use(
    uniqueid: &str,
) -> Result<bool, crate::asterisk::boundary::NativeTextError> {
    let uniqueid = c_string(uniqueid)?;
    Ok(unsafe { native_channel::uniqueid_in_use(&uniqueid) })
}

#[cfg(test)]
mod uniqueid_text_tests {
    use super::*;

    #[test]
    fn assigned_uniqueid_rejects_interior_nul_before_native_lookup() {
        assert!(native_uniqueid_in_use("safe\0suffix").is_err());
    }
}

pub async fn execute_control_effects(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ControlProviderError> {
    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            let provider_error = match &error {
                EffectExecutionError::Backend { .. } => ControlProviderError::Backend,
                EffectExecutionError::Handset { .. } => ControlProviderError::HandsetDelivery,
            };
            handle_effect_error(access, &backend, error).await;
            return Err(provider_error);
        }
    }
    Ok(())
}

pub async fn execute_control_cleanup(
    access: &Access,
    effects: Vec<DriverEffect>,
) -> Result<(), ControlProviderError> {
    let backend = AsteriskBackend::new(access);
    let errors = execute_backend_cleanup_effects(&backend, effects, |effect| {
        execute_handset_effect(access, effect)
    })
    .await;
    if errors.is_empty() {
        return Ok(());
    }
    let handset_failure = errors
        .iter()
        .any(|error| matches!(error, EffectExecutionError::Handset { .. }));
    for error in errors {
        ast_log(
            LogLevel::Warning,
            &format!("SCCP management-control cleanup failed: {error}"),
        );
    }
    Err(if handset_failure {
        ControlProviderError::HandsetDelivery
    } else {
        ControlProviderError::Backend
    })
}
