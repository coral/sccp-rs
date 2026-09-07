//! Optional completion admission that keeps station readers independent of ordinary input load.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::{
    ApplicationId, CallId, CallReference, ClientMessage, CommandAction, ConferenceId,
    DeviceEventKind, Event, LineInstance, PHONE_BACKGROUND_APPLICATION_ID,
    PHONE_RINGTONE_APPLICATION_ID, PhoneServiceRouting, ServerError, SessionCommand, SessionState,
    SoftKey, TransactionId, TransmitOpenOutcome,
};

/// One reserved station completion. Retain its permit until application work completes.
#[derive(Debug)]
pub struct PriorityEvent {
    event: Event,
    permit: PriorityEventPermit,
}

impl PriorityEvent {
    pub fn into_parts(self) -> (Event, PriorityEventPermit) {
        (self.event, self.permit)
    }
}

/// Admission retained across dequeue, application scheduling, and completion.
#[derive(Debug)]
pub struct PriorityEventPermit {
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone, Debug)]
pub(super) struct PrioritySender {
    sender: mpsc::Sender<PriorityEvent>,
    budget: Arc<Semaphore>,
}

impl PrioritySender {
    pub(super) fn channel(capacity: usize) -> (Self, mpsc::Receiver<PriorityEvent>) {
        let (sender, receiver) = mpsc::channel(capacity);
        (
            Self {
                sender,
                budget: Arc::new(Semaphore::new(capacity)),
            },
            receiver,
        )
    }

    fn reserve(&self) -> Result<CompletionReservation, ServerError> {
        let permit = self
            .budget
            .clone()
            .try_acquire_owned()
            .map_err(|_| ServerError::CommandQueueFull)?;
        let slot = self
            .sender
            .clone()
            .try_reserve_owned()
            .map_err(|_| ServerError::CommandQueueFull)?;
        Ok(CompletionReservation {
            slot,
            permit: PriorityEventPermit { _permit: permit },
        })
    }
}

#[derive(Debug)]
pub(super) struct CompletionReservation {
    slot: mpsc::OwnedPermit<PriorityEvent>,
    permit: PriorityEventPermit,
}

impl CompletionReservation {
    fn send(self, event: Event) {
        self.slot.send(PriorityEvent {
            event,
            permit: self.permit,
        });
    }
}

pub(super) enum EventPermit {
    Ordinary(mpsc::OwnedPermit<Event>),
    Priority(CompletionReservation),
}

