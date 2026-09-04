//! SCCP/SPCP message identifiers and direction metadata for the message domain.
//!
//! The numeric values are protocol facts.  The catalog intentionally includes
//! messages which this crate can only preserve opaquely today: knowing the ID
//! is still useful for bounded forwarding and future typed implementations.
//!
//! Start with [`MessageId`] when inspecting an unknown frame. Its
//! [`MessageId::contract`] links the numeric identifier to routing, payload
//! bounds, codec coverage, response selection, and field fidelity. Use
//! [`implemented_message_contracts`] to enumerate the typed subset.

use std::fmt;

use super::values::ProtocolVersion;
use super::wire::{HEADER_SIZE, MAX_FRAME_SIZE};

/// The protocol roles between which a message is normally sent.
///
/// SCCP is not solely a station/client protocol. Conference resources, media
/// resource services, and call-control peers share the same numeric message
/// space. Keeping those routes explicit prevents a decoder or runtime from
/// treating a service-node frame as handset input merely because both travel
/// toward call control.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageRoute {
    StationToControl,
    ControlToStation,
    ControlToServiceNode,
    ServiceNodeToControl,
    IntraControl,
}

/// Legacy station-oriented view of the two handset message directions.
///
/// New code should use [`MessageRoute`]. A service-node or intra-control
/// message deliberately has no `MessageDirection`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum MessageDirection {
    DeviceToServer,
    ServerToDevice,
}

/// How completely the public message model implements a catalog entry.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CodecSupport {
    /// The message has a typed public representation and a checked codec.
    Typed,
    /// Only the identifier, direction, and opaque bytes are preserved.
    OpaqueOnly,
}

/// The rule used to choose and bound a message payload layout.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PayloadLayout {
    /// No semantic payload bytes are carried.
    Empty,
    /// One fixed layout is used for all supported protocol versions.
    Fixed,
    /// The negotiated protocol selects between fixed layouts.
    VersionSelected,
    /// The negotiated protocol and exact body length jointly select a layout.
    VersionAndLengthSelected,
    /// A typed fixed prefix is decoded while a bounded extension is preserved.
    MinimumLengthPreserved,
    /// A bounded length/count field controls a variable tail.
    LengthPrefixed,
    /// A bounded payload is retained exactly while consumers may inspect it.
    BoundedPreserved,
    /// A bounded extension is retained byte-for-byte because its internal
    /// schema is not modeled.
    BoundedOpaque,
    /// NUL-terminated station strings are followed by zero bytes to a
    /// four-byte boundary.
    DynamicWordPadded,
    /// The crate deliberately does not interpret the payload.
    Opaque,
}

/// Whether application code can construct a message without supplying raw
/// wire bytes.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum EmissionSupport {
    /// A typed encoder is available.
    Typed,
    /// Bytes can be forwarded explicitly through `KnownOpaque`, but there is
    /// no typed constructor and runtime code must not synthesize the message.
    PreserveOnly,
}

/// Present production/runtime role of a known message.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum RuntimeUse {
    /// A typed phone-originated input accepted by the session runtime.
    DeviceInput,
    /// A server response required by a currently handled phone request.
    RequiredResponse,
    /// A server output emitted only for the corresponding configured feature
    /// or call state.
    ConditionalServerOutput,
    /// A typed service-node input accepted by its independent runtime.
    ServiceNodeInput,
    /// A service-node output emitted only for an owned reservation transition.
    ConditionalServiceNodeOutput,
    /// The codec is typed for conformance/testing, but ordinary runtime flows
    /// intentionally do not emit it.
    TypedButNotEmitted,
    /// Only catalog metadata and explicit opaque preservation are supported.
    CatalogOnly,
}

/// Whether all semantic wire fields survive typed decoding and re-encoding.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum FieldFidelity {
    /// Every accepted semantic field is represented; reserved/padding bytes
    /// are validated rather than exposed.
    Lossless,
    /// A server-only producer omits or fills the named fields. Decoding may
    /// project other values, so this is not an exact decode/re-encode guarantee.
    CanonicalServerOutput(&'static str),
    /// Typed decoding is intentionally projected onto the named runtime data.
    SemanticProjection(&'static str),
    /// The uninterpreted bounded body is retained exactly.
    OpaquePreserved,
}

/// SCCP-level response expected for a request or media transaction.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ResponseExpectation {
    None,
    Message(MessageId),
    OptionalMessage(MessageId),
    /// The negotiated protocol selects the response identifier.
    VersionSelected {
        /// Response used before `minimum_protocol`.
        before: MessageId,
        /// Response used at and after `minimum_protocol`.
        from: MessageId,
        /// First protocol version that selects `from`.
        minimum_protocol: u8,
    },
    /// Negotiated session inputs select the response identifier.
    SessionSelected {
        /// Response used when `selector` does not select the dynamic form.
        before: MessageId,
        /// Dynamic response selected by `selector`.
        from: MessageId,
        /// Session rule that chooses between the response identifiers.
        selector: SessionResponseSelector,
    },
    /// The response may be any member of this family.
    OneOf(&'static [MessageId]),
}

/// Session rule used to select a dynamic response identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SessionResponseSelector {
    /// Select the dynamic form when the feature is present or the negotiated
    /// protocol meets the stated minimum.
    DynamicMessagesOrProtocol { minimum_protocol: u8 },
    /// Select the dynamic form only when the feature is present.
    DynamicMessages,
}

/// Verification depth for a wire contract.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContractVerification {
    Structural,
    StructuralAndValidated,
}

/// Whether an identifier belongs to the base station-control inventory or an
/// independently supported extension family.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ContractScope {
    Base,
    Supplemental,
}

/// Inclusive payload-size bounds, excluding the 12-byte frame header.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PayloadSizeBounds {
    /// Smallest accepted payload in bytes.
    pub minimum: usize,
    /// Largest accepted payload in bytes.
    pub maximum: usize,
}

/// Machine-readable support record for one known message identifier.
///
/// This is an implementation inventory, not a claim that every cataloged
/// message is safe to send. `OpaqueOnly` entries exist for bounded forwarding
/// and remain non-emittable through the typed API. `response`
/// describes SCCP transaction acknowledgement; TCP acknowledgement is
/// intentionally not treated as application-level acceptance.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct MessageContract {
    pub id: MessageId,
    pub scope: ContractScope,
    pub route: MessageRoute,
    pub codec: CodecSupport,
    pub payload_layout: PayloadLayout,
    /// Canonical typed-encoder payload size when there is one stable,
    /// independently useful value. This excludes the 12-byte frame header;
    /// nominally empty decoders may still accept bounded extension bytes.
    pub fixed_payload_bytes: Option<usize>,
    /// Accepted payload-size range when both bounds are known.
    pub payload_size_bounds: Option<PayloadSizeBounds>,
    /// Typed construction versus explicit opaque preservation.
    pub emission: EmissionSupport,
    /// Production/runtime use, distinct from mere encoder availability.
    pub runtime_use: RuntimeUse,
    /// Whether the typed model retains every accepted semantic wire field.
    pub field_fidelity: FieldFidelity,
    /// SCCP response/acknowledgement family, when one exists.
    pub response: ResponseExpectation,
    /// Depth of contract validation performed by the codec.
    pub verification: ContractVerification,
}

