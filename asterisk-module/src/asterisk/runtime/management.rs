use sccp_protocol::DEFAULT_AUDIO_PACKET_MS;

use super::{
    AppearanceRingMode, AppearanceRingSummary, Arc, AsteriskChannel, AsteriskDatabase,
    AsteriskPartyUpdates, BTreeMap, BTreeSet, CallDirection, CallId, CallState, CallStatus,
    CalledPartyOverride, CalledPartyProvider, CalledPartyProviderError, ChannelAppearanceSnapshot,
    ChannelDirectionSummary, ChannelMediaStateSummary, ChannelQueryLookupError,
    ChannelQueryProvider, ChannelQuerySnapshot, ChannelQueryTarget, ChannelStateSummary, Codec,
    CodecPreferenceContext, CodecPreferenceProvider, CodecPreferenceProviderError,
    CodecPreferenceRejection, ConferenceParticipantStatus, ConferenceStatus,
    ConferenceTaskRegistry, ConfigReconciliation, ConfigurationProvider, ConnectedLineSource,
    ConnectedLineUpdate, ControlOperation, ControlOutcome, ControlProvider, ControlProviderError,
    DeviceCallSummary, DeviceDndSummary, DeviceFeatureState, DeviceFeatureSummary, DeviceId,
    DeviceQueryLookupError, DeviceQueryProvider, DeviceQuerySnapshot, DeviceQueryTarget,
    DialplanRegistration, DirectoryProvider, DirectoryProviderError, DirectoryRecord, DndMode,
    DriverEffect, FeatureControlMutation, FeatureControlOutcome, FeatureControlProvider,
    FeatureControlProviderError, FeatureStore, FeatureStoreError, ForwardingDestination,
    ForwardingKind, Handle, HandsetCallIndication, HandsetCallIndicationProvider,
    HandsetCallIndicationProviderError, HandsetEffect, HandsetMessageOperation,
    HandsetMessageProvider, HandsetMessageProviderError, HashMap, HashSet, HttpRegistration,
    Instant, InventoryProvider, InventoryProviderError, InventoryRegistration, InventorySnapshot,
    InventoryValue, JoinHandle, LineAppearanceSnapshot, LineCallSummary, LineQueryLookupError,
    LineQueryProvider, LineQuerySnapshot, LineQueryTarget, MANAGER_CONTROL_TIMEOUT,
    ManagerActionRegistration, MediaDirection, MediaKind, MediaStatisticsStatus, MediaStreamState,
    MediaStreamStatus, ModuleConfig, Mutex, MutexExt as _, NameCharset, NonNull, NumberPlan,
    ParkingSubscription, PartyIdentity, PartySnapshot, PbxAudioFormat, PbxCallId, PhoneCallState,
    PhoneCommand, PhoneCommandAction, Presentation, RegisteredDeviceSummary, Runtime,
    RuntimeDndMutation, RuntimeDndMutationError, RuntimeRecordingTriggerQueue,
    RuntimeStatusProvider, RuntimeStatusProviderError, RuntimeStatusSnapshot, RwLock,
    RwLockExt as _, ServerHandle, ServiceControlProvider, ServiceOperation, ServiceOutcome,
    ServiceProviderError, StationMediaCapabilities, Weak, configured_inventory,
    execute_dnd_mutation, forwarding_ui_line_instances, mpsc, native_audio_format, native_bridging,
    native_channel, negotiate_audio, pbx_audio_format, publish_device_features,
    publish_feature_changes, raw, records_from_config, runtime_line_binding, state_from_channel,
    sys, update_device_features,
};
use crate::ami::runtime::MediaStatisticsPrivacy;
use crate::asterisk::raw::handles::ChannelRef;
use crate::runtime::controller::{VideoMediaState, VideoStreamState};
use crate::runtime::mailbox::MailboxSender;
use crate::runtime::resource::{ResourceBinding, ResourcePermit};

pub struct Module {
    pub runtime: Runtime,
    pub controller_task: Option<std::thread::JoinHandle<()>>,
    pub access: Access,
    pub server_task: JoinHandle<()>,
    pub event_task: JoinHandle<()>,
    pub event_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub finish_event_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    pub event_drained: Option<tokio::sync::oneshot::Receiver<()>>,
    pub signal_task: JoinHandle<()>,
    pub background_task: JoinHandle<()>,
    pub resolver_task: JoinHandle<()>,
    pub configuration_task: Option<std::thread::JoinHandle<()>>,
    pub(super) media_task: Option<std::thread::JoinHandle<()>>,
    pub(super) presence_task: JoinHandle<()>,
    pub(super) parking_task: JoinHandle<()>,
    pub(super) worker_task: JoinHandle<usize>,
    pub(super) dnd_schedule_task: JoinHandle<()>,
    pub(super) bridge_task: JoinHandle<super::bridge_owner::BridgeState>,
    pub publication_task: JoinHandle<crate::runtime::publication::PublicationState>,
    pub http_registrations: Vec<HttpRegistration>,
    pub dialplan_registrations: Vec<DialplanRegistration>,
    pub manager_registrations: Vec<ManagerActionRegistration>,
    pub parking_subscription: ParkingSubscription,
    pub sorcery_registration: Option<Arc<raw::sorcery::SorceryRegistration>>,
    #[cfg(feature = "telemetry")]
    pub(super) telemetry: Option<crate::asterisk::telemetry::TelemetryReporter>,
}

#[derive(Clone)]
pub struct Access {
    pub handle: Handle,
    pub phone: ServerHandle,
    pub shared: Arc<Shared>,
}

pub struct Shared {
    pub(super) event_diagnostics: RwLock<Arc<super::services::EventDiagnostics>>,
    pub config: RwLock<Arc<ModuleConfig>>,
    pub config_provider: Arc<dyn ConfigurationProvider>,
    pub config_reconciliation: Arc<ConfigReconciliation>,
    pub configuration_transactions:
        crate::runtime::configuration_transaction::ConfigurationTransactions,
    pub controller: crate::runtime::controller::ownership::ControllerHandle,
    pub external_addresses: crate::runtime::resolver::ExternalAddressHandle,
    pub(super) presence: super::presence_owner::PresenceHandle,
    pub parking_events: crate::runtime::parking_events::ParkingEvents,
    pub workers: crate::runtime::workers::WorkerHandle,
    pub channels: Mutex<HashMap<PbxCallId, Arc<ChannelBinding>>>,
    pub channel_allocations: Arc<ResourceBinding<()>>,

    pub(super) media_runtime: super::media_owner::MediaHandle,
    pub conference_destination_tasks:
        Mutex<ConferenceTaskRegistry<native_bridging::ConferenceApplicationCancellation>>,
    pub(super) bridge_runtime: super::bridge_owner::BridgeHandle,

