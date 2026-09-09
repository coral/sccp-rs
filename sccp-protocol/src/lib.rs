//! Typed Skinny Client Control Protocol messages and an asynchronous station server.
//!
//! The crate separates the phone-facing wire protocol from the call-control
//! application. [`Server`] owns station connections and translates inbound
//! packets into semantic [`Event`] values. Applications respond through a
//! cloneable [`ServerHandle`] using typed [`Command`] values; no SIP or PBX
//! policy is built into this crate.
//!
//! # Typical workflow
//!
//! 1. Build and validate one or more [`DeviceDefinition`] values.
//! 2. Start [`Server::bind`], spawn [`Server::run`], and retain its
//!    [`ServerHandle`] and event receiver.
//! 3. Consume events in order. Registration, call input, media
//!    acknowledgements, and disconnects all arrive through the same stream.
//! 4. Send commands through the handle. Use [`ServerHandle::send_confirmed`]
//!    when later work depends on the complete frame having reached the station
//!    socket; [`ServerHandle::send`] confirms queue admission only.
//! 5. Call [`ServerHandle::shutdown`] and await the server task during orderly
//!    application shutdown.
//!
//! ```no_run
//! use sccp_protocol::{
//!     ButtonDefinition, DeviceDefinition, DeviceId, LineAppearance, LineDefinition,
//!     Server, ServerConfig, SoftKeyProfile, StationTransportRequirement, StationUiPolicy,
//! };
//!
//! # async fn example() -> Result<(), Box<dyn std::error::Error>> {
//! let station = DeviceDefinition {
//!     id: DeviceId::new("SEP001122334455")?,
//!     description: "Front desk".into(),
//!     transport: StationTransportRequirement::Either,
//!     signaling_qos: None,
//!     buttons: vec![ButtonDefinition::Line(LineAppearance::new(
//!         1,
//!         LineDefinition {
//!             number: "1001".into(),
//!             display_name: "Reception".into(),
//!         },
//!     ))],
//!     soft_keys: SoftKeyProfile::default(),
//!     ui: StationUiPolicy::default(),
//! };
//! station.validate()?;
//!
//! let (server, handle, mut events) = Server::bind(ServerConfig::default(), [station]).await?;
//! let server_task = tokio::spawn(server.run());
//!
//! if let Some(event) = events.recv().await {
//!     println!("{event:?}");
//! }
//!
//! handle.shutdown().await?;
//! server_task.await??;
//! # Ok(())
//! # }
//! ```
//!
//! # Choosing an API layer
//!
//! Most applications use the crate-root re-exports plus [`server`] and
//! [`types`]. [`message`] exposes framing, message IDs, codecs, and typed wire
//! models for protocol tools or custom transports. [`phone`] contains bounded
//! phone-hosted XML, authentication, service, and provisioning models.
//! [`qos`] owns service-node reservation transitions without sharing handset
//! session state. To supply an externally accepted transport—such as a TLS
//! stream—construct the server with [`Server::with_ingress`] and feed streams through
//! [`ServerIngress`].

#![deny(missing_debug_implementations)]

pub mod message;
pub mod phone;
pub mod qos;
pub mod server;
pub mod types;

/// Validated IANA timezone used for station wall-clock fields (including DST).
pub use chrono_tz::Tz as TimeZone;

