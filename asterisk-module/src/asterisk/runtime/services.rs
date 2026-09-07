use super::backend::{
    AnchoredRecordingSession, ConfirmedRecordingAnchor, MediaAnchorMutation, PendingRecordingAnchor,
};
use super::media::{prepare_anchor_retarget, prepare_direct_retarget};
use super::recording::enqueue_recording_session_change;
use super::{
    Access, AsteriskBackend, CallId, CallState, ControlProviderError, DeviceId, DriverEffect,
    Duration, EffectExecutionError, HandsetEffect, HashSet, Instant, LogLevel,
    MANAGER_CONTROL_DELIVERY_TIMEOUT, PbxCallId, PhoneCallState, PhoneCommand, PhoneCommandAction,
    PhoneEvent, REMOTE_HANGUP_PRESENTATION_TIME, RecordingState, RuntimeCallSignal,
    RuntimeCallSignalKind, RuntimeControlRequest, RuntimeServiceRequest, RwLockExt as _,
    ServiceOperation, ServiceOutcome, ServiceProviderError, Tone, ast_log, c_string,
    cancel_conference_announcement, cancel_no_answer_timer, configured_early_media,
    execute_answer_call_transition, execute_backend_cleanup_effects, execute_cleanup_effects,
    execute_effects, execute_effects_confirmed, execute_forwarding_expiry, execute_handset_effect,
    execute_no_answer_route, execute_one_effect, execute_remote_hangup_plan, handle_effect_error,
    handle_phone_event, mpsc, native_channel, outbound_media_mode, publish_line, remove_channel,
    retry_blf, run_dnd_schedule_tick, send_handset_call_state, show_conference_list,
};
use super::{
    ActiveSystemMessage, AmiConferenceCommand, AmiParkingCommand, AmiRecordingCommand, Arc,
    ConferenceEndRejection, ConferenceId, ConferenceParticipantRejection, ConferencePhase,
    ControlOperation, ControlOutcome, LineInstance, MessageTarget, PARKING_CONFIRM_TIMEOUT,
    ParkingRejection, ParticipantId, PbxAudioFormat, PbxServiceCapabilities, RecordingButtonState,
    RecordingCallback, RecordingDirection, RecordingEvent, RecordingProvider,
    RecordingRegistryError, RecordingSessionControl, RecordingTarget, RecordingTogglePlan,
    RecordingToggleRejection, ResetMode, ResetTarget, ResetType, RuntimeRecordingOwner,
    RuntimeRecordingSession, RuntimeRecordingTrigger, RuntimeRecordings, begin_parking_retrieval,
    execute_call_transition_result, ordered_recording_start, ordered_recording_stop,
    plan_recording_toggle, preferred_codec, registered_device_ids, remove_conference_participant,
    set_conference_participant_moderator, set_conference_participant_muted,
};
use crate::runtime::mailbox::MailboxReceiver;

mod conference;
mod control;
mod event_owner;
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

#[derive(Default)]
pub(super) struct EventDiagnostics {
    pub(super) queues: Vec<(&'static str, crate::runtime::mailbox::QueueMonitor)>,
}

#[derive(Default)]
struct RuntimeEventState {
    recording_sessions: RuntimeRecordings,
    system_message: Option<ActiveSystemMessage>,
}

async fn catch_dispatcher_panic(
    future: impl std::future::Future<Output = ()>,
) -> Result<(), Box<dyn std::any::Any + Send>> {
    let mut future = std::pin::pin!(future);
    std::future::poll_fn(|context| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            future.as_mut().poll(context)
        })) {
            Ok(std::task::Poll::Ready(())) => std::task::Poll::Ready(Ok(())),
            Ok(std::task::Poll::Pending) => std::task::Poll::Pending,
            Err(panic) => std::task::Poll::Ready(Err(panic)),
        }
    })
    .await
}

