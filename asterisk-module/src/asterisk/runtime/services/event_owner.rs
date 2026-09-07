//! Event reservations and owned session handoff. Native work never runs here.

use std::collections::{HashMap, HashSet, VecDeque};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Instant;

use tokio::task::JoinSet;

use super::super::{CallTransition, DriverEffect, PbxEffect, handset_effect_call_id};
use super::{
    Access, ActiveSystemMessage, ControlOperation, ControlProviderError, DeviceId, LogLevel,
    MessageTarget, PbxCallId, PhoneEvent, ResetTarget, RuntimeControlRequest, RuntimeEventState,
    RuntimeRecordingTrigger, RuntimeRecordings, RuntimeServiceRequest, ServiceOperation,
    ServiceProviderError, ast_log, execute_answer_call_transition, execute_cleanup_effects,
    execute_effects, execute_forwarding_expiry, execute_no_answer_route, handle_control_operation,
    handle_phone_event, handle_recording_trigger, handle_service_operation,
    prune_recording_sessions, retry_blf, run_dnd_schedule_tick,
};
use crate::runtime::mailbox::{Admitted, WorkPermit};
use sccp_protocol::DeviceEventKind as PhoneDeviceEventKind;

pub(super) enum EventOperation {
    Phone(PhoneEvent),
    RejectRegistration {
        device: DeviceId,
        generation: sccp_protocol::SessionGeneration,
        permit: Option<sccp_protocol::server::PriorityEventPermit>,
    },
    Session(
        crate::asterisk::phone::PreparedSessionEvent,
        Option<sccp_protocol::server::PriorityEventPermit>,
    ),
    PriorityPhone(PhoneEvent, sccp_protocol::server::PriorityEventPermit),
    Control(RuntimeControlRequest),
    Service(RuntimeServiceRequest),
    Recording(RuntimeRecordingTrigger),
    Deadlines,
    DeadlineEffects(Vec<DeadlineJob>),
    DndSchedule,
    Prune(DeviceId),
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub(super) enum EventResource {
    Device(DeviceId),
    Call(PbxCallId),
    Media(DeviceId, sccp_protocol::CallId),
    SystemMessage,
    Deadlines,
    DndSchedule,
}

struct PendingEvent {
    admitted: Admitted<EventOperation>,
    resources: HashSet<EventResource>,
}

pub(super) struct EventCompletion {
    resources: HashSet<EventResource>,
    recordings: RuntimeRecordings,
    _admission: Arc<WorkPermit>,
    _priority: Option<sccp_protocol::server::PriorityEventPermit>,
}

pub(super) struct EventOwner {
    state: RuntimeEventState,
    waiting: VecDeque<PendingEvent>,
    reserved: HashSet<EventResource>,
    pub(super) workers: JoinSet<EventCompletion>,
    jobs: HashMap<tokio::task::Id, HashSet<EventResource>>,
}

pub(super) enum DeadlineJob {
    Effects(Vec<DriverEffect>),
    Answer(CallTransition),
    Forwarding(crate::call::forwarding::ForwardingExpiryOutcome),
    NoAnswer(crate::call::forwarding::NoAnswerTimer, Option<String>),
    Cleanup(Vec<DriverEffect>),
    Parking(crate::runtime::controller::parking::ParkingUpdate),
}

pub(super) async fn prepare_deadline_effects(access: &Access) -> Vec<DeadlineJob> {
    let (effects, answers) = access
        .shared
        .controller
        .expire_call_deadlines_async(Instant::now())
        .await
        .unwrap_or_default();
    let snapshot = access.shared.controller.snapshot();
    let mut groups: HashMap<Option<PbxCallId>, Vec<DriverEffect>> = HashMap::new();
    for effect in effects {
        let call_id = match &effect {
            DriverEffect::Handset(effect) => handset_effect_call_id(effect)
                .and_then(|id| snapshot.call(id))
                .map(|call| call.pbx_id),
            DriverEffect::Backend(
                PbxEffect::CreateChannel { call_id, .. }
                | PbxEffect::CreateConsultationChannel { call_id, .. }
                | PbxEffect::StartRouting { call_id, .. }
                | PbxEffect::Answer { call_id }
                | PbxEffect::Hangup { call_id }
                | PbxEffect::SendDigit { call_id, .. }
                | PbxEffect::ConfigureMedia { call_id, .. }
                | PbxEffect::ConfigureMediaOnly { call_id, .. }
                | PbxEffect::Hold { call_id }
                | PbxEffect::Resume { call_id },
            ) => Some(*call_id),
            _ => None,
        };
        groups.entry(call_id).or_default().push(effect);
    }
    let mut jobs = groups
        .into_values()
        .map(DeadlineJob::Effects)
        .collect::<Vec<_>>();
    jobs.extend(answers.into_iter().map(DeadlineJob::Answer));
    let cleanup = access
        .shared
        .controller
        .expire_remote_hangups_async(Instant::now())
        .await
        .unwrap_or_default();
    if !cleanup.is_empty() {
        jobs.push(DeadlineJob::Cleanup(cleanup));
    }
    let now = Instant::now();
    let parking = access
        .shared
        .controller
        .expire_parking_attempts_async(now, now + super::super::PARKING_NOTIFICATION_TIME)
        .await
        .unwrap_or_default();
    if !parking.effects.is_empty()
        || !parking.prompts.is_empty()
        || !parking.close.is_empty()
        || !parking.retired_peers.is_empty()
    {
        jobs.push(DeadlineJob::Parking(parking));
    }
    let now = Instant::now();
    jobs.extend(
        access
            .shared
            .controller
            .expire_forwarding_entries_async(now)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(DeadlineJob::Forwarding),
    );
    jobs.extend(
        access
            .shared
            .controller
            .claim_no_answer_routes_async(now)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|(timer, line)| DeadlineJob::NoAnswer(timer, line)),
    );
    jobs
}

