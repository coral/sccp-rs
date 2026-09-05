use super::support::*;

#[test]
fn video_receive_validation_uses_typed_station_policy_without_touching_audio() {
    let mut state = multicast_test_state(ProtocolVersion::V22);
    state.media_capabilities = StationMediaCapabilities::new(
        state.media_capabilities.audio().to_vec(),
        vec![crate::message::capabilities::VideoCapability {
            codec: Codec::H264,
            direction: ReceiveTransmit::RECEIVE,
            level_preferences: Vec::new(),
            codec_parameters: Vec::new(),
            encryption_capability: Some(EncryptionCapability::Capable),
            address_type: Some(IpAddressType::Ipv4AndIpv6),
        }],
    );
    let audio_before = state.media_capabilities.audio().to_vec();
    let descriptor = video_receive_descriptor(
        70,
        MediaEndpointAddress {
            address: "192.0.2.10".parse().unwrap(),
            port: 5004,
        },
    );
    assert!(validate_multimedia_receive(&state, &descriptor).is_ok());
    assert_eq!(state.media_capabilities.audio(), audio_before);

    let receive_capability = state.media_capabilities.video()[0].clone();
    state.media_capabilities = StationMediaCapabilities::new(
        audio_before.clone(),
        vec![crate::message::capabilities::VideoCapability {
            direction: ReceiveTransmit::TRANSMIT,
            ..receive_capability.clone()
        }],
    );
    assert!(matches!(
        validate_multimedia_receive(&state, &descriptor),
        Err(ServerError::UnsupportedMultimediaReceive)
    ));
    state.media_capabilities =
        StationMediaCapabilities::new(audio_before.clone(), vec![receive_capability]);

    let mut wrong_direction = descriptor.clone();
    wrong_direction.payload = video_transmit_descriptor(
        70,
        MediaEndpointAddress {
            address: "192.0.2.10".parse().unwrap(),
            port: 5004,
        },
    )
    .payload;
    assert!(matches!(
        wrong_direction.validate(),
        Err(ServerError::InvalidMultimediaReceive(_))
    ));

    let mut mismatched_address = descriptor.clone();
    mismatched_address.requested_address_type = IpAddressType::Ipv6;
    assert!(matches!(
        mismatched_address.validate(),
        Err(ServerError::InvalidMultimediaReceive(_))
    ));

    for protocol in [
        ProtocolVersion::V3,
        ProtocolVersion::V10,
        ProtocolVersion::V11,
    ] {
        state.registration.protocol = protocol;
        let mut legacy_descriptor = descriptor.clone();
        legacy_descriptor.source = MediaEndpointAddress {
            address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
        };
        legacy_descriptor.requested_address_type = IpAddressType::Ipv4;
        legacy_descriptor.payload = MultimediaPayload::from_wire(
            0,
            test_rtp_payload_number(97),
            [0xa5; crate::MULTIMEDIA_CAPABILITY_BYTES],
            Codec::H264,
            MultimediaPayloadDirection::Receive,
            protocol,
        );
        assert!(validate_multimedia_receive(&state, &legacy_descriptor).is_ok());
        assert_eq!(state.media_capabilities.audio(), audio_before);
    }
}

#[test]
fn video_transmit_validation_requires_exact_direction_endpoint_and_protocol() {
    let mut state = multicast_test_state(ProtocolVersion::V22);
    let audio_before = state.media_capabilities.audio().to_vec();
    let capability = crate::message::capabilities::VideoCapability {
        codec: Codec::H264,
        direction: ReceiveTransmit::TRANSMIT,
        level_preferences: Vec::new(),
        codec_parameters: Vec::new(),
        encryption_capability: Some(EncryptionCapability::Capable),
        address_type: Some(IpAddressType::Ipv4AndIpv6),
    };
    state.media_capabilities =
        StationMediaCapabilities::new(audio_before.clone(), vec![capability.clone()]);
    let descriptor = video_transmit_descriptor(
        80,
        MediaEndpointAddress {
            address: "192.0.2.80".parse().unwrap(),
            port: 5080,
        },
    );
    assert!(validate_multimedia_transmit(&state, &descriptor).is_ok());
    assert_eq!(state.media_capabilities.audio(), audio_before);

    state.media_capabilities = StationMediaCapabilities::new(
        audio_before.clone(),
        vec![crate::message::capabilities::VideoCapability {
            direction: ReceiveTransmit::RECEIVE,
            ..capability.clone()
        }],
    );
    assert!(matches!(
        validate_multimedia_transmit(&state, &descriptor),
        Err(ServerError::UnsupportedMultimediaTransmit)
    ));
    state.media_capabilities =
        StationMediaCapabilities::new(audio_before.clone(), vec![capability]);

    for invalid in [
        MultimediaTransmitDescriptor {
            payload: video_receive_descriptor(
                80,
                MediaEndpointAddress {
                    address: "192.0.2.80".parse().unwrap(),
                    port: 5080,
                },
            )
            .payload,
            ..descriptor.clone()
        },
        MultimediaTransmitDescriptor {
            endpoint: MediaEndpointAddress {
                address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: 5080,
            },
            ..descriptor.clone()
        },
    ] {
        assert!(matches!(
            invalid.validate(),
            Err(ServerError::InvalidMultimediaTransmit(_))
        ));
    }

    for protocol in [ProtocolVersion::V3, ProtocolVersion::V10] {
        state.registration.protocol = protocol;
        let mut legacy = descriptor.clone();
        legacy.payload = MultimediaPayload::from_wire(
            0,
            test_rtp_payload_number(98),
            [0x5a; crate::MULTIMEDIA_CAPABILITY_BYTES],
            Codec::H264,
            MultimediaPayloadDirection::Transmit,
            protocol,
        );
        assert!(validate_multimedia_transmit(&state, &legacy).is_ok());
    }

    state.registration.protocol = ProtocolVersion::V16;
    let ipv6 = MultimediaTransmitDescriptor {
        endpoint: MediaEndpointAddress {
            address: "2001:db8::80".parse().unwrap(),
            port: 5080,
        },
        ..descriptor
    };
    assert!(matches!(
        validate_multimedia_transmit(&state, &ipv6),
        Err(ServerError::InvalidMultimediaTransmit(_))
    ));
    assert_eq!(state.media_capabilities.audio(), audio_before);
}

#[test]
fn multimedia_transmit_controls_encode_only_their_typed_parameter_words() {
    fn assert_control(
        control: MultimediaTransmitControl,
        expected_command: MiscCommandType,
        expected_words: &[u32],
    ) {
        let (command, data) = encode_multimedia_transmit_control(control).unwrap();
        let expected = expected_words
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect::<Vec<_>>();
        assert_eq!(command, expected_command);
        assert_eq!(data.as_bytes(), expected);
    }

    assert_control(
        MultimediaTransmitControl::FreezePicture,
        MiscCommandType::VideoFreezePicture,
        &[],
    );
    assert_control(
        MultimediaTransmitControl::FastPictureUpdate {
            first_gob: 1,
            gob_count: 2,
        },
        MiscCommandType::VideoFastUpdatePicture,
        &[1, 2],
    );
    assert_control(
        MultimediaTransmitControl::FastGobUpdate {
            first_gob: 3,
            gob_count: 4,
        },
        MiscCommandType::VideoFastUpdateGob,
        &[3, 4],
    );
    assert_control(
        MultimediaTransmitControl::FastMacroblockUpdate {
            first_gob: 5,
            first_macroblock: 6,
            macroblock_count: 7,
        },
        MiscCommandType::VideoFastUpdateMacroblock,
        &[5, 6, 7],
    );
    assert_control(
        MultimediaTransmitControl::LostPicture {
            picture_number: 8,
            long_term_picture_index: 9,
        },
        MiscCommandType::LostPicture,
        &[8, 9],
    );
    assert_control(
        MultimediaTransmitControl::LostPartialPicture {
            picture_number: 10,
            long_term_picture_index: 11,
            first_macroblock: 12,
            macroblock_count: 13,
        },
        MiscCommandType::LostPartialPicture,
        &[10, 11, 12, 13],
    );
    let pictures = VideoPictureReferences::new([
        VideoPictureReference {
            picture_number: 14,
            long_term_picture_index: 15,
        },
        VideoPictureReference {
            picture_number: 16,
            long_term_picture_index: 17,
        },
    ])
    .unwrap();
    assert_control(
        MultimediaTransmitControl::RecoveryReferencePicture { pictures },
        MiscCommandType::RecoveryReferencePicture,
        &[2, 14, 15, 16, 17],
    );
    assert_control(
        MultimediaTransmitControl::TemporalSpatialTradeoff { value: 18 },
        MiscCommandType::TemporalSpatialTradeoff,
        &[18],
    );
    assert!(matches!(
        VideoPictureReferences::new(std::iter::repeat(VideoPictureReference {
            picture_number: 1,
            long_term_picture_index: 2,
        })),
        Err(ServerError::InvalidMultimediaTransmitControl(_))
    ));
}

