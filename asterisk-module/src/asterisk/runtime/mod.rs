//! Asterisk runtime composition support.

use crate::asterisk::boundary::{MutexExt, RwLockExt};
use crate::asterisk::phone::{
    RuntimeDndMutation, RuntimeDndMutationError, begin_parking_retrieval, cancel_no_answer_timer,
    clear_no_answer_route, configured_mobility_button, execute_dnd_mutation,
    expire_forwarding_entries, expire_no_answer_routes, expire_parking_attempts,
    handle_parking_event, handle_phone_event, log_feature_store_error, mobility_device_registered,
    publish_ami_event, publish_device_features, publish_feature_changes,
    remove_conference_participant, set_conference_participant_moderator,
    set_conference_participant_muted, show_conference_list, update_device_features_locked,
};
use crate::asterisk::{
    AbortHandle, AddressSelectionPolicy, AmiConferenceCommand, AmiEventError, AmiEventPublisher,
    AmiParkingCommand, AmiRecordingCommand, AnnouncementAdapter, AnnouncementCall,
    AnnouncementFailureStage, AnnouncementGeneration, AppearanceRingMode, AppearanceRingSummary,
    Arc, AsteriskCallCompletion, AsteriskCallFeatures, AsteriskChannel, AsteriskChannelMetadata,
    AsteriskDatabase, AsteriskDialplan, AsteriskHints, AsteriskHttp, AsteriskManager,
    AsteriskParking, AsteriskPartyUpdates, AsteriskRecording, AsteriskRegistrationExtensions,
    AsyncMutex, AtomicU64, AudioProcessingPolicy, AutoAnswerMode, BTreeMap, BTreeSet,
    BargeBridgeSession, BargeOperation, BlfEvent, BlfSubscriptions, BridgeBackend, BridgeOperation,
    BridgeSession, Builder, ButtonDefinition, CONFERENCE_ANNOUNCEMENT_PLAYBACK_WINDOW, CString,
    CallDirection, CallFeatureError, CallId, CallInfo, CallMetadata, CallSelectionOrder,
    CallServiceBackend, CallState, CallStatus, CallTransition, CallTransitionProgress,
    CalledPartyOverride, CalledPartyProvider, CalledPartyProviderError, ChannelAppearanceSnapshot,
    ChannelBackend, ChannelDirectionSummary, ChannelMediaStateSummary, ChannelMetadataError,
    ChannelQueryLookupError, ChannelQueryProvider, ChannelQuerySnapshot, ChannelQueryTarget,
    ChannelStateSummary, Codec, CodecPreferenceContext, CodecPreferenceProvider,
    CodecPreferenceProviderError, CodecPreferenceRejection, ConferenceAnnouncement,
    ConferenceAnnouncementOperation, ConferenceDestinationOperation, ConferenceEndRejection,
    ConferenceId, ConferenceParticipantRejection, ConferenceParticipantStatus, ConferencePhase,
    ConferenceStatus, ConferenceTaskCancellation, ConferenceTaskRegistry, ConferenceTaskStartError,
    ConfigReconciliation, ConfigReconciliationTrigger, ConfigurationProvider,
    ConfiguredChannelMetadata, ConnectedLineSource, ConnectedLineUpdate, ControlOperation,
    ControlOutcome, ControlProvider, ControlProviderError, Controller,
    DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET, DEFAULT_AUDIO_PACKET_MS, DeviceCallSummary,
    DeviceDndSummary, DeviceFeatureState, DeviceFeatureSummary, DeviceId, DeviceQueryLookupError,
    DeviceQueryProvider, DeviceQuerySnapshot, DeviceQueryTarget, DeviceState, DialplanRegistration,
    Digit, DirectMediaPolicy, DirectoryProvider, DirectoryProviderError, DirectoryRecord, DndMode,
    DriverEffect, DtmfMode, Duration, EffectExecutionError, ExternalAddressCache,
    FeatureControlMutation, FeatureControlOutcome, FeatureControlProvider,
    FeatureControlProviderError, FeatureStore, FeatureStoreError, ForwardingDestination,
    ForwardingEntryRegistry, ForwardingKind, ForwardingOperation, ForwardingRouteReason, Handle,
    HandsetCallIndication, HandsetCallIndicationProvider, HandsetCallIndicationProviderError,
    HandsetEffect, HandsetMessageOperation, HandsetMessageProvider, HandsetMessageProviderError,
    HashMap, HashSet, HttpRegistration, Instant, InventoryProvider, InventoryProviderError,
    InventoryRegistration, InventorySnapshot, InventoryValue, IpAddr, IpAddressType, Ipv4Addr,
    JoinHandle, LineAppearanceSnapshot, LineBinding, LineCallSummary, LineInstance,
    LineQueryLookupError, LineQueryProvider, LineQuerySnapshot, LineQueryTarget, LogLevel,
    MANAGER_CONTROL_DELIVERY_TIMEOUT, MANAGER_CONTROL_TIMEOUT, MAX_RESTORE_ATTEMPTS, MODULE,
    ManagementBackend, ManagementEvent, ManagerActionRegistration, MediaAnchorReason,
    MediaAnchorRegistry, MediaAnchorRestores, MediaBackend, MediaDirection, MediaEndpoint,
    MediaEndpointAddress, MediaKind, MediaStatisticsStatus, MediaStreamState, MediaStreamStatus,
    MediaTrafficClass, MessageTarget, MobilityRegistry, MobilitySlot, ModuleConfig,
    MultimediaReceiveDescriptor, MultimediaTransmitControl, MultimediaTransmitDescriptor, Mutex,
    MwiSubscriptionChange, NORMAL_CLEARING, NameCharset, NatMode, NoAnswerTimerRegistry, NonNull,
    NumberPlan, OutboundMediaMode, PARKING_CONFIRM_TIMEOUT, PARKING_NOTIFICATION_TIME,
    ParkingEvent, ParkingOperation, ParkingRegistry, ParkingRejection, ParkingSubscription,
    ParticipantId, PartyIdentity, PartySnapshot, PartyUpdateError, PbxAudioFormat, PbxBackendError,
    PbxBridgeId, PbxCallId, PbxEffect, PbxServiceCapabilities, PbxVideoFormat, PhoneCallState,
    PhoneCommand, PhoneCommandAction, PhoneEvent, PickupOperation, PickupOutcome, Presentation,
    ProtocolVersion, REMOTE_HANGUP_PRESENTATION_TIME, REQUESTED_CHANNEL_UNAVAILABLE,
    ReceiveChannelPurpose, ReceiveTransmit, RecordingButtonState, RecordingCallback,
    RecordingDirection, RecordingError, RecordingEvent, RecordingProvider, RecordingRegistryError,
    RecordingSession, RecordingSessionControl, RecordingState, RecordingTarget,
    RecordingTogglePlan, RecordingToggleRejection, RedirectReasonCode, RedirectingUpdate,
    RegisteredDeviceSummary, RegistrationContextRegistry, RegistrationFallback,
    RegistrationRegistryError, RegistrationTokenPolicy, ReloadPlan, ReloadSelection,
    RemoteHangupPlan, ResetMode, ResetTarget, ResetType, ResolvedExternalAddresses, Runtime,
    RuntimeStatusProvider, RuntimeStatusProviderError, RuntimeStatusSnapshot, RwLock, Semaphore,
    Server, ServerConfig, ServerHandle, ServerIngress, ServiceControlProvider, ServiceOperation,
    ServiceOutcome, ServiceProviderError, SharedNoAnswerRoute, SignalingQos, SignalingSocket,
    StationIo, StationMediaCapabilities, StationTransport, SupplementaryBackend,
    SystemHostResolver, Tone, TransactionId, TransferCompletion, VideoMode, VoicemailOperation,
    Weak, adapters, allocate_announcement_generation, announcement_generation_is_current,
    call_event, canonical_ip_address, compose_channel_metadata, configured_inventory,
    configured_registration_appearances, controller_step, execute_backend_cleanup_effects,
    forwarding_ui_line_instances, mpsc, native_bridging, native_channel, native_pickup_result,
    negotiate_audio, ordered_recording_start, ordered_recording_stop, parse_requestor_mode,
    pbx_audio_format, plan_recording_toggle, ptr, raw, records_from_config, redirected_call_update,
    register_called_party_application, register_channel_query,
    register_codec_preference_application, register_control_actions, register_device_query,
    register_directory_http, register_feature_control_actions,
    register_handset_call_indication_application, register_handset_message_application,
    register_inventory_actions, register_line_query, register_runtime_status_actions,
    register_service_control_actions, replacement_anchor_plan, restore_attempts_exhausted,
    restore_redirecting_update, start_announcement, sys, validate_native_channel_metadata,
    validate_redirecting_update,
};
use crate::pbx::operations::CallFeatureProvider;

