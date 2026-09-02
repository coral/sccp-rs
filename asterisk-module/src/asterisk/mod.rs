//! Production composition root for the Asterisk loadable module.
//!
//! This private module wires normalized [`crate::config::ModuleConfig`]
//! snapshots to the SCCP server, backend-neutral controller, native channel
//! operations, dialplan registrations, HTTP handlers, AMI registrations and
//! event publication. Domain policy belongs in the public owning modules; this
//! module translates committed runtime transitions into Asterisk and handset
//! effects.
//!
//! Module startup owns every native registration and subscription. Stop first
//! prevents new work, invalidates server/runtime producers, drains owned tasks
//! and callbacks, and then drops RAII registrations. Reload stages a complete
//! candidate and follows [`crate::config::reload`] rather than mutating the live
//! graph incrementally.
//!
//! # Native CLI
//!
//! The module registers exactly `sccp version`, `sccp show devices`,
//! `sccp show lines`, `sccp show channels`, `sccp reload`, `sccp reset <device|all>`,
//! `sccp restart <device|all>`, bounded DND/message/call controls, and
//! `sccp set forwarding <device> <line> <all|busy|noanswer>
//! <destination|off>`. The three show commands provide bounded, deterministic
//! list and detail views with completion for their typed selectors. Mutating
//! commands complete only non-sensitive runtime identities and use the typed
//! control or feature provider. Forwarding validates an exact configured
//! appearance and uses the same serialized, persistence-backed transaction as
//! handset and AMI mutation; `off` clears only the selected forwarding kind.

#![cfg_attr(not(test), deny(clippy::expect_used, clippy::unwrap_used))]

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{CStr, CString, c_int};
use std::net::{IpAddr, Ipv4Addr};

mod adapters;
mod boundary;
mod direct;
mod exports;
mod phone;
#[path = "native/mod.rs"]
mod raw;
mod runtime;
mod static_descriptor;
mod sys;
#[cfg(feature = "telemetry")]
mod telemetry;
use adapters::{
    AsteriskCallCompletion, AsteriskDialplan, AsteriskHttp, AsteriskManager, AsteriskRecording,
    DialplanRegistration, HttpRegistration, ManagerActionRegistration, RecordingSession,
};
use raw::{bridge as native_bridging, channel as native_channel};
use runtime::Module;
use static_descriptor::StaticDescriptor;
use std::ptr::{self, NonNull};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::{Duration, Instant};

use boundary::{DeviceState, LogLevel};
use sccp_protocol::phone::xml::ConferenceListAction;
use sccp_protocol::{
    AppearanceRingMode, ApplicationId, AudioProcessingPolicy, ButtonDefinition, ButtonType,
    CallDirection, CallId, CallInfo, CallReference, CallSelectionOrder,
    CallState as PhoneCallState, Codec, Command as PhoneCommand,
    CommandAction as PhoneCommandAction, ConferenceId, DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
    DEFAULT_AUDIO_PACKET_MS, DeviceEvent as PhoneDeviceEvent,
    DeviceEventKind as PhoneDeviceEventKind, DeviceId, Digit,
    DoNotDisturbButtonMode as PhoneDndButtonMode, DoNotDisturbMode as PhoneDndMode, DtmfMode,
    Event as PhoneEvent, HandsetStatusMessage, IncomingOfferDelivery, IncomingOfferReceipt,
    IncomingPresentation, IncomingRing, IpAddressType, LineInstance, MediaEndpoint,
    MediaEndpointAddress, MediaStatus, MediaTrafficClass, MultimediaReceiveDescriptor,
    MultimediaTransmitControl, MultimediaTransmitDescriptor, PARKING_MENU_MAX_ITEMS,
    ParkingMenuEntry, ParticipantId, PhoneAlarmTelemetry, PhoneLocationTelemetry,
    PhoneServiceEvent, PhoneServicePayload, PhoneServicePriority, ProtocolVersion,
    ReceiveChannelPurpose, ReceiveTransmit, RecordingButtonState, RegistrationFallback,
    RegistrationTokenPolicy, ResetType, RingDuration, RingerMode, Server, ServerConfig,
    ServerHandle, ServerIngress, SignalingQos, SignalingSocket, SoftKey, StationIo,
    StationMediaCapabilities, StationSessionTarget, StationTransport, Tone, TransactionId,
    TransmitOpenOutcome,
};
use tokio::runtime::{Builder, Handle, Runtime};
use tokio::sync::{Mutex as AsyncMutex, Semaphore, mpsc};
use tokio::task::{AbortHandle, JoinHandle};