#[tokio::test]
async fn inbound_answer_media_presents_connected_immediately_before_receive_across_versions() {
    for protocol in [ProtocolVersion::V11, ProtocolVersion::V22] {
        let device = definition();
        let device_id = device.id.clone();
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut decoder = FrameDecoder::new();

        phone.write_all(&register_bytes(protocol)).await.unwrap();
        read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ));

        let call_id = handle
            .offer_incoming_call(
                device_id.clone(),
                LineInstance::new(1),
                CallInfo {
                    direction: crate::types::CallDirection::Inbound,
                    calling_number: "1002".into(),
                    called_number: "1001".into(),
                    ..CallInfo::default()
                },
            )
            .await
            .unwrap();
        read_until_message(
            &mut phone,
            &mut decoder,
            if protocol >= ProtocolVersion::V8 {
                wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS
            } else {
                wire_id::DISPLAY_PROMPT_STATUS
            },
        )
        .await;
        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Answer.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, wire_id::SET_LAMP).await;
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::SoftKey {
                    call_id: Some(answered),
                    soft_key: SoftKey::Answer,
                    ..
                },
                ..
            })) if answered == call_id
        ));

        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id,
                    purpose: ReceiveChannelPurpose::InboundAnswer,
                    source: None,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Skinny,
                    audio_processing: AudioProcessingPolicy::default(),
                },
            ))
            .await
            .unwrap();
        let frames =
            read_until_message(&mut phone, &mut decoder, wire_id::OPEN_RECEIVE_CHANNEL).await;
        let connected = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::CallState {
                        state: CallState::Connected,
                        ..
                    })
                )
            })
            .expect("inbound answer media omitted provisional Connected");
        let open = frames
            .iter()
            .position(|frame| frame.message_id == wire_id::OPEN_RECEIVE_CHANNEL)
            .expect("inbound answer media omitted OpenReceiveChannel");
        assert_eq!(open, connected + 1);
        assert!(frames[..open].iter().all(|frame| {
            !matches!(
                frame.message_id,
                wire_id::SET_SPEAKER_MODE
                    | wire_id::DISPLAY_PROMPT_STATUS
                    | wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS
                    | wire_id::SELECT_SOFT_KEYS
            )
        }));

        let (wire_reference, passthrough_party_id) =
            match ServerMessage::decode(frames[open].clone(), protocol).unwrap() {
                ServerMessage::OpenReceiveChannel {
                    call_reference,
                    passthrough_party_id,
                    ..
                } => (call_reference, passthrough_party_id),
                other => panic!("unexpected receive request: {other:?}"),
            };
        phone
            .write_all(
                &ClientMessage::OpenReceiveChannelAck {
                    status: MediaStatus::Ok,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    port: 4000,
                    call_reference: wire_reference,
                    passthrough_party_id,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::ReceiveChannelOpened {
                    call_id: opened,
                    status: MediaStatus::Ok,
                    ..
                },
                ..
            })) if opened == call_id
        ));

        assert!(matches!(
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::OpenMultimediaReceiveChannel {
                        call_id,
                        descriptor: video_receive_descriptor(
                            1,
                            MediaEndpointAddress {
                                address: "192.0.2.1".parse().unwrap(),
                                port: 5000,
                            },
                        ),
                    },
                ))
                .await,
            Err(ServerError::CommandWrite(message)) if message.contains("state OffHook")
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn video_receive_session_correlates_fragmented_acknowledgements_and_preserves_audio() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let call_id = CallId::new(71);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    phone
        .write_all(&capability_update_bytes(
            protocol,
            Codec::Pcmu,
            Codec::H264,
            71,
        ))
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Capabilities { .. },
            ..
        }))
    ));

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    let begin = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::SelectSoftKeys { .. })
    })
    .await;
    let wire_reference = begin
        .iter()
        .find_map(|message| match message {
            ServerMessage::CallState { call_reference, .. } => {
                Some(CallReference::new(*call_reference))
            }
            _ => None,
        })
        .expect("begin call omitted its wire identity");
    assert!(matches!(
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenMultimediaReceiveChannel {
                    call_id,
                    descriptor: video_receive_descriptor(
                        699,
                        MediaEndpointAddress {
                            address: "192.0.2.69".parse().unwrap(),
                            port: 5068,
                        },
                    ),
                },
            ))
            .await,
        Err(ServerError::CommandWrite(_))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenReceiveChannel {
                call_id,
                purpose: ReceiveChannelPurpose::Media,
                source: None,
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 2,
                dtmf_mode: DtmfMode::Rfc2833,
                audio_processing: AudioProcessingPolicy::default(),
            },
        ))
        .await
        .unwrap();
    let audio_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::OpenReceiveChannel { .. })
    })
    .await;
    let audio_token = audio_open
        .iter()
        .find_map(|message| match message {
            ServerMessage::OpenReceiveChannel {
                passthrough_party_id,
                ..
            } => Some(*passthrough_party_id),
            _ => None,
        })
        .unwrap();

    let first_descriptor = video_receive_descriptor(
        700,
        MediaEndpointAddress {
            address: "192.0.2.70".parse().unwrap(),
            port: 5070,
        },
    );
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenMultimediaReceiveChannel {
                call_id,
                descriptor: first_descriptor.clone(),
            },
        ))
        .await
        .unwrap();
    let first_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::OpenMultimediaChannel(_))
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::OpenMultimediaChannel(open) => Some(open),
        _ => None,
    })
    .unwrap();
    assert_eq!(first_open.payload.codec(), Codec::H264);
    assert_eq!(first_open.line_instance, 1);
    assert_eq!(first_open.call_reference, wire_reference);
    assert_eq!(first_open.payload, first_descriptor.payload);
    let first_token = first_open.passthrough_party_id;
    let endpoint = MediaEndpointAddress {
        address: "198.51.100.70".parse().unwrap(),
        port: 6070,
    };
    let wrong_call =
        ClientMessage::OpenMultimediaReceiveChannelAck(crate::OpenMultimediaReceiveChannelAck {
            status: MediaStatus::Ok,
            endpoint,
            passthrough_party_id: first_token,
            call_reference: CallReference::new(wire_reference.get() + 1),
        })
        .encode(protocol)
        .unwrap();
    let exact =
        ClientMessage::OpenMultimediaReceiveChannelAck(crate::OpenMultimediaReceiveChannelAck {
            status: MediaStatus::Ok,
            endpoint,
            passthrough_party_id: first_token,
            call_reference: wire_reference,
        })
        .encode(protocol)
        .unwrap();
    let mut coalesced_prefix = wrong_call;
    coalesced_prefix.extend(
        ClientMessage::OpenMultimediaReceiveChannelAck(crate::OpenMultimediaReceiveChannelAck {
            status: MediaStatus::Ok,
            endpoint: MediaEndpointAddress {
                address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: endpoint.port,
            },
            passthrough_party_id: first_token,
            call_reference: wire_reference,
        })
        .encode(protocol)
        .unwrap(),
    );
    coalesced_prefix.push(exact[0]);
    phone.write_all(&coalesced_prefix).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );
    for fragment in exact[1..].chunks(3) {
        phone.write_all(fragment).await.unwrap();
    }
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::MultimediaReceiveChannelOpened {
                call_id: actual_call,
                codec: Codec::H264,
                endpoint: actual_endpoint,
                passthrough_party_id,
            },
            ..
        })) if actual_call == call_id
            && actual_endpoint == endpoint
            && passthrough_party_id == first_token
    ));
    phone.write_all(&exact).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address: "198.51.100.71".parse().unwrap(),
                port: 6072,
                call_reference: wire_reference.get(),
                passthrough_party_id: audio_token,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::ReceiveChannelOpened {
                call_id: actual_call,
                status: MediaStatus::Ok,
                ..
            },
            ..
        })) if actual_call == call_id
    ));

    let replacement_descriptor = video_receive_descriptor(
        701,
        MediaEndpointAddress {
            address: "192.0.2.71".parse().unwrap(),
            port: 5072,
        },
    );
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenMultimediaReceiveChannel {
                call_id,
                descriptor: replacement_descriptor,
            },
        ))
        .await
        .unwrap();
    let replacement = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::OpenMultimediaChannel(open)
                if open.conference_id == ConferenceId::new(701)
        )
    })
    .await;
    let close_index = replacement
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseMultimediaReceiveChannel(control)
                    if control.passthrough_party_id == first_token
            )
        })
        .expect("replacement omitted the old video close");
    let (open_index, replacement_open) = replacement
        .iter()
        .enumerate()
        .find_map(|(index, message)| match message {
            ServerMessage::OpenMultimediaChannel(open)
                if open.conference_id == ConferenceId::new(701) =>
            {
                Some((index, open))
            }
            _ => None,
        })
        .unwrap();
    assert!(close_index < open_index);
    assert_ne!(replacement_open.passthrough_party_id, first_token);

    let negative =
        ClientMessage::OpenMultimediaReceiveChannelAck(crate::OpenMultimediaReceiveChannelAck {
            status: MediaStatus::OutOfChannels,
            endpoint,
            passthrough_party_id: replacement_open.passthrough_party_id,
            call_reference: wire_reference,
        })
        .encode(protocol)
        .unwrap();
    phone.write_all(&exact).await.unwrap();
    phone.write_all(&negative).await.unwrap();
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::CloseMultimediaReceiveChannel(control)
                if control.passthrough_party_id
                    == replacement_open.passthrough_party_id
        )
    })
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::MultimediaReceiveChannelFailed {
                call_id: actual_call,
                codec: Codec::H264,
                status: MediaStatus::OutOfChannels,
                endpoint: actual_endpoint,
                passthrough_party_id,
            },
            ..
        })) if actual_call == call_id
            && actual_endpoint == endpoint
            && passthrough_party_id == replacement_open.passthrough_party_id
    ));
    phone.write_all(&negative).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenMultimediaReceiveChannel {
                call_id,
                descriptor: video_receive_descriptor(
                    702,
                    MediaEndpointAddress {
                        address: "192.0.2.72".parse().unwrap(),
                        port: 5074,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let final_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::OpenMultimediaChannel(open)
                if open.conference_id == ConferenceId::new(702)
        )
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::OpenMultimediaChannel(open)
            if open.conference_id == ConferenceId::new(702) =>
        {
            Some(open)
        }
        _ => None,
    })
    .unwrap();
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::CloseCall { call_id },
        ))
        .await
        .unwrap();
    let close_messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::CallState {
                state: CallState::OnHook,
                ..
            }
        )
    })
    .await;
    let video_close = close_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseMultimediaReceiveChannel(control)
                    if control.passthrough_party_id
                        == final_open.passthrough_party_id
            )
        })
        .expect("call close omitted the video receive leg");
    let audio_close = close_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseReceiveChannel(control)
                    if control.passthrough_party_id.get() == audio_token
            )
        })
        .expect("call close omitted the independently opened audio receive leg");
    let on_hook = close_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CallState {
                    state: CallState::OnHook,
                    ..
                }
            )
        })
        .unwrap();
    assert!(video_close < audio_close && audio_close < on_hook);

    let reconfigured_call_id = CallId::new(73);
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id: reconfigured_call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id: reconfigured_call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenMultimediaReceiveChannel {
                call_id: reconfigured_call_id,
                descriptor: video_receive_descriptor(
                    705,
                    MediaEndpointAddress {
                        address: "192.0.2.75".parse().unwrap(),
                        port: 5080,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let reconfigure_open =
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::OpenMultimediaChannel(open)
                    if open.conference_id == ConferenceId::new(705)
            )
        })
        .await
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::OpenMultimediaChannel(open)
                if open.conference_id == ConferenceId::new(705) =>
            {
                Some(open)
            }
            _ => None,
        })
        .unwrap();
    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::StartMultimediaTransmission {
                call_id: reconfigured_call_id,
                descriptor: video_transmit_descriptor(
                    706,
                    MediaEndpointAddress {
                        address: "192.0.2.76".parse().unwrap(),
                        port: 5082,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let reconfigure_start =
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StartMultimediaTransmission(start)
                    if start.conference_id == ConferenceId::new(706)
            )
        })
        .await
        .into_iter()
        .find_map(|message| match message {
            ServerMessage::StartMultimediaTransmission(start)
                if start.conference_id == ConferenceId::new(706) =>
            {
                Some(start)
            }
            _ => None,
        })
        .unwrap();
    let mut replacement = definition();
    replacement.description = "replacement".into();
    handle.reconfigure([replacement]).await.unwrap();
    let reconfigure_messages =
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id
                        == reconfigure_start.passthrough_party_id
            )
        })
        .await;
    let receive_close = reconfigure_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseMultimediaReceiveChannel(control)
                    if control.passthrough_party_id
                        == reconfigure_open.passthrough_party_id
            )
        })
        .expect("reconfigure omitted the video receive close");
    let transmit_stop = reconfigure_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id
                        == reconfigure_start.passthrough_party_id
            )
        })
        .expect("reconfigure omitted the video transmit stop");
    assert!(receive_close < transmit_stop);
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Disconnected {},
            ..
        }))
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn video_transmit_session_correlates_frames_and_preserves_receive_and_audio() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let call_id = CallId::new(81);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    phone
        .write_all(&capability_update_bytes(
            protocol,
            Codec::Pcmu,
            Codec::H264,
            81,
        ))
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Capabilities { .. },
            ..
        }))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    let begin = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::SelectSoftKeys { .. })
    })
    .await;
    let call_reference = begin
        .iter()
        .find_map(|message| match message {
            ServerMessage::CallState { call_reference, .. } => {
                Some(CallReference::new(*call_reference))
            }
            _ => None,
        })
        .unwrap();
    assert!(matches!(
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::StartMultimediaTransmission {
                    call_id,
                    descriptor: video_transmit_descriptor(
                        809,
                        MediaEndpointAddress {
                            address: "192.0.2.89".parse().unwrap(),
                            port: 5088,
                        },
                    ),
                },
            ))
            .await,
        Err(ServerError::CommandWrite(_))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenReceiveChannel {
                call_id,
                purpose: ReceiveChannelPurpose::Media,
                source: None,
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 2,
                dtmf_mode: DtmfMode::Rfc2833,
                audio_processing: AudioProcessingPolicy::default(),
            },
        ))
        .await
        .unwrap();
    let audio_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::OpenReceiveChannel { .. })
    })
    .await;
    let audio_token = audio_open
        .iter()
        .find_map(|message| match message {
            ServerMessage::OpenReceiveChannel {
                passthrough_party_id,
                ..
            } => Some(*passthrough_party_id),
            _ => None,
        })
        .unwrap();

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenMultimediaReceiveChannel {
                call_id,
                descriptor: video_receive_descriptor(
                    810,
                    MediaEndpointAddress {
                        address: "192.0.2.81".parse().unwrap(),
                        port: 5082,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let receive_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::OpenMultimediaChannel(_))
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::OpenMultimediaChannel(open) => Some(open),
        _ => None,
    })
    .unwrap();

    let first_descriptor = video_transmit_descriptor(
        811,
        MediaEndpointAddress {
            address: "192.0.2.82".parse().unwrap(),
            port: 5084,
        },
    );
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMultimediaTransmission {
                call_id,
                descriptor: first_descriptor.clone(),
            },
        ))
        .await
        .unwrap();
    let first_start = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::StartMultimediaTransmission(_))
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::StartMultimediaTransmission(start) => Some(start),
        _ => None,
    })
    .unwrap();
    assert_eq!(first_start.call_reference, call_reference);
    assert_eq!(first_start.endpoint, first_descriptor.endpoint);
    assert_eq!(first_start.payload, first_descriptor.payload);
    let first_token = first_start.passthrough_party_id;
    assert!(matches!(
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::ControlMultimediaTransmission {
                    call_id,
                    passthrough_party_id: first_token,
                    control: MultimediaTransmitControl::FreezePicture,
                },
            ))
            .await,
        Err(ServerError::CommandWrite(_))
    ));
    let station_endpoint = MediaEndpointAddress {
        address: "198.51.100.81".parse().unwrap(),
        port: 6082,
    };
    let wrong_conference =
        ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
            conference_id: ConferenceId::new(812),
            passthrough_party_id: first_token,
            call_reference,
            endpoint: station_endpoint,
            status: MediaStatus::Ok,
        })
        .encode(protocol)
        .unwrap();
    let wrong_call =
        ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
            conference_id: first_start.conference_id,
            passthrough_party_id: first_token,
            call_reference: CallReference::new(call_reference.get() + 1),
            endpoint: station_endpoint,
            status: MediaStatus::Ok,
        })
        .encode(protocol)
        .unwrap();
    let exact =
        ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
            conference_id: first_start.conference_id,
            passthrough_party_id: first_token,
            call_reference,
            endpoint: station_endpoint,
            status: MediaStatus::Ok,
        })
        .encode(protocol)
        .unwrap();
    let mut coalesced = wrong_conference;
    coalesced.extend(wrong_call);
    coalesced.extend(
        ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
            conference_id: first_start.conference_id,
            passthrough_party_id: PassthroughPartyId::new(first_token.get() + 1),
            call_reference,
            endpoint: station_endpoint,
            status: MediaStatus::Ok,
        })
        .encode(protocol)
        .unwrap(),
    );
    coalesced.extend(
        ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
            conference_id: first_start.conference_id,
            passthrough_party_id: first_token,
            call_reference,
            endpoint: MediaEndpointAddress {
                address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: station_endpoint.port,
            },
            status: MediaStatus::Ok,
        })
        .encode(protocol)
        .unwrap(),
    );
    coalesced.push(exact[0]);
    phone.write_all(&coalesced).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );
    for fragment in exact[1..].chunks(3) {
        phone.write_all(fragment).await.unwrap();
    }
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::MultimediaTransmitStarted {
                call_id: actual_call,
                codec: Codec::H264,
                endpoint,
                passthrough_party_id,
            },
            ..
        })) if actual_call == call_id
            && endpoint == station_endpoint
            && passthrough_party_id == first_token
    ));
    phone.write_all(&exact).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    assert!(matches!(
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetMultimediaTransmitBitRate {
                    call_id,
                    passthrough_party_id: PassthroughPartyId::new(first_token.get() + 1),
                    maximum_bit_rate: 512_000,
                },
            ))
            .await,
        Err(ServerError::CommandWrite(_))
    ));
    for action in [
        CommandAction::SetMultimediaTransmitBitRate {
            call_id,
            passthrough_party_id: first_token,
            maximum_bit_rate: 512_000,
        },
        CommandAction::NotifyMultimediaTransmitBitRate {
            call_id,
            passthrough_party_id: first_token,
            maximum_bit_rate: 384_000,
        },
        CommandAction::ControlMultimediaTransmission {
            call_id,
            passthrough_party_id: first_token,
            control: MultimediaTransmitControl::FastPictureUpdate {
                first_gob: 4,
                gob_count: 2,
            },
        },
    ] {
        handle
            .send_confirmed(Command::new(device_id.clone(), action))
            .await
            .unwrap();
    }
    let control_messages =
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::MiscellaneousCommand(_))
        })
        .await;
    assert!(matches!(
        control_messages.as_slice(),
        [
            ServerMessage::FlowControlCommand(VideoFlowControl {
                conference_id,
                passthrough_party_id,
                call_reference: actual_call,
                maximum_bit_rate: 512_000,
            }),
            ServerMessage::FlowControlNotify(VideoFlowControl {
                conference_id: notify_conference,
                passthrough_party_id: notify_token,
                call_reference: notify_call,
                maximum_bit_rate: 384_000,
            }),
            ServerMessage::MiscellaneousCommand(MiscellaneousCommand {
                conference_id: command_conference,
                passthrough_party_id: command_token,
                call_reference: command_call,
                command: MiscCommandType::VideoFastUpdatePicture,
                data,
            }),
        ] if *conference_id == first_start.conference_id
            && *passthrough_party_id == first_token
            && *actual_call == call_reference
            && *notify_conference == first_start.conference_id
            && *notify_token == first_token
            && *notify_call == call_reference
            && *command_conference == first_start.conference_id
            && *command_token == first_token
            && *command_call == call_reference
            && data.as_bytes()[..8]
                == [4_u32.to_le_bytes(), 2_u32.to_le_bytes()].concat()
            && data.as_bytes()[8..].iter().all(|byte| *byte == 0)
    ));

    phone
        .write_all(
            &ClientMessage::OpenMultimediaReceiveChannelAck(
                crate::OpenMultimediaReceiveChannelAck {
                    status: MediaStatus::Ok,
                    endpoint: MediaEndpointAddress {
                        address: "198.51.100.82".parse().unwrap(),
                        port: 6084,
                    },
                    passthrough_party_id: receive_open.passthrough_party_id,
                    call_reference,
                },
            )
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::MultimediaReceiveChannelOpened {
                call_id: actual_call,
                ..
            },
            ..
        })) if actual_call == call_id
    ));
    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address: "198.51.100.83".parse().unwrap(),
                port: 6086,
                call_reference: call_reference.get(),
                passthrough_party_id: audio_token,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::ReceiveChannelOpened {
                call_id: actual_call,
                status: MediaStatus::Ok,
                ..
            },
            ..
        })) if actual_call == call_id
    ));

    let replacement_descriptor = video_transmit_descriptor(
        813,
        MediaEndpointAddress {
            address: "192.0.2.83".parse().unwrap(),
            port: 5086,
        },
    );
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMultimediaTransmission {
                call_id,
                descriptor: replacement_descriptor,
            },
        ))
        .await
        .unwrap();
    let replacement = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StartMultimediaTransmission(start)
                if start.conference_id == ConferenceId::new(813)
        )
    })
    .await;
    let stop_index = replacement
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id == first_token
            )
        })
        .expect("replacement omitted the old video transmit stop");
    let (start_index, replacement_start) = replacement
        .iter()
        .enumerate()
        .find_map(|(index, message)| match message {
            ServerMessage::StartMultimediaTransmission(start)
                if start.conference_id == ConferenceId::new(813) =>
            {
                Some((index, start))
            }
            _ => None,
        })
        .unwrap();
    assert!(stop_index < start_index);
    assert_ne!(replacement_start.passthrough_party_id, first_token);
    for passthrough_party_id in [first_token, replacement_start.passthrough_party_id] {
        assert!(matches!(
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::NotifyMultimediaTransmitBitRate {
                        call_id,
                        passthrough_party_id,
                        maximum_bit_rate: 256_000,
                    },
                ))
                .await,
            Err(ServerError::CommandWrite(_))
        ));
    }

    let negative =
        ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
            conference_id: replacement_start.conference_id,
            passthrough_party_id: replacement_start.passthrough_party_id,
            call_reference,
            endpoint: station_endpoint,
            status: MediaStatus::OutOfChannels,
        })
        .encode(protocol)
        .unwrap();
    phone.write_all(&exact).await.unwrap();
    phone.write_all(&negative).await.unwrap();
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMultimediaTransmission(control)
                if control.passthrough_party_id
                    == replacement_start.passthrough_party_id
        )
    })
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::MultimediaTransmitFailed {
                call_id: actual_call,
                codec: Codec::H264,
                status: MediaStatus::OutOfChannels,
                endpoint,
                passthrough_party_id,
            },
            ..
        })) if actual_call == call_id
            && endpoint == station_endpoint
            && passthrough_party_id == replacement_start.passthrough_party_id
    ));
    phone.write_all(&negative).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMultimediaTransmission {
                call_id,
                descriptor: video_transmit_descriptor(
                    814,
                    MediaEndpointAddress {
                        address: "192.0.2.84".parse().unwrap(),
                        port: 5088,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let stopped_start = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StartMultimediaTransmission(start)
                if start.conference_id == ConferenceId::new(814)
        )
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::StartMultimediaTransmission(start)
            if start.conference_id == ConferenceId::new(814) =>
        {
            Some(start)
        }
        _ => None,
    })
    .unwrap();
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StopMultimediaTransmission { call_id },
        ))
        .await
        .unwrap();
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMultimediaTransmission(control)
                if control.passthrough_party_id
                    == stopped_start.passthrough_party_id
        )
    })
    .await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StopMultimediaTransmission { call_id },
        ))
        .await
        .unwrap();
    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    let after_duplicate_stop =
        read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
            matches!(message, ServerMessage::KeepAliveAck)
        })
        .await;
    assert!(!after_duplicate_stop.iter().any(|message| {
        matches!(
            message,
            ServerMessage::StopMultimediaTransmission(control)
                if control.passthrough_party_id
                    == stopped_start.passthrough_party_id
        )
    }));

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMultimediaTransmission {
                call_id,
                descriptor: video_transmit_descriptor(
                    815,
                    MediaEndpointAddress {
                        address: "192.0.2.85".parse().unwrap(),
                        port: 5090,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let final_start = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StartMultimediaTransmission(start)
                if start.conference_id == ConferenceId::new(815)
        )
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::StartMultimediaTransmission(start)
            if start.conference_id == ConferenceId::new(815) =>
        {
            Some(start)
        }
        _ => None,
    })
    .unwrap();
    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::CloseCall { call_id },
        ))
        .await
        .unwrap();
    let close_messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::CallState {
                state: CallState::OnHook,
                ..
            }
        )
    })
    .await;
    let video_receive_close = close_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseMultimediaReceiveChannel(control)
                    if control.passthrough_party_id
                        == receive_open.passthrough_party_id
            )
        })
        .unwrap();
    let video_transmit_stop = close_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMultimediaTransmission(control)
                    if control.passthrough_party_id
                        == final_start.passthrough_party_id
            )
        })
        .unwrap();
    let audio_close = close_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseReceiveChannel(control)
                    if control.passthrough_party_id.get() == audio_token
            )
        })
        .unwrap();
    let on_hook = close_messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CallState {
                    state: CallState::OnHook,
                    ..
                }
            )
        })
        .unwrap();
    assert!(
        video_receive_close < video_transmit_stop
            && video_transmit_stop < audio_close
            && audio_close < on_hook
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn video_receive_deadline_closes_and_retires_the_exact_generation() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) = Server::with_ingress(config, [device]).unwrap();
    let task = tokio::spawn(server.run());
    let (server_stream, mut phone) = tokio::io::duplex(8_192);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_071)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let call_id = CallId::new(72);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    phone
        .write_all(&capability_update_bytes(
            protocol,
            Codec::Pcmu,
            Codec::H264,
            72,
        ))
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Capabilities { .. },
            ..
        }))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenMultimediaReceiveChannel {
                call_id,
                descriptor: video_receive_descriptor(
                    703,
                    MediaEndpointAddress {
                        address: "192.0.2.73".parse().unwrap(),
                        port: 5076,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::OpenMultimediaChannel(_))
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::OpenMultimediaChannel(open) => Some(open),
        _ => None,
    })
    .unwrap();

    tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::CloseMultimediaReceiveChannel(control)
                if control.passthrough_party_id == open.passthrough_party_id
        )
    })
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::MultimediaReceiveChannelTimedOut {
                call_id: actual_call,
                codec: Codec::H264,
                passthrough_party_id,
            },
            ..
        })) if actual_call == call_id
            && passthrough_party_id == open.passthrough_party_id
    ));

    phone
        .write_all(
            &ClientMessage::OpenMultimediaReceiveChannelAck(
                crate::OpenMultimediaReceiveChannelAck {
                    status: MediaStatus::Ok,
                    endpoint: MediaEndpointAddress {
                        address: "198.51.100.73".parse().unwrap(),
                        port: 6076,
                    },
                    passthrough_party_id: open.passthrough_party_id,
                    call_reference: open.call_reference,
                },
            )
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );
    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;

    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::OpenMultimediaReceiveChannel {
                call_id,
                descriptor: video_receive_descriptor(
                    704,
                    MediaEndpointAddress {
                        address: "192.0.2.74".parse().unwrap(),
                        port: 5078,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let shutdown_open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::OpenMultimediaChannel(open)
                if open.conference_id == ConferenceId::new(704)
        )
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::OpenMultimediaChannel(open)
            if open.conference_id == ConferenceId::new(704) =>
        {
            Some(open)
        }
        _ => None,
    })
    .unwrap();
    handle.shutdown().await.unwrap();
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::CloseMultimediaReceiveChannel(control)
                if control.passthrough_party_id == shutdown_open.passthrough_party_id
        )
    })
    .await;
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn video_transmit_deadline_stops_and_retires_the_exact_generation() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) = Server::with_ingress(config, [device]).unwrap();
    let task = tokio::spawn(server.run());
    let (server_stream, mut phone) = tokio::io::duplex(8_192);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_081)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let call_id = CallId::new(82);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    phone
        .write_all(&capability_update_bytes(
            protocol,
            Codec::Pcmu,
            Codec::H264,
            82,
        ))
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Capabilities { .. },
            ..
        }))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMultimediaTransmission {
                call_id,
                descriptor: video_transmit_descriptor(
                    820,
                    MediaEndpointAddress {
                        address: "192.0.2.82".parse().unwrap(),
                        port: 5090,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let start = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::StartMultimediaTransmission(_))
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::StartMultimediaTransmission(start) => Some(start),
        _ => None,
    })
    .unwrap();

    tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMultimediaTransmission(control)
                if control.passthrough_party_id == start.passthrough_party_id
        )
    })
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::MultimediaTransmitTimedOut {
                call_id: actual_call,
                codec: Codec::H264,
                passthrough_party_id,
            },
            ..
        })) if actual_call == call_id
            && passthrough_party_id == start.passthrough_party_id
    ));
    phone
        .write_all(
            &ClientMessage::StartMultimediaTransmissionAck(crate::StartMultimediaTransmissionAck {
                conference_id: start.conference_id,
                passthrough_party_id: start.passthrough_party_id,
                call_reference: start.call_reference,
                endpoint: MediaEndpointAddress {
                    address: "198.51.100.82".parse().unwrap(),
                    port: 6090,
                },
                status: MediaStatus::Ok,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::StartMultimediaTransmission {
                call_id,
                descriptor: video_transmit_descriptor(
                    821,
                    MediaEndpointAddress {
                        address: "192.0.2.83".parse().unwrap(),
                        port: 5092,
                    },
                ),
            },
        ))
        .await
        .unwrap();
    let shutdown_start = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StartMultimediaTransmission(start)
                if start.conference_id == ConferenceId::new(821)
        )
    })
    .await
    .into_iter()
    .find_map(|message| match message {
        ServerMessage::StartMultimediaTransmission(start)
            if start.conference_id == ConferenceId::new(821) =>
        {
            Some(start)
        }
        _ => None,
    })
    .unwrap();
    handle.shutdown().await.unwrap();
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMultimediaTransmission(control)
                if control.passthrough_party_id
                    == shutdown_start.passthrough_party_id
        )
    })
    .await;
    task.await.unwrap().unwrap();
}