pub async fn run_events(
    access: Access,
    mut events: mpsc::Receiver<PhoneEvent>,
    mut priority_events: mpsc::Receiver<sccp_protocol::server::PriorityEvent>,
    mut control_requests: MailboxReceiver<RuntimeControlRequest>,
    mut service_requests: MailboxReceiver<RuntimeServiceRequest>,
    mut recording_triggers: mpsc::Receiver<()>,
    mut shutdown: tokio::sync::oneshot::Receiver<()>,
    mut finish_shutdown: tokio::sync::oneshot::Receiver<()>,
    ordinary_drained: tokio::sync::oneshot::Sender<()>,
) {
    use crate::runtime::mailbox::{RUNTIME_MAILBOX_CAPACITY, mailbox};
    use event_owner::{EventOperation, EventOwner, EventResource};

    let mut owner = EventOwner::new();
    let mut ordinary_drained = Some(ordinary_drained);
    let (events_admission, mut admitted_events) = mailbox(RUNTIME_MAILBOX_CAPACITY);
    let (priority_admission, mut admitted_priority) = mailbox(RUNTIME_MAILBOX_CAPACITY);
    let mut priority_open = true;
    let mut finishing = false;
    let (timer_admission, mut admitted_timers) = mailbox(2);
    let (deadline_admission, mut admitted_deadlines) = mailbox(RUNTIME_MAILBOX_CAPACITY);
    *access.shared.event_diagnostics.write_unpoisoned() = Arc::new(EventDiagnostics {
        queues: vec![
            ("events", events_admission.monitor()),
            ("priority-events", priority_admission.monitor()),
            ("event-maintenance", timer_admission.monitor()),
            ("deadline-effects", deadline_admission.monitor()),
        ],
    });
    let mut deadline_effects_open = true;
    let mut controls_open = true;
    let mut services_open = true;
    let mut phones_open = true;
    let mut recording_open = true;
    let mut stopping = false;
    let mut deadlines = tokio::time::interval(Duration::from_millis(100));
    deadlines.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut dnd_schedule = tokio::time::interval(Duration::from_secs(1));
    dnd_schedule.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut failure = None;
    loop {
        let outcome = catch_dispatcher_panic(async {
    loop {
        if stopping { owner.start_ready(&access); }
        let ordinary_capacity = events_admission.snapshot().outstanding < RUNTIME_MAILBOX_CAPACITY;
        if stopping
            && !controls_open
            && !services_open
            && !deadline_effects_open
            && events_admission.snapshot().outstanding == 0
            && timer_admission.snapshot().outstanding == 0
            && priority_admission.snapshot().outstanding == 0
            && owner.workers.is_empty()
        {
            if let Some(drained) = ordinary_drained.take() {
                let _ = drained.send(());
            }
        }
        if finishing
            && !priority_open
            && !controls_open
            && !services_open
            && !deadline_effects_open
            && priority_admission.snapshot().outstanding == 0
            && owner.workers.is_empty()
        {
            break;
        }
        tokio::select! {
            biased;
            _ = &mut shutdown, if !stopping => {
                stopping = true;
                events.close();
                recording_triggers.close();
                phones_open = false;
                recording_open = false;
                owner.cancel_waiting();
                events_admission.close();
                timer_admission.close();
                deadline_admission.close();
                while admitted_events.try_recv().is_ok() {}
                while admitted_timers.try_recv().is_ok() {}
                access.shared.control_requests.close();
                access.shared.service_requests.close();
            }
            _ = &mut finish_shutdown, if !finishing => {
                finishing = true;
                priority_events.close();
            }
            event = priority_events.recv(), if priority_open => {
                match event {
                    Some(event) => {
                        let (event, permit) = event.into_parts();
                        let operation = match event {
                            PhoneEvent::Device(event) if stopping && matches!(&event.event, sccp_protocol::DeviceEventKind::Registered(_)) => {
                                EventOperation::RejectRegistration { device: event.device_id, generation: event.session_generation, permit: Some(permit) }
                            }
                            event => EventOperation::PriorityPhone(event, permit),
                        };
                        // Every protocol permit remains held through completion,
                        // so this equally sized mailbox always has an available slot.
                        assert!(priority_admission.try_send(operation, None).is_ok(),
                            "priority event admission must match protocol budget");
                    }
                    None => priority_open = false,
                }
            }
            admitted = admitted_priority.recv(), if priority_admission.snapshot().outstanding != 0 => {
                if let Some(admitted) = admitted {
                    let admitted = if stopping { admitted.map(EventOperation::after_admission_close) } else { admitted };
                    owner.admit(&access, admitted).await;
                }
            }
            completion = owner.workers.join_next_with_id(), if !owner.workers.is_empty() => {
                match completion {
                    Some(Ok((id, completion))) => owner.complete(&access, id, completion),
                    Some(Err(error)) => { owner.failed(&access, error.id()); ast_log(LogLevel::Error, &format!("runtime event job failed: {error}")); }
                    None => {}
                }
            }
            _ = deadlines.tick(), if !stopping => {
                owner.start_ready(&access);
                if let Ok(admission) = deadline_admission.try_reserve() {
                    let effects = event_owner::prepare_deadline_effects(&access).await;
                    if !effects.is_empty() { let _ = admission.send(EventOperation::DeadlineEffects(effects), None); }
                }
                if !owner.is_pending(&EventResource::Deadlines) {
                    let _ = timer_admission.try_send(EventOperation::Deadlines, None);
                }
                for device in owner.recorded_devices() {
                    if events_admission.snapshot().outstanding == RUNTIME_MAILBOX_CAPACITY { break; }
                    if !owner.is_pending(&EventResource::Device(device.clone())) {
                        let _ = events_admission.try_send(EventOperation::Prune(device), None);
                    }
                }
            }
            _ = dnd_schedule.tick(), if !stopping => {
                if !owner.is_pending(&EventResource::DndSchedule) {
                    let _ = timer_admission.try_send(EventOperation::DndSchedule, None);
                }
            }
            admitted = admitted_deadlines.recv(), if deadline_effects_open => {
                match admitted { Some(admitted) => owner.admit(&access, admitted).await, None => deadline_effects_open = false }
            }
            admitted = admitted_timers.recv(), if !stopping => {
                if let Some(admitted) = admitted { owner.admit(&access, admitted).await; }
            }
            admitted = admitted_events.recv(), if !stopping => {
                if let Some(admitted) = admitted { owner.admit(&access, admitted).await; }
            }
            event = events.recv(), if phones_open && ordinary_capacity => {
                match event {
                    Some(event) => { let _ = events_admission.try_send(EventOperation::Phone(event), None); }
                    None => phones_open = false,
                }
            }
            request = control_requests.recv(), if controls_open => {
                let Some(request) = request else { controls_open = false; continue; };
                if stopping || access.shared.control_requests.is_closed() {
                    let _ = request.value.response.send(Err(ControlProviderError::Unavailable));
                } else { owner.admit(&access, request.map(EventOperation::Control)).await; }
            }
            request = service_requests.recv(), if services_open => {
                let Some(request) = request else { services_open = false; continue; };
                if stopping || access.shared.service_requests.is_closed() {
                    let _ = request.value.response.send(Err(ServiceProviderError::Unavailable));
                } else { owner.admit(&access, request.map(EventOperation::Service)).await; }
            }
            wake = recording_triggers.recv(), if recording_open && ordinary_capacity => {
                match wake {
                    Some(()) => {
                        for trigger in access.take_recording_triggers() {
                            if events_admission.try_send(EventOperation::Recording(trigger), None).is_err() {
                                super::recording::enqueue_recording_trigger(&access.shared, trigger);
                            }
                        }
                    }
                    None => recording_open = false,
                }
            }
        }
        if !phones_open
            && !controls_open
            && !services_open
            && !recording_open
            && !priority_open
            && priority_admission.snapshot().outstanding == 0
            && owner.workers.is_empty()
        {
            break;
        }
    }
        }).await;
        match outcome {
            Ok(()) => break,
            Err(panic) => {
                if failure.is_none() {
                    failure = Some(panic);
                }
                // Keep the actual jobs and priority receiver alive while the
                // module completes its ordinary and native shutdown phases.
                stopping = true;
                events.close();
                recording_triggers.close();
                phones_open = false;
                recording_open = false;
                events_admission.close();
                timer_admission.close();
                deadline_admission.close();
                access.shared.control_requests.close();
                access.shared.service_requests.close();
                while admitted_events.try_recv().is_ok() {}
                while admitted_timers.try_recv().is_ok() {}
                owner.recover_after_panic();
            }
        }
    }
    if let Some(panic) = failure {
        std::panic::resume_unwind(panic);
    }
}

