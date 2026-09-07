use std::net::Ipv6Addr;

use super::*;
use crate::PhoneSoftKeyPosition;
use crate::message::values::{
    AnnouncementPlayMode, EndOfAnnouncementAck, IpAddressType, RFC2833_TELEPHONE_EVENT_PAYLOAD,
    ReceiveTransmit,
};
use crate::message::wire::Frame;
use crate::message::{
    ClientMessage, MediaTransmissionAck, RegistrationMessage, ServerMessage, wire_id,
};
use crate::types::{
    BlfSpeedDialDefinition, FeatureDefinition, LineAppearance, LineDefinition, ServiceDefinition,
    SpeedDialDefinition,
};

mod blf;
mod call;
mod event_delivery;
mod media;
mod registration;
mod services;
mod ui;

mod support {
    pub(super) use super::*;
}
fn definition() -> DeviceDefinition {
    definition_for("SEP001122334455")
}

fn definition_for(device_id: &str) -> DeviceDefinition {
    DeviceDefinition {
        id: DeviceId::new(device_id).unwrap(),
        description: "Test phone".into(),
        transport: StationTransportRequirement::Either,
        signaling_qos: None,
        buttons: vec![ButtonDefinition::Line(LineAppearance::new(
            1,
            LineDefinition {
                number: "1001".into(),
                display_name: "Desk 1001".into(),
            },
        ))],
        soft_keys: SoftKeyProfile::default(),
        ui: Default::default(),
    }
}

fn multicast_test_state(protocol: ProtocolVersion) -> SessionState {
    let device = definition();
    let registration = DeviceRegistration {
        id: device.id.clone(),
        peer: "127.0.0.1:2000".parse().unwrap(),
        transport: StationTransport::Clear,
        reported_address: Some(Ipv4Addr::LOCALHOST),
        reported_ipv6_address: None,
        device_type: DeviceType::Undefined,
        protocol,
        firmware: "test".into(),
    };
    let mut state = SessionState::new(
        device,
        registration,
        PhoneFeatures::default(),
        SessionGeneration::new(1).unwrap(),
    );
    state.media_capabilities = vec![MediaCapability {
        codec: Codec::Pcmu,
        max_packet_ms: 40,
        codec_parameters: [0; 8],
    }]
    .into();
    state
}

fn multicast_route(address: IpAddr, codec: Codec) -> MulticastMediaRoute {
    MulticastMediaRoute {
        address,
        port: 5004,
        codec,
        packet_millis: 20,
    }
}

const fn test_rtp_payload_number(value: u8) -> crate::RtpPayloadNumber {
    match crate::RtpPayloadNumber::new(value as u32) {
        Ok(payload_number) => payload_number,
        Err(_) => panic!("test RTP payload number is out of range"),
    }
}

fn video_receive_descriptor(
    conference_id: u32,
    source: MediaEndpointAddress,
) -> MultimediaReceiveDescriptor {
    MultimediaReceiveDescriptor {
        conference_id: ConferenceId::new(conference_id),
        payload: MultimediaPayload::from_wire(
            0,
            test_rtp_payload_number(97),
            [0xa5; crate::MULTIMEDIA_CAPABILITY_BYTES],
            Codec::H264,
            MultimediaPayloadDirection::Receive,
            ProtocolVersion::V22,
        ),
        conference_creator: false,
        encryption: None,
        stream_passthrough_id: conference_id + 100,
        associated_stream_id: 0,
        source,
        requested_address_type: IpAddressType::Ipv4AndIpv6,
    }
}

fn video_transmit_descriptor(
    conference_id: u32,
    endpoint: MediaEndpointAddress,
) -> MultimediaTransmitDescriptor {
    MultimediaTransmitDescriptor {
        conference_id: ConferenceId::new(conference_id),
        endpoint,
        payload: MultimediaPayload::from_wire(
            0,
            test_rtp_payload_number(98),
            [0x5a; crate::MULTIMEDIA_CAPABILITY_BYTES],
            Codec::H264,
            MultimediaPayloadDirection::Transmit,
            ProtocolVersion::V22,
        ),
        traffic_class: MediaTrafficClass::from_wire(136),
        encryption: None,
        stream_passthrough_id: conference_id + 200,
        associated_stream_id: 0,
    }
}

fn mixed_definition() -> DeviceDefinition {
    let mut device = definition();
    device.buttons.extend([
        ButtonDefinition::SpeedDial(SpeedDialDefinition {
            instance: 1,
            number: "2001".into(),
            display_name: "Reception".into(),
        }),
        ButtonDefinition::Feature(FeatureDefinition {
            instance: 1,
            label: "DND".into(),
            feature: ButtonType::DoNotDisturb,
        }),
        ButtonDefinition::Service(ServiceDefinition {
            instance: 1,
            label: "Directory".into(),
            url: "http://services.invalid/directory".into(),
        }),
        ButtonDefinition::Unused,
        ButtonDefinition::BlfSpeedDial(BlfSpeedDialDefinition {
            instance: 2,
            number: "2002".into(),
            display_name: "Warehouse".into(),
        }),
    ]);
    device
}

