//! Handset-facing SCCP event handling and feature orchestration.
//!
//! The child modules separate core call signaling, parking and pickup,
//! per-device feature state, and conference behavior while sharing only the
//! private Asterisk composition context from the parent module.

use crate::asterisk::boundary::MutexExt;
use crate::asterisk::runtime::{
    Access, AsteriskBackend, AsteriskBackendError, MediaFailureDisposition, PendingPark,
    PendingParkingNotification, PendingRetrieval, RuntimeRecordings, ast_log,
    cancel_conference_announcement, conference_participant_service_error, configure_pickup_policy,
    dial_terminator_digit, execute_answer_call_transition, execute_call_transition,
    execute_cleanup_effects, execute_effects, execute_effects_confirmed,
    execute_forwarding_mutation, execute_handset_effect, execute_one_effect,
    execute_service_effects, handset_effects, install_blf, normalize_phone_media_endpoint,
    normalize_phone_video_endpoint, parking_service_error, preferred_codec,
    prune_recording_sessions, publish_device_lines, publish_line, publish_recording_button_state,
    recover_failed_media_transmission, registered_device_ids, remove_channel,
    restore_system_message, retain_two_channels, send_confirmed_service, set_remote_video_endpoint,
    toggle_monitor_recording, uninstall_device_blf, with_channel,
};
use crate::asterisk::{
    AmiMediaDirection, AmiMediaKind, AmiMediaState, ApplicationId, AsteriskCallCompletion,
    AsteriskChannel, BargeMode, BargeRejection, BridgeOperation, ButtonDefinition, ButtonType,
    CallCompletionError, CallCompletionOwnership, CallDirection, CallFeatureError, CallId,
    CallInfo, CallReference, CallState, ConferenceAnnouncement, ConferenceConsultationRequest,
    ConferenceDestinationRequest, ConferenceEndRejection, ConferenceId, ConferenceListAction,
    ConferenceMediaPolicy, ConferenceMutationToken, ConferenceParticipantRejection,
    ConferencePhase, ConferenceRejection, ConferenceStartProgress, DeferredTransferAction,
    DeviceFeatureState, DeviceId, Digit, DndButtonMode, DndMode, DndMutation, DriverEffect,
    Duration, EffectExecutionError, FeatureChange, FeatureControlProviderError, FeatureStoreError,
    ForwardingCommit, ForwardingDestination, ForwardingDigitOutcome, ForwardingEntryTiming,
    ForwardingExpiryOutcome, ForwardingKind, ForwardingOperation, ForwardingRejection,
    ForwardingRouteReason, ForwardingWriteOutcome, HandsetEffect, HandsetStatusMessage,
    HookFlashAction, HotlineCallRequest, Instant, LineInstance, LogLevel,
    MANAGER_CONTROL_DELIVERY_TIMEOUT, MOBILITY_APPLICATION_ID, ManagementEvent, MediaEndpoint,
    MediaStatus, MediaStreamState, MobilityAppearanceWriter, MobilityPreparation, MobilitySlot,
    ModuleConfig, NonNull, Ordering, PARKING_CONFIRM_TIMEOUT, PARKING_MENU_MAX_ITEMS,
    PARKING_NOTIFICATION_TIME, ParkedCall, ParkingEvent, ParkingEventKind, ParkingMenuEntry,
    ParkingRejection, ParkingRetrievalBehavior, ParticipantId, PbxAudioFormat, PbxCallId,
    PbxEffect, PhoneAlarmTelemetry, PhoneCallState, PhoneCommand, PhoneCommandAction,
    PhoneDeviceEvent, PhoneDeviceEventKind, PhoneDndButtonMode, PhoneDndMode, PhoneEvent,
    PhoneLocationTelemetry, PhoneServiceEvent, PhoneServicePayload, PhoneServicePriority,
    PickupRejection, PreparedMobilityTransaction, RegistrationStatus, ServiceProviderError,
    SoftKey, Tone, TransactionId, TransferCancellationReason, TransferCompletion,
    TransferCompletionKind, TransferCompletionPlan, TransferConsultationRequest, TransferMode,
    TransferPhase, TransferRejection, TransferSetupMilestone, TransferTrigger, TransmitOpenOutcome,
    VoicemailNativeOutcome, VoicemailPlan, VoicemailTarget, alarm_event, authenticate_line,
    configured_feature_state, controller_step, default_button_mode, execute_mobility_io,
    feature_changes, feature_event, forwarding_ui_line_instances, handset_call_id_from_channel,
    handset_status_message, media_event, mobility_login_document, native_channel,
    parse_mobility_login_submission, registration_event, registration_state_or_fallback,
    rollback_mobility_io, xml_alarm_event,
};

mod calls;
mod conference;
mod features;
mod forwarding;
mod mobility;
mod parking;
mod transfer;

use calls::handle_handset_hangup;
pub use calls::{handle_hold_or_resume, handle_phone_event};
use conference::{
    conference_mutation_is_active, display_conference_prompt, handle_barge_soft_key,
    handle_conference_destination, handle_conference_list_action, handle_conference_soft_key,
    handle_join_soft_key,
};
pub use conference::{
    remove_conference_participant, set_conference_participant_moderator,
    set_conference_participant_muted, show_conference_list,
};
pub(super) use features::{
    RuntimeDndMutation, RuntimeDndMutationError, execute_dnd_mutation,
    execute_dnd_mutation_serialized,
};
use features::{
    cancel_forwarding_entry_for_call, cancel_forwarding_entry_for_device, commit_forwarding_entry,
    forwarding_entry_exists, handle_dnd_button, handle_feature_button, handle_feature_soft_key,
    handle_forwarding_backspace, handle_forwarding_digit, handle_recording_button,
    handle_voicemail_soft_key, replace_and_commit_forwarding_entry, replace_forwarding_entry,
};
pub use features::{
    expire_forwarding_entries, log_feature_store_error, publish_ami_event, publish_device_features,
    publish_feature_changes, update_device_features_locked,
};
pub use forwarding::{cancel_no_answer_timer, clear_no_answer_route, expire_no_answer_routes};
pub use mobility::{configured_mobility_button, mobility_device_registered};
use mobility::{handle_mobility_button, handle_mobility_response, restore_mobility_appearances};
pub use parking::{begin_parking_retrieval, expire_parking_attempts, handle_parking_event};
use parking::{handle_park_request, handle_parking_lot_button, handle_pickup_soft_key};
