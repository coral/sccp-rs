use super::support::*;

#[tokio::test]
async fn saturated_ordinary_input_does_not_block_reserved_media_or_terminal_events() {
    for stimulus in [false, true] {
        saturated_station_completion(stimulus).await;
    }
}

async fn saturated_station_completion(stimulus: bool) {
    let protocol = ProtocolVersion::V22;
    let device = definition();
    let device_id = device.id.clone();
    let (mut server, handle, mut ordinary) = Server::bind(
        ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            ..ServerConfig::default()
        },
        [device],
    )
    .await
    .unwrap();
    let address = server.local_addr().unwrap();
    let normal_sender = server.event_tx.clone();
    let mut priority = server.enable_priority_events(3).unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    let (registered, permit) = priority.recv().await.unwrap().into_parts();
    assert!(matches!(
        registered,
        Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        })
    ));
    drop(permit);
    let call_id = handle
        .offer_incoming_call(device_id.clone(), LineInstance::new(1), CallInfo::default())
        .await
        .unwrap();
    read_until_message(
        &mut phone,
        &mut decoder,
        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
    )
    .await;
    while normal_sender
        .try_send(Event::SessionError {
            peer: address,
            error: String::new(),
        })
        .is_ok()
    {}
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::OpenReceiveChannel {
                call_id,
                purpose: ReceiveChannelPurpose::Media,
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
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::OPEN_RECEIVE_CHANNEL).await;
    let (call_reference, passthrough_party_id) = frames
        .into_iter()
        .find_map(
            |frame| match ServerMessage::decode(frame, protocol).unwrap() {
                ServerMessage::OpenReceiveChannel {
                    call_reference,
                    passthrough_party_id,
                    ..
                } => Some((call_reference, passthrough_party_id)),
                _ => None,
            },
        )
        .unwrap();
    let mut burst = ClientMessage::OffHook {
        line_instance: 1,
        call_reference: 0,
    }
    .encode(protocol)
    .unwrap();
    burst.extend(
        ClientMessage::OpenReceiveChannelAck {
            status: MediaStatus::Ok,
            address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 4000,
            call_reference,
            passthrough_party_id,
        }
        .encode(protocol)
        .unwrap(),
    );
    burst.extend(ClientMessage::KeepAlive.encode(protocol).unwrap());
    phone.write_all(&burst).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;
    let (opened, media_permit) = tokio::time::timeout(Duration::from_secs(1), priority.recv())
        .await
        .unwrap()
        .unwrap()
        .into_parts();
    assert!(
        matches!(opened, Event::Device(DeviceEvent { event: DeviceEventKind::ReceiveChannelOpened { call_id: actual, .. }, .. }) if actual == call_id)
    );
    // Dequeue retains the media result's budget, so another native request must
    // fail before writing rather than borrowing the call's terminal capacity.
    assert!(
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::OpenReceiveChannel {
                    call_id,
                    purpose: ReceiveChannelPurpose::Media,
                    source: None,
                    codec: Codec::Pcmu,
                    packet_ms: 20,
                    max_frames_per_packet: 1,
                    dtmf_mode: DtmfMode::Skinny,
                    audio_processing: AudioProcessingPolicy::default(),
                }
            ))
            .await
            .is_err()
    );
    let terminal = if stimulus {
        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        ClientMessage::Stimulus {
            stimulus: Stimulus::EndCall,
            instance: 1,
            call_reference,
            status: 0,
        }
    } else {
        ClientMessage::OnHook {
            line_instance: 1,
            call_reference,
        }
    };
    phone
        .write_all(&terminal.encode(protocol).unwrap())
        .await
        .unwrap();
    let (terminal, terminal_permit) = tokio::time::timeout(Duration::from_secs(1), priority.recv())
        .await
        .unwrap()
        .unwrap()
        .into_parts();
    assert!(
        matches!(terminal, Event::Device(DeviceEvent { event: DeviceEventKind::OnHook { call_id: actual, .. } | DeviceEventKind::SoftKey { call_id: Some(actual), soft_key: SoftKey::EndCall, .. }, .. }) if actual == call_id)
    );
    assert_eq!(ordinary.len(), EVENT_CAPACITY);
    assert!(
        (0..EVENT_CAPACITY).all(|_| matches!(ordinary.try_recv(), Ok(Event::SessionError { .. })))
    );
    drop(media_permit);
    drop(terminal_permit);
    handle.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let (disconnected, _) = priority.recv().await.unwrap().into_parts();
    assert!(matches!(
        disconnected,
        Event::Device(DeviceEvent {
            event: DeviceEventKind::Disconnected {},
            ..
        })
    ));
}