fn profile_with(mode: KeyMode, actions: Vec<SoftKey>) -> SoftKeyProfile {
    let default = SoftKeyProfile::default();
    SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|candidate| {
        if candidate == mode {
            (candidate, actions.clone())
        } else {
            (candidate, default.actions(candidate).to_vec())
        }
    }))
    .unwrap()
}

fn session_call(call_id: u64) -> SessionCall {
    SessionCall {
        call_id: CallId(call_id),
        wire_reference: call_id as u32,
        line_instance: 1,
        media: CallMedia::new(Codec::Pcmu),
        video_receive: VideoReceive::default(),
        video_transmit: VideoTransmit::default(),
        state: CallState::Connected,
        ringer: None,
        history_disposition: CallHistoryDisposition::Placed,
        dialed_number: String::new(),
        statistics_directory_number: String::new(),
        transfer_role: None,
    }
}

fn register_bytes(protocol: ProtocolVersion) -> Vec<u8> {
    register_bytes_for_device_type(protocol, 115)
}

fn register_bytes_with_features(protocol: ProtocolVersion, features: PhoneFeatures) -> Vec<u8> {
    register_bytes_for_device_with_features(protocol, 115, "SEP001122334455", features)
}

fn register_bytes_for_device_type(protocol: ProtocolVersion, device_type: u32) -> Vec<u8> {
    register_bytes_for_device(protocol, device_type, "SEP001122334455")
}

fn register_bytes_for_device(
    protocol: ProtocolVersion,
    device_type: u32,
    device_id: &str,
) -> Vec<u8> {
    register_bytes_for_device_with_features(
        protocol,
        device_type,
        device_id,
        PhoneFeatures::empty(),
    )
}

fn register_bytes_for_device_with_features(
    protocol: ProtocolVersion,
    device_type: u32,
    device_id: &str,
    features: PhoneFeatures,
) -> Vec<u8> {
    let mut payload = vec![0_u8; 124];
    let device_id = device_id.as_bytes();
    assert!(device_id.len() <= 16);
    payload[..device_id.len()].copy_from_slice(device_id);
    payload[24..28].copy_from_slice(&[127, 0, 0, 1]);
    payload[28..32].copy_from_slice(&device_type.to_le_bytes());
    payload[40..44].copy_from_slice(&(protocol.wire() | features.bits()).to_le_bytes());
    payload[92..101].copy_from_slice(b"SCCP42.9-");
    Frame::new(0, wire_id::REGISTER, payload).encode().unwrap()
}