pub use message::capabilities::{
    CapabilityUpdate, CapabilityUpdateVariant, ConferenceResource, ConferenceServiceResource,
    CustomPictureFormat, DataCapability, StationMediaCapabilities, VideoCapability,
    VideoLevelPreference,
};
pub use message::catalog::{MessageDirection, MessageId, MessageRoute};
pub use message::values::{
    AddParticipantResult, AlarmSeverity, AnnouncementPlayMode, AnnouncementPlayStatus,
    AuditParticipantResult, BusyLampFieldState, ButtonType, CallForwardKind,
    CallHistoryDisposition, CallInfoVisibility, CallPriority, CallSecurityState, CallState,
    CallType, Codec, CodecKind, ConferenceResourceType, CreateConferenceResult,
    DeleteConferenceResult, DeviceType, Digit, DtmfMode, DynamicCallInfoLayout, EchoCancellation,
    EncryptionCapability, EncryptionMethod, EndOfAnnouncementAck, G723BitRate, IpAddressType,
    KeyMode, LampMode, LayoutProfile, MediaPathCapability, MediaPathEvent, MediaPathId,
    MediaStatus, MediaTransport, MediaType, MessageWaitingResult, MicrophoneMode, MiscCommandType,
    ModifyConferenceResult, NotificationPriority, PartyInformationRestrictions, PhoneFeatures,
    ProtocolVersion, QosDirection, QosErrorCode, QosReservationStyle,
    RFC2833_TELEPHONE_EVENT_PAYLOAD, ReceiveTransmit, ResetType, RingDuration, RingerMode,
    RsvpErrorCode, SilenceSuppression, SoftKey, SpeakerMode, StationSessionContext,
    StatisticsProcessing, Stimulus, SubscriptionCause, Tone, ToneDirection, UnregisterStatus,
    VideoFormat,
};
pub use message::wire::{CodecError, Frame, FrameDecoder, MAX_FRAME_SIZE};
pub use message::{
    AddParticipantRequest, AddParticipantResponse, AnnouncementEntry, AudioStreamControl,
    AuditConferenceEntry, AuditConferenceResponse, AuditParticipantResponse, BoundedBytes,
    BoundedBytesError, ButtonTemplateEntry, CALL_COUNT_REQUEST_EXTENDED_BYTES,
    CALL_COUNT_RESPONSE_MAX_LINE_ENTRIES, CONNECTION_QUALITY_MAX_BYTES, CallCountLineData,
    CallCountRequestPayload, CallCountResponse, ChangeParticipantRequest, ClientMessage,
    ConferenceParticipant, ConferenceParticipantChange, ConfigurationStatus,
    ConnectionQualityStatistics, ConnectionStatistics, ControlMessage, CreateConferenceRequest,
    CreateConferenceResponse, DtmfPayloadIdentity, DtmfPayloadRequest, DtmfToneControl,
    ExtensionDeviceCapabilities, KnownOpaqueMessage, MAX_MULTIMEDIA_PICTURE_FORMATS,
    MAX_SIGNALING_SERVERS, MEDIA_PORT_LIST_MAX_PORTS, MULTIMEDIA_CAPABILITY_BYTES, MediaCapability,
    MediaEncryption, MediaEndpointAddress, MediaFailureDetection, MediaPortList,
    MediaResourceNotification, MediaTransmissionAck, MessageWaitingCounts,
    MessageWaitingNotification, MiscellaneousCommand, ModifyConferenceRequest,
    ModifyConferenceResponse, MulticastMediaReception, MulticastMediaTransmission,
    MultimediaCapabilityError, MultimediaPayload, MultimediaPayloadDescriptor,
    MultimediaPictureFormat, MultimediaStreamControl, MultimediaVideoCapability,
    MultimediaVideoCapabilityArm, OpenMultimediaChannel, OpenMultimediaReceiveChannelAck,
    ParticipantChangeRouting, PortClose, PortEndpoint, PortRequest, QosApplicationIdentifier,
    QosFlow, QosTrafficSpecification, RawMessage, RegisterTokenMessage, RegistrationMessage,
    RegistrationWireDetails, RegistrationWireLayout, RtpPayloadNumber, RtpPayloadNumberError,
    ServerMessage, SessionTransmission, SignalingServerEndpoint, SpcpRegisterTokenMessage,
    StartMultimediaTransmission, StartMultimediaTransmissionAck, SubscriptionRequest,
    UserDataMessage, UserDataV1Message, VideoFlowControl, XML_ALARM_CANONICAL_DOCUMENT_BYTES,
    XML_ALARM_CANONICAL_WIRE_BYTES, XML_ALARM_MAX_WIRE_BYTES, XmlAlarmMessage,
};
pub use phone::authentication::{
    OpaquePhoneAuthenticationResponse, PHONE_AUTHENTICATION_MAX_PASSWORD_BYTES,
    PHONE_AUTHENTICATION_MAX_QUERY_BYTES, PHONE_AUTHENTICATION_MAX_RESPONSE_BYTES,
    PHONE_AUTHENTICATION_MAX_USER_ID_BYTES, PhoneAuthenticationError, PhoneAuthenticationPassword,
    PhoneAuthenticationRequest, PhoneAuthenticationResponse, PhoneAuthenticationUserId,
};
pub use phone::service::{
    CiscoIpPhoneError, CiscoIpPhoneResponse, CiscoIpPhoneResponseItem, PhoneExecuteStatus,
    PhoneServiceError, PhoneServiceErrorCode, PhoneServiceEvent, PhoneServiceExtendedRouting,
    PhoneServiceMessageKind, PhoneServicePayload, PhoneServiceRouting, PhoneServiceSubmission,
    PhoneServiceSubmittedValue, parse_phone_service_payload,
};
pub use phone::xml::{
    CiscoIpPhoneAlarm, CiscoIpPhoneAlarmEntry, CiscoIpPhoneAlarmEnum, CiscoIpPhoneAlarmParameter,
    CiscoIpPhoneAlarmParameterList, CiscoIpPhoneAlarmString, CiscoIpPhoneBackground,
    CiscoIpPhoneDirectory, CiscoIpPhoneDirectoryEntry, CiscoIpPhoneExecute,
    CiscoIpPhoneExecuteItem, CiscoIpPhoneGraphicFileMenu, CiscoIpPhoneGraphicMenu,
    CiscoIpPhoneIconFileItem, CiscoIpPhoneIconFileMenu, CiscoIpPhoneIconItem, CiscoIpPhoneIconMenu,
    CiscoIpPhoneIconMenuItem, CiscoIpPhoneIconTitle, CiscoIpPhoneImage, CiscoIpPhoneImageFile,
    CiscoIpPhoneImageList, CiscoIpPhoneImageListItem, CiscoIpPhoneInput, CiscoIpPhoneInputItem,
    CiscoIpPhoneKeyItem, CiscoIpPhoneLocationInformation, CiscoIpPhoneMenu, CiscoIpPhoneMenuItem,
    CiscoIpPhoneOffPremises, CiscoIpPhoneSetBackground, CiscoIpPhoneSetBackgroundPreview,
    CiscoIpPhoneSetRingTone, CiscoIpPhoneSoftKeyItem, CiscoIpPhoneStatus, CiscoIpPhoneStatusFile,
    CiscoIpPhoneText, CiscoIpPhoneTouchAreaMenuItem, CiscoIpPhoneWifiLocation,
    ConferenceListAction, ConferenceListDocument, ConferenceListEntry, ConferenceMenuFamily,
    ConferenceParticipantActionsDocument, OpaquePhoneAlarm, OpaquePhoneLocation,
    PHONE_ALARM_MAX_BYTES, PHONE_BACKGROUND_APPLICATION_ID, PHONE_BACKGROUND_CONTROL_MAX_BYTES,
    PHONE_BACKGROUND_LIST_MAX_BYTES, PHONE_BACKGROUND_LIST_MAX_ITEMS, PHONE_DIRECTORY_MAX_BYTES,
    PHONE_DIRECTORY_MAX_ENTRIES, PHONE_EXECUTE_MAX_BYTES, PHONE_EXECUTE_MAX_ITEMS,
    PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS, PHONE_GRAPHIC_MENU_MAX_ITEMS, PHONE_ICON_MENU_MAX_ICONS,
    PHONE_ICON_MENU_MAX_ITEMS, PHONE_IMAGE_BITMAP_MAX_BYTES, PHONE_IMAGE_MAX_BYTES,
    PHONE_INPUT_MAX_BYTES, PHONE_INPUT_MAX_ITEMS, PHONE_LOCATION_MAX_BYTES, PHONE_MENU_MAX_BYTES,
    PHONE_MENU_MAX_ITEMS, PHONE_RINGTONE_APPLICATION_ID, PHONE_RINGTONE_MAX_BYTES,
    PHONE_STATUS_BITMAP_MAX_BYTES, PHONE_STATUS_MAX_BYTES, PHONE_TEXT_APPLICATION_ID,
    PHONE_TEXT_LEGACY_MAX_CHARS, PHONE_TEXT_MAX_BYTES, PHONE_TEXT_MAX_CHARS,
    PHONE_XML_MAX_NESTING_DEPTH, PhoneActionKind, PhoneAlarmKind, PhoneAlarmSummary,
    PhoneAlarmTelemetry, PhoneBackgroundControlDocument, PhoneBackgroundHttpUrl,
    PhoneBackgroundTftpUrl, PhoneBitmapData, PhoneBssid, PhoneExecutePriority, PhoneExecuteUrl,
    PhoneImageDocument, PhoneImageUrl, PhoneInputFlags, PhoneInputParameterName, PhoneKeypadTarget,
    PhoneLocationKind, PhoneLocationSummary, PhoneLocationTelemetry, PhoneRingtoneUrl,
    PhoneServicePriority, PhoneSoftKeyPosition, PhoneStatusDocument, PhoneTouchArea, PhoneXmlError,
    PhoneXmlKey, PhoneXmlRefresh, from_bytes as parse_phone_xml, parse_phone_alarm,
    parse_phone_location, to_string as serialize_phone_xml,
};
pub use qos::{
    QosReservationController, QosReservationError, QosReservationEvent, QosReservationFailure,
    QosReservationId, QosReservationLimits, QosReservationPolicy, QosReservationRequest,
    QosReservationSetup, QosReservationState, QosTransition,
};
pub use server::{
    AnonymousHotlineDefinition, CallSelectionOrder, Command, CommandAction, DeviceEvent,
    DeviceEventKind, DoNotDisturbButtonMode, DoNotDisturbMode, Event, HandsetAcknowledgement,
    HandsetStatusMessage, IncomingOfferDelivery, IncomingOfferReceipt, IncomingPresentation,
    IncomingRing, MAX_REGISTRATION_BACKOFF, MIN_REGISTRATION_BACKOFF, MediaStatisticsSnapshot,
    MulticastMediaRoute, MultimediaReceiveDescriptor, MultimediaTransmitControl,
    MultimediaTransmitDescriptor, PARKING_MENU_MAX_ITEMS, ParkingMenuEntry, PriorityEvent,
    PriorityEventPermit, ReceiveChannelPurpose, ReconfigureResult, RecordingButtonState,
    RegistrationFallback, RegistrationTokenPolicy, Server, ServerConfig, ServerError, ServerHandle,
    ServerIngress, SignalingServerRoute, SignalingSocket, SocketQosFailure, SocketQosMark,
    SocketQosPolicy, SocketQosReport, StationIo, StationSessionTarget, StationSocketQos,
    TransmitOpenOutcome, VideoPictureReference, VideoPictureReferences, apply_socket_qos,
};
pub use server::{
    ObservationConnectionId, ServerObservation, ServerObservationKind, SignalingDirection,
    SignalingFidelity, SignalingObservation, StationDisconnectReason,
};
pub use types::{
    AddonModuleDefinition, AppearanceId, AppearanceRingMode, ApplicationId, AudioProcessingPolicy,
    BlfCallerInfo, BlfSpeedDialDefinition, BlfState, ButtonDefinition, CallDirection, CallId,
    CallInfo, CallReference, CallerIdOverride, ConferenceId, DEFAULT_AUDIO_MAX_FRAMES_PER_PACKET,
    DEFAULT_AUDIO_PACKET_MS, DateTemplate, DeviceDefinition, DeviceId, DeviceRegistration,
    FeatureDefinition, LegacyCodePage, LineAppearance, LineDefinition, LineInstance, MediaEndpoint,
    MediaTrafficClass, ParticipantId, PassthroughPartyId, RecordingButtonDefinition,
    ServiceDefinition, SessionGeneration, SignalingQos, SoftKeyProfile, SpeedDialDefinition,
    StationTransport, StationTransportRequirement, StationUiPolicy, TransactionId,
};