pub async fn run_call_signals(
    access: Access,
    signals: crate::runtime::call_queue::CallMailbox<PbxCallId, RuntimeCallSignalKind>,
) {
    struct Executor(Access);
    impl crate::runtime::call_queue::CallExecutor<PbxCallId, RuntimeCallSignalKind> for Executor {
        fn prepare_terminal(
            &self,
            pbx_id: PbxCallId,
            command: RuntimeCallSignalKind,
        ) -> impl std::future::Future<Output = RuntimeCallSignalKind> + Send + 'static {
            let access = self.0.clone();
            async move {
                match command {
                    RuntimeCallSignalKind::Hangup { .. } => RuntimeCallSignalKind::PreparedHangup(
                        Box::new(prepare_runtime_hangup(&access, pbx_id).await),
                    ),
                    command => command,
                }
            }
        }
        fn spawn(
            &self,
            effect: crate::runtime::call_queue::CallEffect<PbxCallId, RuntimeCallSignalKind>,
            workers: &mut tokio::task::JoinSet<()>,
        ) -> tokio::task::AbortHandle {
            let access = self.0.clone();
            workers.spawn_blocking(move || {
                access.handle.block_on(async {
                    if *effect.retiring.borrow()
                        && !matches!(&effect.command, RuntimeCallSignalKind::PreparedHangup(_))
                    {
                        if let RuntimeCallSignalKind::Answer { completion } = effect.command {
                            let _ = completion.send(Err(super::RuntimeCallSignalDeliveryError));
                        }
                        return;
                    }
                    handle_runtime_call_signal(
                        &access,
                        RuntimeCallSignal {
                            sequence: effect.sequence,
                            pbx_id: effect.key,
                            kind: effect.command,
                        },
                    )
                    .await;
                })
            })
        }
    }
    crate::runtime::call_queue::CallQueue::new(signals, Executor(access))
        .run()
        .await;
}