/// Contract fields which are declared once beside a message's numeric ID and route.
///
/// Keeping the complete metadata record in the catalog entry prevents independent
/// exhaustive matches from drifting or describing an incoherent wire contract.
#[derive(Clone, Copy)]
struct ContractMetadata {
    scope: ContractScope,
    codec: CodecSupport,
    payload_layout: PayloadLayout,
    fixed_payload_bytes: Option<usize>,
    payload_size_bounds: Option<PayloadSizeBounds>,
    runtime_use: RuntimeUse,
    field_fidelity: FieldFidelity,
    response: ResponseExpectation,
    verification: ContractVerification,
}

impl ContractMetadata {
    const fn into_contract(self, id: MessageId, route: MessageRoute) -> MessageContract {
        MessageContract {
            id,
            scope: self.scope,
            route,
            codec: self.codec,
            payload_layout: self.payload_layout,
            fixed_payload_bytes: self.fixed_payload_bytes,
            payload_size_bounds: self.payload_size_bounds,
            emission: match self.codec {
                CodecSupport::Typed => EmissionSupport::Typed,
                CodecSupport::OpaqueOnly => EmissionSupport::PreserveOnly,
            },
            runtime_use: self.runtime_use,
            field_fidelity: self.field_fidelity,
            response: self.response,
            verification: self.verification,
        }
    }
}