    pub feature_store: FeatureStore<AsteriskDatabase>,
    pub(super) dnd_schedules: super::dnd_schedule::DndScheduleHandle,
    pub(super) background_runtime: super::background::BackgroundRuntimeHandle,
    pub control_requests: MailboxSender<RuntimeControlRequest>,
    pub service_requests: MailboxSender<RuntimeServiceRequest>,
    pub call_signals: crate::runtime::call_queue::CallQueueHandle<PbxCallId, RuntimeCallSignalKind>,
    pub(super) recording_trigger_wake: mpsc::Sender<()>,
    pub(super) pending_recording_triggers: Mutex<RuntimeRecordingTriggerQueue>,
    pub ami_events: crate::runtime::publication::PublicationHandle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeCallSignalDeliveryError;

pub type RuntimeCallSignalDeliveryResult = Result<(), RuntimeCallSignalDeliveryError>;

pub struct ChannelBinding {
    native: Arc<ResourceBinding<ChannelRef>>,
    pub signals: Arc<crate::runtime::call_queue::CallLease<PbxCallId, RuntimeCallSignalKind>>,
}

impl ChannelBinding {
    pub fn new(
        channel: ChannelRef,
        signals: crate::runtime::call_queue::CallLease<PbxCallId, RuntimeCallSignalKind>,
    ) -> Arc<Self> {
        Arc::new(Self {
            native: ResourceBinding::new(channel),
            signals: Arc::new(signals),
        })
    }
}

impl ChannelBinding {
    pub fn replacement(&self, channel: ChannelRef) -> Arc<Self> {
        Arc::new(Self {
            native: ResourceBinding::new(channel),
            signals: Arc::clone(&self.signals),
        })
    }
}

impl ChannelBinding {
    pub fn enter(&self) -> Option<ChannelOperationPermit> {
        self.native.enter()
    }
    pub fn try_enter(&self) -> Option<ChannelOperationPermit> {
        self.native.try_enter()
    }
    pub fn suspend(&self) -> bool {
        self.native.suspend()
    }
    pub fn resume(&self) -> bool {
        self.native.resume()
    }
    pub fn replace_quiescent(&self, channel: ChannelRef) -> bool {
        self.native.replace_quiescent(channel)
    }
    pub fn close(&self) -> Option<ChannelRef> {
        self.native.close()
    }
    pub fn is_closed(&self) -> bool {
        self.native.is_closed()
    }
}
pub type ChannelOperationPermit = ResourcePermit<ChannelRef>;

#[derive(Clone, Debug)]
pub struct RuntimeCallSignal {
    pub sequence: u64,
    pub pbx_id: PbxCallId,
    pub kind: RuntimeCallSignalKind,
}

#[derive(Clone, Debug)]
pub enum RuntimeCallSignalKind {
    StopTone,
    Answer {
        completion: std::sync::mpsc::SyncSender<RuntimeCallSignalDeliveryResult>,
    },
    Hangup {
        handset_call_id: CallId,
    },
    PreparedHangup(Box<super::services::RuntimeHangupPreparation>),
    Proceeding,
    Ringing,
    Progress,
    Busy,
    Congestion,
    VideoUpdate,
    PartyUpdate(Box<PartySnapshot>),
}

#[derive(Clone)]
pub struct ActiveSystemMessage {
    pub text: String,
    pub beep: bool,
    pub expires_at: Option<Instant>,
}

pub struct RuntimeControlRequest {
    pub operation: ControlOperation,
    pub response: std::sync::mpsc::SyncSender<Result<ControlOutcome, ControlProviderError>>,
}

pub struct RuntimeServiceRequest {
    pub operation: ServiceOperation,
    pub response: std::sync::mpsc::SyncSender<Result<ServiceOutcome, ServiceProviderError>>,
}

#[derive(Clone)]
pub struct RuntimeServiceProvider {
    pub requests: MailboxSender<RuntimeServiceRequest>,
}

impl ServiceControlProvider for RuntimeServiceProvider {
    fn execute(&self, operation: ServiceOperation) -> Result<ServiceOutcome, ServiceProviderError> {
        let deadline = Instant::now() + MANAGER_CONTROL_TIMEOUT;
        let (response, result) = std::sync::mpsc::sync_channel(1);
        self.requests
            .try_send(
                RuntimeServiceRequest {
                    operation,
                    response,
                },
                Some(deadline),
            )
            .map_err(|_| ServiceProviderError::Unavailable)?;
        result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| ServiceProviderError::Unavailable)?
    }
}

pub struct RuntimeDirectoryProvider {
    pub shared: Weak<Shared>,
}

pub struct RuntimeInventoryProvider {
    pub shared: Weak<Shared>,
    pub phone: ServerHandle,
}

pub struct RuntimeFeatureControlProvider {
    pub shared: Weak<Shared>,
    pub handle: Handle,
    pub phone: ServerHandle,
}

#[derive(Clone)]
pub struct RuntimeControlProvider {
    pub requests: MailboxSender<RuntimeControlRequest>,
}

impl ControlProvider for RuntimeControlProvider {
    fn execute(&self, operation: ControlOperation) -> Result<ControlOutcome, ControlProviderError> {
        let deadline = Instant::now() + MANAGER_CONTROL_TIMEOUT;
        let (response, result) = std::sync::mpsc::sync_channel(1);
        self.requests
            .try_send(
                RuntimeControlRequest {
                    operation,
                    response,
                },
                Some(deadline),
            )
            .map_err(|_| ControlProviderError::Unavailable)?;
        result
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| ControlProviderError::Unavailable)?
    }
}

impl RuntimeFeatureControlProvider {
    pub fn access(&self) -> Result<Access, FeatureControlProviderError> {
        Ok(Access {
            handle: self.handle.clone(),
            phone: self.phone.clone(),
            shared: self
                .shared
                .upgrade()
                .ok_or(FeatureControlProviderError::Unavailable)?,
        })
    }
}

impl FeatureControlProvider for RuntimeFeatureControlProvider {
    fn apply(
        &self,
        mutation: FeatureControlMutation,
    ) -> Result<FeatureControlOutcome, FeatureControlProviderError> {
        let access = self.access()?;
        let (device_id, line, kind, destination) = match mutation {
            FeatureControlMutation::Dnd { device_id, mode } => {
                let result =
                    execute_dnd_mutation(&access, &device_id, RuntimeDndMutation::Set(mode))
                        .map_err(|error| match error {
                            RuntimeDndMutationError::Unavailable => {
                                FeatureControlProviderError::Unavailable
                            }
                            RuntimeDndMutationError::DeviceNotFound
                            | RuntimeDndMutationError::ButtonNotFound => {
                                FeatureControlProviderError::DeviceNotFound
                            }
                            RuntimeDndMutationError::FeatureDisabled => {
                                FeatureControlProviderError::FeatureDisabled
                            }
                            RuntimeDndMutationError::Store(error) => {
                                feature_control_store_error(&error)
                            }
                        })?;
                return Ok(FeatureControlOutcome::Dnd {
                    device_id,
                    mode: result.current.dnd,
                    changed: result.previous != result.current,
                });
            }
            FeatureControlMutation::Forwarding {
                device_id,
                line,
                kind,
                destination,
            } => (device_id, line, kind, destination),
        };

        execute_forwarding_mutation(&access, device_id, line, kind, destination)
    }
}