#[test]
fn multicast_admission_requires_a_routable_supported_audio_shape() {
    let state = multicast_test_state(ProtocolVersion::V22);
    let valid = multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcmu);
    assert!(validate_multicast_route(&state, valid).is_ok());

    for route in [
        MulticastMediaRoute {
            address: "192.0.2.1".parse().unwrap(),
            ..valid
        },
        MulticastMediaRoute { port: 0, ..valid },
        MulticastMediaRoute {
            packet_millis: 0,
            ..valid
        },
    ] {
        assert!(matches!(
            validate_multicast_route(&state, route),
            Err(ServerError::InvalidMulticastMedia(_))
        ));
    }
    assert!(matches!(
        validate_multicast_route(
            &state,
            multicast_route("239.1.2.3".parse().unwrap(), Codec::H264),
        ),
        Err(ServerError::UnsupportedMulticastCodec)
    ));
    assert!(matches!(
        validate_multicast_route(
            &state,
            multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcma),
        ),
        Err(ServerError::UnsupportedMulticastCodec)
    ));
    assert!(matches!(
        validate_multicast_route(
            &state,
            MulticastMediaRoute {
                packet_millis: 60,
                ..valid
            },
        ),
        Err(ServerError::InvalidMulticastMedia(_))
    ));

    let legacy = multicast_test_state(ProtocolVersion::V16);
    assert!(matches!(
        validate_multicast_route(
            &legacy,
            multicast_route("ff15::1".parse().unwrap(), Codec::Pcmu),
        ),
        Err(ServerError::InvalidMulticastMedia(_))
    ));
    assert!(
        validate_multicast_route(
            &state,
            multicast_route("ff15::1".parse().unwrap(), Codec::Pcmu),
        )
        .is_ok()
    );
}