impl EventOperation {
    pub(super) fn after_admission_close(self) -> Self {
        match self {
            Self::PriorityPhone(PhoneEvent::Device(event), permit)
                if matches!(&event.event, PhoneDeviceEventKind::Registered(_)) =>
            {
                Self::RejectRegistration {
                    device: event.device_id,
                    generation: event.session_generation,
                    permit: Some(permit),
                }
            }
            operation => operation,
        }
    }

    fn reject(self) {
        match self {
            Self::Control(request) => {
                let _ = request
                    .response
                    .send(Err(ControlProviderError::Unavailable));
            }
            Self::Service(request) => {
                let _ = request
                    .response
                    .send(Err(ServiceProviderError::Unavailable));
            }
            _ => {}
        }
    }
}

fn media_acknowledgement_call(event: &PhoneDeviceEventKind) -> Option<sccp_protocol::CallId> {
    match event {
        PhoneDeviceEventKind::ReceiveChannelOpened { call_id, .. }
        | PhoneDeviceEventKind::MultimediaReceiveChannelOpened { call_id, .. }
        | PhoneDeviceEventKind::MultimediaReceiveChannelFailed { call_id, .. }
        | PhoneDeviceEventKind::MultimediaReceiveChannelTimedOut { call_id, .. }
        | PhoneDeviceEventKind::MultimediaTransmitStarted { call_id, .. }
        | PhoneDeviceEventKind::MultimediaTransmitFailed { call_id, .. }
        | PhoneDeviceEventKind::MultimediaTransmitTimedOut { call_id, .. }
        | PhoneDeviceEventKind::TransmitChannelOpen { call_id, .. }
        | PhoneDeviceEventKind::HandsetAcknowledgementTimedOut { call_id, .. }
        | PhoneDeviceEventKind::MediaTransmissionFailed { call_id, .. }
        | PhoneDeviceEventKind::MulticastReceptionStarted { call_id, .. }
        | PhoneDeviceEventKind::MulticastReceptionFailed { call_id, .. }
        | PhoneDeviceEventKind::MulticastReceptionTimedOut { call_id, .. }
        | PhoneDeviceEventKind::MulticastTransmissionStarted { call_id, .. }
        | PhoneDeviceEventKind::MulticastTransmissionFailed { call_id, .. } => Some(*call_id),
        PhoneDeviceEventKind::ConnectionStatisticsCollected { snapshot } => Some(snapshot.call_id),
        _ => None,
    }
}

impl EventOwner {
    pub(super) fn new() -> Self {
        Self {
            state: RuntimeEventState::default(),
            waiting: VecDeque::new(),
            reserved: HashSet::new(),
            workers: JoinSet::new(),
            jobs: HashMap::new(),
        }
    }

    pub(super) fn is_pending(&self, resource: &EventResource) -> bool {
        self.reserved.contains(resource)
            || self
                .waiting
                .iter()
                .any(|event| event.resources.contains(resource))
    }