impl EventPermit {
    pub(super) fn send(self, event: Event) {
        match self {
            Self::Ordinary(slot) => {
                slot.send(event);
            }
            Self::Priority(slot) => slot.send(event),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum CompletionKey {
    Registered,
    Disconnected,
    Terminal(CallId),
    Receive(CallId),
    Transmit(CallId),
    TransmitFailure(CallId),
    VideoReceive(CallId),
    VideoTransmit(CallId),
    MulticastReceive(CallId, ConferenceId),
    MulticastTransmit(CallId, ConferenceId),
    MulticastFailure(CallId, ConferenceId),
    Service(PhoneServiceRouting),
}

impl CompletionKey {
    fn call(self) -> Option<CallId> {
        match self {
            Self::Terminal(call)
            | Self::Receive(call)
            | Self::Transmit(call)
            | Self::TransmitFailure(call)
            | Self::VideoReceive(call)
            | Self::VideoTransmit(call)
            | Self::MulticastReceive(call, _)
            | Self::MulticastTransmit(call, _)
            | Self::MulticastFailure(call, _) => Some(call),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
struct Reservations {
    completions: HashMap<CompletionKey, CompletionReservation>,
    ordinary: Vec<mpsc::OwnedPermit<Event>>,
    new_call: Option<CompletionReservation>,
}

/// The mutex protects only delivery permits used by helpers on the same station task.
#[derive(Clone, Debug)]
pub(super) struct EventSender {
    ordinary: mpsc::Sender<Event>,
    priority: Option<PrioritySender>,
    reservations: Arc<Mutex<Reservations>>,
}

impl From<mpsc::Sender<Event>> for EventSender {
    fn from(ordinary: mpsc::Sender<Event>) -> Self {
        Self {
            ordinary,
            priority: None,
            reservations: Arc::default(),
        }
    }
}

impl EventSender {
    pub(super) fn session(ordinary: mpsc::Sender<Event>, priority: Option<PrioritySender>) -> Self {
        Self {
            ordinary,
            priority,
            reservations: Arc::default(),
        }
    }

    fn reserve_keys(
        &self,
        keys: impl IntoIterator<Item = CompletionKey>,
    ) -> Result<(), ServerError> {
        let Some(priority) = &self.priority else {
            return Ok(());
        };
        let mut reservations = self
            .reservations
            .lock()
            .expect("station event delivery poisoned");
        let mut added = Vec::new();
        for key in keys {
            if !reservations.completions.contains_key(&key)
                && !added.iter().any(|(present, _)| *present == key)
            {
                added.push((key, priority.reserve()?));
            }
        }
        reservations.completions.extend(added);
        Ok(())
    }

    pub(super) fn reserve_registration(&self) -> Result<(), ServerError> {
        self.reserve_keys([CompletionKey::Registered, CompletionKey::Disconnected])
    }

    /// Reserve the largest ordinary event batch before any input changes station state.
    pub(super) fn begin_input(
        &self,
        message: &ClientMessage,
    ) -> Result<InputDelivery<'_>, ServerError> {
        let Some(priority) = &self.priority else {
            return Ok(InputDelivery(self));
        };
        if is_completion(message)
            || service_transaction(message).is_some_and(|transaction| {
                self.reservations
                    .lock()
                    .expect("station event delivery poisoned")
                    .completions
                    .contains_key(&CompletionKey::Service(transaction))
            })
        {
            return Ok(InputDelivery(self));
        }
        let mut ordinary = Vec::with_capacity(3);
        for _ in 0..3 {
            ordinary.push(
                self.ordinary
                    .clone()
                    .try_reserve_owned()
                    .map_err(|_| ServerError::CommandQueueFull)?,
            );
        }
        let new_call = may_create_call(message)
            .then(|| priority.reserve())
            .transpose()?;
        let mut reservations = self
            .reservations
            .lock()
            .expect("station event delivery poisoned");
        reservations.ordinary = ordinary;
        reservations.new_call = new_call;
        Ok(InputDelivery(self))
    }

    pub(super) fn reserve_command(&self, command: &SessionCommand) -> Result<(), ServerError> {
        let mut keys = Vec::new();
        match command {
            SessionCommand::OfferIncoming { call_id, .. } => {
                keys.push(CompletionKey::Terminal(*call_id))
            }
            SessionCommand::Public(command) | SessionCommand::Confirmed { command, .. } => {
                match &command.action {
                    CommandAction::BeginCall { call_id, .. } => {
                        keys.push(CompletionKey::Terminal(*call_id))
                    }
                    CommandAction::BeginTransfer {
                        consultation_call_id,
                        ..
                    } => keys.push(CompletionKey::Terminal(*consultation_call_id)),
                    CommandAction::OpenReceiveChannel { call_id, .. } => {
                        keys.push(CompletionKey::Receive(*call_id))
                    }
                    CommandAction::OpenOutboundMedia { call_id, .. } => keys.extend([
                        CompletionKey::Receive(*call_id),
                        CompletionKey::Transmit(*call_id),
                        CompletionKey::TransmitFailure(*call_id),
                    ]),
                    CommandAction::StartMedia { call_id, .. } => keys.extend([
                        CompletionKey::Transmit(*call_id),
                        CompletionKey::TransmitFailure(*call_id),
                    ]),
                    CommandAction::OpenMultimediaReceiveChannel { call_id, .. } => {
                        keys.push(CompletionKey::VideoReceive(*call_id))
                    }
                    CommandAction::StartMultimediaTransmission { call_id, .. } => {
                        keys.push(CompletionKey::VideoTransmit(*call_id))
                    }
                    CommandAction::StartMulticastReception {
                        call_id,
                        conference_id,
                        ..
                    } => keys.push(CompletionKey::MulticastReceive(*call_id, *conference_id)),
                    CommandAction::StartMulticastTransmission {
                        call_id,
                        conference_id,
                        ..
                    } => keys.extend([
                        CompletionKey::MulticastTransmit(*call_id, *conference_id),
                        CompletionKey::MulticastFailure(*call_id, *conference_id),
                    ]),
                    CommandAction::ShowInputService {
                        application_id,
                        line_instance,
                        call_reference,
                        transaction_id,
                        ..
                    }
                    | CommandAction::ExecutePhoneActions {
                        application_id,
                        line_instance,
                        call_reference,
                        transaction_id,
                        ..
                    } => {
                        keys.push(CompletionKey::Service(PhoneServiceRouting {
                            application_id: *application_id,
                            line_instance: *line_instance,
                            call_reference: *call_reference,
                            transaction_id: *transaction_id,
                        }));
                    }
                    CommandAction::SetBackgroundImage { transaction_id, .. }
                    | CommandAction::DisplayBackgroundImage { transaction_id, .. }
                    | CommandAction::PreviewBackgroundImage { transaction_id, .. } => {
                        keys.push(CompletionKey::Service(service_route(
                            PHONE_BACKGROUND_APPLICATION_ID,
                            *transaction_id,
                        )));
                    }
                    CommandAction::SetRingtone { transaction_id, .. } => {
                        keys.push(CompletionKey::Service(service_route(
                            PHONE_RINGTONE_APPLICATION_ID,
                            *transaction_id,
                        )))
                    }
                    _ => {}
                }
            }
        }
        self.reserve_keys(keys)
    }

    pub(super) fn cancel_service_response(&self, routing: PhoneServiceRouting) {
        self.reservations
            .lock()
            .expect("station event delivery poisoned")
            .completions
            .remove(&CompletionKey::Service(routing));
    }

    pub(super) fn reconcile_calls(&self, state: &SessionState) {
        if self.priority.is_none() {
            return;
        }
        let mut reservations = self
            .reservations
            .lock()
            .expect("station event delivery poisoned");
        reservations.completions.retain(|key, _| {
            use super::{
                MediaChannelState, MulticastKey, MulticastReceiveState, TransmitConfirmation,
            };
            let Some(call_id) = key.call() else {
                return true;
            };
            let Some(call) = state.calls_by_id.get(&call_id) else {
                return false;
            };
            match key {
                CompletionKey::Receive(_) => call.media.receive.state == MediaChannelState::Opening,
                CompletionKey::Transmit(_) => matches!(
                    call.media.transmit_confirmation,
                    TransmitConfirmation::Awaiting { .. } | TransmitConfirmation::NotReported
                ),
                CompletionKey::TransmitFailure(_) => {
                    call.media.transmit.state != MediaChannelState::Closed
                }
                CompletionKey::VideoReceive(_) => call
                    .video_receive
                    .leg
                    .as_ref()
                    .is_some_and(|leg| leg.state == MediaChannelState::Opening),
                CompletionKey::VideoTransmit(_) => call
                    .video_transmit
                    .leg
                    .as_ref()
                    .is_some_and(|leg| leg.state == MediaChannelState::Opening),
                CompletionKey::MulticastReceive(_, conference_id) => state
                    .multicast
                    .get(&MulticastKey {
                        call_id,
                        conference_id: *conference_id,
                    })
                    .and_then(|media| media.receive.as_ref())
                    .is_some_and(|receive| {
                        matches!(
                            receive.state,
                            MulticastReceiveState::AwaitingAcknowledgement { .. }
                        )
                    }),
                CompletionKey::MulticastTransmit(_, conference_id)
                | CompletionKey::MulticastFailure(_, conference_id) => state
                    .multicast
                    .get(&MulticastKey {
                        call_id,
                        conference_id: *conference_id,
                    })
                    .is_some_and(|media| media.transmit.is_some()),
                _ => true,
            }
        });
        if let Some((&call, _)) = state.calls_by_id.iter().find(|(call, _)| {
            !reservations
                .completions
                .contains_key(&CompletionKey::Terminal(**call))
        }) && let Some(terminal) = reservations.new_call.take()
        {
            reservations
                .completions
                .insert(CompletionKey::Terminal(call), terminal);
        }
    }

    pub(super) async fn send(&self, event: Event) -> Result<(), mpsc::error::SendError<Event>> {
        if self.priority.is_none() {
            return self.ordinary.send(event).await;
        }
        let key = event_key(&event);
        let mut reservations = self
            .reservations
            .lock()
            .expect("station event delivery poisoned");
        if let Event::Device(device) = &event
            && let DeviceEventKind::OffHook { call_id, .. } = device.event
            && !reservations
                .completions
                .contains_key(&CompletionKey::Terminal(call_id))
            && let Some(terminal) = reservations.new_call.take()
        {
            reservations
                .completions
                .insert(CompletionKey::Terminal(call_id), terminal);
        }
        if let Some(key) = key {
            if let Some(reservation) = reservations.completions.remove(&key) {
                reservation.send(event);
                return Ok(());
            }
            if !matches!(key, CompletionKey::Service(_)) {
                // Duplicate terminal/media events cannot consume another operation's reservation.
                return Ok(());
            }
        }
        if let Some(slot) = reservations.ordinary.pop() {
            slot.send(event);
            return Ok(());
        }
        match self.ordinary.try_send(event) {
            Ok(()) | Err(mpsc::error::TrySendError::Full(_)) => Ok(()),
            Err(mpsc::error::TrySendError::Closed(event)) => Err(mpsc::error::SendError(event)),
        }
    }

    pub(super) async fn registration_permit(&self) -> Result<EventPermit, ServerError> {
        if self.priority.is_some() {
            let permit = self
                .reservations
                .lock()
                .expect("station event delivery poisoned")
                .completions
                .remove(&CompletionKey::Registered)
                .ok_or(ServerError::CommandQueueFull)?;
            Ok(EventPermit::Priority(permit))
        } else {
            self.ordinary
                .clone()
                .reserve_owned()
                .await
                .map(EventPermit::Ordinary)
                .map_err(|_| ServerError::Stopped)
        }
    }

    pub(super) async fn disconnect_permit(&self, stopping: bool) -> Option<EventPermit> {
        if self.priority.is_some() {
            self.reservations
                .lock()
                .expect("station event delivery poisoned")
                .completions
                .remove(&CompletionKey::Disconnected)
                .map(EventPermit::Priority)
        } else if stopping {
            self.ordinary
                .clone()
                .try_reserve_owned()
                .ok()
                .map(EventPermit::Ordinary)
        } else {
            self.ordinary
                .clone()
                .reserve_owned()
                .await
                .ok()
                .map(EventPermit::Ordinary)
        }
    }
}

pub(super) struct InputDelivery<'a>(&'a EventSender);

impl Drop for InputDelivery<'_> {
    fn drop(&mut self) {
        let mut reservations = self
            .0
            .reservations
            .lock()
            .expect("station event delivery poisoned");
        reservations.ordinary.clear();
        reservations.new_call = None;
    }
}

fn event_key(event: &Event) -> Option<CompletionKey> {
    let Event::Device(device) = event else {
        return None;
    };
    Some(match &device.event {
        DeviceEventKind::Registered(_) => CompletionKey::Registered,
        DeviceEventKind::Disconnected {} => CompletionKey::Disconnected,
        DeviceEventKind::OnHook { call_id, .. }
        | DeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::EndCall,
            ..
        } => CompletionKey::Terminal(*call_id),
        DeviceEventKind::ReceiveChannelOpened { call_id, .. }
        | DeviceEventKind::HandsetAcknowledgementTimedOut { call_id, .. } => {
            CompletionKey::Receive(*call_id)
        }
        DeviceEventKind::TransmitChannelOpen {
            call_id,
            outcome: TransmitOpenOutcome::Rejected(_),
            ..
        } => CompletionKey::TransmitFailure(*call_id),
        DeviceEventKind::TransmitChannelOpen { call_id, .. } => CompletionKey::Transmit(*call_id),
        DeviceEventKind::MediaTransmissionFailed { call_id, .. } => {
            CompletionKey::TransmitFailure(*call_id)
        }
        DeviceEventKind::MultimediaReceiveChannelOpened { call_id, .. }
        | DeviceEventKind::MultimediaReceiveChannelFailed { call_id, .. }
        | DeviceEventKind::MultimediaReceiveChannelTimedOut { call_id, .. } => {
            CompletionKey::VideoReceive(*call_id)
        }
        DeviceEventKind::MultimediaTransmitStarted { call_id, .. }
        | DeviceEventKind::MultimediaTransmitFailed { call_id, .. }
        | DeviceEventKind::MultimediaTransmitTimedOut { call_id, .. } => {
            CompletionKey::VideoTransmit(*call_id)
        }
        DeviceEventKind::MulticastReceptionStarted {
            call_id,
            conference_id,
            ..
        }
        | DeviceEventKind::MulticastReceptionFailed {
            call_id,
            conference_id,
            ..
        }
        | DeviceEventKind::MulticastReceptionTimedOut {
            call_id,
            conference_id,
            ..
        } => CompletionKey::MulticastReceive(*call_id, *conference_id),
        DeviceEventKind::MulticastTransmissionStarted {
            call_id,
            conference_id,
            ..
        } => CompletionKey::MulticastTransmit(*call_id, *conference_id),
        DeviceEventKind::MulticastTransmissionFailed {
            call_id,
            conference_id,
            ..
        } => CompletionKey::MulticastFailure(*call_id, *conference_id),
        DeviceEventKind::PhoneServiceResponse { response } => {
            CompletionKey::Service(response.routing)
        }
        _ => return None,
    })
}

fn is_completion(message: &ClientMessage) -> bool {
    matches!(
        message,
        ClientMessage::KeepAlive
            | ClientMessage::OnHook { .. }
            | ClientMessage::Stimulus {
                stimulus: super::Stimulus::EndCall,
                ..
            }
            | ClientMessage::Unregister { .. }
            | ClientMessage::OpenReceiveChannelAck { .. }
            | ClientMessage::StartMediaTransmissionAck(_)
            | ClientMessage::OpenMultimediaReceiveChannelAck(_)
            | ClientMessage::StartMultimediaTransmissionAck(_)
            | ClientMessage::MulticastMediaReceptionAck { .. }
            | ClientMessage::MediaTransmissionFailure { .. }
            | ClientMessage::MediaPathEvent { .. }
    ) || matches!(message, ClientMessage::SoftKeyEvent { event, .. } if SoftKey::from(*event) == SoftKey::EndCall)
}

fn may_create_call(message: &ClientMessage) -> bool {
    matches!(
        message,
        ClientMessage::OffHook { .. }
            | ClientMessage::OffHookWithCallingParty { .. }
            | ClientMessage::EnblocCall { .. }
            | ClientMessage::Stimulus { .. }
            | ClientMessage::SoftKeyEvent { .. }
    )
}

fn service_transaction(message: &ClientMessage) -> Option<PhoneServiceRouting> {
    match message {
        ClientMessage::DeviceToUserData(message)
        | ClientMessage::DeviceToUserDataResponse(message) => Some(PhoneServiceRouting {
            application_id: ApplicationId::new(message.application_id),
            line_instance: LineInstance::new(message.line_instance),
            call_reference: CallReference::new(message.call_reference),
            transaction_id: TransactionId::new(message.transaction_id),
        }),
        ClientMessage::DeviceToUserDataV1(message)
        | ClientMessage::DeviceToUserDataResponseV1(message) => Some(PhoneServiceRouting {
            application_id: ApplicationId::new(message.application_id),
            line_instance: LineInstance::new(message.line_instance),
            call_reference: CallReference::new(message.call_reference),
            transaction_id: TransactionId::new(message.transaction_id),
        }),
        _ => None,
    }
}

fn service_route(application: u32, transaction_id: TransactionId) -> PhoneServiceRouting {
    PhoneServiceRouting {
        application_id: ApplicationId::new(application),
        line_instance: LineInstance::new(0),
        call_reference: CallReference::new(0),
        transaction_id,
    }
}