pub fn execute_forwarding_mutation(
    access: &Access,
    device_id: DeviceId,
    line: String,
    kind: ForwardingKind,
    destination: Option<ForwardingDestination>,
) -> Result<FeatureControlOutcome, FeatureControlProviderError> {
    let transaction = access
        .shared
        .configuration_transactions
        .begin(
            crate::runtime::configuration_transaction::ConfigurationOperation::Features,
            Instant::now() + MANAGER_CONTROL_TIMEOUT,
        )
        .map_err(|_| FeatureControlProviderError::Unavailable)?;
    let config = access.config();
    let device = config
        .devices
        .get(&device_id)
        .ok_or(FeatureControlProviderError::DeviceNotFound)?;
    if forwarding_ui_line_instances(
        Some(&line),
        config
            .appearances_for_device(&device_id)
            .map(|binding| (binding.line.number.as_str(), binding.line_instance)),
    )
    .is_none()
    {
        return Err(FeatureControlProviderError::LineNotFound);
    }
    let enabled = match kind {
        ForwardingKind::All => device.feature_defaults.forwarding.all_enabled,
        ForwardingKind::Busy => device.feature_defaults.forwarding.busy_enabled,
        ForwardingKind::NoAnswer => device.feature_defaults.forwarding.no_answer_enabled,
    };
    if !enabled {
        return Err(FeatureControlProviderError::FeatureDisabled);
    }
    let mutate = crate::state::features::FeatureMutation::SetForwarding { kind, destination };
    let outcome = FeatureOutcome::Forwarding {
        device_id: device_id.clone(),
        line,
        kind,
    };
    let result = update_device_features(access, &config, &device_id, mutate, &transaction);
    let (previous, state) = match result {
        Ok(Some(states)) => states,
        Ok(None) => return Err(FeatureControlProviderError::DeviceNotFound),
        Err(error) => return Err(feature_control_store_error(&error)),
    };
    publish_device_features(access, &device_id, &state);
    publish_feature_changes(access, &device_id, &previous, &state);
    Ok(outcome.complete(previous != state, &state))
}

pub enum FeatureOutcome {
    Forwarding {
        device_id: DeviceId,
        line: String,
        kind: ForwardingKind,
    },
}

impl FeatureOutcome {
    pub fn complete(self, changed: bool, state: &DeviceFeatureState) -> FeatureControlOutcome {
        match self {
            Self::Forwarding {
                device_id,
                line,
                kind,
            } => {
                let enabled = match kind {
                    ForwardingKind::All => state.forwarding.all.is_some(),
                    ForwardingKind::Busy => state.forwarding.busy.is_some(),
                    ForwardingKind::NoAnswer => state.forwarding.no_answer.is_some(),
                };
                FeatureControlOutcome::Forwarding {
                    device_id,
                    line,
                    kind,
                    enabled,
                    changed,
                }
            }
        }
    }
}

pub fn feature_control_store_error(error: &FeatureStoreError) -> FeatureControlProviderError {
    if matches!(error, FeatureStoreError::Rollback { .. }) {
        FeatureControlProviderError::PersistenceDiverged
    } else {
        FeatureControlProviderError::Persistence
    }
}

impl InventoryProvider for RuntimeInventoryProvider {
    fn snapshot(&self) -> Result<InventorySnapshot, InventoryProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(InventoryProviderError::Unavailable)?;
        let config = shared
            .config
            .read()
            .map_err(|_| InventoryProviderError::Unavailable)?
            .clone();
        let registrations = (|controller: &crate::runtime::controller::ControllerSnapshot| {
            controller
                .registered_devices()
                .map(|(device_id, device)| {
                    (
                        device_id.clone(),
                        InventoryRegistration {
                            model: format!("{:?}", device.registration.device_type),
                            model_id: device.registration.device_type.wire_value(),
                            protocol: device.registration.protocol.to_string(),
                            address: device.registration.peer.to_string(),
                        },
                    )
                })
                .collect::<BTreeMap<_, _>>()
        })(shared.controller.snapshot().as_ref());
        Ok(configured_inventory(&config, &registrations))
    }
}

impl RuntimeStatusProvider for RuntimeInventoryProvider {
    fn snapshot(&self) -> Result<RuntimeStatusSnapshot, RuntimeStatusProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(RuntimeStatusProviderError::Unavailable)?;
        let media_statistics = self.phone.media_statistics();
        let snapshot = {
            let controller = shared.controller.snapshot();
            let pbx_ids = controller
                .calls()
                .map(|call| call.pbx_id.0)
                .collect::<BTreeSet<_>>();
            let mut snapshot = RuntimeStatusSnapshot::default();
            let mut conferences = BTreeMap::new();

            for raw_pbx_id in pbx_ids {
                let pbx_id = PbxCallId(raw_pbx_id);
                let Some(call) = controller.pbx_call(pbx_id) else {
                    continue;
                };
                let appearances = controller
                    .appearances_for_pbx(pbx_id)
                    .cloned()
                    .collect::<Vec<_>>();
                let identity_restricted = appearances.iter().any(|appearance| {
                    appearance.privacy || appearance.info.party_restrictions != 0
                });
                let conference_id =
                    controller
                        .conference_session_by_pbx(pbx_id)
                        .map(|conference| {
                            conferences.insert(conference.id.get(), conference.clone());
                            conference.id
                        });
                snapshot.calls.push(CallStatus {
                    pbx_id,
                    line: call.line.clone(),
                    context: call.context.clone(),
                    state: channel_state_summary(call.state),
                    direction: match call.direction {
                        CallDirection::Inbound => ChannelDirectionSummary::Inbound,
                        CallDirection::Outbound => ChannelDirectionSummary::Outbound,
                    },
                    dialed_number: if call.privacy || identity_restricted {
                        InventoryValue::Redacted
                    } else {
                        InventoryValue::Public(call.digits.clone())
                    },
                    privacy: call.privacy || identity_restricted,
                    active_call_id: controller.active_call_id(pbx_id),
                    appearance_count: appearances.len(),
                    conference_id,
                });

                for appearance in appearances {
                    let privacy = call.privacy
                        || appearance.privacy
                        || appearance.info.party_restrictions != 0;
                    snapshot.media_streams.extend([
                        runtime_media_stream(
                            &appearance,
                            privacy,
                            MediaKind::Audio,
                            MediaDirection::Receive,
                            appearance.audio,
                        ),
                        runtime_media_stream(
                            &appearance,
                            privacy,
                            MediaKind::Audio,
                            MediaDirection::Transmit,
                            appearance.audio_transmit,
                        ),
                        runtime_video_stream(
                            &appearance,
                            privacy,
                            MediaDirection::Receive,
                            appearance.video.receive(),
                        ),
                        runtime_video_stream(
                            &appearance,
                            privacy,
                            MediaDirection::Transmit,
                            appearance.video.transmit(),
                        ),
                    ]);
                }
            }

            for conference in conferences.into_values() {
                snapshot.conferences.push(ConferenceStatus {
                    id: conference.id,
                    bridge_id: conference.bridge_id,
                    owner_device_id: conference.device_id.clone(),
                    phase: conference.phase,
                    origin: conference.origin,
                    participant_count: conference.participants.iter().len(),
                    moderator_count: conference.participants.moderator_count(),
                    pending_invite: conference.pending_invite.is_some(),
                    pending_mutation: conference.pending_participant_mutation.is_some(),
                    music_on_hold_class: conference.media_policy.music_on_hold_class.clone(),
                    general_announcements: conference.media_policy.play_general_announcements,
                    participant_announcements: conference
                        .media_policy
                        .play_participant_announcements,
                });
                snapshot
                    .participants
                    .extend(conference.participants.iter().map(|participant| {
                        let identity_presented = controller
                            .pbx_call(participant.pbx_call_id)
                            .is_some_and(|call| !call.privacy)
                            && controller
                                .appearance_for_call(participant.handset_call_id)
                                .is_some_and(|appearance| {
                                    !appearance.privacy && appearance.info.party_restrictions == 0
                                });
                        ConferenceParticipantStatus {
                            conference_id: conference.id,
                            participant_id: participant.id,
                            pbx_id: participant.pbx_call_id,
                            call_id: participant.handset_call_id,
                            device_id: participant.device_id.clone(),
                            display_name: if identity_presented {
                                InventoryValue::Public(participant.display_name.clone())
                            } else {
                                InventoryValue::Redacted
                            },
                            number: if identity_presented {
                                InventoryValue::Public(participant.number.clone())
                            } else {
                                InventoryValue::Redacted
                            },
                            identity_presented,
                            moderator: participant.moderator,
                            muted: participant.muted,
                        }
                    }));
            }
            snapshot.media_statistics = media_statistics
                .into_iter()
                .map(|(device_id, statistics)| {
                    let privacy =
                        media_statistics_privacy(&controller, &device_id, statistics.call_id);
                    MediaStatisticsStatus::new(device_id, privacy, statistics)
                })
                .collect();
            snapshot
        };
        Ok(snapshot)
    }
}