mod backend;
mod channel;
mod cli;
mod diagnostics;
mod lifecycle;
mod management;
mod media;
mod native_support;
mod presence;
mod recording;
mod services;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct AudioFraming {
    pub(super) packet_ms: u32,
    pub(super) max_frames_per_packet: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(super) enum AudioFramingError {
    #[error("station is unavailable")]
    StationUnavailable,
    #[error("audio packet duration must be non-zero")]
    InvalidRequestedPacketDuration,
    #[error("codec {codec:?} is not advertised by the station")]
    UnsupportedCodec { codec: Codec },
    #[error(
        "audio packet duration {requested_packet_ms} ms exceeds station maximum {maximum_packet_ms} ms"
    )]
    PacketDurationExceedsStationMaximum {
        requested_packet_ms: u32,
        maximum_packet_ms: u32,
    },
}

pub(super) struct ChannelAllocationRequest<'a> {
    pub(super) sccp_id: CallId,
    pub(super) pbx_id: PbxCallId,
    pub(super) binding: &'a LineBinding,
    pub(super) codec: Codec,
    pub(super) pbx_video_formats: &'a [PbxVideoFormat],
    pub(super) assigned_ids: *const sys::ast_assigned_ids,
    pub(super) requestor: *const sys::ast_channel,
    pub(super) metadata: Option<CallMetadata>,
    pub(super) text: channel::ChannelAllocationText,
    pub(super) owner: channel::ChannelAllocationOwner,
}