    pub(super) fn recorded_devices(&self) -> HashSet<DeviceId> {
        self.state
            .recording_sessions
            .sessions
            .iter()
            .map(|(_, session)| session.owner().device_id.clone())
            .collect()
    }

    fn resources(&self, access: &Access, operation: &EventOperation) -> HashSet<EventResource> {
        let snapshot = access.shared.controller.snapshot();
        let mut devices = HashSet::new();
        let mut calls = HashSet::new();
        let mut resources = HashSet::new();
        match operation {
            EventOperation::Phone(PhoneEvent::Device(event))
            | EventOperation::PriorityPhone(PhoneEvent::Device(event), _) => {
                if matches!(
                    &event.event,
                    PhoneDeviceEventKind::OnHook { .. }
                        | PhoneDeviceEventKind::Disconnected {}
                        | PhoneDeviceEventKind::PhoneServiceResponse { .. }
                        | PhoneDeviceEventKind::SoftKey {
                            soft_key: sccp_protocol::SoftKey::EndCall,
                            ..
                        }
                ) {
                    return resources;
                }
                if let Some(call_id) = media_acknowledgement_call(&event.event) {
                    resources.insert(EventResource::Media(event.device_id.clone(), call_id));
                    return resources;
                }
                devices.insert(event.device_id.clone());
            }
            EventOperation::Phone(_) | EventOperation::PriorityPhone(_, _) => {}
            EventOperation::RejectRegistration { .. } => {}
            EventOperation::Session(prepared, _) => {
                if prepared.is_disconnected() {
                    return resources;
                }
                devices.insert(prepared.device_id.clone());
            }
            EventOperation::Control(request) => match &request.operation {
                ControlOperation::Message { target, .. } => match target {
                    MessageTarget::Device(device) => {
                        devices.insert(device.clone());
                    }
                    MessageTarget::RegisteredDevices | MessageTarget::System => {
                        resources.insert(EventResource::SystemMessage);
                    }
                },
                ControlOperation::Reset { target, .. } => match target {
                    ResetTarget::Device(device) => {
                        devices.insert(device.clone());
                    }
                    ResetTarget::RegisteredDevices => {
                        devices.extend(
                            snapshot
                                .registered_devices()
                                .map(|(device_id, _)| device_id.clone()),
                        );
                    }
                },
                ControlOperation::Answer { call_id, device_id } => {
                    devices.extend(device_id.iter().cloned());
                    if let Some(call) = snapshot.call(*call_id) {
                        calls.insert(call.pbx_id);
                        devices.insert(call.device_id);
                    }
                }
                ControlOperation::End { .. } => {}
                ControlOperation::Originate { device_id, .. } => {
                    devices.insert(device_id.clone());
                }
            },
            EventOperation::Service(request) => match &request.operation {
                ServiceOperation::Microphone { device_id, .. }
                | ServiceOperation::Parking { device_id, .. } => {
                    devices.insert(device_id.clone());
                }
                ServiceOperation::Recording { call_id, .. } => {
                    calls.insert(*call_id);
                }
                ServiceOperation::Conference { conference_id, .. } => {
                    if let Some(conference) = snapshot.conference_session_by_id(*conference_id) {
                        devices.insert(conference.device_id.clone());
                    }
                }
            },
            EventOperation::Recording(
                RuntimeRecordingTrigger::Eligible { pbx_id }
                | RuntimeRecordingTrigger::SessionChanged { pbx_id },
            ) => {
                calls.insert(*pbx_id);
            }
            EventOperation::Deadlines => {
                resources.insert(EventResource::Deadlines);
            }
            EventOperation::DeadlineEffects(_) => {}
            EventOperation::DndSchedule => {
                resources.insert(EventResource::DndSchedule);
            }
            EventOperation::Prune(device) => {
                devices.insert(device.clone());
            }
        }
        for call_id in &calls {
            if let Some(call) = snapshot.active_or_primary_call_by_pbx(*call_id) {
                devices.insert(call.device_id);
            }
            if let Some(owner) = self.state.recording_sessions.owner(*call_id) {
                devices.insert(owner.device_id.clone());
            }
        }
        for call in snapshot.calls() {
            if devices.contains(&call.device_id) {
                calls.insert(call.pbx_id);
            }
        }
        for (call_id, session) in self.state.recording_sessions.sessions.iter() {
            if devices.contains(&session.owner().device_id) {
                calls.insert(call_id);
            }
        }
        resources.extend(devices.into_iter().map(EventResource::Device));
        resources.extend(calls.into_iter().map(EventResource::Call));
        resources
    }