fn media_statistics_privacy(
    controller: &crate::runtime::controller::ControllerSnapshot,
    device_id: &DeviceId,
    call_id: CallId,
) -> MediaStatisticsPrivacy {
    let private = controller
        .appearance_for_call(call_id)
        .filter(|appearance| appearance.device_id == *device_id)
        .and_then(|appearance| {
            controller.pbx_call(appearance.pbx_id).map(|call| {
                call.privacy || appearance.privacy || appearance.info.party_restrictions != 0
            })
        });
    private.into()
}

pub fn runtime_media_stream(
    appearance: &crate::runtime::controller::CallAppearance,
    privacy: bool,
    kind: MediaKind,
    direction: MediaDirection,
    state: MediaStreamState,
) -> MediaStreamStatus {
    MediaStreamStatus {
        pbx_id: appearance.pbx_id,
        call_id: appearance.sccp_id,
        device_id: appearance.device_id.clone(),
        line_instance: appearance.line_instance,
        kind,
        direction,
        state: channel_media_state_summary(state),
        privacy,
        endpoint: match state {
            MediaStreamState::Open(endpoint) => Some(endpoint),
            MediaStreamState::Closed | MediaStreamState::Opening => None,
        },
    }
}

pub fn runtime_video_stream(
    appearance: &crate::runtime::controller::CallAppearance,
    privacy: bool,
    direction: MediaDirection,
    state: VideoStreamState,
) -> MediaStreamStatus {
    MediaStreamStatus {
        pbx_id: appearance.pbx_id,
        call_id: appearance.sccp_id,
        device_id: appearance.device_id.clone(),
        line_instance: appearance.line_instance,
        kind: MediaKind::Video,
        direction,
        state: video_stream_state_summary(state),
        privacy,
        endpoint: None,
    }
}

impl DirectoryProvider for RuntimeDirectoryProvider {
    fn records(&self) -> Result<Vec<DirectoryRecord>, DirectoryProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(DirectoryProviderError::Unavailable)?;
        let config = shared
            .config
            .read()
            .map_err(|_| DirectoryProviderError::Unavailable)?;
        Ok(records_from_config(&config))
    }
}

pub struct RuntimeDeviceQueryProvider {
    pub shared: Weak<Shared>,
}

impl DeviceQueryProvider for RuntimeDeviceQueryProvider {
    fn snapshot(
        &self,
        target: &DeviceQueryTarget,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<Option<DeviceQuerySnapshot>, DeviceQueryLookupError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(DeviceQueryLookupError::Unavailable)?;
        let device_id = match target {
            DeviceQueryTarget::Device(device_id) => device_id.clone(),
            DeviceQueryTarget::Current => {
                let channel = channel.ok_or(DeviceQueryLookupError::CurrentDeviceUnavailable)?;
                let pbx_id = NonNull::new(channel.as_raw().cast::<sys::ast_channel>())
                    .and_then(|channel| unsafe { native_channel::channel_pbx_id(channel) })
                    .ok_or(DeviceQueryLookupError::CurrentDeviceUnavailable)?;
                shared
                    .controller
                    .snapshot()
                    .active_or_primary_call_by_pbx(PbxCallId(pbx_id))
                    .map(|call| call.device_id)
                    .ok_or(DeviceQueryLookupError::CurrentDeviceUnavailable)?
            }
        };
        Ok(device_query_snapshot(&shared, &device_id))
    }
}

pub fn device_query_snapshot(shared: &Shared, device_id: &DeviceId) -> Option<DeviceQuerySnapshot> {
    let config = shared.config.read_unpoisoned().clone();
    let configured = config.devices.get(device_id);
    let (registration, features, calls) = {
        let controller = shared.controller.snapshot();
        let registered = controller.registered_device(device_id);
        let registration = registered.map(|device| RegisteredDeviceSummary {
            model: device.registration.device_type,
            protocol: device.registration.protocol,
            address: device.registration.peer,
            reported_address: device.registration.reported_address_for_peer(),
            firmware: device.registration.firmware.clone(),
            capability_count: device.capabilities.as_ref().map_or(0, |capabilities| {
                capabilities.audio().len() + capabilities.video().len()
            }),
        });
        let features = controller
            .feature_state(device_id)
            .cloned()
            .unwrap_or_default();
        let mut calls = DeviceCallSummary {
            selected: registered.map_or(0, |device| device.selected_calls().count()),
            selected_line: registered.and_then(|device| device.selected_line),
            ..DeviceCallSummary::default()
        };
        for appearance in controller.appearances_for_device(device_id) {
            calls.total += 1;
            match appearance.state {
                CallState::Ringing => calls.ringing += 1,
                CallState::Connected | CallState::RemoteInUse | CallState::Barged => {
                    calls.connected += 1;
                }
                CallState::Held | CallState::SharedHeld => calls.held += 1,
                _ => {}
            }
        }
        (registration, features, calls)
    };
    if configured.is_none() && registration.is_none() {
        return None;
    }
    Some(DeviceQuerySnapshot {
        id: device_id.clone(),
        configured: configured.is_some(),
        description: configured.map(|device| device.description.clone()),
        line_count: configured.map_or(0, |device| device.lines.len()),
        button_count: configured.map_or(0, |device| device.buttons.len()),
        registration,
        features: DeviceFeatureSummary {
            dnd: match features.dnd {
                DndMode::Off => DeviceDndSummary::Off,
                DndMode::Silent => DeviceDndSummary::Silent,
                DndMode::Reject => DeviceDndSummary::Reject,
            },
            privacy: features.privacy,
            forward_all: features
                .forwarding
                .all
                .map(ForwardingDestination::into_string),
            forward_busy: features
                .forwarding
                .busy
                .map(ForwardingDestination::into_string),
            forward_no_answer: features
                .forwarding
                .no_answer
                .map(ForwardingDestination::into_string),
            enabled_feature_buttons: features
                .buttons
                .values()
                .filter(|enabled| **enabled)
                .count(),
        },
        calls,
    })
}

