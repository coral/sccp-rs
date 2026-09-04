use sccp_protocol::message::values::{PhoneFeatures, StationSessionContext};
use sccp_protocol::{
    BusyLampFieldState, ButtonTemplateEntry, ButtonType, CallCountLineData,
    CallCountRequestPayload, CallCountResponse, CallDirection, CallHistoryDisposition, CallInfo,
    CallState, ClientMessage, Digit, Frame, FrameDecoder, KeyMode, LampMode, MediaPathEvent,
    MediaPathId, MessageId, MicrophoneMode, NotificationPriority, ProtocolVersion, RingDuration,
    RingerMode, ServerMessage, SoftKey, SoftKeyProfile, SpeakerMode, Stimulus, SubscriptionCause,
    SubscriptionRequest, Tone, ToneDirection,
};

fn raw_frame(protocol: ProtocolVersion, message_id: u32, payload: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(payload.len() + 12);
    bytes.extend_from_slice(&(payload.len() as u32 + 4).to_le_bytes());
    bytes.extend_from_slice(&protocol.wire().to_le_bytes());
    bytes.extend_from_slice(&message_id.to_le_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn one_frame(bytes: &[u8]) -> Frame {
    let mut decoder = FrameDecoder::new();
    let mut frames = decoder.push(bytes).expect("fixture frame must decode");
    assert_eq!(frames.len(), 1);
    assert_eq!(decoder.buffered_len(), 0);
    frames.remove(0)
}

fn words(values: &[u32]) -> Vec<u8> {
    values
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

fn assert_client_fixture(
    name: &str,
    protocol: ProtocolVersion,
    message_id: u32,
    payload: &[u8],
    expected: ClientMessage,
) {
    let raw = raw_frame(protocol, message_id, payload);
    let frame = one_frame(&raw);
    assert_eq!(frame.message_id, message_id, "{name} message ID");
    assert_eq!(frame.payload, payload, "{name} payload");
    assert_eq!(
        ClientMessage::decode_with_version(frame, protocol).expect(name),
        expected,
        "{name} semantic layout"
    );
}

fn assert_server_fixture(
    name: &str,
    protocol: ProtocolVersion,
    message_id: u32,
    payload_len: usize,
    expected: ServerMessage,
) {
    let encoded = expected.encode(protocol).expect(name);
    let frame = one_frame(&encoded);
    assert_eq!(frame.message_id, message_id, "{name} message ID");
    assert_eq!(frame.payload.len(), payload_len, "{name} payload size");
    assert_eq!(
        ServerMessage::decode(frame, protocol).expect(name),
        expected,
        "{name} semantic layout"
    );
}

fn assert_server_session_fixture(
    name: &str,
    session: StationSessionContext,
    message_id: u32,
    payload_len: usize,
    expected: ServerMessage,
) {
    let encoded = expected.encode_for_session(session).expect(name);
    let frame = one_frame(&encoded);
    assert_eq!(frame.message_id, message_id, "{name} message ID");
    assert_eq!(frame.payload.len(), payload_len, "{name} payload size");
    assert_eq!(
        ServerMessage::decode(frame, session.protocol).expect(name),
        expected,
        "{name} semantic layout"
    );
}

fn assert_encoded_client_fixture(
    name: &str,
    protocol: ProtocolVersion,
    message_id: u32,
    payload_len: usize,
    expected: ClientMessage,
) {
    let encoded = expected.encode(protocol).expect(name);
    let frame = one_frame(&encoded);
    assert_eq!(frame.message_id, message_id, "{name} message ID");
    assert_eq!(frame.payload.len(), payload_len, "{name} payload size");
    assert_eq!(
        ClientMessage::decode_with_version(frame, protocol).expect(name),
        expected,
        "{name} semantic layout"
    );
}

#[test]
fn station_ui_input_fixtures_cover_key_hook_and_accessory_layouts() {
    for (name, protocol, message_id, payload_len, expected) in [
        (
            "keypad button",
            ProtocolVersion::V22,
            MessageId::KeypadButton.wire_value(),
            20,
            ClientMessage::KeypadButton {
                button: Digit::Pound,
                line_instance: 2,
                call_reference: 42,
                wire_layout: None,
            },
        ),
        (
            "legacy enbloc call",
            ProtocolVersion::V3,
            MessageId::EnblocCall.wire_value(),
            24,
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: 0,
            },
        ),
        (
            "v17 enbloc call",
            ProtocolVersion::V17,
            MessageId::EnblocCall.wire_value(),
            28,
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: 2,
            },
        ),
        (
            "v22 enbloc call",
            ProtocolVersion::V22,
            MessageId::EnblocCall.wire_value(),
            32,
            ClientMessage::EnblocCall {
                called_party: "2001".into(),
                line_instance: 2,
            },
        ),
        (
            "v18 off-hook with calling party",
            ProtocolVersion::V18,
            MessageId::OffHookWithCallingParty.wire_value(),
            52,
            ClientMessage::OffHookWithCallingParty {
                calling_party_number: "2001".into(),
                voice_mailbox: "5000".into(),
                line_instance: 2,
            },
        ),
        (
            "v19 off-hook with calling party",
            ProtocolVersion::V19,
            MessageId::OffHookWithCallingParty.wire_value(),
            56,
            ClientMessage::OffHookWithCallingParty {
                calling_party_number: "2001".into(),
                voice_mailbox: "5000".into(),
                line_instance: 2,
            },
        ),
        (
            "stimulus",
            ProtocolVersion::V22,
            MessageId::Stimulus.wire_value(),
            16,
            ClientMessage::Stimulus {
                stimulus: Stimulus::Line,
                instance: 2,
                call_reference: 42,
                status: 1,
            },
        ),
        (
            "off-hook",
            ProtocolVersion::V22,
            MessageId::OffHook.wire_value(),
            8,
            ClientMessage::OffHook {
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "on-hook",
            ProtocolVersion::V22,
            MessageId::OnHook.wire_value(),
            8,
            ClientMessage::OnHook {
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "hook flash",
            ProtocolVersion::V22,
            MessageId::HookFlash.wire_value(),
            8,
            ClientMessage::HookFlash {
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "soft-key event",
            ProtocolVersion::V22,
            MessageId::SoftKeyEvent.wire_value(),
            12,
            ClientMessage::SoftKeyEvent {
                event: SoftKey::Answer.wire_value(),
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "headset status",
            ProtocolVersion::V22,
            MessageId::HeadsetStatus.wire_value(),
            4,
            ClientMessage::HeadsetStatus { enabled: true },
        ),
        (
            "accessory status",
            ProtocolVersion::V22,
            MessageId::MediaPathEvent.wire_value(),
            8,
            ClientMessage::MediaPathEvent {
                path: MediaPathId::Headset,
                event: MediaPathEvent::On,
            },
        ),
        (
            "call-count request",
            ProtocolVersion::V22,
            MessageId::CallCountRequest.wire_value(),
            0,
            ClientMessage::CallCountRequest(CallCountRequestPayload::Empty),
        ),
        (
            "BLF subscription request",
            ProtocolVersion::V22,
            MessageId::SubscriptionStatusRequest.wire_value(),
            268,
            ClientMessage::SubscriptionStatusRequest(SubscriptionRequest {
                transaction_id: 7,
                feature_id: 1,
                timer_seconds: 30,
                subscription_id: "2001".into(),
            }),
        ),
    ] {
        assert_encoded_client_fixture(name, protocol, message_id, payload_len, expected);
    }
}

#[test]
fn station_ui_input_fixtures_accept_length_selected_dial_and_hook_layouts() {
    let protocol = ProtocolVersion::V22;
    let expected = ClientMessage::EnblocCall {
        called_party: "2001".into(),
        line_instance: 2,
    };

    let mut aligned = vec![0; 32];
    aligned[..4].copy_from_slice(b"2001");
    aligned[28..32].copy_from_slice(&2_u32.to_le_bytes());
    assert_client_fixture(
        "aligned enbloc call",
        protocol,
        MessageId::EnblocCall.wire_value(),
        &aligned,
        expected.clone(),
    );

    let mut packed = vec![0; 29];
    packed[..4].copy_from_slice(b"2001");
    packed[25..29].copy_from_slice(&2_u32.to_le_bytes());
    assert_client_fixture(
        "packed enbloc call",
        protocol,
        MessageId::EnblocCall.wire_value(),
        &packed,
        expected.clone(),
    );

    packed.extend_from_slice(&[0; 3]);
    assert_client_fixture(
        "padded packed enbloc call",
        protocol,
        MessageId::EnblocCall.wire_value(),
        &packed,
        expected,
    );

    assert_client_fixture(
        "fieldless on-hook",
        protocol,
        MessageId::OnHook.wire_value(),
        &[],
        ClientMessage::OnHook {
            line_instance: 0,
            call_reference: 0,
        },
    );
}

#[test]
fn station_ui_request_fixtures_cover_every_status_query_layout() {
    let protocol = ProtocolVersion::V22;
    for (name, message_id, payload, expected) in [
        (
            "forward status request",
            MessageId::ForwardStatusRequest.wire_value(),
            words(&[3]),
            ClientMessage::ForwardStatusRequest { line_instance: 3 },
        ),
        (
            "speed-dial status request",
            MessageId::SpeedDialStatusRequest.wire_value(),
            words(&[4]),
            ClientMessage::SpeedDialStatusRequest {
                speed_dial_instance: 4,
            },
        ),
        (
            "line status request",
            MessageId::LineStatusRequest.wire_value(),
            words(&[5]),
            ClientMessage::LineStatRequest { line_instance: 5 },
        ),
        (
            "configuration status request",
            MessageId::ConfigStatusRequest.wire_value(),
            Vec::new(),
            ClientMessage::ConfigStatRequest,
        ),
        (
            "time/date request",
            MessageId::TimeDateRequest.wire_value(),
            Vec::new(),
            ClientMessage::TimeDateRequest,
        ),
        (
            "button-template request",
            MessageId::ButtonTemplateRequest.wire_value(),
            Vec::new(),
            ClientMessage::ButtonTemplateRequest,
        ),
        (
            "version request",
            MessageId::VersionRequest.wire_value(),
            Vec::new(),
            ClientMessage::VersionRequest,
        ),
        (
            "soft-key set request",
            MessageId::SoftKeySetRequest.wire_value(),
            Vec::new(),
            ClientMessage::SoftKeySetRequest,
        ),
        (
            "soft-key template request",
            MessageId::SoftKeyTemplateRequest.wire_value(),
            Vec::new(),
            ClientMessage::SoftKeyTemplateRequest,
        ),
        (
            "service URL status request",
            MessageId::ServiceUrlStatusRequest.wire_value(),
            words(&[6]),
            ClientMessage::ServiceUrlStatusRequest { index: 6 },
        ),
        (
            "feature status request",
            MessageId::FeatureStatusRequest.wire_value(),
            words(&[7, 0x0102_0304]),
            ClientMessage::FeatureStatusRequest {
                index: 7,
                capabilities: 0x0102_0304,
            },
        ),
    ] {
        assert_client_fixture(name, protocol, message_id, &payload, expected);
    }
}

#[test]
fn station_ui_response_fixtures_cover_static_and_dynamic_layouts() {
    let profile = SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
        let actions = match mode {
            KeyMode::OnHook => vec![SoftKey::Redial, SoftKey::NewCall],
            KeyMode::RingIn => vec![SoftKey::Answer, SoftKey::EndCall],
            KeyMode::Connected => vec![SoftKey::Hold, SoftKey::EndCall],
            _ => Vec::new(),
        };
        (mode, actions)
    }))
    .unwrap();

    for (name, protocol, message_id, payload_len, expected) in [
        (
            "configuration status",
            ProtocolVersion::V3,
            MessageId::ConfigStatus.wire_value(),
            112,
            ServerMessage::ConfigStatus(sccp_protocol::ConfigurationStatus {
                device_name: "SEP001122334455".into(),
                station_user_id: 0,
                station_instance: 1,
                user_name: "Alice".into(),
                server_name: "sccp".into(),
                line_count: 2,
                speed_dial_count: 0,
            }),
        ),
        (
            "legacy line status",
            ProtocolVersion::V3,
            MessageId::LineStatus.wire_value(),
            112,
            ServerMessage::LineStatus {
                instance: 1,
                directory_number: "1001".into(),
                fully_qualified_display_name: "Alice".into(),
                display_label: "Alice".into(),
            },
        ),
        (
            "dynamic line status",
            ProtocolVersion::V22,
            MessageId::LineStatusDynamic.wire_value(),
            28,
            ServerMessage::LineStatus {
                instance: 1,
                directory_number: "1001".into(),
                fully_qualified_display_name: "Alice".into(),
                display_label: "Alice".into(),
            },
        ),
        (
            "button template",
            ProtocolVersion::V22,
            MessageId::ButtonTemplate.wire_value(),
            96,
            ServerMessage::ButtonTemplate {
                offset: 0,
                total: 2,
                buttons: vec![
                    ButtonTemplateEntry {
                        instance: 1,
                        button_type: ButtonType::Line,
                    },
                    ButtonTemplateEntry {
                        instance: 1,
                        button_type: ButtonType::SpeedDial,
                    },
                ],
            },
        ),
        (
            "firmware version",
            ProtocolVersion::V22,
            MessageId::Version.wire_value(),
            16,
            ServerMessage::Version {
                firmware: "SCCP45.9-4".into(),
            },
        ),
        (
            "time/date",
            ProtocolVersion::V22,
            MessageId::DefineTimeDate.wire_value(),
            36,
            ServerMessage::TimeDate {
                year: 2026,
                month: 8,
                weekday: 4,
                day: 20,
                hour: 13,
                minute: 14,
                second: 15,
                milliseconds: 250,
                unix_seconds: 1_787_254_455,
            },
        ),
        (
            "soft-key template",
            ProtocolVersion::V22,
            MessageId::SoftKeyTemplateResponse.wire_value(),
            652,
            ServerMessage::SoftKeyTemplate {
                actions: profile.template_actions(),
            },
        ),
        (
            "soft-key set",
            ProtocolVersion::V22,
            MessageId::SoftKeySetResponse.wire_value(),
            780,
            ServerMessage::SoftKeySet { profile },
        ),
        (
            "legacy forwarding status",
            ProtocolVersion::V3,
            MessageId::ForwardStatus.wire_value(),
            92,
            ServerMessage::ForwardStatus {
                line_instance: 1,
                forward_all: Some("2001".into()),
                forward_busy: Some("2002".into()),
                forward_no_answer: Some("2003".into()),
            },
        ),
        (
            "extended forwarding status",
            ProtocolVersion::V22,
            MessageId::ForwardStatus.wire_value(),
            104,
            ServerMessage::ForwardStatus {
                line_instance: 1,
                forward_all: Some("2001".into()),
                forward_busy: Some("2002".into()),
                forward_no_answer: Some("2003".into()),
            },
        ),
        (
            "legacy speed-dial status",
            ProtocolVersion::V3,
            MessageId::SpeedDialStatus.wire_value(),
            68,
            ServerMessage::SpeedDialStatus {
                instance: 2,
                number: "2001".into(),
                display_name: "Reception".into(),
            },
        ),
        (
            "dynamic speed-dial status",
            ProtocolVersion::V22,
            MessageId::SpeedDialStatusDynamic.wire_value(),
            20,
            ServerMessage::SpeedDialStatus {
                instance: 2,
                number: "2001".into(),
                display_name: "Reception".into(),
            },
        ),
        (
            "legacy feature status",
            ProtocolVersion::V3,
            MessageId::FeatureStatus.wire_value(),
            52,
            ServerMessage::FeatureStatus {
                instance: 3,
                button_type: ButtonType::DoNotDisturb,
                label: "DND".into(),
                state: 0x010000,
            },
        ),
        (
            "legacy service URL status",
            ProtocolVersion::V3,
            MessageId::ServiceUrlStatus.wire_value(),
            300,
            ServerMessage::ServiceUrlStatus {
                index: 4,
                url: "http://phone.invalid/services".into(),
                label: "Services".into(),
                extension_text: String::new(),
            },
        ),
        (
            "dynamic service URL status",
            ProtocolVersion::V22,
            MessageId::ServiceUrlStatusDynamic.wire_value(),
            44,
            ServerMessage::ServiceUrlStatus {
                index: 4,
                url: "http://phone.invalid/services".into(),
                label: "Services".into(),
                extension_text: String::new(),
            },
        ),
    ] {
        assert_server_fixture(name, protocol, message_id, payload_len, expected);
    }

    assert_server_session_fixture(
        "feature status selected by session capability",
        StationSessionContext::new(ProtocolVersion::V8, PhoneFeatures::DYNAMIC_MESSAGES),
        MessageId::FeatureStatusDynamic.wire_value(),
        136,
        ServerMessage::FeatureStatus {
            instance: 3,
            button_type: ButtonType::DoNotDisturb,
            label: "DND".into(),
            state: 0x010000,
        },
    );
}

#[test]
fn station_ui_control_fixtures_cover_call_plane_display_and_device_layouts() {
    for (name, protocol, message_id, payload_len, expected) in [
        (
            "register acknowledgement",
            ProtocolVersion::V22,
            MessageId::RegisterAck.wire_value(),
            20,
            ServerMessage::RegisterAck {
                keepalive_seconds: 30,
                secondary_keepalive_seconds: 30,
                protocol: ProtocolVersion::V22,
                features: PhoneFeatures::empty(),
                date_template: sccp_protocol::DateTemplate::new("D/M/Y").unwrap(),
            },
        ),
        (
            "start tone",
            ProtocolVersion::V22,
            MessageId::StartTone.wire_value(),
            16,
            ServerMessage::StartTone {
                tone: Tone::Zip,
                direction: ToneDirection::User,
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "legacy stop tone",
            ProtocolVersion::V3,
            MessageId::StopTone.wire_value(),
            8,
            ServerMessage::StopTone {
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "extended stop tone",
            ProtocolVersion::V22,
            MessageId::StopTone.wire_value(),
            12,
            ServerMessage::StopTone {
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "ringer state",
            ProtocolVersion::V22,
            MessageId::SetRinger.wire_value(),
            16,
            ServerMessage::SetRinger {
                mode: RingerMode::Inside,
                duration: RingDuration::Single,
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "lamp state",
            ProtocolVersion::V22,
            MessageId::SetLamp.wire_value(),
            12,
            ServerMessage::SetLamp {
                stimulus: ButtonType::Line,
                instance: 2,
                mode: LampMode::Blink,
            },
        ),
        (
            "speaker mode",
            ProtocolVersion::V22,
            MessageId::SetSpeakerMode.wire_value(),
            4,
            ServerMessage::SetSpeakerMode(SpeakerMode::On),
        ),
        (
            "microphone mode",
            ProtocolVersion::V22,
            MessageId::SetMicrophoneMode.wire_value(),
            4,
            ServerMessage::SetMicrophoneMode(MicrophoneMode::Off),
        ),
        (
            "select soft keys",
            ProtocolVersion::V22,
            MessageId::SelectSoftKeys.wire_value(),
            16,
            ServerMessage::SelectSoftKeys {
                line_instance: 2,
                call_reference: 42,
                set: KeyMode::Connected,
                valid_mask: 0b11,
            },
        ),
        (
            "call state",
            ProtocolVersion::V22,
            MessageId::CallState.wire_value(),
            24,
            ServerMessage::CallState {
                state: CallState::Connected,
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "legacy prompt",
            ProtocolVersion::V3,
            MessageId::DisplayPromptStatus.wire_value(),
            44,
            ServerMessage::DisplayPrompt {
                timeout_seconds: 5,
                text: "Connected".into(),
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "dynamic prompt",
            ProtocolVersion::V22,
            MessageId::DisplayDynamicPromptStatus.wire_value(),
            24,
            ServerMessage::DisplayPrompt {
                timeout_seconds: 5,
                text: "Connected".into(),
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "clear prompt",
            ProtocolVersion::V22,
            MessageId::ClearPromptStatus.wire_value(),
            8,
            ServerMessage::ClearPrompt {
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "legacy notification",
            ProtocolVersion::V3,
            MessageId::DisplayNotify.wire_value(),
            36,
            ServerMessage::DisplayNotify {
                timeout_seconds: 5,
                text: "Saved".into(),
            },
        ),
        (
            "dynamic notification",
            ProtocolVersion::V22,
            MessageId::DisplayDynamicNotify.wire_value(),
            12,
            ServerMessage::DisplayNotify {
                timeout_seconds: 5,
                text: "Saved".into(),
            },
        ),
        (
            "clear notification",
            ProtocolVersion::V22,
            MessageId::ClearNotify.wire_value(),
            0,
            ServerMessage::ClearNotify,
        ),
        (
            "legacy priority notification",
            ProtocolVersion::V3,
            MessageId::DisplayPriorityNotify.wire_value(),
            40,
            ServerMessage::DisplayPriorityNotify {
                timeout_seconds: 5,
                priority: NotificationPriority::Voicemail,
                text: "Voicemail".into(),
            },
        ),
        (
            "dynamic priority notification",
            ProtocolVersion::V22,
            MessageId::DisplayDynamicPriorityNotify.wire_value(),
            20,
            ServerMessage::DisplayPriorityNotify {
                timeout_seconds: 5,
                priority: NotificationPriority::Voicemail,
                text: "Voicemail".into(),
            },
        ),
        (
            "clear priority notification",
            ProtocolVersion::V22,
            MessageId::ClearPriorityNotify.wire_value(),
            4,
            ServerMessage::ClearPriorityNotify {
                priority: NotificationPriority::Voicemail,
            },
        ),
        (
            "display text",
            ProtocolVersion::V22,
            MessageId::DisplayText.wire_value(),
            32,
            ServerMessage::DisplayText {
                text: "Ready".into(),
            },
        ),
        (
            "clear display",
            ProtocolVersion::V22,
            MessageId::ClearDisplay.wire_value(),
            0,
            ServerMessage::ClearDisplay,
        ),
        (
            "activate call plane",
            ProtocolVersion::V22,
            MessageId::ActivateCallPlane.wire_value(),
            4,
            ServerMessage::ActivateCallPlane { line_instance: 2 },
        ),
        (
            "deactivate call plane",
            ProtocolVersion::V22,
            MessageId::DeactivateCallPlane.wire_value(),
            0,
            ServerMessage::DeactivateCallPlane,
        ),
        (
            "backspace response",
            ProtocolVersion::V22,
            MessageId::BackspaceResponse.wire_value(),
            8,
            ServerMessage::BackspaceResponse {
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "legacy dialed number",
            ProtocolVersion::V3,
            MessageId::DialedNumber.wire_value(),
            32,
            ServerMessage::DialedNumber {
                number: "2001".into(),
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "extended dialed number",
            ProtocolVersion::V22,
            MessageId::DialedNumber.wire_value(),
            36,
            ServerMessage::DialedNumber {
                number: "2001".into(),
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "call selection status",
            ProtocolVersion::V22,
            MessageId::CallSelectStatus.wire_value(),
            12,
            ServerMessage::CallSelectStatus {
                status: 1,
                call_reference: 42,
                line_instance: 2,
            },
        ),
        (
            "call-history disposition",
            ProtocolVersion::V22,
            MessageId::CallHistoryDisposition.wire_value(),
            12,
            ServerMessage::CallHistoryDisposition {
                disposition: CallHistoryDisposition::Received,
                line_instance: 2,
                call_reference: 42,
            },
        ),
        (
            "call-count response",
            ProtocolVersion::V22,
            MessageId::CallCountResponse.wire_value(),
            180,
            ServerMessage::CallCountResponse(CallCountResponse {
                total_configured_lines: 1,
                starting_line_instance: 1,
                line_data: vec![CallCountLineData {
                    max_calls: 4,
                    busy_trigger: 2,
                }],
            }),
        ),
        (
            "recording status",
            ProtocolVersion::V22,
            MessageId::RecordingStatus.wire_value(),
            8,
            ServerMessage::RecordingStatus {
                call_reference: 42,
                active: true,
            },
        ),
    ] {
        assert_server_fixture(name, protocol, message_id, payload_len, expected);
    }
}

#[test]
fn station_ui_call_information_fixtures_cover_legacy_and_dynamic_layouts() {
    let info = CallInfo {
        direction: CallDirection::Inbound,
        calling_name: "Alice".into(),
        calling_number: "1001".into(),
        called_name: "Bob".into(),
        called_number: "2001".into(),
        original_called_name: "Carol".into(),
        original_called_number: "3001".into(),
        last_redirecting_name: "Dave".into(),
        last_redirecting_number: "4001".into(),
        original_redirect_reason: 2,
        last_redirect_reason: 4,
        party_restrictions: 0,
    };
    let message = ServerMessage::CallInfo {
        info,
        line_instance: 2,
        call_reference: 42,
    };

    assert_server_fixture(
        "legacy call information",
        ProtocolVersion::V3,
        MessageId::CallInfo.wire_value(),
        384,
        message.clone(),
    );
    assert_server_fixture(
        "dynamic call information",
        ProtocolVersion::V22,
        MessageId::CallInfoDynamic.wire_value(),
        80,
        message,
    );

    assert_server_fixture(
        "BLF subscription status",
        ProtocolVersion::V22,
        MessageId::SubscriptionStatus.wire_value(),
        16,
        ServerMessage::SubscriptionStatus {
            transaction_id: 7,
            feature_id: 1,
            timer_seconds: 30,
            cause: SubscriptionCause::Ok,
        },
    );
    assert_server_fixture(
        "BLF notification",
        ProtocolVersion::V22,
        MessageId::Notification.wire_value(),
        112,
        ServerMessage::Notification {
            transaction_id: 7,
            feature_id: 1,
            status: BusyLampFieldState::InUse,
            text: "2001".into(),
        },
    );
}