    pub(super) async fn admit(&mut self, access: &Access, admitted: Admitted<EventOperation>) {
        let preparation = match &admitted.value {
            EventOperation::Phone(PhoneEvent::Device(event))
            | EventOperation::PriorityPhone(PhoneEvent::Device(event), _)
                if matches!(
                    &event.event,
                    PhoneDeviceEventKind::Registered(_) | PhoneDeviceEventKind::Disconnected {}
                ) =>
            {
                Some(crate::asterisk::phone::prepare_session_event(access, event).await)
            }
            _ => None,
        };
        let admitted = match preparation {
            Some(Some(prepared)) => admitted.map(|operation| {
                let priority = match operation {
                    EventOperation::PriorityPhone(_, permit) => Some(permit),
                    _ => None,
                };
                EventOperation::Session(prepared, priority)
            }),
            Some(None) => return,
            None => admitted,
        };
        let resources = self.resources(access, &admitted.value);
        self.waiting.push_back(PendingEvent {
            admitted,
            resources,
        });
        self.start_ready(access);
    }

    pub(super) fn cancel_waiting(&mut self) {
        let waiting = std::mem::take(&mut self.waiting);
        for pending in waiting {
            if matches!(
                pending.admitted.value,
                EventOperation::DeadlineEffects(_)
                    | EventOperation::PriorityPhone(_, _)
                    | EventOperation::Session(_, _)
                    | EventOperation::RejectRegistration { .. }
            ) {
                self.waiting.push_back(pending);
            } else {
                pending.admitted.value.reject();
            }
        }
    }