#[tokio::test]
async fn completion_admission_rejection_does_not_partially_reserve_a_media_operation() {
    use super::super::event_delivery::{EventSender, PrioritySender};
    let (ordinary, _) = mpsc::channel(3);
    let (priority, _events) = PrioritySender::channel(2);
    let sender = EventSender::session(ordinary, Some(priority));
    let call_id = CallId::new(1);
    let command = SessionCommand::Public(Box::new(Command::new(
        definition().id,
        CommandAction::OpenOutboundMedia {
            call_id,
            source: None,
            endpoint: MediaEndpoint {
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                rtp_port: 4000,
                rtcp_port: 4001,
                codec: Codec::Pcmu,
                packet_ms: 20,
                max_frames_per_packet: 1,
                telephone_event_payload: 0,
            },
            codec: Codec::Pcmu,
            packet_ms: 20,
            max_frames_per_packet: 1,
            dtmf_mode: DtmfMode::Skinny,
            audio_processing: AudioProcessingPolicy::default(),
            traffic_class: MediaTrafficClass::default(),
        },
    )));
    assert!(matches!(
        sender.reserve_command(&command),
        Err(ServerError::CommandQueueFull)
    ));
    // Both slots are available after rejecting the three-result operation.
    sender.reserve_registration().unwrap();
}

#[tokio::test]
async fn service_cancellation_releases_only_the_exact_unused_response_reservation() {
    use super::super::event_delivery::{EventSender, PrioritySender};
    let (ordinary, _) = mpsc::channel(3);
    let (priority, mut events) = PrioritySender::channel(1);
    let sender = EventSender::session(ordinary, Some(priority));
    let routing = PhoneServiceRouting {
        application_id: ApplicationId::new(70),
        line_instance: LineInstance::new(1),
        call_reference: CallReference::new(12),
        transaction_id: TransactionId::new(3),
    };
    let command = SessionCommand::Public(Box::new(Command::new(
        definition().id,
        CommandAction::ExecutePhoneActions {
            application_id: routing.application_id,
            line_instance: routing.line_instance,
            call_reference: routing.call_reference,
            transaction_id: routing.transaction_id,
            priority: PhoneServicePriority::NORMAL,
            document: CiscoIpPhoneExecute::new(vec![
                CiscoIpPhoneExecuteItem::new("Play:beep.wav").unwrap(),
            ])
            .unwrap(),
        },
    )));
    sender.reserve_command(&command).unwrap();
    sender.cancel_service_response(PhoneServiceRouting {
        application_id: ApplicationId::new(71),
        ..routing
    });
    assert!(matches!(
        sender.reserve_registration(),
        Err(ServerError::CommandQueueFull)
    ));
    sender
        .send(Event::device(
            definition().id,
            SessionGeneration::new(1).unwrap(),
            DeviceEventKind::PhoneServiceResponse {
                response: PhoneServiceEvent {
                    kind: PhoneServiceMessageKind::Response,
                    routing,
                    extended: None,
                    payload: PhoneServicePayload::Opaque(Vec::new()),
                },
            },
        ))
        .await
        .unwrap();
    let (_event, delivered) = events.recv().await.unwrap().into_parts();
    sender.cancel_service_response(routing);
    assert!(matches!(
        sender.reserve_command(&command),
        Err(ServerError::CommandQueueFull)
    ));
    drop(delivered);
    sender.reserve_command(&command).unwrap();
    sender.cancel_service_response(routing);
    sender.reserve_command(&command).unwrap();
}