pub struct RuntimeLineQueryProvider {
    pub shared: Weak<Shared>,
}

impl LineQueryProvider for RuntimeLineQueryProvider {
    fn snapshot(
        &self,
        target: &LineQueryTarget,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<Option<LineQuerySnapshot>, LineQueryLookupError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(LineQueryLookupError::Unavailable)?;
        let line = match target {
            LineQueryTarget::Line(line) => line.clone(),
            LineQueryTarget::Current => {
                let channel = channel.ok_or(LineQueryLookupError::CurrentLineUnavailable)?;
                let pbx_id = NonNull::new(channel.as_raw().cast::<sys::ast_channel>())
                    .and_then(|channel| unsafe { native_channel::channel_pbx_id(channel) })
                    .ok_or(LineQueryLookupError::CurrentLineUnavailable)?;
                shared
                    .controller
                    .snapshot()
                    .active_or_primary_call_by_pbx(PbxCallId(pbx_id))
                    .map(|call| call.line)
                    .ok_or(LineQueryLookupError::CurrentLineUnavailable)?
            }
        };
        Ok(line_query_snapshot(&shared, &line))
    }
}

pub fn line_query_snapshot(shared: &Shared, number: &str) -> Option<LineQuerySnapshot> {
    let config = shared.config.read_unpoisoned().clone();
    let line = config.lines.get(number)?;
    let mut appearances: Vec<_> = config
        .appearances_for_line(number)
        .map(|binding| LineAppearanceSnapshot {
            id: binding.appearance.id.get(),
            device_id: binding.device_id.clone(),
            instance: binding.line_instance,
            label: binding.appearance.display_label().to_owned(),
            ring: match binding.appearance.ring_mode {
                AppearanceRingMode::Normal => AppearanceRingSummary::Normal,
                AppearanceRingMode::Silent => AppearanceRingSummary::Silent,
                AppearanceRingMode::Disabled => AppearanceRingSummary::Disabled,
            },
            privacy: binding.appearance.privacy,
            subscription: binding.appearance.subscription_identity.clone(),
            registered: false,
            calls: LineCallSummary::default(),
        })
        .collect();
    appearances.extend(
        shared
            .controller
            .snapshot()
            .mobility()
            .appearances_for_line(number)
            .map(|roaming| LineAppearanceSnapshot {
                id: roaming.binding.appearance.id.get(),
                device_id: roaming.binding.device_id.clone(),
                instance: roaming.binding.line_instance,
                label: roaming.binding.appearance.display_label().to_owned(),
                ring: AppearanceRingSummary::Normal,
                privacy: roaming.binding.appearance.privacy,
                subscription: roaming.binding.appearance.subscription_identity.clone(),
                registered: false,
                calls: LineCallSummary::default(),
            }),
    );
    appearances.sort_by(|left, right| {
        (&left.device_id, left.instance, left.id).cmp(&(&right.device_id, right.instance, right.id))
    });
    let calls = {
        let controller = shared.controller.snapshot();
        let mut pbx_ids = HashSet::new();
        for configured in &mut appearances {
            configured.registered = controller.is_registered(&configured.device_id);
            for appearance in controller
                .appearances_for_device(&configured.device_id)
                .filter(|appearance| appearance.line_instance == configured.instance)
            {
                pbx_ids.insert(appearance.pbx_id);
                configured.calls.total += 1;
                count_line_call_state(&mut configured.calls, appearance.state);
            }
        }
        let mut calls = LineCallSummary::default();
        for pbx_id in pbx_ids {
            let Some(call) = controller.pbx_call(pbx_id) else {
                continue;
            };
            calls.total += 1;
            count_line_call_state(&mut calls, call.state);
        }
        calls
    };
    Some(LineQuerySnapshot {
        number: line.number.clone(),
        label: line.label.clone(),
        context: line.context.clone(),
        caller_name: line.caller_name.clone(),
        caller_number: line.caller_number.clone(),
        mailbox: line.mailbox.clone(),
        calls,
        appearances,
    })
}

pub fn count_line_call_state(summary: &mut LineCallSummary, state: CallState) {
    match state {
        CallState::Ringing => summary.ringing += 1,
        CallState::Connected | CallState::RemoteInUse | CallState::Barged => {
            summary.connected += 1;
        }
        CallState::Held | CallState::SharedHeld => summary.held += 1,
        _ => {}
    }
}

pub struct RuntimeChannelQueryProvider {
    pub shared: Weak<Shared>,
}

pub struct RuntimeCodecPreferenceProvider {
    pub shared: Weak<Shared>,
}

pub struct RuntimeCalledPartyProvider {
    pub shared: Weak<Shared>,
    pub phone: ServerHandle,
}

pub struct RuntimeHandsetMessageProvider {
    pub shared: Weak<Shared>,
    pub phone: ServerHandle,
}

pub struct RuntimeHandsetCallIndicationProvider {
    pub shared: Weak<Shared>,
    pub phone: ServerHandle,
}

pub struct AudioPreferencePolicy {
    pub configured: Vec<PbxAudioFormat>,
    pub codecs: Vec<Codec>,
    pub station: Option<StationMediaCapabilities>,
}

impl CalledPartyProvider for RuntimeCalledPartyProvider {
    fn replace(
        &self,
        channel: &AsteriskChannel<'_>,
        called_party: &CalledPartyOverride,
    ) -> Result<(), CalledPartyProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(CalledPartyProviderError::Unavailable)?;
        let state = unsafe { state_from_channel(channel.as_raw().cast::<sys::ast_channel>()) }
            .ok_or(CalledPartyProviderError::NotDriverChannel)?;
        let owned = shared
            .controller
            .snapshot()
            .active_or_primary_call_by_pbx(state.pbx_id)
            .is_some();
        if !owned {
            return Err(CalledPartyProviderError::Unavailable);
        }
        let update = ConnectedLineUpdate {
            party: PartyIdentity {
                name: called_party.name.clone(),
                number: Some(called_party.number.clone()),
                name_charset: NameCharset::UTF_8,
                name_presentation: Presentation::ALLOWED_NOT_SCREENED,
                number_plan: NumberPlan::UNKNOWN,
                number_presentation: Presentation::ALLOWED_NOT_SCREENED,
            },
            private_party: PartyIdentity::default(),
            source: ConnectedLineSource::UNKNOWN,
        };
        AsteriskPartyUpdates::new()
            .set_connected_line(channel, &update)
            .map_err(|_| CalledPartyProviderError::NativeRejected)?;
        let effects = shared
            .controller
            .set_called_party(
                state.pbx_id,
                called_party.name.clone(),
                called_party.number.clone(),
            )
            .unwrap_or_else(|_| Vec::new());
        for effect in effects {
            let DriverEffect::Handset(HandsetEffect::SetCallInfo {
                device_id,
                call_id,
                info,
            }) = effect
            else {
                continue;
            };
            self.phone
                .try_send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::SetCallInfo { call_id, info },
                ))
                .map_err(|_| CalledPartyProviderError::HandsetRejected)?;
        }
        Ok(())
    }
}