#[test]
fn multicast_transactions_correlate_exactly_and_retire_once_in_wire_order() {
    let now = Instant::now();
    let mut state = multicast_test_state(ProtocolVersion::V22);
    let call = insert_call(&mut state, CallId(10), 1, Codec::Pcmu, CallState::Connected);
    let key = MulticastKey {
        conference_id: ConferenceId::new(90),
        call_id: call.call_id,
    };
    let receive_request =
        MediaRequestIdentity::new(1, MediaRequestToken::new(101).expect("nonzero media token"))
            .expect("nonzero generation");
    let transmit_request =
        MediaRequestIdentity::new(2, MediaRequestToken::new(102).expect("nonzero media token"))
            .expect("nonzero generation");
    let route = multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcmu);
    state.multicast.insert(
        key,
        MulticastSession {
            wire_call_reference: call.wire_reference,
            receive: Some(MulticastReceive {
                request: receive_request,
                route,
                state: MulticastReceiveState::AwaitingAcknowledgement { deadline: now },
            }),
            transmit: Some(MulticastTransmit {
                request: transmit_request,
                route,
            }),
        },
    );

    assert_eq!(
        find_multicast_receive_key(&state, call.wire_reference, 100),
        None
    );
    assert_eq!(
        find_multicast_receive_key(&state, call.wire_reference + 1, 101),
        None
    );
    assert_eq!(
        find_multicast_receive_key(&state, call.wire_reference, 101),
        Some(key)
    );
    assert_eq!(
        find_multicast_transmit_key(
            &state,
            90,
            call.wire_reference,
            102,
            route.address,
            route.port + 1,
        ),
        None
    );
    assert_eq!(
        find_multicast_transmit_key(
            &state,
            90,
            call.wire_reference,
            102,
            route.address,
            route.port,
        ),
        Some(key)
    );

    let expired = expire_multicast_reception_acknowledgements(&mut state, now);
    assert!(matches!(
        expired.as_slice(),
        [(
            actual_key,
            ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
        )] if *actual_key == key && passthrough_party_id.get() == 101
    ));
    assert!(expire_multicast_reception_acknowledgements(&mut state, now).is_empty());

    let other_call = insert_call(&mut state, CallId(20), 1, Codec::Pcmu, CallState::Connected);
    let other_key = MulticastKey {
        conference_id: ConferenceId::new(91),
        call_id: other_call.call_id,
    };
    state.multicast.insert(
        other_key,
        MulticastSession {
            wire_call_reference: other_call.wire_reference,
            receive: Some(MulticastReceive {
                request: MediaRequestIdentity::new(
                    3,
                    MediaRequestToken::new(103).expect("nonzero media token"),
                )
                .expect("nonzero generation"),
                route,
                state: MulticastReceiveState::Open,
            }),
            transmit: None,
        },
    );

    let remaining = take_multicast_stops_for_call(&mut state, call.call_id);
    assert!(matches!(
        remaining.as_slice(),
        [ServerMessage::StopMulticastMediaTransmission { passthrough_party_id, .. }]
            if passthrough_party_id.get() == 102
    ));
    assert!(take_multicast_stops_for_call(&mut state, call.call_id).is_empty());
    assert!(state.multicast.contains_key(&other_key));
    let shutdown_stops = take_all_multicast_stops(&mut state);
    assert!(matches!(
        shutdown_stops.as_slice(),
        [ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }]
            if passthrough_party_id.get() == 103
    ));
    assert!(take_all_multicast_stops(&mut state).is_empty());
    assert!(state.multicast.is_empty());
}