pub use backend::{
    ActiveConferenceAnnouncement, AsteriskBackend, AsteriskBackendError,
    cancel_conference_announcement, execute_answer_call_transition, execute_call_transition,
    execute_call_transition_result, execute_cleanup_effects, execute_effects,
    execute_effects_confirmed, execute_handset_effect, execute_one_effect,
    execute_remote_hangup_plan, handle_effect_error, handset_effects, send_handset_call_state,
    shutdown_conferences, shutdown_one_way_microphones, shutdown_remote_hangups,
};
pub use channel::{
    ChannelAllocationError, ChannelAllocationOwner, allocate_channel, channel_binding,
    configure_pickup_policy, handset_effect_call_id, preferred_codec, preferred_codec_upgrade,
    preferred_inbound_codec, prepare_channel_allocation_text, queue_unavailable, remove_channel,
    retain_two_channels, take_pending_retrieval_by_pbx, with_channel, with_channels,
    with_two_channels,
};
use channel::{ChannelAvailability, channel_availability};
pub use cli::{
    RuntimeCliInventoryError, complete_runtime_cli_inventory, render_runtime_cli_inventory,
};
pub use diagnostics::{
    RuntimeCliDiagnosticError, complete_runtime_cli_diagnostics, render_runtime_cli_diagnostics,
};
pub use lifecycle::{
    ChannelState, DirectMediaCall, config_path, module_access, registered_device_ids, reload,
    reload_selected, reload_sorcery, runtime_line_binding,
};
pub use management::{
    Access, ActiveSystemMessage, ChannelBinding, ChannelOperationPermit, Module, PendingPark,
    PendingParkingNotification, PendingRetrieval, RuntimeCallSignal,
    RuntimeCallSignalDeliveryError, RuntimeCallSignalDeliveryResult, RuntimeCallSignalKind,
    RuntimeCallSignalQueue, RuntimeCalledPartyProvider, RuntimeChannelQueryProvider,
    RuntimeCodecPreferenceProvider, RuntimeControlProvider, RuntimeControlRequest,
    RuntimeDeviceQueryProvider, RuntimeDirectoryProvider, RuntimeFeatureControlProvider,
    RuntimeHandsetCallIndicationProvider, RuntimeHandsetMessageProvider, RuntimeInventoryProvider,
    RuntimeLineQueryProvider, RuntimeRegistrationContexts, RuntimeServiceProvider,
    RuntimeServiceRequest, Shared, execute_forwarding_mutation,
};
pub use media::{
    MediaFailureDisposition, configured_audio_processing, configured_audio_traffic_class,
    configured_dtmf_mode, configured_early_media, configured_video_traffic_class,
    direct_media_call, direct_media_policy, enqueue_media_retarget, local_media_endpoint,
    local_video_endpoint, normalize_phone_media_endpoint, normalize_phone_video_endpoint,
    outbound_media_mode, recover_failed_media_transmission, set_remote_video_endpoint,
    station_nat_active,
};

pub(super) fn audio_framing(
    access: &Access,
    device: &DeviceId,
    call_id: CallId,
    codec: Codec,
) -> Result<AudioFraming, AudioFramingError> {
    media::audio_framing(access, device, call_id, codec)
}

pub use native_support::{
    anonymous_hotline_definition, ast_log, c_string, dial_terminator_digit, format_for,
    native_audio_format, pbx_audio_format_from_native, read_channel_metadata, read_party_snapshot,
    requestor_auto_answer_mode, state_from_channel, take_state_from_channel,
};
pub use presence::{
    StagedMwiSubscriptions, device_state, handle_blf_event, install_blf, install_mwi,
    publish_device_lines, publish_line, retry_blf, uninstall_blf, uninstall_device_blf,
    uninstall_mwi,
};
pub(super) use recording::RuntimeRecordings;
use recording::{
    RECORDING_TRIGGER_WAKE_CAPACITY, RuntimeRecordingOwner, RuntimeRecordingSession,
    RuntimeRecordingTrigger, RuntimeRecordingTriggerQueue,
};
pub use services::{
    conference_participant_service_error, execute_service_effects, handle_runtime_hangup_signal,
    parking_service_error, prune_recording_sessions, restore_system_message, run_call_signals,
    run_events, send_confirmed_service, toggle_monitor_recording,
};

pub(super) fn publish_recording_button_state(
    access: &Access,
    recordings: &RuntimeRecordings,
    device_id: &DeviceId,
) {
    services::publish_recording_button_state(access, recordings, device_id);
}

pub(super) fn retarget_station_to_anchor(access: &Access, call: &DirectMediaCall) -> bool {
    media::retarget_to_anchor(access, call)
}