impl HandsetMessageProvider for RuntimeHandsetMessageProvider {
    fn apply(
        &self,
        channel: &AsteriskChannel<'_>,
        operation: &HandsetMessageOperation,
    ) -> Result<(), HandsetMessageProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(HandsetMessageProviderError::Unavailable)?;
        let state = unsafe { state_from_channel(channel.as_raw().cast::<sys::ast_channel>()) }
            .ok_or(HandsetMessageProviderError::NotDriverChannel)?;
        let call = shared
            .controller
            .snapshot()
            .active_or_primary_call_by_pbx(state.pbx_id)
            .ok_or(HandsetMessageProviderError::Unavailable)?;
        let registered = shared.controller.snapshot().is_registered(&call.device_id);
        if !registered {
            return Err(HandsetMessageProviderError::NotRegistered);
        }
        self.phone
            .try_send(PhoneCommand::new(
                call.device_id,
                PhoneCommandAction::SetStatusMessage {
                    message: operation.0.clone(),
                    beep: false,
                },
            ))
            .map_err(|_| HandsetMessageProviderError::HandsetRejected)
    }
}

impl HandsetCallIndicationProvider for RuntimeHandsetCallIndicationProvider {
    fn apply(
        &self,
        channel: &AsteriskChannel<'_>,
        indication: HandsetCallIndication,
    ) -> Result<(), HandsetCallIndicationProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(HandsetCallIndicationProviderError::Unavailable)?;
        let state = unsafe { state_from_channel(channel.as_raw().cast::<sys::ast_channel>()) }
            .ok_or(HandsetCallIndicationProviderError::NotDriverChannel)?;
        let call = shared
            .controller
            .snapshot()
            .active_or_primary_call_by_pbx(state.pbx_id)
            .ok_or(HandsetCallIndicationProviderError::Unavailable)?;
        let registered = shared.controller.snapshot().is_registered(&call.device_id);
        if !registered {
            return Err(HandsetCallIndicationProviderError::NotRegistered);
        }
        let (phone_state, prompt) = match indication {
            HandsetCallIndication::Busy => (PhoneCallState::Busy, None),
            HandsetCallIndication::Congestion => (PhoneCallState::Congestion, None),
            HandsetCallIndication::Unavailable => (PhoneCallState::Congestion, Some("Unavailable")),
            HandsetCallIndication::InvalidNumber => (PhoneCallState::InvalidNumber, None),
        };
        self.phone
            .try_send(PhoneCommand::new(
                call.device_id.clone(),
                PhoneCommandAction::SetCallState {
                    call_id: call.sccp_id,
                    state: phone_state,
                },
            ))
            .map_err(|_| HandsetCallIndicationProviderError::HandsetRejected)?;
        if let Some(text) = prompt {
            self.phone
                .try_send(PhoneCommand::new(
                    call.device_id,
                    PhoneCommandAction::DisplayPrompt {
                        call_id: call.sccp_id,
                        timeout_seconds: 0,
                        text: text.into(),
                    },
                ))
                .map_err(|_| HandsetCallIndicationProviderError::HandsetRejected)?;
        }
        Ok(())
    }
}

impl CodecPreferenceProvider for RuntimeCodecPreferenceProvider {
    fn context(
        &self,
        channel: &AsteriskChannel<'_>,
    ) -> Result<CodecPreferenceContext, CodecPreferenceProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(CodecPreferenceProviderError::Unavailable)?;
        let pbx_id = codec_preference_pbx_id(channel)?;
        codec_preference_context(&shared, pbx_id)
    }

    fn replace(
        &self,
        channel: &AsteriskChannel<'_>,
        preferences: &[PbxAudioFormat],
    ) -> Result<(), CodecPreferenceProviderError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(CodecPreferenceProviderError::Unavailable)?;
        let pbx_id = codec_preference_pbx_id(channel)?;
        let (device_id, line_instance) = codec_preference_appearance(&shared, pbx_id)?;
        let policy = audio_preference_policy(&shared, &device_id, line_instance)?;
        if preferences.is_empty()
            || preferences
                .iter()
                .any(|format| !policy.configured.contains(format))
        {
            return Err(CodecPreferenceProviderError::FormatUnavailable);
        }
        let selected = negotiate_audio(
            &policy.codecs,
            policy.station.as_ref().map(StationMediaCapabilities::audio),
            &preferences[..1],
        )
        .map_err(|_| CodecPreferenceProviderError::FormatUnavailable)?
        .codec;
        let native_channel = NonNull::new(channel.as_raw().cast::<sys::ast_channel>())
            .ok_or(CodecPreferenceProviderError::NotDriverChannel)?;
        let stored_preferences =
            (preferences != policy.configured.as_slice()).then(|| preferences.to_vec());
        let mutation = shared
            .controller
            .prepare_pre_dial_codec(pbx_id, selected, stored_preferences)
            .map_err(|_| CodecPreferenceProviderError::Unavailable)?
            .map_err(map_codec_preference_rejection)?;
        let native_result = unsafe {
            native_channel::set_audio_format(native_channel, native_audio_format(preferences[0]))
        };
        let committed = native_result.is_ok()
            && shared
                .controller
                .commit_codec_mutation(mutation)
                .unwrap_or(false);
        if !committed {
            let restored =
                pbx_audio_format(mutation.previous)
                    .ok()
                    .is_some_and(|previous| unsafe {
                        native_channel::set_audio_format(
                            native_channel,
                            native_audio_format(previous),
                        )
                        .is_ok()
                    });
            let _ = shared.controller.abort_codec_mutation(mutation);
            return Err(if restored {
                CodecPreferenceProviderError::NativeRejected
            } else {
                CodecPreferenceProviderError::RollbackFailed
            });
        }
        Ok(())
    }
}

pub fn codec_preference_pbx_id(
    channel: &AsteriskChannel<'_>,
) -> Result<PbxCallId, CodecPreferenceProviderError> {
    NonNull::new(channel.as_raw().cast::<sys::ast_channel>())
        .and_then(|channel| unsafe { native_channel::channel_pbx_id(channel) })
        .map(PbxCallId)
        .ok_or(CodecPreferenceProviderError::NotDriverChannel)
}

pub fn codec_preference_appearance(
    shared: &Shared,
    pbx_id: PbxCallId,
) -> Result<(DeviceId, u32), CodecPreferenceProviderError> {
    (|controller: &crate::runtime::controller::ControllerSnapshot| {
        let call = controller
            .pbx_call(pbx_id)
            .ok_or(CodecPreferenceProviderError::Unavailable)?;
        let mut appearances = call.appearance_ids();
        let first = appearances
            .next()
            .ok_or(CodecPreferenceProviderError::Unavailable)?;
        if appearances.next().is_some() {
            return Err(CodecPreferenceProviderError::AmbiguousChannel);
        }
        let appearance = controller
            .call_appearance(first)
            .ok_or(CodecPreferenceProviderError::Unavailable)?;
        Ok((appearance.device_id.clone(), appearance.line_instance))
    })(shared.controller.snapshot().as_ref())
}