    pub(super) fn start_ready(&mut self, access: &Access) {
        let live_calls = access
            .shared
            .controller
            .snapshot()
            .calls()
            .map(|call| call.pbx_id)
            .collect();
        self.state.recording_sessions.retain_live_calls(&live_calls);
        let waiting = std::mem::take(&mut self.waiting);
        let mut blocked = HashSet::new();
        for mut pending in waiting {
            if pending.admitted.is_expired(Instant::now()) {
                pending.admitted.record_expiration();
                pending.admitted.value.reject();
                continue;
            }
            // Device/call membership can change while a request waits. Expand
            // its conservative keys and revalidate before native work starts.
            pending
                .resources
                .extend(self.resources(access, &pending.admitted.value));
            if pending
                .resources
                .iter()
                .any(|resource| self.reserved.contains(resource) || blocked.contains(resource))
            {
                blocked.extend(pending.resources.iter().cloned());
                self.waiting.push_back(pending);
                continue;
            }
            if matches!(&pending.admitted.value, EventOperation::DeadlineEffects(_)) {
                let (operation, permit) = pending.admitted.into_parts();
                if let EventOperation::DeadlineEffects(jobs) = operation {
                    let permit = Arc::new(permit);
                    for job in jobs {
                        let access = access.clone();
                        let permit = Arc::clone(&permit);
                        let task = self.workers.spawn_blocking(move || {
                            let outcome = catch_unwind(AssertUnwindSafe(|| {
                                access.handle.block_on(async {
                                    match job {
                                        DeadlineJob::Effects(effects) => {
                                            execute_effects(&access, effects).await
                                        }
                                        DeadlineJob::Answer(answer) => {
                                            execute_answer_call_transition(&access, answer).await;
                                        }
                                        DeadlineJob::Cleanup(effects) => {
                                            execute_cleanup_effects(&access, effects).await
                                        }
                                        DeadlineJob::Forwarding(outcome) => {
                                            execute_forwarding_expiry(&access, outcome).await
                                        }
                                        DeadlineJob::NoAnswer(timer, line) => {
                                            execute_no_answer_route(&access, timer, line).await
                                        }
                                        DeadlineJob::Parking(update) => {
                                            super::super::execute_parking_update(&access, update)
                                                .await;
                                        }
                                    }
                                })
                            }));
                            if outcome.is_err() {
                                ast_log(LogLevel::Error, "runtime deadline worker panicked");
                            }
                            EventCompletion {
                                resources: HashSet::new(),
                                recordings: RuntimeRecordings::default(),
                                _admission: permit,
                                _priority: None,
                            }
                        });
                        self.jobs.insert(task.id(), HashSet::new());
                    }
                }
                continue;
            }
            self.reserved.extend(pending.resources.iter().cloned());
            let call_ids = pending
                .resources
                .iter()
                .filter_map(|resource| match resource {
                    EventResource::Call(call_id) => Some(*call_id),
                    _ => None,
                })
                .collect();
            let mut recordings = self.state.recording_sessions.extract_calls(&call_ids);
            self.prepare_system_message(&pending.admitted.value);
            let mut system_message = self.state.system_message.clone();
            let access = access.clone();
            let (operation, admission) = pending.admitted.into_parts();
            let (operation, priority) = match operation {
                EventOperation::PriorityPhone(event, permit) => {
                    (EventOperation::Phone(event), Some(permit))
                }
                EventOperation::Session(prepared, permit) => {
                    (EventOperation::Session(prepared, None), permit)
                }
                EventOperation::RejectRegistration {
                    device,
                    generation,
                    permit,
                } => (
                    EventOperation::RejectRegistration {
                        device,
                        generation,
                        permit: None,
                    },
                    permit,
                ),
                other => (other, None),
            };
            let resources = pending.resources.clone();
            let task = self.workers.spawn_blocking(move || {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    access.handle.block_on(async {
                        execute(&access, &mut recordings, &mut system_message, operation).await;
                        prune_recording_sessions(&access, &mut recordings).await;
                    })
                }));
                if result.is_err() {
                    ast_log(LogLevel::Error, "runtime event worker panicked");
                }
                EventCompletion {
                    resources: pending.resources,
                    recordings,
                    _admission: Arc::new(admission),
                    _priority: priority,
                }
            });
            self.jobs.insert(task.id(), resources);
        }
    }

    fn prepare_system_message(&mut self, operation: &EventOperation) {
        if self.state.system_message.as_ref().is_some_and(|message| {
            message
                .expires_at
                .is_some_and(|expiry| expiry <= Instant::now())
        }) {
            self.state.system_message = None;
        }
        if let EventOperation::Control(RuntimeControlRequest {
            operation:
                ControlOperation::Message {
                    target: MessageTarget::System,
                    text,
                    beep,
                    timeout_seconds,
                },
            ..
        }) = operation
        {
            self.state.system_message = Some(ActiveSystemMessage {
                text: text.clone(),
                beep: *beep,
                expires_at: (*timeout_seconds != 0).then(|| {
                    Instant::now() + std::time::Duration::from_secs(u64::from(*timeout_seconds))
                }),
            });
        }
    }

    pub(super) fn recover_after_panic(&mut self) {
        self.reserved = self
            .jobs
            .values()
            .flat_map(|resources| resources.iter().cloned())
            .collect();
        self.cancel_waiting();
    }

    pub(super) fn failed(&mut self, access: &Access, id: tokio::task::Id) {
        if let Some(resources) = self.jobs.remove(&id) {
            for resource in resources {
                self.reserved.remove(&resource);
            }
        }
        self.start_ready(access);
    }

    pub(super) fn complete(
        &mut self,
        access: &Access,
        id: tokio::task::Id,
        completion: EventCompletion,
    ) {
        self.jobs.remove(&id);
        self.state
            .recording_sessions
            .return_sessions(completion.recordings);
        for resource in completion.resources {
            self.reserved.remove(&resource);
        }
        self.start_ready(access);
    }
}

async fn execute(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    system_message: &mut Option<ActiveSystemMessage>,
    operation: EventOperation,
) {
    match operation {
        EventOperation::RejectRegistration {
            device, generation, ..
        } => {
            let _ = access.phone.disconnect_session(device, generation).await;
        }
        EventOperation::Phone(event) | EventOperation::PriorityPhone(event, _) => {
            handle_phone_event(access, recordings, system_message, event).await
        }
        EventOperation::Session(prepared, _) => {
            crate::asterisk::phone::handle_prepared_session_event(
                access,
                recordings,
                system_message,
                prepared,
            )
            .await;
        }
        EventOperation::Control(request) => {
            let result = handle_control_operation(access, request.operation).await;
            let _ = request.response.send(result);
        }
        EventOperation::Service(request) => {
            let result = handle_service_operation(access, recordings, request.operation).await;
            let _ = request.response.send(result);
        }
        EventOperation::Recording(trigger) => {
            handle_recording_trigger(access, recordings, trigger).await
        }
        EventOperation::Deadlines => {
            retry_blf(access, Instant::now());
        }
        EventOperation::DeadlineEffects(_) => {}
        EventOperation::DndSchedule => run_dnd_schedule_tick(access),
        EventOperation::Prune(_) => {}
    }
}