#[tokio::test]
async fn multicast_session_enforces_transaction_identity_order_and_teardown() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let call_id = CallId(41);
    let conference_id = ConferenceId::new(900);
    let first_route = multicast_route("239.1.2.3".parse().unwrap(), Codec::Pcmu);
    let second_route = MulticastMediaRoute {
        address: "239.1.2.4".parse().unwrap(),
        port: 5006,
        ..first_route
    };

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    phone
        .write_all(
            &ClientMessage::CapabilitiesResponse(vec![MediaCapability {
                codec: Codec::Pcmu,
                max_packet_ms: 40,
                codec_parameters: [0; 8],
            }])
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Capabilities { .. },
            ..
        }))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::SelectSoftKeys { .. })
    })
    .await;
    let wire_call_reference = messages
        .iter()
        .find_map(|message| match message {
            ServerMessage::CallState {
                state: CallState::OffHook,
                call_reference,
                ..
            } => Some(*call_reference),
            _ => None,
        })
        .expect("begin call omitted its wire reference");

    for invalid_route in [
        MulticastMediaRoute {
            address: "192.0.2.1".parse().unwrap(),
            ..first_route
        },
        MulticastMediaRoute {
            codec: Codec::Pcma,
            ..first_route
        },
    ] {
        assert!(matches!(
            handle
                .send_confirmed(Command::new(
                    device_id.clone(),
                    CommandAction::StartMulticastReception {
                        conference_id,
                        call_id,
                        route: invalid_route,
                        echo_cancellation: EchoCancellation::On,
                        g723_bitrate: G723BitRate::Rate5_3,
                    },
                ))
                .await,
            Err(ServerError::CommandWrite(_))
        ));
    }
    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMulticastReception {
                conference_id,
                call_id,
                route: first_route,
                echo_cancellation: EchoCancellation::On,
                g723_bitrate: G723BitRate::Rate5_3,
            },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::StartMulticastMediaReception(_))
    })
    .await;
    let first_receive_token = messages
        .iter()
        .find_map(|message| match message {
            ServerMessage::StartMulticastMediaReception(request) => {
                assert_eq!(request.conference_id, conference_id);
                assert_eq!(request.call_reference.get(), wire_call_reference);
                assert_eq!(request.address, first_route.address);
                assert_eq!(request.port, first_route.port);
                assert_eq!(request.codec, first_route.codec);
                Some(request.passthrough_party_id)
            }
            _ => None,
        })
        .expect("multicast reception omitted its request");

    let mismatched = ClientMessage::MulticastMediaReceptionAck {
        status: MediaStatus::Ok,
        passthrough_party_id: first_receive_token,
        call_reference: CallReference::new(wire_call_reference + 1),
    }
    .encode(protocol)
    .unwrap();
    let exact = ClientMessage::MulticastMediaReceptionAck {
        status: MediaStatus::Ok,
        passthrough_party_id: first_receive_token,
        call_reference: CallReference::new(wire_call_reference),
    }
    .encode(protocol)
    .unwrap();
    let mut coalesced_prefix = mismatched;
    coalesced_prefix.push(exact[0]);
    phone.write_all(&coalesced_prefix).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "a mismatched acknowledgement completed the transaction"
    );
    for fragment in exact[1..].chunks(2) {
        phone.write_all(fragment).await.unwrap();
    }
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _,
            event: DeviceEventKind::MulticastReceptionStarted {
                conference_id: actual_conference,
                call_id: actual_call,
                route,
            },
            ..
        })) if actual_conference == conference_id && actual_call == call_id && route == first_route
    ));
    phone.write_all(&exact).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "a duplicate acknowledgement emitted another event"
    );

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMulticastReception {
                conference_id,
                call_id,
                route: second_route,
                echo_cancellation: EchoCancellation::Off,
                g723_bitrate: G723BitRate::Rate6_3,
            },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StartMulticastMediaReception(request)
                if request.address == second_route.address
        )
    })
    .await;
    let stop_index = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
                    if *passthrough_party_id == first_receive_token
            )
        })
        .expect("replacement did not stop the previous generation");
    let (start_index, second_receive_token) = messages
        .iter()
        .enumerate()
        .find_map(|(index, message)| match message {
            ServerMessage::StartMulticastMediaReception(request)
                if request.address == second_route.address =>
            {
                Some((index, request.passthrough_party_id))
            }
            _ => None,
        })
        .expect("replacement did not start a fresh generation");
    assert!(stop_index < start_index);
    assert_ne!(first_receive_token, second_receive_token);

    let negative = ClientMessage::MulticastMediaReceptionAck {
        status: MediaStatus::OutOfChannels,
        passthrough_party_id: second_receive_token,
        call_reference: CallReference::new(wire_call_reference),
    }
    .encode(protocol)
    .unwrap();
    phone.write_all(&negative).await.unwrap();
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
                if *passthrough_party_id == second_receive_token
        )
    })
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _,
            event: DeviceEventKind::MulticastReceptionFailed {
                conference_id: actual_conference,
                call_id: actual_call,
                status: MediaStatus::OutOfChannels,
            },
            ..
        })) if actual_conference == conference_id && actual_call == call_id
    ));
    phone.write_all(&negative).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "a duplicate failure acknowledgement emitted another event"
    );

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMulticastTransmission {
                conference_id,
                call_id,
                route: first_route,
                precedence: 0,
                silence_suppression: SilenceSuppression::Off,
                max_frames_per_packet: 2,
                g723_bitrate: G723BitRate::Rate5_3,
            },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::StartMulticastMediaTransmission(_))
    })
    .await;
    let transmit_token = messages
        .iter()
        .find_map(|message| match message {
            ServerMessage::StartMulticastMediaTransmission(request) => {
                Some(request.passthrough_party_id.get())
            }
            _ => None,
        })
        .expect("multicast transmission omitted its request");
    let started_event = events.recv().await;
    assert!(matches!(
        started_event,
        Some(Event::Device(DeviceEvent { session_generation: _,
            event: DeviceEventKind::MulticastTransmissionStarted {
                conference_id: actual_conference,
                call_id: actual_call,
                route,
            },
            ..
        })) if actual_conference == conference_id && actual_call == call_id && route == first_route
    ));
    let mismatch_failure = ClientMessage::MediaTransmissionFailure {
        conference_id: conference_id.get(),
        passthrough_party_id: transmit_token,
        address: first_route.address,
        port: first_route.port + 1,
        call_reference: wire_call_reference,
        status: MediaStatus::UnspecifiedError,
    }
    .encode(protocol)
    .unwrap();
    phone.write_all(&mismatch_failure).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "a mismatched transmission failure retired the transaction"
    );
    let exact_failure = ClientMessage::MediaTransmissionFailure {
        conference_id: conference_id.get(),
        passthrough_party_id: transmit_token,
        address: first_route.address,
        port: first_route.port,
        call_reference: wire_call_reference,
        status: MediaStatus::UnspecifiedError,
    }
    .encode(protocol)
    .unwrap();
    phone.write_all(&exact_failure).await.unwrap();
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMulticastMediaTransmission { passthrough_party_id, .. }
                if passthrough_party_id.get() == transmit_token
        )
    })
    .await;
    let failure_event = events.recv().await;
    assert!(
        matches!(
            failure_event,
            Some(Event::Device(DeviceEvent { session_generation: _,
                event: DeviceEventKind::MulticastTransmissionFailed {
                    conference_id: actual_conference,
                    call_id: actual_call,
                    status: MediaStatus::UnspecifiedError,
                    ..
                },
                ..
            })) if actual_conference == conference_id && actual_call == call_id
        ),
        "unexpected multicast transmission failure event: {failure_event:?}"
    );
    phone.write_all(&exact_failure).await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "a duplicate transmission failure emitted another event"
    );

    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMulticastReception {
                conference_id,
                call_id,
                route: first_route,
                echo_cancellation: EchoCancellation::On,
                g723_bitrate: G723BitRate::Rate5_3,
            },
        ))
        .await
        .unwrap();
    read_until_message(
        &mut phone,
        &mut decoder,
        wire_id::START_MULTICAST_MEDIA_RECEPTION,
    )
    .await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartMulticastTransmission {
                conference_id,
                call_id,
                route: first_route,
                precedence: 0,
                silence_suppression: SilenceSuppression::Off,
                max_frames_per_packet: 2,
                g723_bitrate: G723BitRate::Rate5_3,
            },
        ))
        .await
        .unwrap();
    read_until_message(
        &mut phone,
        &mut decoder,
        wire_id::START_MULTICAST_MEDIA_TRANSMISSION,
    )
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::MulticastTransmissionStarted { .. },
            ..
        }))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::CloseCall { call_id },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::CallState {
                state: CallState::OnHook,
                ..
            }
        )
    })
    .await;
    let receive_stop = messages
        .iter()
        .position(|message| matches!(message, ServerMessage::StopMulticastMediaReception { .. }))
        .expect("call close omitted multicast reception stop");
    let transmit_stop = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMulticastMediaTransmission { .. }
            )
        })
        .expect("call close omitted multicast transmission stop");
    let on_hook = messages
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CallState {
                    state: CallState::OnHook,
                    ..
                }
            )
        })
        .unwrap();
    assert!(receive_stop < transmit_stop && transmit_stop < on_hook);

    let disconnect_call = CallId(42);
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id: disconnect_call,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    for action in [
        CommandAction::StartMulticastReception {
            conference_id,
            call_id: disconnect_call,
            route: first_route,
            echo_cancellation: EchoCancellation::On,
            g723_bitrate: G723BitRate::Rate5_3,
        },
        CommandAction::StartMulticastTransmission {
            conference_id,
            call_id: disconnect_call,
            route: first_route,
            precedence: 0,
            silence_suppression: SilenceSuppression::Off,
            max_frames_per_packet: 2,
            g723_bitrate: G723BitRate::Rate5_3,
        },
    ] {
        handle
            .send_confirmed(Command::new(device_id.clone(), action))
            .await
            .unwrap();
    }
    read_until_message(
        &mut phone,
        &mut decoder,
        wire_id::START_MULTICAST_MEDIA_TRANSMISSION,
    )
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _,
            event: DeviceEventKind::MulticastTransmissionStarted {
                call_id: actual_call,
                ..
            },
            ..
        })) if actual_call == disconnect_call
    ));

    let mut replacement = definition();
    replacement.description = "replacement".into();
    handle.reconfigure([replacement]).await.unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMulticastMediaTransmission { .. }
        )
    })
    .await;
    let receive_stops = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            matches!(message, ServerMessage::StopMulticastMediaReception { .. }).then_some(index)
        })
        .collect::<Vec<_>>();
    let transmit_stops = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| {
            matches!(
                message,
                ServerMessage::StopMulticastMediaTransmission { .. }
            )
            .then_some(index)
        })
        .collect::<Vec<_>>();
    assert_eq!(receive_stops.len(), 1);
    assert_eq!(transmit_stops.len(), 1);
    assert!(receive_stops[0] < transmit_stops[0]);
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Disconnected {},
            ..
        }))
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn multicast_receive_deadline_stops_and_retires_the_pending_generation() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) = Server::with_ingress(config, [device]).unwrap();
    let task = tokio::spawn(server.run());
    let (server_stream, mut phone) = tokio::io::duplex(8_192);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_000)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let call_id = CallId(43);
    let conference_id = ConferenceId::new(901);
    let route = multicast_route("239.1.2.5".parse().unwrap(), Codec::Pcmu);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    phone
        .write_all(
            &ClientMessage::CapabilitiesResponse(vec![MediaCapability {
                codec: Codec::Pcmu,
                max_packet_ms: 20,
                codec_parameters: [0; 8],
            }])
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Capabilities { .. },
            ..
        }))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::StartMulticastReception {
                conference_id,
                call_id,
                route,
                echo_cancellation: EchoCancellation::On,
                g723_bitrate: G723BitRate::Rate5_3,
            },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::StartMulticastMediaReception(_))
    })
    .await;
    let request = messages
        .iter()
        .find_map(|message| match message {
            ServerMessage::StartMulticastMediaReception(request) => Some(request.clone()),
            _ => None,
        })
        .unwrap();

    tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
    read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::StopMulticastMediaReception { passthrough_party_id, .. }
                if *passthrough_party_id == request.passthrough_party_id
        )
    })
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _,
            event: DeviceEventKind::MulticastReceptionTimedOut {
                conference_id: actual_conference,
                call_id: actual_call,
            },
            ..
        })) if actual_conference == conference_id && actual_call == call_id
    ));
    phone
        .write_all(
            &ClientMessage::MulticastMediaReceptionAck {
                status: MediaStatus::Ok,
                passthrough_party_id: request.passthrough_party_id,
                call_reference: request.call_reference,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "a late acknowledgement resurrected the expired generation"
    );
    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn outbound_media_writes_receive_then_transmit_without_an_ack_boundary() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::Line,
                instance: 1,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { .. }
        }))
    ));

    phone
        .write_all(
            &ClientMessage::KeypadButton {
                button: Digit::Number(2),
                line_instance: 1,
                call_reference: 1,
                wire_layout: None,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Digit { .. }
        }))
    ));
    phone
        .write_all(
            &ClientMessage::KeypadButton {
                button: Digit::Pound,
                line_instance: 1,
                call_reference: 1,
                wire_layout: None,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Digit {
                digit: Digit::Pound,
                ..
            }
        }))
    ));
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::CommitOutboundCall {
                call_id: CallId(1),
                info: CallInfo {
                    direction: crate::CallDirection::Outbound,
                    called_number: "2".into(),
                    ..CallInfo::default()
                },
            },
        ))
        .await
        .unwrap();
    let prefix = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    let stop_tone = prefix
        .iter()
        .position(|frame| frame.message_id == wire_id::STOP_TONE)
        .expect("outbound route prefix omitted StopTone");
    let call_info = prefix
        .iter()
        .position(|frame| frame.message_id == wire_id::CALL_INFO_DYNAMIC)
        .expect("outbound route prefix omitted CallInfo");
    let dialed_number = prefix
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "2"
            )
        })
        .expect("outbound route prefix omitted DialedNumber");
    let proceed = prefix
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::CallState {
                    state: CallState::Proceed,
                    ..
                })
            )
        })
        .expect("outbound media prefix omitted Proceed");
    assert!(stop_tone < call_info && call_info < dialed_number && dialed_number < proceed);
    assert!(prefix[..proceed].iter().all(|frame| {
        !matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::OffHook,
                ..
            })
        ) && frame.message_id != wire_id::ACTIVATE_CALL_PLANE
    }));

    let outbound_info = CallInfo {
        direction: crate::CallDirection::Outbound,
        called_name: "Remote Party".into(),
        called_number: "2".into(),
        ..CallInfo::default()
    };
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::PresentOutboundProceeding {
                call_id: CallId(1),
                info: outbound_info.clone(),
            },
        ))
        .await
        .unwrap();
    let proceeding = read_until_message(
        &mut phone,
        &mut decoder,
        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
    )
    .await;
    let proceeding_ids = proceeding
        .iter()
        .map(|frame| frame.message_id)
        .collect::<Vec<_>>();
    let stop = proceeding_ids
        .iter()
        .position(|message_id| *message_id == wire_id::STOP_TONE)
        .unwrap();
    let state = proceeding_ids
        .iter()
        .position(|message_id| *message_id == wire_id::CALL_STATE)
        .unwrap();
    let info = proceeding_ids
        .iter()
        .position(|message_id| *message_id == wire_id::CALL_INFO_DYNAMIC)
        .unwrap();
    let prompt = proceeding_ids
        .iter()
        .position(|message_id| *message_id == wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS)
        .unwrap();
    assert!(stop < state && state < info && info < prompt);

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::PresentOutboundRinging {
                call_id: CallId(1),
                info: outbound_info,
            },
        ))
        .await
        .unwrap();
    let ringing = read_until_message(&mut phone, &mut decoder, wire_id::CALL_INFO_DYNAMIC).await;
    let ringing_ids = ringing
        .iter()
        .map(|frame| frame.message_id)
        .collect::<Vec<_>>();
    let state = ringing_ids
        .iter()
        .position(|message_id| *message_id == wire_id::CALL_STATE)
        .unwrap();
    let prompt = ringing_ids
        .iter()
        .position(|message_id| *message_id == wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS)
        .unwrap();
    let stop = ringing_ids
        .iter()
        .position(|message_id| *message_id == wire_id::STOP_TONE)
        .unwrap();
    let tone = ringing_ids
        .iter()
        .position(|message_id| *message_id == wire_id::START_TONE)
        .unwrap();
    let keys = ringing_ids
        .iter()
        .position(|message_id| *message_id == wire_id::SELECT_SOFT_KEYS)
        .unwrap();
    let info = ringing_ids
        .iter()
        .position(|message_id| *message_id == wire_id::CALL_INFO_DYNAMIC)
        .unwrap();
    assert!(state < prompt && prompt < stop && stop < tone && tone < keys && keys < info);
    assert!(matches!(
        ServerMessage::decode(ringing[stop].clone(), protocol),
        Ok(ServerMessage::StopTone {
            line_instance: 1,
            call_reference: 1,
        })
    ));
    assert!(matches!(
        ServerMessage::decode(ringing[tone].clone(), protocol),
        Ok(ServerMessage::StartTone {
            tone: Tone::Alerting,
            direction: ToneDirection::User,
            line_instance: 1,
            call_reference: 1,
        })
    ));
    assert_eq!(
        ringing
            .iter()
            .filter(|frame| frame.message_id == wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS)
            .count(),
        1,
        "outbound ringing flashed an intermediate prompt"
    );

    let endpoint = MediaEndpoint {
        address: "198.51.100.20".parse().unwrap(),
        rtp_port: 6000,
        rtcp_port: 6001,
        codec: Codec::Pcma,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 0,
    };
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenOutboundMedia {
                call_id: CallId(1),
                source: None,
                endpoint,
                codec: Codec::Pcma,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::START_MEDIA_TRANSMISSION).await;
    let receive = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::OPEN_RECEIVE_CHANNEL)
        .expect("coupled transaction omitted OpenReceiveChannel");
    let transmit = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::START_MEDIA_TRANSMISSION)
        .expect("coupled transaction omitted StartMediaTransmission");
    let first_request_party = coupled_media_request_party(&frames, protocol);
    assert_eq!(transmit, receive + 1);
    assert!(matches!(
        ServerMessage::decode(frames[receive].clone(), protocol).unwrap(),
        ServerMessage::OpenReceiveChannel {
            source_address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            source_port: 0,
            codec: Codec::Pcma,
            ..
        }
    ));
    assert!(matches!(
        ServerMessage::decode(frames[transmit].clone(), protocol).unwrap(),
        ServerMessage::StartMediaTransmission {
            endpoint: actual,
            ..
        } if actual.address == endpoint.address
            && actual.rtp_port == endpoint.rtp_port
            && actual.codec == endpoint.codec
    ));

    let receive_peer = MediaEndpoint {
        address: "192.0.2.44".parse().unwrap(),
        rtp_port: 4000,
        rtcp_port: 4001,
        codec: Codec::Pcma,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 0,
    };
    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address: receive_peer.address,
                port: receive_peer.rtp_port,
                call_reference: 1,
                passthrough_party_id: first_request_party,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::ReceiveChannelOpened {
            call_id: CallId(1),
            status: MediaStatus::Ok,
            endpoint: actual,
            ..
        } })) if actual == receive_peer
    ));
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelOpen {
            call_id: CallId(1),
            outcome: TransmitOpenOutcome::Implied,
            endpoint: actual,
            ..
        } })) if actual == endpoint
    ));

    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: first_request_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: endpoint.address,
                port: endpoint.rtp_port,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "late explicit transmit acknowledgement re-settled coupled media"
    );

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::OpenOutboundMedia {
                call_id: CallId(1),
                source: None,
                endpoint,
                codec: Codec::Pcma,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::START_MEDIA_TRANSMISSION).await;
    let second_request_party = coupled_media_request_party(&frames, protocol);
    assert_ne!(second_request_party, first_request_party);
    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: first_request_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: endpoint.address,
                port: endpoint.rtp_port,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "a prior media generation settled the reopened transmit request"
    );
    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: second_request_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: endpoint.address,
                port: endpoint.rtp_port,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::TransmitChannelOpen {
                call_id: CallId(1),
                outcome: TransmitOpenOutcome::Acknowledged,
                ..
            }
        }))
    ));
    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address: receive_peer.address,
                port: receive_peer.rtp_port,
                call_reference: 1,
                passthrough_party_id: second_request_party,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::ReceiveChannelOpened {
                call_id: CallId(1),
                status: MediaStatus::Ok,
                ..
            }
        }))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "receive acknowledgement duplicated an explicitly settled transmit event"
    );

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::OpenOutboundMedia {
                call_id: CallId(1),
                source: None,
                endpoint,
                codec: Codec::Pcma,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::START_MEDIA_TRANSMISSION).await;
    let third_request_party = coupled_media_request_party(&frames, protocol);
    assert_ne!(third_request_party, second_request_party);
    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::UnspecifiedError,
                address: receive_peer.address,
                port: receive_peer.rtp_port,
                call_reference: 1,
                passthrough_party_id: third_request_party,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let rollback = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::CloseReceiveChannel(_))
    })
    .await;
    let stop = rollback
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMediaTransmission(control)
                    if control.passthrough_party_id.get() == third_request_party
            )
        })
        .unwrap();
    let close = rollback
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseReceiveChannel(control)
                    if control.passthrough_party_id.get() == third_request_party
            )
        })
        .unwrap();
    assert!(stop < close);
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::ReceiveChannelOpened {
                call_id: CallId(1),
                status: MediaStatus::UnspecifiedError,
                ..
            }
        }))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "failed coupled receive emitted a transmit-success event"
    );
    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: third_request_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: endpoint.address,
                port: endpoint.rtp_port,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "late transmit acknowledgement resurrected a failed coupled transaction"
    );

    handle
        .send(Command::new(
            device_id,
            CommandAction::OpenOutboundMedia {
                call_id: CallId(1),
                source: None,
                endpoint,
                codec: Codec::Pcma,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::START_MEDIA_TRANSMISSION).await;
    let fourth_request_party = coupled_media_request_party(&frames, protocol);
    assert_ne!(fourth_request_party, third_request_party);
    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: fourth_request_party,
                call_reference: 1,
                status: MediaStatus::UnspecifiedError,
                address: endpoint.address,
                port: endpoint.rtp_port,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::TransmitChannelOpen {
                call_id: CallId(1),
                outcome: TransmitOpenOutcome::Rejected(MediaStatus::UnspecifiedError),
                ..
            }
        }))
    ));
    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address: receive_peer.address,
                port: receive_peer.rtp_port,
                call_reference: 1,
                passthrough_party_id: fourth_request_party,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "late receive acknowledgement resurrected a failed coupled transaction"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn receive_acknowledgement_timeout_preserves_a_silent_session() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) = Server::with_ingress(config, [device]).unwrap();
    let task = tokio::spawn(server.run());
    let (server_stream, mut phone) = tokio::io::duplex(8_192);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_090)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    let protocol = ProtocolVersion::V22;
    let call_id = CallId::new(90);
    let mut decoder = FrameDecoder::new();
    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    let generation = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Registered(_),
            ..
        })) => session_generation,
        event => panic!("expected registration, got {event:?}"),
    };
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id,
                state: CallState::Proceed,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    let endpoint = MediaEndpoint {
        address: "198.51.100.90".parse().unwrap(),
        rtp_port: 6090,
        rtcp_port: 6091,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 0,
    };
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenOutboundMedia {
                call_id,
                source: None,
                endpoint,
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Skinny,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::StartMediaTransmission { .. })
    })
    .await;
    let request = open
        .iter()
        .find_map(|message| match message {
            ServerMessage::OpenReceiveChannel {
                passthrough_party_id,
                ..
            } => Some(*passthrough_party_id),
            _ => None,
        })
        .unwrap();

    phone
        .write_all(&ClientMessage::TimeDateRequest.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::DEFINE_TIME_DATE).await;
    tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
    let rollback = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::CloseReceiveChannel(_))
    })
    .await;
    let stop = rollback
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMediaTransmission(control)
                    if control.passthrough_party_id.get() == request
            )
        })
        .unwrap();
    let close = rollback
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseReceiveChannel(control)
                    if control.passthrough_party_id.get() == request
            )
        })
        .unwrap();
    assert!(stop < close);
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::HandsetAcknowledgementTimedOut {
                call_id: actual_call_id,
                acknowledgement: HandsetAcknowledgement::OpenReceiveChannel,
            },
            ..
        })) if session_generation == generation && actual_call_id == call_id
    ));
    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;
    assert!(events.try_recv().is_err());

    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::OpenOutboundMedia {
                call_id,
                source: None,
                endpoint,
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Skinny,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let open = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::StartMediaTransmission { .. })
    })
    .await;
    let request = open
        .iter()
        .find_map(|message| match message {
            ServerMessage::OpenReceiveChannel {
                passthrough_party_id,
                ..
            } => Some(*passthrough_party_id),
            _ => None,
        })
        .unwrap();

    tokio::time::advance(HANDSET_ACKNOWLEDGEMENT_TIMEOUT + Duration::from_millis(100)).await;
    let rollback = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(message, ServerMessage::CloseReceiveChannel(_))
    })
    .await;
    let stop = rollback
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::StopMediaTransmission(control)
                    if control.passthrough_party_id.get() == request
            )
        })
        .unwrap();
    let close = rollback
        .iter()
        .position(|message| {
            matches!(
                message,
                ServerMessage::CloseReceiveChannel(control)
                    if control.passthrough_party_id.get() == request
            )
        })
        .unwrap();
    assert!(stop < close);
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::HandsetAcknowledgementTimedOut {
                call_id: actual_call_id,
                acknowledgement: HandsetAcknowledgement::OpenReceiveChannel,
            },
            ..
        })) if session_generation == generation && actual_call_id == call_id
    ));
    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;
    assert!(events.try_recv().is_err());

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn invalid_coupled_media_is_rejected_without_disconnect() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::Line,
                instance: 1,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { .. }
        }))
    ));

    let endpoint = MediaEndpoint {
        address: "198.51.100.20".parse().unwrap(),
        rtp_port: 6000,
        rtcp_port: 6001,
        codec: Codec::Pcma,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 0,
    };
    assert!(matches!(
        handle
            .send_confirmed(Command::new(device_id.clone(), CommandAction::OpenOutboundMedia {
                call_id: CallId(1),
                source: None,
                endpoint,
                codec: Codec::Pcma,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            }))
            .await,
        Err(ServerError::CommandWrite(message))
            if message.contains("cannot open coupled outbound media while in state OffHook")
    ));
    assert!(!task.is_finished());

    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::SetCallState {
                call_id: CallId(1),
                state: CallState::Proceed,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::CallState {
            state: CallState::Proceed,
            ..
        })
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn configured_dtmf_mode_selects_rtp_or_signaling_without_duplicate_digits() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;

    phone
        .write_all(&register_bytes_with_features(
            protocol,
            PhoneFeatures::RFC2833,
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::Line,
                instance: 1,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook {
                call_id: CallId(1),
                ..
            }
        }))
    ));

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id: CallId(1),
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::OpenReceiveChannel {
                call_id: CallId(1),
                purpose: ReceiveChannelPurpose::Media,
                source: Some(MediaEndpoint {
                    address: "192.0.2.1".parse().unwrap(),
                    rtp_port: 4000,
                    rtcp_port: 4001,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
                }),
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy {
                    echo_cancellation: crate::EchoCancellation::Off,
                    silence_suppression: crate::SilenceSuppression::On,
                },
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::OPEN_RECEIVE_CHANNEL).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.message_id == wire_id::SUBSCRIBE_DTMF_PAYLOAD_REQ)
            .count(),
        0,
        "RFC2833 is negotiated in the media messages, not with an unsolicited subscription"
    );
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StopMedia { call_id: CallId(1) },
        ))
        .await
        .unwrap();
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::OPEN_RECEIVE_CHANNEL)
        .unwrap();
    assert!(matches!(
        ServerMessage::decode(frame, protocol).unwrap(),
        ServerMessage::OpenReceiveChannel {
            echo_cancellation: crate::EchoCancellation::Off,
            telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
            source_address,
            source_port: 4000,
            ..
        } if source_address == "192.0.2.1".parse::<std::net::IpAddr>().unwrap()
    ));
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StartMedia {
                call_id: CallId(1),
                endpoint: MediaEndpoint {
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    rtp_port: 4000,
                    rtcp_port: 4001,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    telephone_event_payload: 0,
                },
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy {
                    echo_cancellation: crate::EchoCancellation::Off,
                    silence_suppression: crate::SilenceSuppression::On,
                },
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::START_MEDIA_TRANSMISSION).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame.message_id != wire_id::SUBSCRIBE_DTMF_PAYLOAD_REQ),
        "starting the second media direction resubscribed RFC2833"
    );
    let start_media_party = start_media_request_party(&frames, protocol);
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::START_MEDIA_TRANSMISSION)
        .unwrap();
    assert!(matches!(
        ServerMessage::decode(frame, protocol).unwrap(),
        ServerMessage::StartMediaTransmission {
            silence_suppression: crate::SilenceSuppression::On,
            endpoint: MediaEndpoint {
                telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
                ..
            },
            ..
        }
    ));

    phone
        .write_all(
            &ClientMessage::KeypadButton {
                button: Digit::Number(4),
                line_instance: 1,
                call_reference: 1,
                wire_layout: None,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "an RTP-negotiated digit was duplicated on the signaling channel"
    );

    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 99,
                passthrough_party_id: start_media_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 4998,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "mismatched conference identifier was correlated to a call"
    );

    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: start_media_party.saturating_add(1),
                call_reference: 1,
                status: MediaStatus::Ok,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 4999,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "mismatched media identifiers were correlated to a call"
    );

    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 0,
                passthrough_party_id: start_media_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: "192.168.10.20".parse().unwrap(),
                port: 4000,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelOpen {
            call_id: CallId(1),
            outcome: TransmitOpenOutcome::Acknowledged,
            endpoint: MediaEndpoint {
                address,
                rtp_port: 4000,
                telephone_event_payload: RFC2833_TELEPHONE_EVENT_PAYLOAD,
                ..
            },
            ..
        } })) if address == "192.168.10.20".parse::<IpAddr>().unwrap()
    ));

    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: start_media_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 4000,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "duplicate transmit acknowledgement emitted a second event"
    );

    let failed_address = "192.168.10.20".parse().unwrap();
    phone
        .write_all(
            &ClientMessage::MediaTransmissionFailure {
                conference_id: 99,
                passthrough_party_id: start_media_party,
                address: failed_address,
                port: 4000,
                call_reference: 1,
                status: MediaStatus::UnspecifiedError,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "mismatched conference identifier emitted a media failure"
    );
    let failure = ClientMessage::MediaTransmissionFailure {
        conference_id: 1,
        passthrough_party_id: start_media_party,
        address: failed_address,
        port: 4000,
        call_reference: 1,
        status: MediaStatus::UnspecifiedError,
    };
    phone
        .write_all(&failure.encode(protocol).unwrap())
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::MediaTransmissionFailed {
            call_id: CallId(1),
            status: MediaStatus::UnspecifiedError,
            endpoint: MediaEndpoint {
                address,
                rtp_port: 4000,
                ..
            },
            ..
        } })) if address == failed_address
    ));
    phone
        .write_all(&failure.encode(protocol).unwrap())
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "duplicate media failure emitted a second event"
    );

    let recovery_address = IpAddr::V4(Ipv4Addr::LOCALHOST);
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StartMedia {
                call_id: CallId(1),
                endpoint: MediaEndpoint {
                    address: recovery_address,
                    rtp_port: 5000,
                    rtcp_port: 5001,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    telephone_event_payload: 0,
                },
                dtmf_mode: DtmfMode::Auto,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::START_MEDIA_TRANSMISSION).await;
    let recovery_media_party = start_media_request_party(&frames, protocol);
    assert_ne!(recovery_media_party, start_media_party);
    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 1,
                passthrough_party_id: recovery_media_party,
                call_reference: 1,
                status: MediaStatus::Ok,
                address: recovery_address,
                port: 5000,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelOpen {
            call_id: CallId(1),
            outcome: TransmitOpenOutcome::Acknowledged,
            endpoint: MediaEndpoint {
                address,
                rtp_port: 5000,
                ..
            },
            ..
        } })) if address == recovery_address
    ));

    phone
        .write_all(
            &ClientMessage::KeypadButton {
                button: Digit::Number(5),
                line_instance: 1,
                call_reference: 1,
                wire_layout: None,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "acknowledged RTP DTMF also emitted a signaling digit"
    );

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::OpenReceiveChannel {
                call_id: CallId(1),
                purpose: ReceiveChannelPurpose::Media,
                source: Some(MediaEndpoint {
                    address: "192.0.2.1".parse().unwrap(),
                    rtp_port: 4000,
                    rtcp_port: 4001,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    telephone_event_payload: 0,
                }),
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 1,
                dtmf_mode: DtmfMode::Skinny,
                audio_processing: AudioProcessingPolicy::default(),
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::OPEN_RECEIVE_CHANNEL).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.message_id == wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ)
            .count(),
        0,
        "changing one media direction unsubscribed the remaining RFC2833 stream"
    );
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::OPEN_RECEIVE_CHANNEL)
        .unwrap();
    assert!(matches!(
        ServerMessage::decode(frame, protocol).unwrap(),
        ServerMessage::OpenReceiveChannel {
            telephone_event_payload: 0,
            ..
        }
    ));
    phone
        .write_all(
            &ClientMessage::KeypadButton {
                button: Digit::Number(6),
                line_instance: 1,
                call_reference: 1,
                wire_layout: None,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "the remaining RTP direction also emitted a signaling digit"
    );

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StopMedia { call_id: CallId(1) },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::STOP_MEDIA_TRANSMISSION).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame.message_id != wire_id::UNSUBSCRIBE_DTMF_PAYLOAD_REQ)
    );
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StopMedia { call_id: CallId(1) },
        ))
        .await
        .unwrap();
    phone
        .write_all(
            &ClientMessage::KeypadButton {
                button: Digit::Number(7),
                line_instance: 1,
                call_reference: 1,
                wire_layout: None,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Digit {
                call_id: CallId(1),
                digit: Digit::Number(7),
                ..
            }
        }))
    ));

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::CloseReceiveChannel { call_id: CallId(1) },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CLOSE_RECEIVE_CHANNEL).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.message_id == wire_id::CLOSE_RECEIVE_CHANNEL)
            .count(),
        1
    );
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::CloseReceiveChannel { call_id: CallId(1) },
        ))
        .await
        .unwrap();
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::CloseCall { call_id: CallId(1) },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_RINGER).await;
    assert!(frames.iter().all(|frame| !matches!(
        frame.message_id,
        wire_id::STOP_MEDIA_TRANSMISSION | wire_id::CLOSE_RECEIVE_CHANNEL
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn handset_acknowledgement_deadlines_are_bounded_ordered_and_exactly_once() {
    let now = Instant::now();
    let mut first = session_call(20);
    first.media.receive.state = MediaChannelState::Opening;
    first.media.receive.deadline = Some(now);
    first.media.transmit.state = MediaChannelState::Open;
    first.media.transmit_confirmation = TransmitConfirmation::Awaiting {
        deadline: now + Duration::from_millis(1),
    };
    first.media.coupled_transmit_endpoint = Some(MediaEndpoint {
        address: "198.51.100.20".parse().unwrap(),
        rtp_port: 6000,
        rtcp_port: 6001,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 0,
    });
    let mut second = session_call(10);
    let second_endpoint = MediaEndpoint {
        address: "198.51.100.10".parse().unwrap(),
        rtp_port: 5000,
        rtcp_port: 5001,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 0,
    };
    second.media.transmit.state = MediaChannelState::Open;
    second.media.transmit.peer = Some(second_endpoint);
    second.media.transmit_confirmation = TransmitConfirmation::Awaiting { deadline: now };
    let mut calls = HashMap::from([(first.call_id, first), (second.call_id, second)]);

    assert_eq!(
        expire_handset_acknowledgements(&mut calls, now),
        [
            ExpiredHandsetAcknowledgement::Transmit {
                call_id: CallId(10),
                endpoint: second_endpoint,
            },
            ExpiredHandsetAcknowledgement::Receive {
                call_id: CallId(20),
            },
        ]
    );
    assert_eq!(
        calls[&CallId(10)].media.transmit.state,
        MediaChannelState::Open
    );
    assert_eq!(
        calls[&CallId(20)].media.receive.state,
        MediaChannelState::Opening
    );
    assert_eq!(
        calls[&CallId(20)].media.transmit.state,
        MediaChannelState::Open
    );
    assert!(calls[&CallId(20)].media.receive.deadline.is_none());
    assert!(calls[&CallId(20)].media.coupled_transmit_endpoint.is_some());
    assert!(expire_handset_acknowledgements(&mut calls, now).is_empty());
    assert!(expire_handset_acknowledgements(&mut calls, now + Duration::from_millis(1)).is_empty());
    assert!(expire_handset_acknowledgements(&mut calls, now + Duration::from_secs(1)).is_empty());
}

#[test]
fn late_transmit_acknowledgements_confirm_silently_but_rejections_remain_reportable() {
    assert_eq!(
        TransmitConfirmation::NotReported.acknowledgement_is_reportable(MediaStatus::Ok),
        Some(false)
    );
    assert_eq!(
        TransmitConfirmation::NotReported
            .acknowledgement_is_reportable(MediaStatus::UnspecifiedError),
        Some(true)
    );
    assert_eq!(
        TransmitConfirmation::Settled(TransmitOpenOutcome::Acknowledged)
            .acknowledgement_is_reportable(MediaStatus::Ok),
        None
    );
}

#[tokio::test]
async fn capability_snapshots_replace_atomically_and_remain_session_scoped() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let protocol = ProtocolVersion::V22;

    let mut first_phone = TcpStream::connect(address).await.unwrap();
    let mut first_decoder = FrameDecoder::new();
    first_phone
        .write_all(&register_bytes(protocol))
        .await
        .unwrap();
    read_until_message(
        &mut first_phone,
        &mut first_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    let first_generation = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Registered(_),
            ..
        })) => session_generation,
        event => panic!("expected first registration, got {event:?}"),
    };

    first_phone
        .write_all(&capability_update_bytes(
            protocol,
            Codec::Pcmu,
            Codec::H264,
            11,
        ))
        .await
        .unwrap();
    let first_capabilities = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Capabilities { capabilities },
            ..
        })) => {
            assert_eq!(session_generation, first_generation);
            capabilities
        }
        event => panic!("expected first capability update, got {event:?}"),
    };
    assert_eq!(first_capabilities.audio()[0].codec, Codec::Pcmu);
    assert_eq!(first_capabilities.video()[0].codec, Codec::H264);
    assert_eq!(
        first_capabilities.video()[0].direction,
        ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT
    );
    assert_eq!(
        first_capabilities.video()[0].encryption_capability,
        Some(EncryptionCapability::Capable)
    );
    assert_eq!(
        first_capabilities.video()[0].address_type,
        Some(IpAddressType::Ipv4AndIpv6)
    );
    assert_eq!(first_capabilities.video()[0].codec_parameters[5], 11);

    first_phone
        .write_all(&capability_update_bytes(
            protocol,
            Codec::G72264k,
            Codec::H263,
            22,
        ))
        .await
        .unwrap();
    let replacement_capabilities = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Capabilities { capabilities },
            ..
        })) => {
            assert_eq!(session_generation, first_generation);
            capabilities
        }
        event => panic!("expected replacement capability update, got {event:?}"),
    };
    assert_eq!(replacement_capabilities.audio().len(), 1);
    assert_eq!(replacement_capabilities.audio()[0].codec, Codec::G72264k);
    assert_eq!(replacement_capabilities.video().len(), 1);
    assert_eq!(replacement_capabilities.video()[0].codec, Codec::H263);
    assert_eq!(replacement_capabilities.video()[0].codec_parameters[5], 22);
    assert_eq!(first_capabilities.video()[0].codec, Codec::H264);

    let mut second_phone = TcpStream::connect(address).await.unwrap();
    let mut second_decoder = FrameDecoder::new();
    second_phone
        .write_all(&register_bytes(protocol))
        .await
        .unwrap();
    read_until_message(
        &mut second_phone,
        &mut second_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    let second_generation = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Registered(_),
            ..
        })) => session_generation,
        event => panic!("expected replacement registration, got {event:?}"),
    };
    assert!(second_generation > first_generation);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "replaced session emitted a late disconnect"
    );

    second_phone
        .write_all(
            &ClientMessage::CapabilitiesResponse(vec![MediaCapability {
                codec: Codec::Pcma,
                max_packet_ms: 40,
                codec_parameters: [0; 8],
            }])
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Capabilities { capabilities },
            ..
        })) => {
            assert_eq!(session_generation, second_generation);
            assert_eq!(capabilities.audio()[0].codec, Codec::Pcma);
            assert!(capabilities.video().is_empty());
        }
        event => panic!("expected reconnect capability response, got {event:?}"),
    }
    assert_ne!(first_generation, second_generation);

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn hangup_statistics_are_exactly_correlated_retained_and_not_replayed() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let call_id = CallId(7001);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance(1),
                call_id,
                codec: Codec::Pcma,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::SetCallInfo {
                call_id,
                info: CallInfo {
                    direction: crate::types::CallDirection::Outbound,
                    called_number: "2002".into(),
                    ..CallInfo::default()
                },
            },
        ))
        .await
        .unwrap();
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::OpenReceiveChannel {
                call_id,
                purpose: ReceiveChannelPurpose::Media,
                source: Some(MediaEndpoint {
                    address: "192.0.2.1".parse().unwrap(),
                    rtp_port: 5000,
                    rtcp_port: 5001,
                    codec: Codec::Pcma,
                    packet_ms: 30,
                    max_frames_per_packet: 2,
                    telephone_event_payload: 0,
                }),
                codec: Codec::Pcma,
                packet_ms: 30,
                max_frames_per_packet: 2,
                dtmf_mode: DtmfMode::Skinny,
                audio_processing: AudioProcessingPolicy::default(),
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::OPEN_RECEIVE_CHANNEL).await;
    let receive_media_party = open_receive_request_party(&frames, protocol);
    let receive_peer = MediaEndpoint {
        address: "192.0.2.10".parse().unwrap(),
        rtp_port: 4000,
        rtcp_port: 4001,
        codec: Codec::Pcma,
        packet_ms: 30,
        max_frames_per_packet: 2,
        telephone_event_payload: 0,
    };
    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address: receive_peer.address,
                port: receive_peer.rtp_port,
                call_reference: 7001,
                passthrough_party_id: receive_media_party,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::ReceiveChannelOpened { endpoint, .. } })) if endpoint == receive_peer
    ));
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StartMedia {
                call_id,
                endpoint: MediaEndpoint {
                    address: "198.51.100.20".parse().unwrap(),
                    rtp_port: 6000,
                    rtcp_port: 6001,
                    codec: Codec::Pcma,
                    packet_ms: 30,
                    max_frames_per_packet: 2,
                    telephone_event_payload: 0,
                },
                dtmf_mode: DtmfMode::Skinny,
                audio_processing: AudioProcessingPolicy::default(),
                traffic_class: MediaTrafficClass::default(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::START_MEDIA_TRANSMISSION).await;
    let transmit_media_party = start_media_request_party(&frames, protocol);
    let transmit_peer = MediaEndpoint {
        address: "2001:db8::20".parse().unwrap(),
        rtp_port: 5000,
        rtcp_port: 5001,
        codec: Codec::Pcma,
        packet_ms: 30,
        max_frames_per_packet: 2,
        telephone_event_payload: 0,
    };
    phone
        .write_all(
            &ClientMessage::StartMediaTransmissionAck(MediaTransmissionAck {
                conference_id: 7001,
                passthrough_party_id: transmit_media_party,
                call_reference: 7001,
                status: MediaStatus::Ok,
                address: transmit_peer.address,
                port: transmit_peer.rtp_port,
                wire: None,
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::TransmitChannelOpen { outcome: TransmitOpenOutcome::Acknowledged, endpoint, .. } })) if endpoint == transmit_peer
    ));
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::CloseReceiveChannel { call_id },
        ))
        .await
        .unwrap();
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StopMedia { call_id },
        ))
        .await
        .unwrap();
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::CloseCall { call_id },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::CONNECTION_STATISTICS_REQ).await;
    let trailing = if frames
        .iter()
        .any(|frame| frame.message_id == wire_id::SET_RINGER)
    {
        Vec::new()
    } else {
        read_until_message(&mut phone, &mut decoder, wire_id::SET_RINGER).await
    };
    assert_eq!(
        frames
            .iter()
            .chain(&trailing)
            .filter(|frame| frame.message_id == wire_id::STOP_MEDIA_TRANSMISSION)
            .count(),
        1
    );
    assert_eq!(
        frames
            .iter()
            .chain(&trailing)
            .filter(|frame| frame.message_id == wire_id::CLOSE_RECEIVE_CHANNEL)
            .count(),
        1
    );
    let close_receive = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::CLOSE_RECEIVE_CHANNEL)
        .expect("hangup did not close receive media");
    let stop_media = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::STOP_MEDIA_TRANSMISSION)
        .expect("hangup did not stop transmit media");
    let on_hook = frames
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::CallState {
                    state: CallState::OnHook,
                    ..
                })
            )
        })
        .expect("hangup did not publish OnHook");
    let statistics = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::CONNECTION_STATISTICS_REQ)
        .expect("hangup did not request connection statistics");
    assert!(close_receive < stop_media && stop_media < on_hook && on_hook < statistics);
    assert!(
        frames
            .iter()
            .all(|frame| frame.message_id != wire_id::CALL_HISTORY_DISPOSITION)
    );
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::ConnectionStatisticsRequest {
            directory_number,
            call_reference: 7001,
            processing: StatisticsProcessing::Clear,
        }) if directory_number == "2002"
    )));

    phone
        .write_all(
            &ClientMessage::ConnectionStatisticsResponse(test_connection_statistics("wrong", 7001))
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    let mut unknown_processing = test_connection_statistics("2002", 7001);
    unknown_processing.processing = StatisticsProcessing::Unknown(9);
    phone
        .write_all(
            &ClientMessage::ConnectionStatisticsResponse(unknown_processing)
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    let mut expected = test_connection_statistics("2002", 7001);
    expected.quality = crate::ConnectionQualityStatistics::new(Vec::new()).unwrap();
    phone
        .write_all(&packed_base_only_connection_statistics_bytes(
            &expected, protocol,
        ))
        .await
        .unwrap();
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id: actual_device,
        event: DeviceEventKind::ConnectionStatisticsCollected { snapshot },
    })) = events.recv().await
    else {
        panic!("expected a correlated statistics event");
    };
    assert_eq!(actual_device, device_id);
    assert_eq!(snapshot.call_id, call_id);
    assert_eq!(snapshot.line_instance, LineInstance::new(1));
    assert_eq!(snapshot.codec, Codec::Pcma);
    assert_eq!(snapshot.packet_ms, 30);
    assert_eq!(snapshot.max_frames_per_packet, 2);
    assert_eq!(snapshot.receive_peer, Some(receive_peer));
    assert_eq!(snapshot.transmit_peer, Some(transmit_peer));
    assert_eq!(snapshot.packets_sent, expected.packets_sent);
    assert_eq!(snapshot.octets_sent, expected.octets_sent);
    assert_eq!(snapshot.packets_received, expected.packets_received);
    assert_eq!(snapshot.octets_received, expected.octets_received);
    assert_eq!(snapshot.packets_lost, expected.packets_lost);
    assert_eq!(snapshot.jitter_millis, expected.jitter_millis);
    assert_eq!(snapshot.latency_millis, expected.latency_millis);
    assert_eq!(
        snapshot.quality_byte_count,
        expected.quality.as_bytes().len()
    );
    let debug = format!("{snapshot:?}");
    assert!(!debug.contains("2002"));
    assert!(!debug.contains("MLQK"));
    assert_eq!(
        handle.latest_media_statistics(&device_id),
        Some(snapshot.clone())
    );
    assert_eq!(
        handle.media_statistics(),
        vec![(device_id.clone(), snapshot.clone())]
    );

    phone
        .write_all(&packed_base_only_connection_statistics_bytes(
            &expected, protocol,
        ))
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "a duplicate response emitted a second event"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn expired_statistics_requests_are_pruned_at_the_deadline() {
    let now = Instant::now();
    let mut pending = HashMap::from([(
        42,
        PendingConnectionStatistics {
            session_generation: SessionGeneration::new(1).unwrap(),
            request_generation: 2,
            call_id: CallId(3),
            line_instance: 1,
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            receive_peer: None,
            transmit_peer: None,
            directory_number: "2002".into(),
            processing: StatisticsProcessing::Clear,
            expires_at: now,
        },
    )]);
    prune_connection_statistics(&mut pending, now);
    assert!(pending.is_empty());
}