pub fn audio_preference_policy(
    shared: &Shared,
    device_id: &DeviceId,
    line_instance: u32,
) -> Result<AudioPreferencePolicy, CodecPreferenceProviderError> {
    let config = shared.config.read_unpoisoned().clone();
    let binding = runtime_line_binding(shared, device_id, line_instance)
        .ok_or(CodecPreferenceProviderError::Unavailable)?;
    let media = config
        .media_for_binding(&binding)
        .ok_or(CodecPreferenceProviderError::Unavailable)?;
    let station = shared
        .controller
        .snapshot()
        .registered_device(device_id)
        .map(|device| device.capabilities.clone())
        .ok_or(CodecPreferenceProviderError::Unavailable)?;
    let station = station.filter(|capabilities| !capabilities.audio().is_empty());
    let mut configured = Vec::new();
    for codec in media.codecs.iter().copied() {
        if station.is_none() && !matches!(codec, Codec::Pcma | Codec::Pcmu) {
            continue;
        }
        let Ok(format) = pbx_audio_format(codec) else {
            continue;
        };
        if station.as_ref().is_some_and(|capabilities| {
            !capabilities.audio().iter().any(|capability| {
                capability.codec == codec && capability.max_packet_ms >= DEFAULT_AUDIO_PACKET_MS
            })
        }) {
            continue;
        }
        if !configured.contains(&format) {
            configured.push(format);
        }
    }
    if configured.is_empty() {
        return Err(CodecPreferenceProviderError::FormatUnavailable);
    }
    Ok(AudioPreferencePolicy {
        configured,
        codecs: media.codecs,
        station,
    })
}

pub fn codec_preference_context(
    shared: &Shared,
    pbx_id: PbxCallId,
) -> Result<CodecPreferenceContext, CodecPreferenceProviderError> {
    let (device_id, line_instance) = codec_preference_appearance(shared, pbx_id)?;
    let policy = audio_preference_policy(shared, &device_id, line_instance)?;
    let effective = shared
        .controller
        .snapshot()
        .call_runtime_record(pbx_id)
        .and_then(|record| record.audio_preferences.as_ref())
        .map(|preferences| {
            preferences
                .iter()
                .copied()
                .filter(|format| policy.configured.contains(format))
                .collect::<Vec<_>>()
        })
        .filter(|preferences| !preferences.is_empty())
        .unwrap_or_else(|| policy.configured.clone());
    Ok(CodecPreferenceContext {
        configured: policy.configured,
        effective,
    })
}

pub const fn map_codec_preference_rejection(
    rejection: CodecPreferenceRejection,
) -> CodecPreferenceProviderError {
    match rejection {
        CodecPreferenceRejection::Unavailable => CodecPreferenceProviderError::Unavailable,
        CodecPreferenceRejection::NotPreDial => CodecPreferenceProviderError::NotPreDial,
        CodecPreferenceRejection::Ambiguous => CodecPreferenceProviderError::AmbiguousChannel,
    }
}

impl ChannelQueryProvider for RuntimeChannelQueryProvider {
    fn snapshot(
        &self,
        target: &ChannelQueryTarget,
        channel: Option<&AsteriskChannel<'_>>,
    ) -> Result<Option<ChannelQuerySnapshot>, ChannelQueryLookupError> {
        let shared = self
            .shared
            .upgrade()
            .ok_or(ChannelQueryLookupError::Unavailable)?;
        let (pbx_id, selected_call_id) = match target {
            ChannelQueryTarget::Current => {
                let channel = channel.ok_or(ChannelQueryLookupError::CurrentChannelUnavailable)?;
                let pbx_id = NonNull::new(channel.as_raw().cast::<sys::ast_channel>())
                    .and_then(|channel| unsafe { native_channel::channel_pbx_id(channel) })
                    .ok_or(ChannelQueryLookupError::CurrentChannelUnavailable)?;
                (PbxCallId(pbx_id), None)
            }
            ChannelQueryTarget::Pbx(pbx_id) => (*pbx_id, None),
            ChannelQueryTarget::Call(call_id) => {
                let pbx_id = shared
                    .controller
                    .snapshot()
                    .appearance_for_call(*call_id)
                    .map(|appearance| appearance.pbx_id);
                let Some(pbx_id) = pbx_id else {
                    return Ok(None);
                };
                (pbx_id, Some(*call_id))
            }
            ChannelQueryTarget::Name(name) => {
                let Some(pbx_id) = pbx_id_for_channel_name(&shared, name)? else {
                    return Ok(None);
                };
                (pbx_id, None)
            }
        };
        Ok(channel_query_snapshot(&shared, pbx_id, selected_call_id))
    }
}

pub fn referenced_channels(shared: &Shared) -> Vec<(PbxCallId, ChannelOperationPermit)> {
    let bindings = shared
        .channels
        .lock_unpoisoned()
        .iter()
        .map(|(pbx_id, binding)| (*pbx_id, Arc::clone(binding)))
        .collect::<Vec<_>>();
    bindings
        .into_iter()
        .filter_map(|(pbx_id, binding)| binding.try_enter().map(|permit| (pbx_id, permit)))
        .collect()
}

pub fn referenced_channel(shared: &Shared, pbx_id: PbxCallId) -> Option<ChannelOperationPermit> {
    let binding = {
        let channels = shared.channels.lock_unpoisoned();
        Arc::clone(channels.get(&pbx_id)?)
    };
    binding.try_enter()
}

pub fn channel_name(channel: &ChannelRef) -> Option<String> {
    raw::bridge::channel_name(channel).ok().flatten()
}

pub fn pbx_id_for_channel_name(
    shared: &Shared,
    requested: &str,
) -> Result<Option<PbxCallId>, ChannelQueryLookupError> {
    let mut matching = referenced_channels(shared)
        .into_iter()
        .filter_map(|(pbx_id, channel)| {
            (channel_name(channel.resource()).as_deref() == Some(requested)).then_some(pbx_id)
        })
        .collect::<Vec<_>>();
    matching.sort_by_key(|pbx_id| pbx_id.0);
    matching.dedup();
    match matching.as_slice() {
        [] => Ok(None),
        [pbx_id] => Ok(Some(*pbx_id)),
        _ => Err(ChannelQueryLookupError::AmbiguousChannelName),
    }
}