pub async fn handle_runtime_call_signal(access: &Access, signal: RuntimeCallSignal) {
    ast_log(
        LogLevel::Debug,
        &format!(
            "processing native call signal {} for PBX call {}",
            signal.sequence, signal.pbx_id.0
        ),
    );
    let line = access
        .shared
        .controller
        .snapshot()
        .active_or_primary_call_by_pbx(signal.pbx_id)
        .map(|call| call.line);
    match signal.kind {
        RuntimeCallSignalKind::StopTone => {
            let effects = access
                .shared
                .controller
                .snapshot()
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
                .collect();
            execute_effects(access, effects).await;
        }
        RuntimeCallSignalKind::Answer { completion } => {
            let actions = access
                .shared
                .controller
                .pbx_answer(signal.pbx_id)
                .unwrap_or_else(|_| Vec::new());
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
        RuntimeCallSignalKind::PreparedHangup(prepared) => {
            execute_prepared_runtime_hangup(access, signal.pbx_id, *prepared).await;
        }
        RuntimeCallSignalKind::Proceeding => {
            let actions = access
                .shared
                .controller
                .pbx_proceeding(signal.pbx_id)
                .unwrap_or_else(|_| Vec::new());
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Ringing => {
            let actions = access
                .shared
                .controller
                .pbx_ringing(signal.pbx_id)
                .unwrap_or_else(|_| Vec::new());
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Progress => {
            let Some(call) = access
                .shared
                .controller
                .snapshot()
                .active_or_primary_call_by_pbx(signal.pbx_id)
            else {
                return;
            };
            let early_media = configured_early_media(access, &call.device_id, call.sccp_id);
            let media_mode = outbound_media_mode(access, &call.device_id);
            let actions = access
                .shared
                .controller
                .pbx_progress_with_media_mode(signal.pbx_id, early_media, media_mode)
                .unwrap_or_else(|_| Vec::new());
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::Busy | RuntimeCallSignalKind::Congestion => {
            let Some(call) = access
                .shared
                .controller
                .snapshot()
                .active_or_primary_call_by_pbx(signal.pbx_id)
                .filter(|call| call.state == CallState::Calling)
            else {
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
            let actions = access
                .shared
                .controller
                .snapshot()
                .refresh_video_for_pbx(signal.pbx_id);
            execute_effects(access, actions).await;
        }
        RuntimeCallSignalKind::PartyUpdate(snapshot) => {
            let actions = access
                .shared
                .controller
                .apply_party_snapshot(signal.pbx_id, *snapshot)
                .unwrap_or_else(|_| Vec::new());
            execute_effects(access, actions).await;
        }
    }
    if let Some(line) = line {
        publish_line(access, &line);
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeHangupPreparation {
    conference_id: Option<ConferenceId>,
    plan: Option<crate::runtime::controller::RemoteHangupPlan>,
    surviving_conference: Option<crate::runtime::controller::ConferenceSession>,
}

async fn prepare_runtime_hangup(access: &Access, pbx_id: PbxCallId) -> RuntimeHangupPreparation {
    let remote_hangup_tone = access.config().general.remote_hangup_tone;
    let (conference_id, plan, surviving_conference) = access
        .shared
        .controller
        .prepare_remote_hangup_async(
            pbx_id,
            remote_hangup_tone,
            REMOTE_HANGUP_PRESENTATION_TIME,
            Instant::now(),
        )
        .await
        .unwrap_or_else(|_| (None, None, None));
    RuntimeHangupPreparation {
        conference_id,
        plan,
        surviving_conference,
    }
}

pub async fn handle_runtime_hangup_signal(
    access: &Access,
    pbx_id: PbxCallId,
    _handset_call_id: CallId,
) {
    let prepared = prepare_runtime_hangup(access, pbx_id).await;
    execute_prepared_runtime_hangup(access, pbx_id, prepared).await;
}

async fn execute_prepared_runtime_hangup(
    access: &Access,
    pbx_id: PbxCallId,
    prepared: RuntimeHangupPreparation,
) {
    let RuntimeHangupPreparation {
        conference_id,
        mut plan,
        surviving_conference,
    } = prepared;
    if let Some(plan) = &mut plan {
        if let Some(token) = plan.pending {
            let active = access
                .shared
                .controller
                .activate_remote_hangup_async(
                    token,
                    REMOTE_HANGUP_PRESENTATION_TIME,
                    Instant::now(),
                )
                .await
                .unwrap_or(false);
            if !active {
                let primary = plan.outcome.primary.as_ref().map(|call| call.sccp_id);
                plan.outcome.effects.retain(|effect| match effect {
                    DriverEffect::Handset(effect) => {
                        super::handset_effect_call_id(effect).is_none_or(|id| Some(id) != primary)
                    }
                    _ => true,
                });
                plan.pending = None;
            }
        }
    }
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
    let live = access
        .shared
        .controller
        .snapshot()
        .calls()
        .map(|call| call.pbx_id)
        .collect::<HashSet<_>>();
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
    let live = access
        .shared
        .controller
        .snapshot()
        .calls()
        .any(|call| call.pbx_id == call_id);
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
        let Some(mutation) = MediaAnchorMutation::acquire(access, call_id).await else {
            let _ = recordings.sessions.insert(call_id, session);
            return;
        };
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
    let call_id = (|controller: &crate::runtime::controller::ControllerSnapshot| {
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
    })(access.shared.controller.snapshot().as_ref())
    .ok_or_else(|| {
        if access
            .shared
            .controller
            .snapshot()
            .is_registered(&device_id)
        {
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