#[test]
fn statistics_directory_follows_the_call_direction() {
    let inbound = CallInfo {
        direction: crate::types::CallDirection::Inbound,
        calling_number: "inbound-peer".into(),
        called_number: "local-line".into(),
        ..CallInfo::default()
    };
    let outbound = CallInfo {
        direction: crate::types::CallDirection::Outbound,
        calling_number: "local-line".into(),
        called_number: "outbound-peer".into(),
        ..CallInfo::default()
    };
    assert_eq!(statistics_directory_for_call_info(&inbound), "inbound-peer");
    assert_eq!(
        statistics_directory_for_call_info(&outbound),
        "outbound-peer"
    );
}

#[test]
fn replacement_calls_and_media_requests_never_reuse_identifiers() {
    let device = definition();
    let registration = DeviceRegistration {
        id: device.id.clone(),
        peer: "127.0.0.1:2000".parse().unwrap(),
        transport: StationTransport::Clear,
        reported_address: Some(Ipv4Addr::LOCALHOST),
        reported_ipv6_address: None,
        device_type: DeviceType::Cisco7962,
        protocol: ProtocolVersion::V22,
        firmware: "test".into(),
    };
    let mut state = SessionState::new(
        device,
        registration,
        PhoneFeatures::default(),
        SessionGeneration::new(1).unwrap(),
    );
    state.statistics_references = HashSet::from([42]);
    let replacement = insert_call(&mut state, CallId(42), 1, Codec::Pcmu, CallState::OffHook);
    assert_eq!(replacement.wire_reference, 43);
    assert_eq!(state.calls_by_wire.get(&42), None);
    assert_eq!(state.calls_by_wire.get(&43), Some(&CallId(42)));

    state.next_media_token = MediaRequestToken::new(u32::MAX);
    let final_identity = allocate_media_request_identity(&mut state, CallId(42)).unwrap();
    assert_eq!(final_identity.token().get(), u32::MAX);
    assert!(state.next_media_token.is_none());
    let generation_after_final_token = state.calls_by_id[&CallId(42)].media.generation;
    assert!(matches!(
        allocate_media_request_identity(&mut state, CallId(42)),
        Err(ServerError::MediaRequestIdentityExhausted)
    ));
    assert_eq!(
        state.calls_by_id[&CallId(42)].media.generation,
        generation_after_final_token,
        "failed allocation mutated the call generation"
    );

    state.next_media_token = MediaRequestToken::new(7);
    state
        .calls_by_id
        .get_mut(&CallId(42))
        .unwrap()
        .media
        .generation = u64::MAX;
    assert!(matches!(
        allocate_media_request_identity(&mut state, CallId(42)),
        Err(ServerError::MediaRequestIdentityExhausted)
    ));
    assert_eq!(state.next_media_token.unwrap().get(), 7);
}