pub fn channel_query_snapshot(
    shared: &Shared,
    pbx_id: PbxCallId,
    selected_call_id: Option<CallId>,
) -> Option<ChannelQuerySnapshot> {
    let (
        line,
        context,
        state,
        direction,
        dialed_number,
        privacy,
        metadata,
        active_call_id,
        appearances,
    ) = (|controller: &crate::runtime::controller::ControllerSnapshot| {
        let call = controller.pbx_call(pbx_id)?;
        let active_call_id = call
            .active_appearance()
            .and_then(|id| controller.call_appearance(id))
            .map(|appearance| appearance.sccp_id);
        let appearances = controller
            .appearances_for_pbx(pbx_id)
            .map(|appearance| ChannelAppearanceSnapshot {
                call_id: appearance.sccp_id,
                device_id: appearance.device_id.clone(),
                line_instance: appearance.line_instance,
                state: channel_state_summary(appearance.state),
                privacy: appearance.privacy,
                codec: appearance.codec,
                audio: channel_media_state_summary(appearance.audio),
                video: video_media_state_summary(&appearance.video),
            })
            .collect();
        Some((
            call.line.clone(),
            call.context.clone(),
            channel_state_summary(call.state),
            match call.direction {
                CallDirection::Inbound => ChannelDirectionSummary::Inbound,
                CallDirection::Outbound => ChannelDirectionSummary::Outbound,
            },
            call.digits.clone(),
            call.privacy,
            call.metadata.clone(),
            active_call_id,
            appearances,
        ))
    })(shared.controller.snapshot().as_ref())?;
    let name =
        referenced_channel(shared, pbx_id).and_then(|channel| channel_name(channel.resource()));
    let audio_packet_ms = shared
        .controller
        .snapshot()
        .call_runtime_record(pbx_id)
        .and_then(|record| record.audio_packet_ms);
    let audio_preferences = codec_preference_context(shared, pbx_id)
        .map(|context| context.effective)
        .unwrap_or_default();
    Some(ChannelQuerySnapshot {
        pbx_id,
        name,
        line,
        context,
        state,
        direction,
        dialed_number,
        ani: (!privacy)
            .then(|| metadata.visible_ani_number().map(str::to_owned))
            .flatten(),
        dnid: metadata.dnid.clone(),
        rdnis: (!privacy)
            .then(|| metadata.visible_rdnis_number().map(str::to_owned))
            .flatten(),
        account_code_set: metadata.account_code.is_some(),
        language: metadata.language.clone(),
        variable_count: metadata.variables.len(),
        privacy,
        selected_call_id,
        active_call_id,
        audio_packet_ms,
        audio_preferences,
        appearances,
    })
}

pub fn channel_state_summary(state: CallState) -> ChannelStateSummary {
    match state {
        CallState::Collecting => ChannelStateSummary::Collecting,
        CallState::PickupCollecting => ChannelStateSummary::PickupCollecting,
        CallState::Ringing => ChannelStateSummary::Ringing,
        CallState::Calling => ChannelStateSummary::Calling,
        CallState::Connected => ChannelStateSummary::Connected,
        CallState::Parking => ChannelStateSummary::Parking,
        CallState::Retrieving => ChannelStateSummary::Retrieving,
        CallState::Held => ChannelStateSummary::Held,
        CallState::RemoteInUse => ChannelStateSummary::RemoteInUse,
        CallState::SharedHeld => ChannelStateSummary::SharedHeld,
        CallState::Barged => ChannelStateSummary::Barged,
        CallState::TransferCollecting => ChannelStateSummary::TransferCollecting,
        CallState::Ended => ChannelStateSummary::Ended,
    }
}

pub fn channel_media_state_summary(state: MediaStreamState) -> ChannelMediaStateSummary {
    match state {
        MediaStreamState::Closed => ChannelMediaStateSummary::Closed,
        MediaStreamState::Opening => ChannelMediaStateSummary::Opening,
        MediaStreamState::Open(_) => ChannelMediaStateSummary::Open,
    }
}

pub fn video_stream_state_summary(state: VideoStreamState) -> ChannelMediaStateSummary {
    match state {
        VideoStreamState::Closed => ChannelMediaStateSummary::Closed,
        VideoStreamState::Opening => ChannelMediaStateSummary::Opening,
        VideoStreamState::Open { .. } => ChannelMediaStateSummary::Open,
    }
}

pub fn video_media_state_summary(state: &VideoMediaState) -> ChannelMediaStateSummary {
    let receive = video_stream_state_summary(state.receive());
    let transmit = video_stream_state_summary(state.transmit());
    match (receive, transmit) {
        (ChannelMediaStateSummary::Open, ChannelMediaStateSummary::Open) => {
            ChannelMediaStateSummary::Open
        }
        (ChannelMediaStateSummary::Closed, ChannelMediaStateSummary::Closed) => {
            ChannelMediaStateSummary::Closed
        }
        _ => ChannelMediaStateSummary::Opening,
    }
}

#[cfg(test)]
mod media_statistics_privacy_tests {
    use super::*;

    #[test]
    fn registration_context_default_is_empty_and_reconciles_empty_registration_set() {
        let config = ModuleConfig::parse(include_str!("../../../sccp.conf.example")).unwrap();
        let mut contexts = RuntimeRegistrationContexts::default();
        assert!(contexts.suppressed_devices.is_empty());
        assert_eq!(contexts.registry.active_target_count(), 0);
        contexts.reconcile(&config, &[]).unwrap();
        assert_eq!(contexts.registry.active_target_count(), 0);
    }

    fn binding(device_id: &DeviceId) -> LineBinding {
        LineBinding {
            device_id: device_id.clone(),
            line_instance: 1,
            appearance: sccp_protocol::LineAppearance::new(
                1,
                sccp_protocol::LineDefinition {
                    number: "1001".into(),
                    display_name: "Desk".into(),
                },
            ),
            line: crate::config::LineConfig {
                number: "1001".into(),
                label: "Desk".into(),
                context: "from-sccp".into(),
                caller_name: "Desk".into(),
                caller_number: "1001".into(),
                mailbox: None,
                language: "en".into(),
                account_code: None,
                channel_variables: Vec::new(),
            },
        }
    }

    #[test]
    fn statistics_privacy_requires_an_exact_live_public_appearance() {
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        let other_device = DeviceId::new("SEP112233445566").unwrap();
        let call_id = CallId(7);
        let mut controller = crate::runtime::controller::Controller::new(Duration::from_secs(1));

        assert_eq!(
            media_statistics_privacy(&controller, &device_id, call_id),
            MediaStatisticsPrivacy::Private
        );

        controller.begin_phone_call(call_id, binding(&device_id), Codec::Pcmu, Instant::now());
        let pbx_id = controller.appearance_for_call(call_id).unwrap().pbx_id;
        assert_eq!(
            media_statistics_privacy(&controller, &device_id, call_id),
            MediaStatisticsPrivacy::Public
        );
        assert_eq!(
            media_statistics_privacy(&controller, &other_device, call_id),
            MediaStatisticsPrivacy::Private
        );

        assert!(controller.set_call_privacy(call_id, true));
        assert_eq!(
            media_statistics_privacy(&controller, &device_id, call_id),
            MediaStatisticsPrivacy::Private
        );

        controller.pbx_hangup(pbx_id);
        assert_eq!(
            media_statistics_privacy(&controller, &device_id, call_id),
            MediaStatisticsPrivacy::Private
        );
    }
}

#[cfg(feature = "telemetry")]
impl Shared {
    pub fn bridge_snapshot(&self) -> (Vec<super::PbxBridgeId>, Vec<super::PbxBridgeId>) {
        self.bridge_runtime.snapshot_keys()
    }
}