fn capability_update_bytes(
    protocol: ProtocolVersion,
    audio_codec: Codec,
    video_codec: Codec,
    marker: u32,
) -> Vec<u8> {
    const AUDIO_OFFSET: usize = 312;
    const VIDEO_OFFSET: usize = 600;

    fn put(payload: &mut [u8], offset: usize, value: u32) {
        payload[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    let mut payload = vec![0; 2_380];
    put(&mut payload, 0, 1);
    put(&mut payload, 4, 1);
    put(&mut payload, AUDIO_OFFSET, audio_codec.wire_value());
    put(&mut payload, AUDIO_OFFSET + 4, marker);
    payload[AUDIO_OFFSET + 8..AUDIO_OFFSET + 16].copy_from_slice(&marker.to_le_bytes().repeat(2));

    put(&mut payload, VIDEO_OFFSET, video_codec.wire_value());
    put(
        &mut payload,
        VIDEO_OFFSET + 4,
        (ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT).bits(),
    );
    put(&mut payload, VIDEO_OFFSET + 8, 1);
    for (index, value) in [marker, 5, 4_000, 128, 2, 7].into_iter().enumerate() {
        put(&mut payload, VIDEO_OFFSET + 12 + index * 4, value);
    }
    put(
        &mut payload,
        VIDEO_OFFSET + 108,
        u32::from(EncryptionCapability::Capable),
    );
    for (index, value) in [66, 31, 120, 240, 360, marker].into_iter().enumerate() {
        put(&mut payload, VIDEO_OFFSET + 112 + index * 4, value);
    }
    put(
        &mut payload,
        VIDEO_OFFSET + 136,
        u32::from(IpAddressType::Ipv4AndIpv6),
    );
    Frame::new(protocol.wire(), wire_id::UPDATE_CAPABILITIES_V3, payload)
        .encode()
        .unwrap()
}

async fn read_until_message(
    phone: &mut dyn StationIo,
    decoder: &mut FrameDecoder,
    message_id: u32,
) -> Vec<Frame> {
    let mut frames = Vec::new();
    let mut buffer = [0_u8; 2048];
    while !frames
        .iter()
        .any(|frame: &Frame| frame.message_id == message_id)
    {
        let count = tokio::time::timeout(Duration::from_secs(1), phone.read(&mut buffer))
            .await
            .expect("timed out waiting for SCCP response")
            .expect("could not read SCCP response");
        assert_ne!(count, 0, "SCCP session closed while waiting for response");
        frames.extend(decoder.push(&buffer[..count]).unwrap());
    }
    frames
}

async fn read_until_server_message(
    phone: &mut dyn StationIo,
    decoder: &mut FrameDecoder,
    protocol: ProtocolVersion,
    predicate: impl Fn(&ServerMessage) -> bool,
) -> Vec<ServerMessage> {
    let mut messages = Vec::new();
    let mut buffer = [0_u8; 2048];
    while !messages.iter().any(&predicate) {
        let count = tokio::time::timeout(Duration::from_secs(1), phone.read(&mut buffer))
            .await
            .expect("timed out waiting for SCCP response")
            .expect("could not read SCCP response");
        assert_ne!(count, 0, "SCCP session closed while waiting for response");
        messages.extend(
            decoder
                .push(&buffer[..count])
                .unwrap()
                .into_iter()
                .map(|frame| ServerMessage::decode(frame, protocol).unwrap()),
        );
    }
    messages
}

fn open_receive_request_party(frames: &[Frame], protocol: ProtocolVersion) -> u32 {
    frames
        .iter()
        .find_map(
            |frame| match ServerMessage::decode(frame.clone(), protocol).ok()? {
                ServerMessage::OpenReceiveChannel {
                    passthrough_party_id,
                    ..
                } => Some(passthrough_party_id),
                _ => None,
            },
        )
        .expect("transaction omitted OpenReceiveChannel")
}

fn start_media_request_party(frames: &[Frame], protocol: ProtocolVersion) -> u32 {
    frames
        .iter()
        .find_map(
            |frame| match ServerMessage::decode(frame.clone(), protocol).ok()? {
                ServerMessage::StartMediaTransmission {
                    passthrough_party_id,
                    ..
                } => Some(passthrough_party_id),
                _ => None,
            },
        )
        .expect("transaction omitted StartMediaTransmission")
}

fn coupled_media_request_party(frames: &[Frame], protocol: ProtocolVersion) -> u32 {
    let receive = open_receive_request_party(frames, protocol);
    let transmit = start_media_request_party(frames, protocol);
    assert_ne!(receive, 0);
    assert_eq!(receive, transmit, "coupled request identities diverged");
    receive
}

fn test_connection_statistics(directory_number: &str, call_reference: u32) -> ConnectionStatistics {
    ConnectionStatistics {
        directory_number: directory_number.into(),
        call_reference,
        processing: StatisticsProcessing::Clear,
        packets_sent: 120,
        octets_sent: 9_600,
        packets_received: 118,
        octets_received: 9_440,
        packets_lost: 2,
        jitter_millis: 6,
        latency_millis: 17,
        quality: crate::ConnectionQualityStatistics::new(b"MLQK=4.4".to_vec()).unwrap(),
    }
}

fn packed_base_only_connection_statistics_bytes(
    statistics: &ConnectionStatistics,
    protocol: ProtocolVersion,
) -> Vec<u8> {
    assert!(protocol.wire() >= ProtocolVersion::V22.wire());
    let directory_number = statistics.directory_number.as_bytes();
    assert!(directory_number.len() < 28);

    let mut payload = vec![0; 28];
    payload[..directory_number.len()].copy_from_slice(directory_number);
    payload.extend_from_slice(&statistics.call_reference.to_le_bytes());
    payload.push(
        u8::try_from(statistics.processing.wire_value())
            .expect("test processing value fits packed statistics layout"),
    );
    for counter in [
        statistics.packets_sent,
        statistics.octets_sent,
        statistics.packets_received,
        statistics.octets_received,
        statistics.packets_lost,
        statistics.jitter_millis,
        statistics.latency_millis,
    ] {
        payload.extend_from_slice(&counter.to_le_bytes());
    }
    assert_eq!(payload.len(), 61);

    Frame::new(protocol.wire(), wire_id::CONNECTION_STATISTICS_RES, payload)
        .encode()
        .unwrap()
}

#[derive(Debug)]
struct RecordingSocketQos {
    applied: Arc<std::sync::Mutex<Vec<SignalingQos>>>,
    fail: bool,
}

impl StationSocketQos for RecordingSocketQos {
    fn apply(&self, qos: SignalingQos) -> SocketQosReport {
        self.applied.lock().unwrap().push(qos);
        if self.fail {
            SocketQosReport::failed(
                SocketQosMark::SocketPriority,
                std::io::Error::new(std::io::ErrorKind::Unsupported, "test platform"),
            )
        } else {
            SocketQosReport::default()
        }
    }
}