#[tokio::test]
async fn registration_applies_device_socket_qos_without_making_failure_fatal() {
    let baseline = SignalingQos::new(8, 1);
    let device_policy = SignalingQos::new(26, 5);
    let mut station = definition();
    station.signaling_qos = Some(device_policy);
    let config = ServerConfig {
        signaling_qos: baseline,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) = Server::with_ingress(config, [station]).unwrap();
    let task = tokio::spawn(server.run());
    let (server_stream, mut phone) = tokio::io::duplex(8_192);
    let applied = Arc::new(std::sync::Mutex::new(Vec::new()));
    ingress
        .accept_with_socket_qos(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_000)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
            RecordingSocketQos {
                applied: Arc::clone(&applied),
                fail: true,
            },
        )
        .await
        .unwrap();
    phone
        .write_all(&register_bytes(ProtocolVersion::V22))
        .await
        .unwrap();

    let mut decoder = FrameDecoder::new();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::REGISTER_ACK).await;
    assert!(
        frames
            .iter()
            .any(|frame| frame.message_id == wire_id::REGISTER_ACK)
    );
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    assert_eq!(*applied.lock().unwrap(), vec![baseline, device_policy]);

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn unpaired_active_media_path_release_completes_on_hook_after_grace() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let call_id = CallId(7101);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send(Command::new(
            device_id,
            CommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;

    for event in [crate::MediaPathEvent::On, crate::MediaPathEvent::Off] {
        phone
            .write_all(
                &ClientMessage::MediaPathEvent {
                    path: crate::MediaPathId::Speaker,
                    event,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MediaPathChanged {
                    path: crate::MediaPathId::Speaker,
                    event: actual,
                },
                ..
            })) if actual == event
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err(),
        "media-path release bypassed its route-change grace period"
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_millis(300), events.recv()).await,
        Ok(Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::OnHook {
                call_id: ended,
                line_instance: LineInstance(1),
            },
            ..
        }))) if ended == call_id
    ));
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_RINGER).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::CallState {
            state: CallState::OnHook,
            ..
        })
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn replacement_media_path_cancels_pending_on_hook_completion() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let call_id = CallId(7102);

    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id,
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send(Command::new(
            device_id,
            CommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;

    for (path, event) in [
        (crate::MediaPathId::Speaker, crate::MediaPathEvent::On),
        (crate::MediaPathId::Speaker, crate::MediaPathEvent::Off),
        (crate::MediaPathId::Headset, crate::MediaPathEvent::On),
    ] {
        phone
            .write_all(
                &ClientMessage::MediaPathEvent { path, event }
                    .encode(protocol)
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::MediaPathChanged {
                    path: actual_path,
                    event: actual_event,
                },
                ..
            })) if actual_path == path && actual_event == event
        ));
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(350), events.recv())
            .await
            .is_err(),
        "a replacement audio path was mistaken for terminal OnHook"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), phone.read_u8())
            .await
            .is_err(),
        "route switching emitted terminal handset UI"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}