use crate::ami::controls::{
    CliControlError, ControlOperation, ControlOutcome, ControlProvider, ControlProviderError,
    MAX_ASSIGNED_CHANNEL_ID_BYTES, MAX_BOOLEAN_BYTES, MAX_CALL_ID_BYTES, MAX_DEVICE_SELECTOR_BYTES,
    MAX_DIAL_DESTINATION_BYTES, MAX_LINE_SELECTOR_BYTES, MAX_MESSAGE_BYTES, MAX_TIMEOUT_BYTES,
    MessageTarget, ResetMode, ResetTarget, complete_cli_device, complete_cli_reset_target,
    complete_cli_value, execute_cli_answer, execute_cli_device_control, execute_cli_end,
    execute_cli_message, execute_cli_originate, register_control_actions,
};
use crate::ami::events::{
    AmiEventError, AmiEventPublisher, FeatureChange, MediaDirection as AmiMediaDirection,
    MediaKind as AmiMediaKind, MediaState as AmiMediaState, RegistrationStatus, alarm_event,
    call_event, feature_changes, feature_event, media_event, registration_event, xml_alarm_event,
};
use crate::ami::features::{
    FeatureControlMutation, FeatureControlOutcome, FeatureControlProvider,
    FeatureControlProviderError, ForwardingKind, MAX_DND_MODE_BYTES, execute_cli_dnd,
    forwarding_ui_line_instances, parse_cli_forwarding_mutation, register_feature_control_actions,
};
use crate::ami::inventory::{
    InventoryProvider, InventoryProviderError, InventoryRegistration, InventorySnapshot,
    InventoryValue, configured_inventory, register_inventory_actions,
};
use crate::ami::runtime::{
    CallStatus, ConferenceParticipantStatus, ConferenceStatus, MediaDirection, MediaKind,
    MediaStatisticsStatus, MediaStreamStatus, RuntimeStatusProvider, RuntimeStatusProviderError,
    RuntimeStatusSnapshot, register_runtime_status_actions,
};
use crate::ami::services::{
    ConferenceCommand as AmiConferenceCommand, ParkingCommand as AmiParkingCommand,
    RecordingCommand as AmiRecordingCommand, RecordingRegistryError, ServiceControlProvider,
    ServiceOperation, ServiceOutcome, ServiceProviderError, register_service_control_actions,
};
use crate::call::auto_answer::{
    AutoAnswerMode, AutoAnswerPolicy, InboundDialRequest, parse_requestor_mode,
};
use crate::call::called_party::{
    CalledPartyOverride, CalledPartyProvider, CalledPartyProviderError,
    register_called_party_application,
};
use crate::call::completion::{CallCompletionError, CallCompletionOwnership};
use crate::call::dnd::{DndMutation, default_button_mode, handset_status_message};
use crate::call::forwarding::{
    ForwardingCommit, ForwardingContext, ForwardingDestination, ForwardingDigitOutcome,
    ForwardingEntryRegistry, ForwardingEntryTiming, ForwardingExpiryOutcome, ForwardingOperation,
    ForwardingRejection, ForwardingRouteReason, ForwardingWriteOutcome, NoAnswerTimerRegistry,
};
use crate::call::metadata::{
    CallMetadata, ConfiguredChannelMetadata,
    configured_channel_metadata as compose_channel_metadata,
};
use crate::call::mobility::{
    MOBILITY_APPLICATION_ID, MobilityAppearanceWriter, MobilityPreparation, MobilityRegistry,
    MobilitySlot, PreparedMobilityTransaction, authenticate_line, execute_mobility_io,
    mobility_login_document, parse_mobility_login_submission, rollback_mobility_io,
};
use crate::call::parking::{
    ParkedCall, ParkingEvent, ParkingEventKind, ParkingRegistry, ParkingSubscription,
    handset_call_id_from_channel,
};
use crate::call::shared_lines::{
    NoAnswerPolicy, SharedNoAnswerRoute, plan_inbound_bindings, plan_shared_no_answer_route,
};
use crate::call::transfer::{
    DeferredTransferAction, TransferCancellationReason, TransferCompletion, TransferCompletionKind,
    TransferMode, TransferPhase, TransferRejection, TransferSetupMilestone, TransferTrigger,
};
use crate::call::voicemail::{VoicemailOperation, VoicemailTarget};
use crate::config::convergence::{
    ConfigReconciliation, ConfigReconciliationObjectType, ConfigReconciliationOperation,
    ConfigReconciliationTrigger,
};
use crate::config::provider::{ConfigurationProvider, HybridConfigurationProvider};
use crate::config::reload::{MwiSubscriptionChange, ReloadPlan, ReloadSelection};
use crate::config::sorcery::SorceryConfigurationProvider;
use crate::config::{
    ConfigurationSource, DndButtonMode, LineBinding, ModuleConfig, NatMode,
    ParkingRetrievalBehavior, VideoMode,
};
use crate::http::directory::{
    DirectoryProvider, DirectoryProviderError, DirectoryRecord, records_from_config,
    register_directory_http,
};
use crate::media::addressing::{
    AddressSelectionPolicy, ExternalAddressCache, ResolvedExternalAddresses, SystemHostResolver,
    canonical_ip_address,
};
use crate::media::codec_preference::{
    CodecPreferenceContext, CodecPreferenceProvider, CodecPreferenceProviderError,
    register_codec_preference_application,
};
use crate::media::direct::{
    CONFERENCE_ANNOUNCEMENT_PLAYBACK_WINDOW, DirectMediaPolicy, DirectMediaRoute,
    MediaAnchorReason, MediaAnchorRegistry, MediaAnchorRestores,
};
use crate::media::formats::{
    PbxAudioFormat, PbxVideoFormat, negotiate_audio, pbx_audio_format, pbx_audio_formats_from_mask,
    pbx_video_formats_from_mask,
};
use crate::media::recording::{
    RecordingCallback, RecordingDirection, RecordingError, RecordingEvent, RecordingProvider,
    RecordingSessionControl, RecordingState, RecordingTarget, RecordingTogglePlan,
    RecordingToggleRejection, ordered_recording_start, ordered_recording_stop,
    plan_recording_toggle,
};
use crate::pbx::call_indication::{
    HandsetCallIndication, HandsetCallIndicationProvider, HandsetCallIndicationProviderError,
    register_handset_call_indication_application,
};
use crate::pbx::channel_metadata::{ChannelMetadataError, validate_native_channel_metadata};
use crate::pbx::handset_message::{
    HandsetMessageOperation, HandsetMessageProvider, HandsetMessageProviderError,
    register_handset_message_application,
};
use crate::pbx::operations::{BargeBridgeSession, BridgeSession, CallFeatureError};
use crate::pbx::party::{
    AsteriskChannel, ConnectedLineSource, ConnectedLineUpdate, NameCharset, NumberPlan,
    PartyIdentity, PartySnapshot, PartyUpdateError, Presentation, RedirectReasonCode,
    RedirectingUpdate, redirected_call_update, restore_redirecting_update,
    validate_redirecting_update,
};
use crate::pbx::query::channel::{
    ChannelAppearanceSnapshot, ChannelDirectionSummary, ChannelMediaStateSummary,
    ChannelQueryLookupError, ChannelQueryProvider, ChannelQuerySnapshot, ChannelQueryTarget,
    ChannelStateSummary, register_channel_query,
};
use crate::pbx::query::device::{
    DeviceCallSummary, DeviceDndSummary, DeviceFeatureSummary, DeviceQueryLookupError,
    DeviceQueryProvider, DeviceQuerySnapshot, DeviceQueryTarget, RegisteredDeviceSummary,
    register_device_query,
};
use crate::pbx::query::line::{
    AppearanceRingSummary, LineAppearanceSnapshot, LineCallSummary, LineQueryLookupError,
    LineQueryProvider, LineQuerySnapshot, LineQueryTarget, register_line_query,
};
use crate::pbx::registration::{
    RegistrationContextRegistry, RegistrationRegistryError, configured_registration_appearances,
};
use crate::presence::blf::{BlfEvent, BlfSubscriptions};
use crate::runtime::backend::{
    BargeOperation, BridgeBackend, BridgeOperation, CallServiceBackend, ChannelBackend,
    ConferenceAnnouncement, ConferenceAnnouncementOperation, ConferenceDestinationOperation,
    ConferenceStartProgress, DriverEffect, EffectExecutionError, HandsetEffect, ManagementBackend,
    ManagementEvent, MediaBackend, ParkingOperation, PbxBackendError, PbxBridgeId, PbxCallId,
    PbxEffect, PbxServiceCapabilities, PickupOperation, PickupOutcome, SupplementaryBackend,
    execute_cleanup_effects as execute_backend_cleanup_effects,
};
use crate::runtime::conference_announcement::{
    AnnouncementAdapter, AnnouncementCall, AnnouncementFailureStage, AnnouncementGeneration,
    MAX_RESTORE_ATTEMPTS, allocate_generation as allocate_announcement_generation,
    generation_is_current as announcement_generation_is_current, replacement_anchor_plan,
    restore_attempts_exhausted, start_announcement,
};
use crate::runtime::conference_tasks::{
    ConferenceTaskCancellation, ConferenceTaskRegistry, ConferenceTaskStartError,
};
use crate::runtime::controller::{
    BargeMode, BargeRejection, CallState, CallTransition, CallTransitionProgress,
    CodecPreferenceRejection, ConferenceConsultationRequest, ConferenceDestinationRequest,
    ConferenceEndRejection, ConferenceMediaPolicy, ConferenceMutationToken,
    ConferenceParticipantRejection, ConferencePhase, ConferenceRejection, Controller,
    DeviceFeatureState, DndMode, HookFlashAction, HotlineCallRequest, InboundCallDisposition,
    InboundUnavailableReason, MediaStreamState, OutboundMediaMode, ParkingRejection,
    PickupRejection, RemoteHangupPlan, TransferCompletionPlan, TransferConsultationRequest,
    VoicemailNativeOutcome, VoicemailPlan, controller_step,
};
use crate::state::features::{
    FeatureStore, FeatureStoreError, configured_feature_state, registration_state_or_fallback,
};
use adapters::bridging::native_pickup_result;
use adapters::{
    AsteriskCallFeatures, AsteriskChannelMetadata, AsteriskHints, AsteriskPartyUpdates,
};
use adapters::{
    AsteriskDatabase, AsteriskParking, AsteriskRealtime, AsteriskRegistrationExtensions,
    AsteriskSorcerySource,
};

const NORMAL_CLEARING: c_int = 16;
const REQUESTED_CHANNEL_UNAVAILABLE: c_int = 44;
const USER_BUSY: c_int = 17;
const PARKING_CONFIRM_TIMEOUT: Duration = Duration::from_secs(8);
const PARKING_NOTIFICATION_TIME: Duration = Duration::from_secs(3);
const MANAGER_CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const MANAGER_CONTROL_DELIVERY_TIMEOUT: Duration = Duration::from_secs(8);
const REMOTE_HANGUP_PRESENTATION_TIME: Duration = Duration::from_secs(15);

static MODULE: Mutex<Option<Module>> = Mutex::new(None);
