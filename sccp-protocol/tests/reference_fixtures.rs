use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use sccp_protocol::message::{
    MediaTransmissionAckWire, OpenReceiveChannelWire, StartMediaTransmissionWire,
};
use sccp_protocol::{
    CapabilityUpdateVariant, ClientMessage, CodecError, Frame, FrameDecoder, ProtocolVersion,
    ServerMessage,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

const FIXTURE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/golden");

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format_version: u32,
    capture: Vec<Capture>,
    fixture: Vec<Fixture>,
    trace: Vec<Trace>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    id: String,
    source_kind: SourceKind,
    source_file: Option<String>,
    source_sha256: Option<String>,
    captured_on: Option<String>,
    phone_model: String,
    firmware: Option<String>,
    asterisk_version: Option<String>,
    module_sha256: Option<String>,
    annotation: String,
    outcome: Observation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum SourceKind {
    ExternalPcap,
    LegacyExtract,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixture {
    name: String,
    file: String,
    capture: String,
    direction: Direction,
    message_id: u32,
    header_protocol: u32,
    decode_protocol: u32,
    contains: Contains,
    sha256: String,
    handset_observation: HandsetObservation,
    extraction: Option<Extraction>,
    #[serde(default)]
    sanitization: Vec<Sanitization>,
    #[serde(default)]
    normalization: Vec<Normalization>,
    canonical_evidence: Option<CanonicalEvidence>,
    expect: Expectation,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Extraction {
    tool: ExtractionTool,
    version: u32,
    tcp_stream: u32,
    direction_ordinal: usize,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum ExtractionTool {
    SccpFixturesTshark,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sanitization {
    field: String,
    replacement: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Normalization {
    field: String,
    captured: String,
    canonical: String,
    rationale: String,
    evidence: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CanonicalEvidence {
    ImplementationOutputOnly,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Trace {
    name: String,
    capture: String,
    tcp_stream: u32,
    outcome: Observation,
    step: Vec<TraceStep>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TraceStep {
    sequence: usize,
    fixture: String,
    frame_number: u64,
    timestamp_epoch: String,
    inventory_sequence: usize,
    wire_validity: WireValidity,
    handset_observation: HandsetObservation,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum WireValidity {
    Exact,
    Normalized,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum HandsetObservation {
    NotObserved,
    Emitted,
    Acknowledged,
    NoResponse,
    Displayed,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Direction {
    DeviceToServer,
    ServerToDevice,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Contains {
    Frame,
    Stream,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum Observation {
    Accepted,
    NoAck,
    UiFailed,
    Timeout,
    NotObserved,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case", deny_unknown_fields)]
enum Expectation {
    Decoded {
        canonical: CanonicalMode,
        canonical_file: Option<String>,
        semantic: SemanticExpectation,
    },
    MessageError {
        error: ErrorExpectation,
    },
    FrameError {
        error: ErrorExpectation,
    },
    IncompleteStream {
        buffered_bytes: usize,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CanonicalMode {
    Exact,
    Normalized,
    OpaqueExact,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum SemanticExpectation {
    StartMediaTransmission {
        call_reference: u32,
        passthrough_party_id: u32,
        address: IpAddr,
        rtp_port: u16,
        rtcp_port: u16,
        packet_ms: u32,
        max_frames_per_packet: u32,
        codec: u32,
        telephone_event_payload: u8,
        silence_suppression: u32,
        wire: Option<StartMediaWireExpectation>,
    },
    OpenReceiveChannel {
        call_reference: u32,
        passthrough_party_id: u32,
        packet_ms: u32,
        codec: u32,
        echo_cancellation: u32,
        telephone_event_payload: u8,
        source_address: IpAddr,
        source_port: u16,
        wire: Option<OpenReceiveWireExpectation>,
    },
    StartMediaTransmissionAck {
        conference_id: u32,
        call_reference: u32,
        passthrough_party_id: u32,
        address: IpAddr,
        port: u16,
        status: u32,
        wire: Option<MediaAckWireExpectation>,
    },
    OpenReceiveChannelAck {
        call_reference: u32,
        passthrough_party_id: u32,
        address: IpAddr,
        port: u16,
        status: u32,
    },
    LineStatus {
        instance: u32,
        directory_number: String,
        fully_qualified_display_name: String,
        display_label: String,
    },
    CapabilitiesUpdate {
        variant: CapabilityVariant,
        payload_bytes: usize,
        rtp_payload_format: u32,
        audio_codecs: Vec<u32>,
        video_count: usize,
        data_count: usize,
    },
    KnownOpaque {
        message_id: u32,
        payload_bytes: usize,
    },
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct OpenReceiveWireExpectation {
    conference_id: u32,
    g723_bitrate: u32,
    stream_passthrough_id: u32,
    associated_stream_id: u32,
    dtmf_type: u32,
    mixing_mode: u32,
    direction: u32,
    requested_address_type: u32,
    audio_level_adjustment: u32,
    latent_nonzero_bytes: usize,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct StartMediaWireExpectation {
    conference_id: u32,
    precedence: u32,
    g723_bitrate: u32,
    stream_passthrough_id: u32,
    associated_stream_id: u32,
    dtmf_type: u32,
    mixing_mode: u32,
    direction: u32,
    latent_nonzero_bytes: usize,
}

#[derive(Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
struct MediaAckWireExpectation {
    extension_hex: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CapabilityVariant {
    Version1,
    Version1ExpandedVideo,
    Version2,
    Version3,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum ErrorExpectation {
    InvalidLength {
        value: u32,
    },
    FrameTooLarge {
        size: usize,
    },
    Truncated {
        needed: usize,
        actual: usize,
    },
    TrailingBytes {
        count: usize,
    },
    InvalidValue {
        field: String,
        value: u64,
    },
    CountTooLarge {
        field: String,
        count: usize,
        maximum: usize,
    },
    InvalidText,
    UnsupportedProtocol {
        value: u32,
    },
}

#[derive(Debug)]
enum DecodedMessage {
    Client(ClientMessage),
    Server(ServerMessage),
}

#[test]
fn golden_manifest_is_complete_strict_and_semantic() {
    let manifest = load_manifest();
    assert_eq!(manifest.format_version, 2, "unsupported fixture manifest");
    let captures = validate_captures(&manifest.capture);
    let mut names = BTreeSet::new();
    let mut files = BTreeSet::new();
    for fixture in &manifest.fixture {
        assert!(
            names.insert(&fixture.name),
            "duplicate fixture {}",
            fixture.name
        );
        let capture = captures.get(fixture.capture.as_str()).unwrap_or_else(|| {
            panic!(
                "{} references missing capture {}",
                fixture.name, fixture.capture
            )
        });
        match capture.source_kind {
            SourceKind::ExternalPcap => {
                assert!(fixture.extraction.is_some(), "{} extraction", fixture.name);
                validate_live_privacy(fixture);
            }
            SourceKind::LegacyExtract => {
                assert!(
                    fixture.extraction.is_none(),
                    "{} legacy extraction",
                    fixture.name
                )
            }
        }
        assert!(
            files.insert(fixture.file.clone()),
            "duplicate fixture file {}",
            fixture.file
        );
        for sanitization in &fixture.sanitization {
            assert!(!sanitization.field.trim().is_empty());
            assert!(!sanitization.replacement.trim().is_empty());
        }
        validate_fixture(fixture, &mut files);
    }
    validate_traces(&manifest.trace, &captures, &manifest.fixture);
    assert_eq!(
        files,
        fixture_hex_files(),
        "manifest and fixture files diverged"
    );
}

fn validate_live_privacy(fixture: &Fixture) {
    let Expectation::Decoded { semantic, .. } = &fixture.expect else {
        panic!("live fixture {} must decode semantically", fixture.name)
    };
    let sanitized = !fixture.sanitization.is_empty();
    match semantic {
        SemanticExpectation::CapabilitiesUpdate { .. } => assert!(!sanitized),
        SemanticExpectation::LineStatus {
            directory_number,
            fully_qualified_display_name,
            display_label,
            ..
        } => {
            assert!(sanitized);
            assert_eq!(directory_number, "1001");
            assert_eq!(fully_qualified_display_name, "1001");
            assert_eq!(display_label, "1001");
        }
        SemanticExpectation::OpenReceiveChannel {
            source_address,
            source_port,
            ..
        } if source_address.is_unspecified() && *source_port == 0 => assert!(!sanitized),
        SemanticExpectation::OpenReceiveChannel { source_address, .. }
        | SemanticExpectation::StartMediaTransmission {
            address: source_address,
            ..
        }
        | SemanticExpectation::OpenReceiveChannelAck {
            address: source_address,
            ..
        }
        | SemanticExpectation::StartMediaTransmissionAck {
            address: source_address,
            ..
        } => {
            assert!(sanitized);
            assert_eq!(*source_address, "192.0.2.1".parse::<IpAddr>().unwrap());
        }
        SemanticExpectation::KnownOpaque { .. } => {
            panic!("live opaque fixtures are not privacy-reviewable")
        }
    }
}

#[test]
fn open_receive_pairs_preserve_observed_bytes_without_inferring_a_single_cause() {
    const SOURCE_ADDRESS: std::ops::Range<usize> = 112..132;
    const SOURCE_PORT: std::ops::Range<usize> = 132..136;
    let accepted_wildcard = fixture_bytes("open_receive_7961_v22_accepted_wildcard.hex");
    let no_ack_wildcard = fixture_bytes("open_receive_7961_v22_no_ack_wildcard.hex");
    let concrete = fixture_bytes("open_receive_7961_v22_no_ack_concrete_sanitized.hex");

    // Identical OpenReceive bytes had different handset outcomes in different
    // lifecycle contexts, so the frame alone cannot explain acknowledgement.
    assert_eq!(accepted_wildcard, no_ack_wildcard);

    // This second pair establishes only the byte delta between these frames;
    // it does not claim that the concrete filter was the sole cause of no ACK.
    assert_eq!(accepted_wildcard.len(), concrete.len());
    assert_eq!(
        &accepted_wildcard[..SOURCE_ADDRESS.start],
        &concrete[..SOURCE_ADDRESS.start]
    );
    assert_ne!(
        &accepted_wildcard[SOURCE_ADDRESS.clone()],
        &concrete[SOURCE_ADDRESS.clone()]
    );
    assert_ne!(
        &accepted_wildcard[SOURCE_PORT.clone()],
        &concrete[SOURCE_PORT.clone()]
    );
    assert_eq!(
        &accepted_wildcard[SOURCE_PORT.end..],
        &concrete[SOURCE_PORT.end..]
    );
    assert!(
        accepted_wildcard[SOURCE_ADDRESS]
            .iter()
            .all(|byte| *byte == 0)
    );
    assert!(accepted_wildcard[SOURCE_PORT].iter().all(|byte| *byte == 0));

    let manifest = load_manifest();
    let observation = |name: &str| {
        manifest
            .fixture
            .iter()
            .find(|fixture| fixture.name == name)
            .unwrap()
            .handset_observation
    };
    assert_eq!(
        observation("open_receive_7961_v22_accepted_wildcard"),
        HandsetObservation::Acknowledged
    );
    assert_eq!(
        observation("open_receive_7961_v22_no_ack_wildcard"),
        HandsetObservation::NoResponse
    );
}

fn load_manifest() -> Manifest {
    let source = fs::read_to_string(Path::new(FIXTURE_ROOT).join("manifest.toml")).unwrap();
    toml::from_str(&source).expect("fixture manifest must match the strict typed schema")
}

fn validate_captures(captures: &[Capture]) -> BTreeMap<&str, &Capture> {
    let mut indexed = BTreeMap::new();
    for capture in captures {
        assert!(
            indexed.insert(capture.id.as_str(), capture).is_none(),
            "duplicate capture {}",
            capture.id
        );
        assert!(!capture.phone_model.trim().is_empty());
        assert!(!capture.annotation.trim().is_empty());
        let _ = (
            capture.captured_on.as_deref(),
            capture.firmware.as_deref(),
            capture.asterisk_version.as_deref(),
            capture.outcome,
        );
        if let Some(module_sha256) = &capture.module_sha256 {
            assert_hash(module_sha256);
        }
        match capture.source_kind {
            SourceKind::ExternalPcap => {
                assert!(
                    capture
                        .source_file
                        .as_deref()
                        .is_some_and(|value| value.ends_with(".pcap"))
                );
                assert_hash(
                    capture
                        .source_sha256
                        .as_deref()
                        .expect("external capture hash"),
                );
                let source = Path::new(env!("CARGO_MANIFEST_DIR"))
                    .parent()
                    .expect("workspace root")
                    .join(capture.source_file.as_deref().unwrap());
                if source.is_file() {
                    assert_eq!(
                        sha256(&fs::read(&source).unwrap()),
                        capture.source_sha256.as_deref().unwrap(),
                        "{} source capture hash",
                        capture.id
                    );
                }
            }
            SourceKind::LegacyExtract => {
                assert!(capture.source_file.is_none());
                assert!(capture.source_sha256.is_none());
            }
        }
    }
    indexed
}

fn validate_fixture(fixture: &Fixture, files: &mut BTreeSet<String>) {
    assert_hash(&fixture.sha256);
    let bytes = fixture_bytes(&fixture.file);
    assert_eq!(
        sha256(&bytes),
        fixture.sha256,
        "{} fixture hash",
        fixture.name
    );
    let protocol = ProtocolVersion::negotiate(fixture.decode_protocol).expect("manifest protocol");
    if let Some(extraction) = &fixture.extraction {
        assert_eq!(extraction.tool, ExtractionTool::SccpFixturesTshark);
        assert_eq!(extraction.version, 1);
        let _ = (extraction.tcp_stream, extraction.direction_ordinal);
    }
    match &fixture.expect {
        Expectation::Decoded {
            canonical,
            canonical_file,
            semantic,
        } => {
            assert_eq!(
                fixture.contains,
                Contains::Frame,
                "decoded fixture must contain one frame"
            );
            let frame = one_frame(&bytes, &fixture.name);
            assert_frame_metadata(fixture, &frame);
            let message = decode(fixture.direction, frame, protocol)
                .unwrap_or_else(|error| panic!("{} decode failed: {error}", fixture.name));
            assert_semantic(&fixture.name, &message, semantic, bytes.len() - 12);
            let encoded = encode(&message, protocol)
                .unwrap_or_else(|error| panic!("{} encode failed: {error}", fixture.name));
            match canonical {
                CanonicalMode::Exact => {
                    assert!(canonical_file.is_none());
                    assert!(fixture.normalization.is_empty());
                    assert!(fixture.canonical_evidence.is_none());
                    assert_eq!(
                        encoded, bytes,
                        "{} did not round-trip exactly",
                        fixture.name
                    );
                }
                CanonicalMode::OpaqueExact => {
                    assert!(is_opaque(&message), "{} is not opaque", fixture.name);
                    assert!(canonical_file.is_none());
                    assert!(fixture.normalization.is_empty());
                    assert!(fixture.canonical_evidence.is_none());
                    assert_eq!(encoded, bytes, "{} opaque bytes changed", fixture.name);
                }
                CanonicalMode::Normalized => {
                    assert!(!fixture.normalization.is_empty());
                    assert_eq!(
                        fixture.canonical_evidence,
                        Some(CanonicalEvidence::ImplementationOutputOnly)
                    );
                    for change in &fixture.normalization {
                        assert!(!change.field.trim().is_empty());
                        assert!(!change.captured.trim().is_empty());
                        assert!(!change.canonical.trim().is_empty());
                        assert!(!change.rationale.trim().is_empty());
                        assert!(!change.evidence.trim().is_empty());
                    }
                    let canonical_file = canonical_file
                        .as_deref()
                        .expect("normalized fixture canonical file");
                    assert!(
                        files.insert(canonical_file.to_owned()),
                        "duplicate canonical file {canonical_file}"
                    );
                    let canonical = fixture_bytes(canonical_file);
                    assert_eq!(encoded, canonical, "{} canonical bytes", fixture.name);
                    let canonical_frame =
                        one_frame(&canonical, &format!("{} canonical", fixture.name));
                    let canonical_message =
                        decode(fixture.direction, canonical_frame, protocol).unwrap();
                    assert_semantic(
                        &fixture.name,
                        &canonical_message,
                        semantic,
                        canonical.len() - 12,
                    );
                    assert_eq!(
                        encode(&canonical_message, protocol).unwrap(),
                        canonical,
                        "{} canonical encoding is not idempotent",
                        fixture.name
                    );
                }
            }
        }
        Expectation::MessageError { error } => {
            let frame = one_frame(&bytes, &fixture.name);
            assert_frame_metadata(fixture, &frame);
            let actual = decode(fixture.direction, frame, protocol)
                .expect_err("message decode unexpectedly succeeded");
            assert_error(error, &actual);
        }
        Expectation::FrameError { error } => {
            let actual = FrameDecoder::new()
                .push(&bytes)
                .expect_err("frame decode unexpectedly succeeded");
            assert_error(error, &actual);
        }
        Expectation::IncompleteStream { buffered_bytes } => {
            assert_eq!(fixture.contains, Contains::Stream);
            let mut decoder = FrameDecoder::new();
            assert!(decoder.push(&bytes).unwrap().is_empty());
            assert_eq!(decoder.buffered_len(), *buffered_bytes);
        }
    }
    let _ = fixture.handset_observation;
}

fn validate_traces(traces: &[Trace], captures: &BTreeMap<&str, &Capture>, fixtures: &[Fixture]) {
    let fixture_index = fixtures
        .iter()
        .map(|fixture| (fixture.name.as_str(), fixture))
        .collect::<BTreeMap<_, _>>();
    let mut names = BTreeSet::new();
    for trace in traces {
        assert!(
            names.insert(trace.name.as_str()),
            "duplicate trace {}",
            trace.name
        );
        assert!(captures.contains_key(trace.capture.as_str()));
        assert!(!trace.step.is_empty());
        let mut previous_timestamp = None;
        let mut previous_inventory_sequence = None;
        let mut traced_fixtures = BTreeSet::new();
        for (index, step) in trace.step.iter().enumerate() {
            assert_eq!(step.sequence, index + 1, "{} trace order", trace.name);
            assert!(step.frame_number > 0, "{} packet frame number", trace.name);
            let timestamp = parse_epoch_nanos(&step.timestamp_epoch)
                .unwrap_or_else(|| panic!("{} epoch timestamp", trace.name));
            if let Some(previous) = previous_timestamp {
                assert!(
                    timestamp > previous,
                    "{} timestamps are not ordered",
                    trace.name
                );
            }
            previous_timestamp = Some(timestamp);
            assert!(
                step.inventory_sequence > 0,
                "{} inventory sequence",
                trace.name
            );
            if let Some(previous) = previous_inventory_sequence {
                assert!(
                    step.inventory_sequence > previous,
                    "{} inventory sequence is not ordered",
                    trace.name
                );
            }
            previous_inventory_sequence = Some(step.inventory_sequence);
            assert!(
                traced_fixtures.insert(step.fixture.as_str()),
                "{} repeats fixture {}",
                trace.name,
                step.fixture
            );
            let fixture = fixture_index.get(step.fixture.as_str()).unwrap_or_else(|| {
                panic!("{} references missing fixture {}", trace.name, step.fixture)
            });
            assert_eq!(fixture.capture, trace.capture);
            let extraction = fixture
                .extraction
                .as_ref()
                .expect("traced live fixture extraction");
            assert_eq!(extraction.tcp_stream, trace.tcp_stream);
            let expected_wire = match &fixture.expect {
                Expectation::Decoded {
                    canonical: CanonicalMode::Normalized,
                    ..
                } => WireValidity::Normalized,
                Expectation::Decoded { .. } => WireValidity::Exact,
                _ => panic!("trace steps require decoded fixtures"),
            };
            assert_eq!(step.wire_validity, expected_wire);
            assert_eq!(step.handset_observation, fixture.handset_observation);
        }
        let _ = trace.outcome;
    }
}

fn parse_epoch_nanos(value: &str) -> Option<u128> {
    let (seconds, fraction) = value.split_once('.')?;
    if fraction.is_empty()
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    Some(
        seconds.parse::<u128>().ok()? * 1_000_000_000
            + fraction.parse::<u128>().ok()? * 10_u128.pow(9 - fraction.len() as u32),
    )
}

fn decode(
    direction: Direction,
    frame: Frame,
    protocol: ProtocolVersion,
) -> Result<DecodedMessage, CodecError> {
    match direction {
        Direction::DeviceToServer => {
            ClientMessage::decode_with_version(frame, protocol).map(DecodedMessage::Client)
        }
        Direction::ServerToDevice => {
            ServerMessage::decode(frame, protocol).map(DecodedMessage::Server)
        }
    }
}

fn encode(message: &DecodedMessage, protocol: ProtocolVersion) -> Result<Vec<u8>, CodecError> {
    match message {
        DecodedMessage::Client(message) => message.encode(protocol),
        DecodedMessage::Server(message) => message.encode(protocol),
    }
}

fn is_opaque(message: &DecodedMessage) -> bool {
    matches!(
        message,
        DecodedMessage::Client(ClientMessage::KnownOpaque(_) | ClientMessage::Unknown(_))
            | DecodedMessage::Server(ServerMessage::KnownOpaque(_) | ServerMessage::Unknown(_))
    )
}

fn assert_semantic(
    name: &str,
    actual: &DecodedMessage,
    expected: &SemanticExpectation,
    payload_bytes: usize,
) {
    match (actual, expected) {
        (
            DecodedMessage::Server(ServerMessage::StartMediaTransmission {
                call_reference,
                passthrough_party_id,
                endpoint,
                silence_suppression,
                traffic_class,
                encryption,
                wire,
            }),
            SemanticExpectation::StartMediaTransmission {
                call_reference: expected_call,
                passthrough_party_id: expected_party,
                address,
                rtp_port,
                rtcp_port,
                packet_ms,
                max_frames_per_packet,
                codec,
                telephone_event_payload,
                silence_suppression: expected_silence,
                wire: expected_wire,
            },
        ) => {
            assert_eq!(
                (*call_reference, *passthrough_party_id),
                (*expected_call, *expected_party),
                "{name} identities"
            );
            assert_eq!(
                (
                    endpoint.address,
                    endpoint.rtp_port,
                    endpoint.rtcp_port,
                    endpoint.packet_ms,
                    endpoint.max_frames_per_packet,
                    endpoint.codec.wire_value(),
                    endpoint.telephone_event_payload,
                ),
                (
                    *address,
                    *rtp_port,
                    *rtcp_port,
                    *packet_ms,
                    *max_frames_per_packet,
                    *codec,
                    *telephone_event_payload,
                ),
                "{name} media endpoint"
            );
            assert_eq!(
                silence_suppression.wire_value(),
                *expected_silence,
                "{name} silence suppression"
            );
            assert!(encryption.is_none());
            assert_eq!(
                traffic_class.get(),
                expected_wire
                    .as_ref()
                    .map_or(0, |wire| wire.precedence as u8),
                "{name} media traffic class"
            );
            assert_start_media_wire(wire.as_ref(), expected_wire.as_ref());
        }
        (
            DecodedMessage::Server(ServerMessage::OpenReceiveChannel {
                call_reference,
                passthrough_party_id,
                packet_ms,
                codec,
                echo_cancellation,
                telephone_event_payload,
                source_address,
                source_port,
                encryption,
                wire,
            }),
            SemanticExpectation::OpenReceiveChannel {
                call_reference: expected_call,
                passthrough_party_id: expected_party,
                packet_ms: expected_packet,
                codec: expected_codec,
                echo_cancellation: expected_echo,
                telephone_event_payload: expected_event,
                source_address: expected_address,
                source_port: expected_port,
                wire: expected_wire,
            },
        ) => {
            assert_eq!(
                (
                    *call_reference,
                    *passthrough_party_id,
                    *packet_ms,
                    codec.wire_value(),
                    echo_cancellation.wire_value(),
                    *telephone_event_payload,
                    *source_address,
                    *source_port,
                ),
                (
                    *expected_call,
                    *expected_party,
                    *expected_packet,
                    *expected_codec,
                    *expected_echo,
                    *expected_event,
                    *expected_address,
                    *expected_port,
                ),
                "{name} open receive semantics"
            );
            assert!(encryption.is_none());
            assert_open_receive_wire(wire.as_ref(), expected_wire.as_ref());
        }
        (
            DecodedMessage::Client(ClientMessage::StartMediaTransmissionAck(ack)),
            SemanticExpectation::StartMediaTransmissionAck {
                conference_id,
                call_reference,
                passthrough_party_id,
                address,
                port,
                status,
                wire,
            },
        ) => {
            assert_eq!(
                (
                    ack.conference_id,
                    ack.call_reference,
                    ack.passthrough_party_id,
                    ack.address,
                    ack.port,
                    ack.status.wire_value(),
                ),
                (
                    *conference_id,
                    *call_reference,
                    *passthrough_party_id,
                    *address,
                    *port,
                    *status,
                ),
                "{name} ACK semantics"
            );
            assert_media_ack_wire(ack.wire.as_ref(), wire.as_ref());
        }
        (
            DecodedMessage::Client(ClientMessage::OpenReceiveChannelAck {
                status,
                address: actual_address,
                port: actual_port,
                passthrough_party_id: actual_party,
                call_reference: actual_call,
                ..
            }),
            SemanticExpectation::OpenReceiveChannelAck {
                call_reference,
                passthrough_party_id,
                address,
                port,
                status: expected_status,
            },
        ) => assert_eq!(
            (
                *actual_call,
                *actual_party,
                *actual_address,
                *actual_port,
                status.wire_value(),
            ),
            (
                *call_reference,
                *passthrough_party_id,
                *address,
                *port,
                *expected_status,
            ),
            "{name} open receive ACK semantics"
        ),
        (
            DecodedMessage::Server(ServerMessage::LineStatus {
                instance: actual_instance,
                directory_number: actual_number,
                fully_qualified_display_name: actual_display_name,
                display_label: actual_display_label,
            }),
            SemanticExpectation::LineStatus {
                instance,
                directory_number,
                fully_qualified_display_name,
                display_label,
            },
        ) => assert_eq!(
            (
                actual_instance,
                actual_number,
                actual_display_name,
                actual_display_label,
            ),
            (
                instance,
                directory_number,
                fully_qualified_display_name,
                display_label,
            ),
            "{name} line status semantics"
        ),
        (
            DecodedMessage::Client(ClientMessage::CapabilitiesUpdate(update)),
            SemanticExpectation::CapabilitiesUpdate {
                variant,
                payload_bytes: expected_bytes,
                rtp_payload_format,
                audio_codecs,
                video_count,
                data_count,
            },
        ) => {
            assert_eq!(payload_bytes, *expected_bytes, "{name} payload size");
            assert_eq!(capability_variant(update.variant()), *variant);
            assert_eq!(update.rtp_payload_format(), *rtp_payload_format);
            assert_eq!(
                update
                    .audio()
                    .iter()
                    .map(|capability| capability.codec.wire_value())
                    .collect::<Vec<_>>(),
                *audio_codecs
            );
            assert_eq!(update.video().len(), *video_count);
            assert_eq!(update.data().len(), *data_count);
        }
        (
            DecodedMessage::Client(ClientMessage::KnownOpaque(message)),
            SemanticExpectation::KnownOpaque {
                message_id,
                payload_bytes,
            },
        ) => {
            assert_eq!(message.id.wire_value(), *message_id);
            assert_eq!(message.payload.len(), *payload_bytes);
        }
        (
            DecodedMessage::Server(ServerMessage::KnownOpaque(message)),
            SemanticExpectation::KnownOpaque {
                message_id,
                payload_bytes,
            },
        ) => {
            assert_eq!(message.id.wire_value(), *message_id);
            assert_eq!(message.payload.len(), *payload_bytes);
        }
        _ => panic!("{name} decoded as {actual:?}, expected {expected:?}"),
    }
}

fn assert_start_media_wire(
    actual: Option<&StartMediaTransmissionWire>,
    expected: Option<&StartMediaWireExpectation>,
) {
    match (actual, expected) {
        (None, None) => {}
        (Some(actual), Some(expected)) => assert_eq!(
            (
                actual.conference_id,
                actual.g723_bitrate,
                actual.stream_passthrough_id,
                actual.associated_stream_id,
                actual.dtmf_type,
                actual.mixing_mode,
                actual.direction,
                actual
                    .latent_capabilities
                    .iter()
                    .filter(|byte| **byte != 0)
                    .count(),
            ),
            (
                expected.conference_id,
                expected.g723_bitrate,
                expected.stream_passthrough_id,
                expected.associated_stream_id,
                expected.dtmf_type,
                expected.mixing_mode,
                expected.direction,
                expected.latent_nonzero_bytes,
            )
        ),
        _ => panic!("start-media wire detail presence differs"),
    }
}

fn assert_open_receive_wire(
    actual: Option<&OpenReceiveChannelWire>,
    expected: Option<&OpenReceiveWireExpectation>,
) {
    match (actual, expected) {
        (None, None) => {}
        (Some(actual), Some(expected)) => assert_eq!(
            (
                actual.conference_id,
                actual.g723_bitrate,
                actual.stream_passthrough_id,
                actual.associated_stream_id,
                actual.dtmf_type,
                actual.mixing_mode,
                actual.direction,
                actual.requested_address_type,
                actual.audio_level_adjustment,
                actual
                    .latent_capabilities
                    .iter()
                    .filter(|byte| **byte != 0)
                    .count(),
            ),
            (
                expected.conference_id,
                expected.g723_bitrate,
                expected.stream_passthrough_id,
                expected.associated_stream_id,
                expected.dtmf_type,
                expected.mixing_mode,
                expected.direction,
                expected.requested_address_type,
                expected.audio_level_adjustment,
                expected.latent_nonzero_bytes,
            )
        ),
        _ => panic!("open-receive wire detail presence differs"),
    }
}

fn assert_media_ack_wire(
    actual: Option<&MediaTransmissionAckWire>,
    expected: Option<&MediaAckWireExpectation>,
) {
    match (actual, expected) {
        (None, None) => {}
        (Some(actual), Some(expected)) => {
            assert_eq!(
                actual.extension.as_ref().map(|bytes| hex_bytes(bytes)),
                expected.extension_hex
            );
        }
        _ => panic!("media-ACK wire detail presence differs"),
    }
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn capability_variant(value: CapabilityUpdateVariant) -> CapabilityVariant {
    match value {
        CapabilityUpdateVariant::Version1 => CapabilityVariant::Version1,
        CapabilityUpdateVariant::Version1ExpandedVideo => CapabilityVariant::Version1ExpandedVideo,
        CapabilityUpdateVariant::Version2 => CapabilityVariant::Version2,
        CapabilityUpdateVariant::Version3 => CapabilityVariant::Version3,
    }
}

fn assert_error(expected: &ErrorExpectation, actual: &CodecError) {
    match (expected, actual) {
        (ErrorExpectation::InvalidLength { value }, CodecError::InvalidLength(actual)) => {
            assert_eq!(actual, value)
        }
        (ErrorExpectation::FrameTooLarge { size }, CodecError::FrameTooLarge(actual)) => {
            assert_eq!(actual, size)
        }
        (
            ErrorExpectation::Truncated {
                needed,
                actual: expected_actual,
            },
            CodecError::Truncated {
                needed: actual_needed,
                actual,
                ..
            },
        ) => assert_eq!((*actual_needed, *actual), (*needed, *expected_actual)),
        (
            ErrorExpectation::TrailingBytes { count },
            CodecError::TrailingBytes { count: actual, .. },
        ) => assert_eq!(actual, count),
        (
            ErrorExpectation::InvalidValue { field, value },
            CodecError::InvalidValue {
                field: actual_field,
                value: actual_value,
                ..
            },
        ) => assert_eq!((*actual_field, *actual_value), (field.as_str(), *value)),
        (
            ErrorExpectation::CountTooLarge {
                field,
                count,
                maximum,
            },
            CodecError::CountTooLarge {
                field: actual_field,
                count: actual_count,
                maximum: actual_maximum,
                ..
            },
        ) => assert_eq!(
            (*actual_field, *actual_count, *actual_maximum),
            (field.as_str(), *count, *maximum)
        ),
        (ErrorExpectation::InvalidText, CodecError::InvalidText) => {}
        (
            ErrorExpectation::UnsupportedProtocol { value },
            CodecError::UnsupportedProtocol(actual),
        ) => assert_eq!(actual, value),
        _ => panic!("unexpected codec error {actual:?}, expected {expected:?}"),
    }
}

fn assert_frame_metadata(fixture: &Fixture, frame: &Frame) {
    assert_eq!(
        frame.message_id, fixture.message_id,
        "{} message ID",
        fixture.name
    );
    assert_eq!(
        frame.protocol_version, fixture.header_protocol,
        "{} header protocol",
        fixture.name
    );
    let actual = frame
        .message_type()
        .direction()
        .expect("fixture message has known direction");
    let expected = match fixture.direction {
        Direction::DeviceToServer => sccp_protocol::MessageDirection::DeviceToServer,
        Direction::ServerToDevice => sccp_protocol::MessageDirection::ServerToDevice,
    };
    assert_eq!(actual, expected, "{} catalog direction", fixture.name);
}

fn one_frame(bytes: &[u8], name: &str) -> Frame {
    let mut decoder = FrameDecoder::new();
    let mut frames = decoder
        .push(bytes)
        .unwrap_or_else(|error| panic!("{name} framing: {error}"));
    assert_eq!(frames.len(), 1, "{name} must contain one frame");
    assert_eq!(decoder.buffered_len(), 0, "{name} has a partial tail");
    frames.remove(0)
}

fn fixture_bytes(file: &str) -> Vec<u8> {
    let path = Path::new(FIXTURE_ROOT).join(file);
    let source =
        fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
    source
        .lines()
        .flat_map(|line| {
            line.split_once('#')
                .map_or(line, |(data, _)| data)
                .split_whitespace()
        })
        .map(|byte| {
            assert_eq!(
                byte.len(),
                2,
                "{} contains a non-byte token {byte:?}",
                path.display()
            );
            u8::from_str_radix(byte, 16)
                .unwrap_or_else(|_| panic!("{} contains invalid hex {byte:?}", path.display()))
        })
        .collect()
}

fn fixture_hex_files() -> BTreeSet<String> {
    fn visit(root: &Path, directory: &Path, output: &mut BTreeSet<String>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, output);
            } else if path.extension().is_some_and(|extension| extension == "hex") {
                output.insert(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let root = PathBuf::from(FIXTURE_ROOT);
    let mut files = BTreeSet::new();
    visit(&root, &root, &mut files);
    files
}

fn assert_hash(value: &str) {
    assert_eq!(
        value.len(),
        64,
        "SHA-256 must contain 64 lowercase hex digits"
    );
    assert!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    );
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        write!(encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    encoded
}