macro_rules! message_catalog {
    ($(($variant:ident $(=> $wire_name:ident)?, $value:expr, $route:ident, $metadata:expr)),+ $(,)?) => {
        /// A Skinny message identifier.
        ///
        /// Unknown values are retained to keep decoding forward-compatible.
        #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
        pub enum MessageId {
            $($variant,)+
            Unknown(u32),
        }

        impl MessageId {
            pub const ALL_KNOWN: &'static [Self] = &[$(Self::$variant,)+];
            /// Declarative contract records generated in catalog order.
            pub const ALL_CONTRACTS: &'static [MessageContract] = &[
                $($metadata.into_contract(Self::$variant, MessageRoute::$route),)+
            ];

            pub const fn wire_value(self) -> u32 {
                match self {
                    $(Self::$variant => $value,)+
                    Self::Unknown(value) => value,
                }
            }

            /// Returns the protocol route for a known identifier.
            ///
            /// Unknown identifiers return `None` because direction cannot be
            /// inferred from their numeric value alone.
            pub const fn route(self) -> Option<MessageRoute> {
                match self {
                    $(Self::$variant => Some(MessageRoute::$route),)+
                    Self::Unknown(_) => None,
                }
            }

            /// Return the legacy two-ended station direction, if applicable.
            pub const fn direction(self) -> Option<MessageDirection> {
                match self.route() {
                    Some(MessageRoute::StationToControl) => {
                        Some(MessageDirection::DeviceToServer)
                    }
                    Some(MessageRoute::ControlToStation) => {
                        Some(MessageDirection::ServerToDevice)
                    }
                    Some(MessageRoute::ControlToServiceNode)
                    | Some(MessageRoute::ServiceNodeToControl)
                    | Some(MessageRoute::IntraControl)
                    | None => None,
                }
            }

            pub const fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => stringify!($variant),)+
                    Self::Unknown(_) => "Unknown",
                }
            }

            pub const fn is_known(self) -> bool {
                !matches!(self, Self::Unknown(_))
            }

            /// Return the codec and wire contract for this identifier.
            pub const fn contract(self) -> Option<MessageContract> {
                match self {
                    $(
                        Self::$variant => Some(
                            $metadata.into_contract(Self::$variant, MessageRoute::$route)
                        ),
                    )+
                    Self::Unknown(_) => None,
                }
            }
        }

        impl From<u32> for MessageId {
            fn from(value: u32) -> Self {
                match value {
                    $($value => Self::$variant,)+
                    value => Self::Unknown(value),
                }
            }
        }

        /// Raw identifiers used internally where Rust patterns require integer constants.
        pub(crate) mod wire_id {
            $(
                $(pub(crate) const $wire_name: u32 =
                    super::MessageId::$variant.wire_value();)?
            )+
        }
    };
}
message_catalog! {
    (KeepAlive => KEEP_ALIVE, 0x0000, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("nominally empty request; bounded extension bytes are accepted but not modeled"), response: ResponseExpectation::Message(MessageId::KeepAliveAck), verification: ContractVerification::StructuralAndValidated }),
    (Register => REGISTER, 0x0001, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 32, maximum: 172 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("device and firmware text are normalized while the exact length-selected layout is retained"), response: ResponseExpectation::Message(MessageId::RegisterAck), verification: ContractVerification::StructuralAndValidated }),
    (IpPort => IP_PORT, 0x0002, StationToControl, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (KeypadButton => KEYPAD_BUTTON, 0x0003, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (EnblocCall => ENBLOC_CALL, 0x0004, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 24, maximum: 32 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (Stimulus => STIMULUS, 0x0005, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (OffHook => OFF_HOOK, 0x0006, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (OnHook => ON_HOOK, 0x0007, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: 8 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("fieldless form omits line and call identity"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (HookFlash => HOOK_FLASH, 0x0008, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ForwardStatusRequest => FORWARD_STAT_REQ, 0x0009, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SpeedDialStatusRequest => SPEED_DIAL_STAT_REQ, 0x000a, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::SessionSelected { before: MessageId::SpeedDialStatus, from: MessageId::SpeedDialStatusDynamic, selector: SessionResponseSelector::DynamicMessagesOrProtocol { minimum_protocol: 9 } }, verification: ContractVerification::Structural }),
    (LineStatusRequest => LINE_STAT_REQ, 0x000b, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::SessionSelected { before: MessageId::LineStatus, from: MessageId::LineStatusDynamic, selector: SessionResponseSelector::DynamicMessagesOrProtocol { minimum_protocol: 9 } }, verification: ContractVerification::Structural }),
    (ConfigStatusRequest => CONFIG_STAT_REQ, 0x000c, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("nominally empty request; bounded extension bytes are accepted but not modeled"), response: ResponseExpectation::SessionSelected { before: MessageId::ConfigStatus, from: MessageId::ConfigStatusDynamic, selector: SessionResponseSelector::DynamicMessagesOrProtocol { minimum_protocol: 9 } }, verification: ContractVerification::Structural }),
    (TimeDateRequest => TIME_DATE_REQ, 0x000d, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("nominally empty request; bounded extension bytes are accepted but not modeled"), response: ResponseExpectation::Message(MessageId::DefineTimeDate), verification: ContractVerification::Structural }),
    (ButtonTemplateRequest => BUTTON_TEMPLATE_REQ, 0x000e, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("the optional total-button-count request word is accepted but not modeled"), response: ResponseExpectation::Message(MessageId::ButtonTemplate), verification: ContractVerification::Structural }),
    (VersionRequest => VERSION_REQ, 0x000f, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("nominally empty request; bounded extension bytes are accepted but not modeled"), response: ResponseExpectation::Message(MessageId::Version), verification: ContractVerification::Structural }),
    (CapabilitiesResponse => CAPABILITIES_RES, 0x0010, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 4, maximum: 388 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("advertised capability count; inactive fixed-reservoir entries are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (MediaPortList => MEDIA_PORT_LIST, 0x0011, StationToControl, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(68), payload_size_bounds: Some(PayloadSizeBounds { minimum: 68, maximum: 68 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("inactive fixed-array entries are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (ServerRequest => SERVER_REQ, 0x0012, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("nominally empty request; bounded extension bytes are accepted but not modeled"), response: ResponseExpectation::Message(MessageId::ServerResponse), verification: ContractVerification::Structural }),
    (Alarm => ALARM, 0x0020, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (MulticastMediaReceptionAck => MULTICAST_MEDIA_RECEPTION_ACK, 0x0021, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (OpenReceiveChannelAck => OPEN_RECEIVE_CHANNEL_ACK, 0x0022, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (ConnectionStatisticsResponse => CONNECTION_STATISTICS_RES, 0x0023, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 61, maximum: 668 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("inactive fixed quality-reservoir bytes are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (OffHookWithCallingParty => OFF_HOOK_WITH_CALLING_PARTY, 0x0024, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SoftKeySetRequest => SOFT_KEY_SET_REQ, 0x0025, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("nominally empty request; bounded extension bytes are accepted but not modeled"), response: ResponseExpectation::Message(MessageId::SoftKeySetResponse), verification: ContractVerification::Structural }),
    (SoftKeyEvent => SOFT_KEY_EVENT, 0x0026, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (Unregister => UNREGISTER, 0x0027, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("an empty reason-zero body is accepted and normalized to the typed reason"), response: ResponseExpectation::Message(MessageId::UnregisterAck), verification: ContractVerification::Structural }),
    (SoftKeyTemplateRequest => SOFT_KEY_TEMPLATE_REQ, 0x0028, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("nominally empty request; bounded extension bytes are accepted but not modeled"), response: ResponseExpectation::Message(MessageId::SoftKeyTemplateResponse), verification: ContractVerification::Structural }),
    (RegisterTokenRequest => REGISTER_TOKEN_REQ, 0x0029, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("simultaneously populated IPv4 and IPv6 station addresses collapse to one address"), response: ResponseExpectation::OneOf(&[MessageId::RegisterTokenAck, MessageId::RegisterTokenReject]), verification: ContractVerification::Structural }),
    (MediaTransmissionFailure => MEDIA_TRANSMISSION_FAILURE, 0x002a, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("the public status is synthesized because the failure wire layouts carry no status"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (HeadsetStatus => HEADSET_STATUS, 0x002b, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("non-canonical raw states are projected onto a boolean"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (MediaResourceNotification => MEDIA_RESOURCE_NOTIFICATION, 0x002c, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (RegisterAvailableLines => REGISTER_AVAILABLE_LINES, 0x002d, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("an absent or short legacy body is projected onto zero available lines"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DeviceToUserData => DEVICE_TO_USER_DATA, 0x002e, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DeviceToUserDataResponse => DEVICE_TO_USER_DATA_RESPONSE, 0x002f, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UpdateCapabilities => UPDATE_CAPABILITIES, 0x0030, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (OpenMultimediaReceiveChannelAck => OPEN_MULTIMEDIA_RECEIVE_CHANNEL_ACK, 0x0031, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ClearConference => CLEAR_CONFERENCE, 0x0032, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ServiceUrlStatusRequest => SERVICE_URL_STAT_REQ, 0x0033, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::SessionSelected { before: MessageId::ServiceUrlStatus, from: MessageId::ServiceUrlStatusDynamic, selector: SessionResponseSelector::DynamicMessagesOrProtocol { minimum_protocol: 9 } }, verification: ContractVerification::Structural }),
    (FeatureStatusRequest => FEATURE_STAT_REQ, 0x0034, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::SessionSelected { before: MessageId::FeatureStatus, from: MessageId::FeatureStatusDynamic, selector: SessionResponseSelector::DynamicMessages }, verification: ContractVerification::Structural }),
    (CreateConferenceResponse => CREATE_CONFERENCE_RES, 0x0035, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DeleteConferenceResponse => DELETE_CONFERENCE_RES, 0x0036, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ModifyConferenceResponse => MODIFY_CONFERENCE_RES, 0x0037, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (AddParticipantResponse => ADD_PARTICIPANT_RES, 0x0038, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::MinimumLengthPreserved, fixed_payload_bytes: Some(272), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 272 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (AuditConferenceResponse => AUDIT_CONFERENCE_RES, 0x0039, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (AuditParticipantResponse => AUDIT_PARTICIPANT_RES, 0x0040, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::BoundedOpaque, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DeviceToUserDataV1 => DEVICE_TO_USER_DATA_V1, 0x0041, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DeviceToUserDataResponseV1 => DEVICE_TO_USER_DATA_RESPONSE_V1, 0x0042, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UpdateCapabilitiesV2 => UPDATE_CAPABILITIES_V2, 0x0043, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(2000), payload_size_bounds: Some(PayloadSizeBounds { minimum: 2000, maximum: 2000 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UpdateCapabilitiesV3 => UPDATE_CAPABILITIES_V3, 0x0044, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::MinimumLengthPreserved, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 20, maximum: 2380 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (PortResponse => PORT_RESPONSE, 0x0045, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::SemanticProjection("pre-v20 bodies omit media type, which is synthesized on decode"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (QosReservationNotify => QOS_RESERVATION_NOTIFY, 0x0046, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(24), payload_size_bounds: Some(PayloadSizeBounds { minimum: 24, maximum: 24 }), runtime_use: RuntimeUse::ServiceNodeInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (QosErrorNotify => QOS_ERROR_NOTIFY, 0x0047, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(44), payload_size_bounds: Some(PayloadSizeBounds { minimum: 44, maximum: 44 }), runtime_use: RuntimeUse::ServiceNodeInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SubscriptionStatusRequest => SUBSCRIPTION_STAT_REQ, 0x0048, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (MediaPathEvent => ACCESSORY_STATUS, 0x0049, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (MediaPathCapability => MEDIA_PATH_CAPABILITY, 0x004a, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (MwiNotification => MWI_NOTIFICATION, 0x004c, ServiceNodeToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(88), payload_size_bounds: Some(PayloadSizeBounds { minimum: 88, maximum: 88 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (RegisterAck => REGISTER_ACK, 0x0081, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(20), payload_size_bounds: Some(PayloadSizeBounds { minimum: 20, maximum: 20 }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (StartTone => START_TONE, 0x0082, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StopTone => STOP_TONE, 0x0083, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::CanonicalServerOutput("post-v11 tone word"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SetRinger => SET_RINGER, 0x0085, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (SetLamp => SET_LAMP, 0x0086, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (SetHookFlashDetect => SET_HOOK_FLASH_DETECT, 0x0087, ControlToStation, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: 0 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SetSpeakerMode => SET_SPEAKER_MODE, 0x0088, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SetMicrophoneMode => SET_MICROPHONE_MODE, 0x0089, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StartMediaTransmission => START_MEDIA_TRANSMISSION, 0x008a, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::OptionalMessage(MessageId::StartMediaTransmissionAck), verification: ContractVerification::StructuralAndValidated }),
    (StopMediaTransmission => STOP_MEDIA_TRANSMISSION, 0x008b, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StartMediaReception => START_MEDIA_RECEPTION, 0x008c, ControlToStation, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: 0 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StopMediaReception => STOP_MEDIA_RECEPTION, 0x008d, ControlToStation, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(8), payload_size_bounds: Some(PayloadSizeBounds { minimum: 8, maximum: 8 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CallInfo => CALL_INFO, 0x008f, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::CanonicalServerOutput("mailboxes, call instance/security and version-selected party metadata"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ForwardStatus => FORWARD_STAT, 0x0090, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::CanonicalServerOutput("aggregate active flag and inactive forwarding-number slots"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SpeedDialStatus => SPEED_DIAL_STAT, 0x0091, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (LineStatus => LINE_STAT, 0x0092, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(112), payload_size_bounds: Some(PayloadSizeBounds { minimum: 112, maximum: 112 }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("display-options word"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ConfigStatus => CONFIG_STAT, 0x0093, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DefineTimeDate => DEFINE_TIME_DATE, 0x0094, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(36), payload_size_bounds: Some(PayloadSizeBounds { minimum: 36, maximum: 36 }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (StartSessionTransmission => START_SESSION_TRANSMISSION, 0x0095, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StopSessionTransmission => STOP_SESSION_TRANSMISSION, 0x0096, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ButtonTemplate => BUTTON_TEMPLATE, 0x0097, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(96), payload_size_bounds: Some(PayloadSizeBounds { minimum: 96, maximum: 96 }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("unused fixed-array entries outside the declared template count"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (Version => VERSION, 0x0098, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DisplayText => DISPLAY_TEXT, 0x0099, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ClearDisplay => CLEAR_DISPLAY, 0x009a, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::CanonicalServerOutput("display-control word"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CapabilitiesRequest => CAPABILITIES_REQ, 0x009b, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("nominally empty response; accepted extension bytes are not modeled"), response: ResponseExpectation::OneOf(&[MessageId::CapabilitiesResponse, MessageId::UpdateCapabilities, MessageId::UpdateCapabilitiesV2, MessageId::UpdateCapabilitiesV3]), verification: ContractVerification::Structural }),
    (EnunciatorCommand => ENUNCIATOR_COMMAND, 0x009c, ControlToStation, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: 0 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (RegisterReject => REGISTER_REJECT, 0x009d, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ServerResponse => SERVER_RES, 0x009e, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::SemanticProjection("empty server-list slot positions are not retained"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (Reset => RESET, 0x009f, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (KeepAliveAck => KEEP_ALIVE_ACK, 0x0100, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("nominally empty response; accepted extension bytes are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StartMulticastMediaReception => START_MULTICAST_MEDIA_RECEPTION, 0x0101, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StartMulticastMediaTransmission => START_MULTICAST_MEDIA_TRANSMISSION, 0x0102, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StopMulticastMediaReception => STOP_MULTICAST_MEDIA_RECEPTION, 0x0103, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StopMulticastMediaTransmission => STOP_MULTICAST_MEDIA_TRANSMISSION, 0x0104, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (OpenReceiveChannel => OPEN_RECEIVE_CHANNEL, 0x0105, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::OpenReceiveChannelAck), verification: ContractVerification::StructuralAndValidated }),
    (CloseReceiveChannel => CLOSE_RECEIVE_CHANNEL, 0x0106, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (ConnectionStatisticsRequest => CONNECTION_STATISTICS_REQ, 0x0107, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::CanonicalServerOutput("post-v18 directory-number alignment bytes"), response: ResponseExpectation::Message(MessageId::ConnectionStatisticsResponse), verification: ContractVerification::Structural }),
    (SoftKeyTemplateResponse => SOFT_KEY_TEMPLATE_RES, 0x0108, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(652), payload_size_bounds: Some(PayloadSizeBounds { minimum: 652, maximum: 652 }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("unused fixed-array entries outside the declared template count"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (SoftKeySetResponse => SOFT_KEY_SET_RES, 0x0109, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(780), payload_size_bounds: Some(PayloadSizeBounds { minimum: 780, maximum: 780 }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("unused fixed-array entries outside the declared template count"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (SelectSoftKeys => SELECT_SOFT_KEYS, 0x0110, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (CallState => CALL_STATE, 0x0111, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::CanonicalServerOutput("visibility, precedence and domain"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (DisplayPromptStatus => DISPLAY_PROMPT_STATUS, 0x0112, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ClearPromptStatus => CLEAR_PROMPT_STATUS, 0x0113, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (DisplayNotify => DISPLAY_NOTIFY, 0x0114, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ClearNotify => CLEAR_NOTIFY, 0x0115, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::CanonicalServerOutput("nominally empty response; accepted extension bytes are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ActivateCallPlane => ACTIVATE_CALL_PLANE, 0x0116, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (DeactivateCallPlane => DEACTIVATE_CALL_PLANE, 0x0117, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::CanonicalServerOutput("nominally empty response; accepted extension bytes are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UnregisterAck => UNREGISTER_ACK, 0x0118, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("acknowledgement body word"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (BackspaceResponse => BACKSPACE_RESPONSE, 0x0119, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (RegisterTokenAck => REGISTER_TOKEN_ACK, 0x011a, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Empty, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: MAX_FRAME_SIZE - HEADER_SIZE }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("nominally empty response; accepted extension bytes are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (RegisterTokenReject => REGISTER_TOKEN_REJECT, 0x011b, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StartMediaFailureDetection => START_MEDIA_FAILURE_DETECTION, 0x011c, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(28), payload_size_bounds: Some(PayloadSizeBounds { minimum: 28, maximum: 28 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DialedNumber => DIALED_NUMBER, 0x011d, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UserToDeviceData => USER_TO_DEVICE_DATA, 0x011e, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (FeatureStatus => FEATURE_STAT, 0x011f, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DisplayPriorityNotify => DISPLAY_PRIORITY_NOTIFY, 0x0120, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ClearPriorityNotify => CLEAR_PRIORITY_NOTIFY, 0x0121, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StartAnnouncement => START_ANNOUNCEMENT, 0x0122, IntraControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::CanonicalServerOutput("unused announcement and conference-party array entries"), response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StopAnnouncement => STOP_ANNOUNCEMENT, 0x0123, IntraControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (AnnouncementFinish => ANNOUNCEMENT_FINISH, 0x0124, IntraControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (NotifyDtmfTone => NOTIFY_DTMF_TONE, 0x0127, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SendDtmfTone => SEND_DTMF_TONE, 0x0128, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SubscribeDtmfPayloadRequest => SUBSCRIBE_DTMF_PAYLOAD_REQ, 0x0129, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::SubscribeDtmfPayloadResponse), verification: ContractVerification::Structural }),
    (SubscribeDtmfPayloadResponse => SUBSCRIBE_DTMF_PAYLOAD_RES, 0x012a, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SubscribeDtmfPayloadError => SUBSCRIBE_DTMF_PAYLOAD_ERR, 0x012b, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UnsubscribeDtmfPayloadRequest => UNSUBSCRIBE_DTMF_PAYLOAD_REQ, 0x012c, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::UnsubscribeDtmfPayloadResponse), verification: ContractVerification::Structural }),
    (UnsubscribeDtmfPayloadResponse => UNSUBSCRIBE_DTMF_PAYLOAD_RES, 0x012d, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UnsubscribeDtmfPayloadError => UNSUBSCRIBE_DTMF_PAYLOAD_ERR, 0x012e, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ServiceUrlStatus => SERVICE_URL_STAT, 0x012f, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CallSelectStatus => CALL_SELECT_STAT, 0x0130, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (OpenMultimediaChannel => OPEN_MULTIMEDIA_CHANNEL, 0x0131, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::OpenMultimediaReceiveChannelAck), verification: ContractVerification::Structural }),
    (StartMultimediaTransmission => START_MULTIMEDIA_TRANSMISSION, 0x0132, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::StartMultimediaTransmissionAck), verification: ContractVerification::Structural }),
    (StopMultimediaTransmission => STOP_MULTIMEDIA_TRANSMISSION, 0x0133, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (MiscellaneousCommand => MISCELLANEOUS_COMMAND, 0x0134, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(52), payload_size_bounds: Some(PayloadSizeBounds { minimum: 52, maximum: 52 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (FlowControlCommand => FLOW_CONTROL_COMMAND, 0x0135, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CloseMultimediaReceiveChannel => CLOSE_MULTIMEDIA_RECEIVE_CHANNEL, 0x0136, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CreateConferenceRequest => CREATE_CONFERENCE_REQ, 0x0137, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::CreateConferenceResponse), verification: ContractVerification::Structural }),
    (DeleteConferenceRequest => DELETE_CONFERENCE_REQ, 0x0138, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::DeleteConferenceResponse), verification: ContractVerification::Structural }),
    (ModifyConferenceRequest => MODIFY_CONFERENCE_REQ, 0x0139, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::ModifyConferenceResponse), verification: ContractVerification::Structural }),
    (AddParticipantRequest => ADD_PARTICIPANT_REQ, 0x013a, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::AddParticipantResponse), verification: ContractVerification::Structural }),
    (DropParticipantRequest => DROP_PARTICIPANT_REQ, 0x013b, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (AuditConferenceRequest => AUDIT_CONFERENCE_REQ, 0x013c, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(0), payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: 0 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::AuditConferenceResponse), verification: ContractVerification::Structural }),
    (AuditParticipantRequest => AUDIT_PARTICIPANT_REQ, 0x013d, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::AuditParticipantResponse), verification: ContractVerification::Structural }),
    (ChangeParticipantRequest => CHANGE_PARTICIPANT_REQ, 0x013e, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UserToDeviceDataV1 => USER_TO_DEVICE_DATA_V1, 0x013f, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::LengthPrefixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (VideoDisplayCommand => VIDEO_DISPLAY_COMMAND, 0x0140, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(12), payload_size_bounds: Some(PayloadSizeBounds { minimum: 12, maximum: 12 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (FlowControlNotify => FLOW_CONTROL_NOTIFY, 0x0141, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(16), payload_size_bounds: Some(PayloadSizeBounds { minimum: 16, maximum: 16 }), runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ConfigStatusDynamic => CONFIG_STAT_DYNAMIC, 0x0142, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::DynamicWordPadded, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DisplayDynamicNotify => DISPLAY_DYNAMIC_NOTIFY, 0x0143, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::DynamicWordPadded, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DisplayDynamicPriorityNotify => DISPLAY_DYNAMIC_PRIORITY_NOTIFY, 0x0144, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::DynamicWordPadded, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (DisplayDynamicPromptStatus => DISPLAY_DYNAMIC_PROMPT_STATUS, 0x0145, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::DynamicWordPadded, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (FeatureStatusDynamic => FEATURE_STAT_DYNAMIC, 0x0146, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (LineStatusDynamic => LINE_STAT_DYNAMIC, 0x0147, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::DynamicWordPadded, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::CanonicalServerOutput("display-options word"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (ServiceUrlStatusDynamic => SERVICE_URL_STAT_DYNAMIC, 0x0148, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::DynamicWordPadded, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SpeedDialStatusDynamic => SPEED_DIAL_STAT_DYNAMIC, 0x0149, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CallInfoDynamic => CALL_INFO_DYNAMIC, 0x014a, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::DynamicWordPadded, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::CanonicalServerOutput("mailboxes, call instance/security and version-selected party metadata"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (PortRequest => PORT_REQUEST, 0x014b, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::PortResponse), verification: ContractVerification::Structural }),
    (PortClose => PORT_CLOSE, 0x014c, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (QosListen => QOS_LISTEN, 0x014d, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(172), payload_size_bounds: Some(PayloadSizeBounds { minimum: 172, maximum: 172 }), runtime_use: RuntimeUse::ConditionalServiceNodeOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (QosPath => QOS_PATH, 0x014e, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(168), payload_size_bounds: Some(PayloadSizeBounds { minimum: 168, maximum: 168 }), runtime_use: RuntimeUse::ConditionalServiceNodeOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (QosTeardown => QOS_TEARDOWN, 0x014f, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(24), payload_size_bounds: Some(PayloadSizeBounds { minimum: 24, maximum: 24 }), runtime_use: RuntimeUse::ConditionalServiceNodeOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (UpdateDscp => UPDATE_DSCP, 0x0150, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(24), payload_size_bounds: Some(PayloadSizeBounds { minimum: 24, maximum: 24 }), runtime_use: RuntimeUse::ConditionalServiceNodeOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (QosModify => QOS_MODIFY, 0x0151, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(152), payload_size_bounds: Some(PayloadSizeBounds { minimum: 152, maximum: 152 }), runtime_use: RuntimeUse::ConditionalServiceNodeOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SubscriptionStatus => SUBSCRIPTION_STAT, 0x0152, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (Notification => NOTIFICATION, 0x0153, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (StartMediaTransmissionAck => START_MEDIA_TRANSMISSION_ACK, 0x0154, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (StartMultimediaTransmissionAck => START_MULTIMEDIA_TRANSMISSION_ACK, 0x0155, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionSelected, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CallHistoryDisposition => CALL_HISTORY_DISPOSITION, 0x0156, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (LocationInfo => LOCATION_INFO, 0x0157, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(2404), payload_size_bounds: Some(PayloadSizeBounds { minimum: 2404, maximum: 2404 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (MwiResponse => MWI_RESPONSE, 0x0158, ControlToServiceNode, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(32), payload_size_bounds: Some(PayloadSizeBounds { minimum: 32, maximum: 32 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (ExtensionDeviceCapabilities => EXTENSION_DEVICE_CAPABILITIES, 0x0159, StationToControl, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(164), payload_size_bounds: Some(PayloadSizeBounds { minimum: 164, maximum: 164 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (XmlAlarm => XML_ALARM, 0x015a, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::BoundedPreserved, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: 2048 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (CallCountRequest => CALL_COUNT_REQ, 0x015e, StationToControl, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::VersionAndLengthSelected, fixed_payload_bytes: None, payload_size_bounds: Some(PayloadSizeBounds { minimum: 0, maximum: 152 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::Message(MessageId::CallCountResponse), verification: ContractVerification::StructuralAndValidated }),
    (CallCountResponse => CALL_COUNT_RES, 0x015f, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(180), payload_size_bounds: Some(PayloadSizeBounds { minimum: 180, maximum: 180 }), runtime_use: RuntimeUse::RequiredResponse, field_fidelity: FieldFidelity::SemanticProjection("inactive fixed-array line entries are not modeled"), response: ResponseExpectation::None, verification: ContractVerification::StructuralAndValidated }),
    (RecordingStatus => RECORDING_STATUS, 0x0160, ControlToStation, ContractMetadata { scope: ContractScope::Base, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: None, payload_size_bounds: None, runtime_use: RuntimeUse::ConditionalServerOutput, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SpcpRegisterTokenRequest => SPCP_REGISTER_TOKEN_REQ, 0x8000, StationToControl, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(36), payload_size_bounds: Some(PayloadSizeBounds { minimum: 36, maximum: 36 }), runtime_use: RuntimeUse::DeviceInput, field_fidelity: FieldFidelity::SemanticProjection("reserved station identifier word is not modeled"), response: ResponseExpectation::OneOf(&[MessageId::SpcpRegisterTokenAck, MessageId::SpcpRegisterTokenReject]), verification: ContractVerification::StructuralAndValidated }),
    (SpcpRegisterTokenAck => SPCP_REGISTER_TOKEN_ACK, 0x8100, ControlToStation, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(4), payload_size_bounds: Some(PayloadSizeBounds { minimum: 4, maximum: 4 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
    (SpcpRegisterTokenReject => SPCP_REGISTER_TOKEN_REJECT, 0x8101, ControlToStation, ContractMetadata { scope: ContractScope::Supplemental, codec: CodecSupport::Typed, payload_layout: PayloadLayout::Fixed, fixed_payload_bytes: Some(4), payload_size_bounds: Some(PayloadSizeBounds { minimum: 4, maximum: 4 }), runtime_use: RuntimeUse::TypedButNotEmitted, field_fidelity: FieldFidelity::Lossless, response: ResponseExpectation::None, verification: ContractVerification::Structural }),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObservationSanitization {
    Preserve,
    Redact { start: usize, end: usize },
    SuppressPayload,
}

pub(crate) fn observation_sanitization(
    message_id: Option<u32>,
    protocol_header: Option<u32>,
) -> ObservationSanitization {
    let from_version_17 =
        protocol_header.is_some_and(|version| version >= ProtocolVersion::V17.wire());
    match (message_id.map(MessageId::from), from_version_17) {
        (Some(MessageId::OpenReceiveChannel), _) => {
            ObservationSanitization::Redact { start: 48, end: 80 }
        }
        (Some(MessageId::StartMediaTransmission), false) => {
            ObservationSanitization::Redact { start: 64, end: 96 }
        }
        (Some(MessageId::StartMediaTransmission), true) => ObservationSanitization::Redact {
            start: 80,
            end: 112,
        },
        (Some(MessageId::OpenMultimediaChannel), _) => ObservationSanitization::Redact {
            start: 128,
            end: 160,
        },
        (Some(MessageId::StartMultimediaTransmission), false) => ObservationSanitization::Redact {
            start: 132,
            end: 164,
        },
        (Some(MessageId::StartMultimediaTransmission), true) => ObservationSanitization::Redact {
            start: 148,
            end: 180,
        },
        (
            Some(
                MessageId::DeviceToUserData
                | MessageId::DeviceToUserDataResponse
                | MessageId::DeviceToUserDataV1
                | MessageId::DeviceToUserDataResponseV1,
            ),
            _,
        ) => ObservationSanitization::SuppressPayload,
        _ => ObservationSanitization::Preserve,
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unknown(value) => write!(f, "Unknown(0x{value:04x})"),
            known => f.write_str(known.name()),
        }
    }
}

/// Iterates the complete typed implementation inventory in wire-ID order.
///
/// Opaque-only contracts are intentionally excluded. To inspect every known
/// identifier, iterate [`MessageId::ALL_KNOWN`] and call
/// [`MessageId::contract`] instead.
pub fn implemented_message_contracts() -> impl Iterator<Item = MessageContract> {
    MessageId::ALL_CONTRACTS
        .iter()
        .copied()
        .filter(|contract| contract.codec == CodecSupport::Typed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};
    use std::collections::HashSet;

    #[test]
    fn known_catalog_values_are_unique_and_round_trip() {
        let mut values = HashSet::new();
        for id in MessageId::ALL_KNOWN {
            assert!(values.insert(id.wire_value()), "duplicate {id}");
            assert_eq!(MessageId::from(id.wire_value()), *id);
            assert!(id.route().is_some());
            assert!(id.is_known());
        }
        assert!(MessageId::ALL_KNOWN.len() > 140);
    }

    #[test]
    fn supplemental_contract_scope_is_explicit_and_closed() {
        let supplemental = MessageId::ALL_KNOWN
            .iter()
            .copied()
            .filter(|id| id.contract().unwrap().scope == ContractScope::Supplemental)
            .collect::<Vec<_>>();

        assert_eq!(
            supplemental,
            [
                MessageId::IpPort,
                MessageId::MediaPortList,
                MessageId::SetHookFlashDetect,
                MessageId::StartMediaReception,
                MessageId::StopMediaReception,
                MessageId::EnunciatorCommand,
                MessageId::ExtensionDeviceCapabilities,
                MessageId::SpcpRegisterTokenRequest,
                MessageId::SpcpRegisterTokenAck,
                MessageId::SpcpRegisterTokenReject,
            ]
        );
    }

    #[test]
    fn message_contract_catalog_matches_the_golden_snapshot() {
        let snapshot = MessageId::ALL_KNOWN
            .iter()
            .filter_map(|id| id.contract())
            .map(|contract| format!("{contract:?}\n"))
            .collect::<String>();
        let digest = Sha256::digest(snapshot.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            digest,
            "7725c445a29bac40bd0827bdf6bfa531ecca4377aac4f297a5e950d31149e2b4"
        );
    }

    #[test]
    fn unknown_identifiers_remain_lossless() {
        let id = MessageId::from(0xdead_beef);
        assert_eq!(id, MessageId::Unknown(0xdead_beef));
        assert_eq!(id.wire_value(), 0xdead_beef);
        assert_eq!(id.direction(), None);
    }

    #[test]
    fn dtmf_subscription_responses_have_the_device_to_server_direction() {
        assert_eq!(
            MessageId::SubscribeDtmfPayloadRequest.direction(),
            Some(MessageDirection::ServerToDevice)
        );
        assert_eq!(
            MessageId::SubscribeDtmfPayloadResponse.direction(),
            Some(MessageDirection::DeviceToServer)
        );
        assert_eq!(
            MessageId::UnsubscribeDtmfPayloadRequest.direction(),
            Some(MessageDirection::ServerToDevice)
        );
        assert_eq!(
            MessageId::UnsubscribeDtmfPayloadResponse.direction(),
            Some(MessageDirection::DeviceToServer)
        );
    }

    #[test]
    fn every_known_id_has_an_explicit_support_and_runtime_contract() {
        for id in MessageId::ALL_KNOWN {
            let contract = id.contract().expect("known ID has a contract");
            assert_eq!(contract.id, *id);
            assert_eq!(contract.route, id.route().unwrap());
            match contract.codec {
                CodecSupport::Typed => {
                    assert_eq!(contract.emission, EmissionSupport::Typed);
                    assert_ne!(contract.runtime_use, RuntimeUse::CatalogOnly);
                }
                CodecSupport::OpaqueOnly => {
                    assert_eq!(contract.emission, EmissionSupport::PreserveOnly);
                    assert_eq!(contract.runtime_use, RuntimeUse::CatalogOnly);
                    assert_eq!(contract.payload_layout, PayloadLayout::Opaque);
                }
            }
            match contract.field_fidelity {
                FieldFidelity::CanonicalServerOutput(detail) => {
                    assert!(matches!(
                        contract.route,
                        MessageRoute::ControlToStation
                            | MessageRoute::ControlToServiceNode
                            | MessageRoute::IntraControl
                    ));
                    assert!(!detail.is_empty());
                }
                FieldFidelity::SemanticProjection(detail) => {
                    assert_eq!(contract.codec, CodecSupport::Typed);
                    assert!(!detail.is_empty());
                }
                FieldFidelity::OpaquePreserved => {
                    assert_eq!(contract.codec, CodecSupport::OpaqueOnly);
                }
                FieldFidelity::Lossless => {}
            }
        }
        assert!(implemented_message_contracts().count() > 100);

        for id in [
            MessageId::OpenReceiveChannel,
            MessageId::StartMediaTransmission,
            MessageId::StartMediaTransmissionAck,
            MessageId::KeypadButton,
            MessageId::EnblocCall,
            MessageId::Alarm,
            MessageId::DefineTimeDate,
        ] {
            assert_eq!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::Lossless
            );
        }
    }

    #[test]
    fn semantic_field_fidelity_overclaims_are_explicitly_excluded() {
        for (id, omitted) in [
            (MessageId::LineStatus, "display-options"),
            (MessageId::CallInfo, "mailboxes"),
            (MessageId::CallState, "visibility"),
        ] {
            let FieldFidelity::CanonicalServerOutput(detail) =
                id.contract().unwrap().field_fidelity
            else {
                panic!("{id} must not claim lossless field fidelity");
            };
            assert!(detail.contains(omitted), "{id}: {detail}");
        }

        for id in [MessageId::ConfigStatus, MessageId::ConfigStatusDynamic] {
            assert_eq!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::Lossless
            );
        }

        for id in [
            MessageId::CapabilitiesResponse,
            MessageId::MediaTransmissionFailure,
            MessageId::PortResponse,
            MessageId::Register,
        ] {
            assert!(matches!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::SemanticProjection(_)
            ));
        }
    }

    #[test]
    fn variable_layout_messages_never_claim_one_fixed_payload_size() {
        for contract in MessageId::ALL_KNOWN.iter().filter_map(|id| id.contract()) {
            if matches!(
                contract.payload_layout,
                PayloadLayout::VersionSelected
                    | PayloadLayout::VersionAndLengthSelected
                    | PayloadLayout::BoundedPreserved
            ) {
                assert_eq!(
                    contract.fixed_payload_bytes, None,
                    "{} has a variable payload layout",
                    contract.id
                );
            }
        }
    }

    #[test]
    fn bounded_and_counted_payload_contracts_report_their_wire_limits() {
        for id in [
            MessageId::KeepAlive,
            MessageId::ConfigStatusRequest,
            MessageId::ButtonTemplateRequest,
            MessageId::KeepAliveAck,
        ] {
            assert_eq!(
                id.contract().unwrap().payload_size_bounds,
                Some(PayloadSizeBounds {
                    minimum: 0,
                    maximum: MAX_FRAME_SIZE - HEADER_SIZE,
                }),
                "{id}"
            );
        }

        let capabilities = MessageId::CapabilitiesResponse.contract().unwrap();
        assert_eq!(
            capabilities.payload_layout,
            PayloadLayout::VersionAndLengthSelected
        );
        assert_eq!(capabilities.fixed_payload_bytes, None);
        assert_eq!(
            capabilities.payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 4,
                maximum: 388,
            })
        );

        for (id, minimum, maximum) in [
            (MessageId::EnblocCall, 24, 32),
            (MessageId::OnHook, 0, 8),
            (MessageId::ConnectionStatisticsResponse, 61, 668),
        ] {
            assert_eq!(
                id.contract().unwrap().payload_size_bounds,
                Some(PayloadSizeBounds { minimum, maximum }),
                "{id}"
            );
        }

        let on_hook = MessageId::OnHook.contract().unwrap();
        assert_eq!(
            on_hook.payload_layout,
            PayloadLayout::VersionAndLengthSelected
        );
        assert_eq!(
            on_hook.field_fidelity,
            FieldFidelity::SemanticProjection("fieldless form omits line and call identity")
        );

        let call_count = MessageId::CallCountRequest.contract().unwrap();
        assert_eq!(
            call_count.payload_layout,
            PayloadLayout::VersionAndLengthSelected
        );
        assert_eq!(
            call_count.payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 0,
                maximum: 152,
            })
        );

        let call_count_response = MessageId::CallCountResponse.contract().unwrap();
        assert_eq!(call_count_response.payload_layout, PayloadLayout::Fixed);
        assert_eq!(call_count_response.fixed_payload_bytes, Some(180));
        assert_eq!(
            call_count_response.payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 180,
                maximum: 180,
            })
        );

        let version_two = MessageId::UpdateCapabilitiesV2.contract().unwrap();
        assert_eq!(version_two.payload_layout, PayloadLayout::Fixed);
        assert_eq!(version_two.fixed_payload_bytes, Some(2_000));

        let version_three = MessageId::UpdateCapabilitiesV3.contract().unwrap();
        assert_eq!(
            version_three.payload_layout,
            PayloadLayout::MinimumLengthPreserved
        );
        assert_eq!(
            version_three.payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 20,
                maximum: 2_380,
            })
        );
    }

    #[test]
    fn service_message_payload_bounds_are_explicit() {
        assert_eq!(
            MessageId::XmlAlarm.contract().unwrap().payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 0,
                maximum: 2_048,
            })
        );
        assert_eq!(
            MessageId::AddParticipantResponse
                .contract()
                .unwrap()
                .payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 12,
                maximum: 272,
            })
        );

        for (id, size) in [
            (MessageId::AuditConferenceRequest, 0),
            (MessageId::SubscribeDtmfPayloadRequest, 16),
            (MessageId::SubscribeDtmfPayloadResponse, 12),
            (MessageId::SubscribeDtmfPayloadError, 12),
            (MessageId::UnsubscribeDtmfPayloadRequest, 16),
            (MessageId::UnsubscribeDtmfPayloadResponse, 12),
            (MessageId::UnsubscribeDtmfPayloadError, 12),
        ] {
            assert_eq!(
                id.contract().unwrap().payload_size_bounds,
                Some(PayloadSizeBounds {
                    minimum: size,
                    maximum: size,
                }),
                "{id}"
            );
        }
    }

    #[test]
    fn media_contracts_record_fixed_and_version_selected_sizes() {
        for id in [
            MessageId::OpenMultimediaReceiveChannelAck,
            MessageId::StartMultimediaTransmissionAck,
            MessageId::StartSessionTransmission,
            MessageId::StopSessionTransmission,
            MessageId::OpenMultimediaChannel,
            MessageId::StartMultimediaTransmission,
            MessageId::PortRequest,
            MessageId::PortClose,
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.payload_layout, PayloadLayout::VersionSelected);
            assert_eq!(contract.fixed_payload_bytes, None);
        }

        for (id, size) in [
            (MessageId::MulticastMediaReceptionAck, 12),
            (MessageId::CloseReceiveChannel, 16),
            (MessageId::StopMediaTransmission, 16),
            (MessageId::MiscellaneousCommand, 52),
            (MessageId::QosReservationNotify, 24),
            (MessageId::QosErrorNotify, 44),
            (MessageId::QosListen, 172),
            (MessageId::QosPath, 168),
            (MessageId::QosTeardown, 24),
            (MessageId::UpdateDscp, 24),
            (MessageId::QosModify, 152),
        ] {
            assert_eq!(id.contract().unwrap().fixed_payload_bytes, Some(size));
        }

        assert_eq!(
            MessageId::StartMediaTransmissionAck
                .contract()
                .unwrap()
                .payload_layout,
            PayloadLayout::VersionAndLengthSelected
        );
        assert_eq!(
            MessageId::LocationInfo
                .contract()
                .unwrap()
                .payload_size_bounds,
            Some(PayloadSizeBounds {
                minimum: 2_404,
                maximum: 2_404,
            })
        );
        for id in [
            MessageId::CloseReceiveChannel,
            MessageId::StopMediaTransmission,
        ] {
            assert_eq!(
                id.contract().unwrap().field_fidelity,
                FieldFidelity::Lossless
            );
        }
    }

    #[test]
    fn session_transmission_contracts_use_the_service_node_codec() {
        for id in [
            MessageId::StartSessionTransmission,
            MessageId::StopSessionTransmission,
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.route, MessageRoute::ControlToServiceNode);
            assert_eq!(contract.codec, CodecSupport::Typed);
            assert_eq!(contract.emission, EmissionSupport::Typed);
            assert_eq!(contract.runtime_use, RuntimeUse::TypedButNotEmitted);
            assert_eq!(contract.field_fidelity, FieldFidelity::Lossless);
            assert_eq!(contract.payload_layout, PayloadLayout::VersionSelected);
        }
    }

    #[test]
    fn supplemental_token_messages_are_typed() {
        for (id, size) in [
            (MessageId::SpcpRegisterTokenRequest, 36),
            (MessageId::SpcpRegisterTokenAck, 4),
            (MessageId::SpcpRegisterTokenReject, 4),
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.codec, CodecSupport::Typed);
            assert_eq!(contract.emission, EmissionSupport::Typed);
            assert_eq!(contract.payload_layout, PayloadLayout::Fixed);
            assert_eq!(contract.fixed_payload_bytes, Some(size));
        }

        assert_eq!(
            MessageId::SpcpRegisterTokenRequest
                .contract()
                .unwrap()
                .response,
            ResponseExpectation::OneOf(&[
                MessageId::SpcpRegisterTokenAck,
                MessageId::SpcpRegisterTokenReject,
            ])
        );
    }

    #[test]
    fn runtime_emission_is_distinct_from_typed_encodability() {
        let dtmf = MessageId::SubscribeDtmfPayloadRequest.contract().unwrap();
        assert_eq!(dtmf.codec, CodecSupport::Typed);
        assert_eq!(dtmf.emission, EmissionSupport::Typed);
        assert_eq!(dtmf.runtime_use, RuntimeUse::TypedButNotEmitted);

        let open = MessageId::OpenReceiveChannel.contract().unwrap();
        assert_eq!(open.runtime_use, RuntimeUse::ConditionalServerOutput);
        assert_eq!(
            open.response,
            ResponseExpectation::Message(MessageId::OpenReceiveChannelAck)
        );

        assert_eq!(
            MessageId::StartMediaTransmission
                .contract()
                .unwrap()
                .response,
            ResponseExpectation::OptionalMessage(MessageId::StartMediaTransmissionAck)
        );

        for id in [
            MessageId::MiscellaneousCommand,
            MessageId::FlowControlCommand,
            MessageId::FlowControlNotify,
        ] {
            assert_eq!(
                id.contract().unwrap().runtime_use,
                RuntimeUse::ConditionalServerOutput
            );
        }

        for (id, response, runtime_use) in [
            (
                MessageId::OpenMultimediaChannel,
                MessageId::OpenMultimediaReceiveChannelAck,
                RuntimeUse::ConditionalServerOutput,
            ),
            (
                MessageId::StartMultimediaTransmission,
                MessageId::StartMultimediaTransmissionAck,
                RuntimeUse::ConditionalServerOutput,
            ),
        ] {
            let contract = id.contract().unwrap();
            assert_eq!(contract.route, MessageRoute::ControlToStation);
            assert_eq!(contract.codec, CodecSupport::Typed);
            assert_eq!(contract.emission, EmissionSupport::Typed);
            assert_eq!(contract.runtime_use, runtime_use);
            assert_eq!(contract.field_fidelity, FieldFidelity::Lossless);
            assert_eq!(contract.payload_layout, PayloadLayout::VersionSelected);
            assert_eq!(contract.response, ResponseExpectation::Message(response));
        }
    }

    #[test]
    fn dynamic_response_contracts_include_every_session_selector() {
        for (request, before, from) in [
            (
                MessageId::ConfigStatusRequest,
                MessageId::ConfigStatus,
                MessageId::ConfigStatusDynamic,
            ),
            (
                MessageId::LineStatusRequest,
                MessageId::LineStatus,
                MessageId::LineStatusDynamic,
            ),
            (
                MessageId::ServiceUrlStatusRequest,
                MessageId::ServiceUrlStatus,
                MessageId::ServiceUrlStatusDynamic,
            ),
            (
                MessageId::SpeedDialStatusRequest,
                MessageId::SpeedDialStatus,
                MessageId::SpeedDialStatusDynamic,
            ),
        ] {
            assert_eq!(
                request.contract().unwrap().response,
                ResponseExpectation::SessionSelected {
                    before,
                    from,
                    selector: SessionResponseSelector::DynamicMessagesOrProtocol {
                        minimum_protocol: 9,
                    },
                }
            );
        }

        assert_eq!(
            MessageId::FeatureStatusRequest.contract().unwrap().response,
            ResponseExpectation::SessionSelected {
                before: MessageId::FeatureStatus,
                from: MessageId::FeatureStatusDynamic,
                selector: SessionResponseSelector::DynamicMessages,
            }
        );
    }
}
