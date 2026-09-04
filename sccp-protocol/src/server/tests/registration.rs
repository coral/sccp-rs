use super::support::*;

struct TokenRejectWriteFailure {
    inner: tokio::io::DuplexStream,
}

impl tokio::io::AsyncRead for TokenRejectWriteFailure {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl tokio::io::AsyncWrite for TokenRejectWriteFailure {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let message_id = bytes
            .get(8..12)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes);
        if message_id == Some(wire_id::REGISTER_TOKEN_REJECT) {
            return std::task::Poll::Ready(Err(std::io::Error::other(
                "injected registration-token rejection write failure",
            )));
        }
        std::pin::Pin::new(&mut self.inner).poll_write(context, bytes)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[test]
fn session_generations_are_nonzero_monotonic_and_fail_closed_at_exhaustion() {
    let next = AtomicU64::new(1);
    let first = allocate_session_generation(&next).unwrap();
    let second = allocate_session_generation(&next).unwrap();
    assert_eq!(u64::from(first), 1);
    assert_eq!(u64::from(second), 2);

    let exhausted = AtomicU64::new(u64::MAX);
    assert!(matches!(
        allocate_session_generation(&exhausted),
        Err(ServerError::SessionGenerationExhausted)
    ));
    assert_eq!(exhausted.load(Ordering::Relaxed), u64::MAX);

    let boundary = AtomicU64::new(u64::MAX - 1);
    assert_eq!(
        u64::from(allocate_session_generation(&boundary).unwrap()),
        u64::MAX - 1
    );
    assert!(matches!(
        allocate_session_generation(&boundary),
        Err(ServerError::SessionGenerationExhausted)
    ));
    assert_eq!(boundary.load(Ordering::Relaxed), u64::MAX);

    let invalid = AtomicU64::new(0);
    assert!(matches!(
        allocate_session_generation(&invalid),
        Err(ServerError::SessionGenerationExhausted)
    ));
    assert_eq!(invalid.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn short_registration_prefixes_negotiate_from_field_presence() {
    for (payload_bytes, advertised_protocol, expected_protocol) in [
        (36, None, ProtocolVersion::V3),
        (44, Some(0), ProtocolVersion::V3),
        (64, Some(ProtocolVersion::V11.wire()), ProtocolVersion::V11),
    ] {
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

        let mut payload = vec![0_u8; payload_bytes];
        let device_id = b"SEP001122334455";
        payload[..device_id.len()].copy_from_slice(device_id);
        payload[24..28].copy_from_slice(&Ipv4Addr::LOCALHOST.octets());
        payload[28..32].copy_from_slice(&DeviceType::Cisco7925.wire_value().to_le_bytes());
        payload[32..36].copy_from_slice(&5_u32.to_le_bytes());
        if let Some(protocol) = advertised_protocol {
            payload[40..44].copy_from_slice(&protocol.to_le_bytes());
        }
        phone
            .write_all(&Frame::new(0, wire_id::REGISTER, payload).encode().unwrap())
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;

        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                event: DeviceEventKind::Registered(DeviceRegistration { protocol, .. }),
                ..
            })) if protocol == expected_protocol
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn registered_phone_receives_its_mixed_button_template() {
    let mut device = mixed_definition();
    device
        .buttons
        .push(ButtonDefinition::SpeedDial(SpeedDialDefinition {
            instance: u32::from(u8::MAX),
            number: "2255".into(),
            display_name: "Wire boundary".into(),
        }));
    let expected = button_template(&device);
    assert!(expected.iter().any(|button| {
        button.instance == u32::from(u8::MAX) && button.button_type == ButtonType::SpeedDial
    }));
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, _events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();

    phone
        .write_all(&register_bytes(ProtocolVersion::V22))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::REGISTER_ACK).await;
    phone
        .write_all(
            &ClientMessage::ButtonTemplateRequest
                .encode(ProtocolVersion::V22)
                .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::BUTTON_TEMPLATE).await;
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::BUTTON_TEMPLATE)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
        ServerMessage::ButtonTemplate {
            offset: 0,
            total: expected.len() as u32,
            buttons: expected,
        }
    );

    phone
        .write_all(
            &ClientMessage::SpeedDialStatusRequest {
                speed_dial_instance: 1,
            }
            .encode(ProtocolVersion::V22)
            .unwrap(),
        )
        .await
        .unwrap();
    let speed_dial_status_id = wire_id::SPEED_DIAL_STAT_DYNAMIC;
    let frames = read_until_message(&mut phone, &mut decoder, speed_dial_status_id).await;
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == speed_dial_status_id)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
        ServerMessage::SpeedDialStatus {
            instance: 1,
            number: "2001".into(),
            display_name: "Reception".into(),
        }
    );

    phone
        .write_all(
            &ClientMessage::SpeedDialStatusRequest {
                speed_dial_instance: 2,
            }
            .encode(ProtocolVersion::V22)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, speed_dial_status_id).await;
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == speed_dial_status_id)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
        ServerMessage::SpeedDialStatus {
            instance: 2,
            number: String::new(),
            display_name: String::new(),
        }
    );

    phone
        .write_all(
            &ClientMessage::SpeedDialStatusRequest {
                speed_dial_instance: 99,
            }
            .encode(ProtocolVersion::V22)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, speed_dial_status_id).await;
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == speed_dial_status_id)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
        ServerMessage::SpeedDialStatus {
            instance: 99,
            number: String::new(),
            display_name: String::new(),
        }
    );

    phone
        .write_all(
            &ClientMessage::FeatureStatusRequest {
                index: 1,
                capabilities: 0,
            }
            .encode(ProtocolVersion::V22)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::FEATURE_STAT).await;
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::FEATURE_STAT)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
        ServerMessage::FeatureStatus {
            instance: 1,
            button_type: ButtonType::DoNotDisturb,
            label: "DND".into(),
            state: 0,
        }
    );

    phone
        .write_all(
            &ClientMessage::FeatureStatusRequest {
                index: 2,
                capabilities: 0,
            }
            .encode(ProtocolVersion::V22)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::FEATURE_STAT).await;
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::FEATURE_STAT)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
        ServerMessage::FeatureStatus {
            instance: 2,
            button_type: ButtonType::BlfSpeedDial,
            label: "Warehouse".into(),
            state: BusyLampFieldState::UnknownState.wire_value(),
        }
    );

    phone
        .write_all(
            &ClientMessage::ServiceUrlStatusRequest { index: 1 }
                .encode(ProtocolVersion::V22)
                .unwrap(),
        )
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::SERVICE_URL_STAT_DYNAMIC).await;
    let frame = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::SERVICE_URL_STAT_DYNAMIC)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
        ServerMessage::ServiceUrlStatus {
            index: 1,
            url: "http://services.invalid/directory".into(),
            label: "Directory".into(),
            extension_text: String::new(),
        }
    );

    let unknown_requests = [
        ClientMessage::FeatureStatusRequest {
            index: 99,
            capabilities: 0,
        }
        .encode(ProtocolVersion::V22)
        .unwrap(),
        ClientMessage::ServiceUrlStatusRequest { index: 99 }
            .encode(ProtocolVersion::V22)
            .unwrap(),
        ClientMessage::KeepAlive
            .encode(ProtocolVersion::V22)
            .unwrap(),
    ]
    .concat();
    phone.write_all(&unknown_requests).await.unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;
    assert!(
        frames.iter().all(|frame| !matches!(
            frame.message_id,
            wire_id::FEATURE_STAT
                | wire_id::FEATURE_STAT_DYNAMIC
                | wire_id::SERVICE_URL_STAT
                | wire_id::SERVICE_URL_STAT_DYNAMIC
        )),
        "unknown feature and service requests must not produce placeholder statuses"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn anonymous_hotline_registration_gets_one_restricted_public_line() {
    let label = "Guest assistance";
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        anonymous_hotline: Some(AnonymousHotlineDefinition::new(label).unwrap()),
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;
    let device = "SEPFFEEDDCCBBAA";

    phone
        .write_all(&register_bytes_for_device(protocol, 115, device))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Registered(registration) })) if registration.id.as_str() == device
    ));

    phone
        .write_all(
            &[
                ClientMessage::LineStatRequest { line_instance: 1 }
                    .encode(protocol)
                    .unwrap(),
                ClientMessage::ButtonTemplateRequest
                    .encode(protocol)
                    .unwrap(),
                ClientMessage::SoftKeySetRequest.encode(protocol).unwrap(),
            ]
            .concat(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SOFT_KEY_SET_RES).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::LineStatus {
            instance: 1,
            directory_number,
            fully_qualified_display_name,
            display_label,
        }) if directory_number == "hotline"
            && fully_qualified_display_name == label
            && display_label == label
    )));
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::ButtonTemplate { buttons, .. })
            if buttons == vec![ButtonTemplateEntry {
                instance: 1,
                button_type: ButtonType::Line,
            }]
    )));
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SoftKeySet { profile })
            if profile.actions(KeyMode::OnHook) == [SoftKey::NewCall]
                && profile.actions(KeyMode::OffHook) == [SoftKey::EndCall]
                && profile.actions(KeyMode::RingOut) == [SoftKey::EndCall]
                && profile.actions(KeyMode::Connected).is_empty()
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn anonymous_hotline_reload_isolated_from_configured_session_and_is_idempotent() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        anonymous_hotline: Some(AnonymousHotlineDefinition::new("Guest A").unwrap()),
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let protocol = ProtocolVersion::V22;
    let mut configured = TcpStream::connect(address).await.unwrap();
    let mut configured_decoder = FrameDecoder::new();
    configured
        .write_all(&register_bytes(protocol))
        .await
        .unwrap();
    read_until_message(
        &mut configured,
        &mut configured_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));

    let guest_id = "SEPFFEEDDCCBBAA";
    let mut guest = TcpStream::connect(address).await.unwrap();
    let mut guest_decoder = FrameDecoder::new();
    guest
        .write_all(&register_bytes_for_device(protocol, 115, guest_id))
        .await
        .unwrap();
    read_until_message(&mut guest, &mut guest_decoder, wire_id::CAPABILITIES_REQ).await;
    loop {
        match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(registration),
            })) if registration.id.as_str() == guest_id => {
                break;
            }
            Some(_) => {}
            None => panic!("server stopped before replacement guest registration"),
        }
    }

    assert_eq!(
        handle
            .reconfigure_anonymous_hotline(Some(
                AnonymousHotlineDefinition::new("Guest A").unwrap(),
            ))
            .await
            .unwrap(),
        0
    );
    let station_policy = handle
        .reconfigure_station_policy(
            [definition()],
            [],
            Some(AnonymousHotlineDefinition::new("Guest B").unwrap()),
        )
        .await
        .unwrap();
    assert!(station_policy.is_unchanged());
    let mut closed = [0_u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), guest.read(&mut closed))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    configured
        .write_all(
            &ClientMessage::LineStatRequest { line_instance: 1 }
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(
        &mut configured,
        &mut configured_decoder,
        wire_id::LINE_STAT_DYNAMIC,
    )
    .await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::LineStatus { directory_number, .. }) if directory_number == "1001"
    )));

    let mut replacement = TcpStream::connect(address).await.unwrap();
    let mut replacement_decoder = FrameDecoder::new();
    replacement
        .write_all(&register_bytes_for_device(protocol, 115, guest_id))
        .await
        .unwrap();
    read_until_message(
        &mut replacement,
        &mut replacement_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    loop {
        match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(registration),
            })) if registration.id.as_str() == guest_id => {
                break;
            }
            Some(_) => {}
            None => panic!("server stopped before replacement guest registration"),
        }
    }
    replacement
        .write_all(
            &ClientMessage::LineStatRequest { line_instance: 1 }
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(
        &mut replacement,
        &mut replacement_decoder,
        wire_id::LINE_STAT_DYNAMIC,
    )
    .await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::LineStatus {
            directory_number,
            fully_qualified_display_name,
            display_label,
            ..
        }) if directory_number == "hotline"
            && fully_qualified_display_name == "Guest B"
            && display_label == "Guest B"
    )));

    assert_eq!(handle.reconfigure_anonymous_hotline(None).await.unwrap(), 1);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), replacement.read(&mut closed))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    assert_eq!(handle.reconfigure_anonymous_hotline(None).await.unwrap(), 0);

    assert_eq!(
        handle
            .reconfigure_anonymous_hotline(Some(
                AnonymousHotlineDefinition::new("Guest C").unwrap(),
            ))
            .await
            .unwrap(),
        0
    );
    let mut promoted = TcpStream::connect(address).await.unwrap();
    let mut promoted_decoder = FrameDecoder::new();
    promoted
        .write_all(&register_bytes_for_device(protocol, 115, guest_id))
        .await
        .unwrap();
    read_until_message(
        &mut promoted,
        &mut promoted_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    loop {
        match events.recv().await {
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(registration),
            })) if registration.id.as_str() == guest_id => {
                break;
            }
            Some(_) => {}
            None => panic!("server stopped before promoted guest registration"),
        }
    }
    let result = handle
        .reconfigure_affected(
            [definition(), definition_for(guest_id)],
            [DeviceId::new(guest_id).unwrap()],
        )
        .await
        .unwrap();
    assert_eq!(result.added, [DeviceId::new(guest_id).unwrap()]);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), promoted.read(&mut closed))
            .await
            .unwrap()
            .unwrap(),
        0
    );
    let mut configured_guest = TcpStream::connect(address).await.unwrap();
    let mut configured_guest_decoder = FrameDecoder::new();
    configured_guest
        .write_all(&register_bytes_for_device(protocol, 115, guest_id))
        .await
        .unwrap();
    read_until_message(
        &mut configured_guest,
        &mut configured_guest_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    configured_guest
        .write_all(
            &ClientMessage::LineStatRequest { line_instance: 1 }
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(
        &mut configured_guest,
        &mut configured_guest_decoder,
        wire_id::LINE_STAT_DYNAMIC,
    )
    .await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::LineStatus { directory_number, .. }) if directory_number == "1001"
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn duplicate_anonymous_registration_replaces_only_the_previous_guest_session() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        anonymous_hotline: Some(AnonymousHotlineDefinition::new("Guest").unwrap()),
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, []).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let protocol = ProtocolVersion::V22;
    let guest_id = "SEPFFEEDDCCBBAA";
    let mut first = TcpStream::connect(address).await.unwrap();
    let mut first_decoder = FrameDecoder::new();
    first
        .write_all(&register_bytes_for_device(protocol, 115, guest_id))
        .await
        .unwrap();
    read_until_message(&mut first, &mut first_decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));

    let mut second = TcpStream::connect(address).await.unwrap();
    let mut second_decoder = FrameDecoder::new();
    second
        .write_all(&register_bytes_for_device(protocol, 115, guest_id))
        .await
        .unwrap();
    read_until_message(&mut second, &mut second_decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));
    let mut closed = [0_u8; 1];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), first.read(&mut closed))
            .await
            .unwrap()
            .unwrap(),
        0
    );

    second
        .write_all(
            &ClientMessage::LineStatRequest { line_instance: 1 }
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut second, &mut second_decoder, wire_id::LINE_STAT_DYNAMIC).await;

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn anonymous_hotline_disabled_rejects_unknown_without_affecting_configured_devices() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, _events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();

    phone
        .write_all(&register_bytes_for_device(
            ProtocolVersion::V22,
            115,
            "SEPFFEEDDCCBBAA",
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::REGISTER_REJECT).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, ProtocolVersion::V17),
        Ok(ServerMessage::RegisterReject { reason }) if reason == "Device not configured"
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn anonymous_hotline_definition_bounds_and_debug_are_destination_free() {
    assert!(AnonymousHotlineDefinition::new("").is_err());
    assert!(AnonymousHotlineDefinition::new("x".repeat(80)).is_err());
    assert!(AnonymousHotlineDefinition::new("guest\nline").is_err());
    let definition = AnonymousHotlineDefinition::new("Guest").unwrap();
    assert!(!format!("{definition:?}").contains("111"));
}

#[tokio::test]
async fn registered_phone_receives_configured_soft_key_sets_and_masks() {
    let mut device = definition();
    device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::NewCall, SoftKey::Redial]);
    let default = device.soft_keys.clone();
    device.soft_keys = SoftKeyProfile::new(KeyMode::ALL_KNOWN.iter().copied().map(|mode| {
        if mode == KeyMode::OffHook {
            (
                mode,
                vec![SoftKey::EndCall, SoftKey::Pickup, SoftKey::GroupPickup],
            )
        } else {
            (mode, default.actions(mode).to_vec())
        }
    }))
    .unwrap();
    let expected_profile = device.soft_keys.clone();
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
            &[
                ClientMessage::SoftKeyTemplateRequest
                    .encode(protocol)
                    .unwrap(),
                ClientMessage::SoftKeySetRequest.encode(protocol).unwrap(),
            ]
            .concat(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SOFT_KEY_SET_RES).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SoftKeyTemplate { actions })
            if actions == expected_profile.template_actions()
    )));
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SoftKeySet { profile }) if profile == expected_profile
    )));

    phone
        .write_all(
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::NewCall.wire_value(),
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SelectSoftKeys {
            set: KeyMode::OffHook,
            valid_mask: 0b111,
            ..
        })
    )));
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { .. }
        }))
    ));
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::SoftKey {
                soft_key: SoftKey::NewCall,
                ..
            }
        }))
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn stale_public_command_does_not_stop_the_listener() {
    let device = definition();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());

    handle
        .send(Command::new(
            DeviceId::new("SEPFFFFFFFFFFFF").unwrap(),
            CommandAction::SetMwi {
                line_instance: LineInstance(1),
                enabled: true,
            },
        ))
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert!(!task.is_finished());

    let protocol = ProtocolVersion::V22;
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
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

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_handset_routes_every_configured_conference_control_with_exact_call() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let conference_keys = vec![
        SoftKey::Conference,
        SoftKey::Join,
        SoftKey::ConferenceList,
        SoftKey::Select,
        SoftKey::Hold,
        SoftKey::Resume,
        SoftKey::EndCall,
    ];
    let mut device = definition();
    device.soft_keys = SoftKeyProfile::new(
        KeyMode::ALL_KNOWN
            .iter()
            .copied()
            .map(|mode| (mode, conference_keys.clone())),
    )
    .unwrap();
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
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
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;

    for soft_key in conference_keys {
        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: soft_key.wire_value(),
                    line_instance: 1,
                    call_reference: 7001,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::SoftKey {
                line_instance: LineInstance(1),
                call_id: Some(actual_call),
                soft_key: actual_key,
            } })) if actual_device == device_id
                && actual_call == call_id
                && actual_key == soft_key
        ));
    }

    for (stimulus, soft_key) in [
        (Stimulus::Conference, SoftKey::Conference),
        (Stimulus::ConferenceList, SoftKey::ConferenceList),
    ] {
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus,
                    instance: 1,
                    call_reference: 7001,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::SoftKey {
                line_instance: LineInstance(1),
                call_id: Some(actual_call),
                soft_key: actual_key,
            } })) if actual_device == device_id
                && actual_call == call_id
                && actual_key == soft_key
        ));
    }

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn reconfiguration_classifies_added_changed_removed_and_unchanged_devices() {
    let unchanged = definition_for("SEP001122334455");
    let mut changed = definition_for("SEP112233445566");
    let removed = definition_for("SEP223344556677");
    let added = definition_for("SEP334455667788");
    let current = HashMap::from([
        (unchanged.id.clone(), unchanged.clone()),
        (changed.id.clone(), changed.clone()),
        (removed.id.clone(), removed),
    ]);
    changed.description = "Changed station".into();
    let next = HashMap::from([
        (unchanged.id.clone(), unchanged),
        (changed.id.clone(), changed),
        (added.id.clone(), added),
    ]);

    assert_eq!(
        reconfigure_result(&current, &next, &HashSet::new()),
        ReconfigureResult {
            added: vec![DeviceId::new("SEP334455667788").unwrap()],
            changed: vec![DeviceId::new("SEP112233445566").unwrap()],
            removed: vec![DeviceId::new("SEP223344556677").unwrap()],
        }
    );
    assert!(reconfigure_result(&current, &current, &HashSet::new()).is_unchanged());
    assert_eq!(
        reconfigure_result(
            &current,
            &current,
            &HashSet::from([DeviceId::new("SEP001122334455").unwrap()]),
        )
        .changed,
        vec![DeviceId::new("SEP001122334455").unwrap()]
    );
}

#[tokio::test]
async fn reconfiguration_preserves_unchanged_session_calls_and_rolls_back_invalid_candidates() {
    let original = definition();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [original.clone()]).await.unwrap();
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
            &ClientMessage::OffHook {
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("unexpected event: {event:?}"),
    };

    let added = definition_for("SEP112233445566");
    assert_eq!(
        handle
            .reconfigure([original.clone(), added.clone()])
            .await
            .unwrap(),
        ReconfigureResult {
            added: vec![added.id],
            ..ReconfigureResult::default()
        }
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    let mut invalid = original.clone();
    let ButtonDefinition::Line(line) = &mut invalid.buttons[0] else {
        panic!("expected line button");
    };
    line.instance = 0;
    assert!(handle.reconfigure([invalid]).await.is_err());

    phone
        .write_all(
            &ClientMessage::KeypadButton {
                button: Digit::Number(7),
                line_instance: 1,
                call_reference: 0,
                wire_layout: None,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Digit {
            call_id: same_call,
            digit: Digit::Number(7),
            ..
        } })) if same_call == call_id
    ));

    let mut changed = original;
    changed.description = "Changed station".into();
    let report = handle.reconfigure([changed]).await.unwrap();
    assert_eq!(
        report.changed,
        vec![DeviceId::new("SEP001122334455").unwrap()]
    );
    assert_eq!(
        report.removed,
        vec![DeviceId::new("SEP112233445566").unwrap()]
    );
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), events.recv()).await,
        Ok(Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::Disconnected {} })))
            if device_id == DeviceId::new("SEP001122334455").unwrap()
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn concurrent_registration_and_disconnect_storm_retires_every_session_once() {
    const PHONE_COUNT: usize = 48;
    let device_ids = (0..PHONE_COUNT)
        .map(|index| format!("SEP{index:012X}"))
        .collect::<Vec<_>>();
    let definitions = device_ids
        .iter()
        .map(|device_id| definition_for(device_id))
        .collect::<Vec<_>>();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, definitions).await.unwrap();
    let address = server.local_addr().unwrap();
    let server_task = tokio::spawn(server.run());
    let barrier = Arc::new(tokio::sync::Barrier::new(PHONE_COUNT));
    let mut registrations = tokio::task::JoinSet::new();
    for device_id in &device_ids {
        let barrier = Arc::clone(&barrier);
        let device_id = device_id.clone();
        registrations.spawn(async move {
            let mut phone = TcpStream::connect(address).await.unwrap();
            let mut decoder = FrameDecoder::new();
            barrier.wait().await;
            phone
                .write_all(&register_bytes_for_device(
                    ProtocolVersion::V22,
                    115,
                    &device_id,
                ))
                .await
                .unwrap();
            read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
            (device_id, phone)
        });
    }
    let mut phones = Vec::with_capacity(PHONE_COUNT);
    while let Some(result) = tokio::time::timeout(Duration::from_secs(5), registrations.join_next())
        .await
        .expect("registration storm exceeded its bound")
    {
        phones.push(result.unwrap());
    }
    assert_eq!(phones.len(), PHONE_COUNT);

    let mut registered = HashSet::new();
    while registered.len() < PHONE_COUNT {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("registration events exceeded their bound")
            .expect("server stopped during registration storm");
        if let Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(registration),
        }) = event
        {
            assert!(registered.insert(registration.id));
        }
    }
    drop(phones);

    let mut disconnected = HashSet::new();
    while disconnected.len() < PHONE_COUNT {
        let event = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("disconnect events exceeded their bound")
            .expect("server stopped during disconnect storm");
        if let Event::Device(DeviceEvent {
            session_generation: _,
            device_id,
            event: DeviceEventKind::Disconnected {},
        }) = event
        {
            assert!(disconnected.insert(device_id));
        }
    }
    assert_eq!(registered, disconnected);

    handle.shutdown().await.unwrap();
    server_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn repeated_server_load_and_unload_releases_all_shared_runtime_state() {
    const CYCLES: usize = 32;
    for _ in 0..CYCLES {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, events) = Server::bind(config, [definition()]).await.unwrap();
        let sessions = Arc::downgrade(&server.sessions);
        let call_ids = Arc::downgrade(&handle.next_call_id);
        let statistics = Arc::downgrade(&handle.latest_media_statistics);
        let answer_order = Arc::downgrade(&handle.call_answer_order);
        let server_task = tokio::spawn(server.run());

        handle.shutdown().await.unwrap();
        server_task.await.unwrap().unwrap();
        drop(events);
        drop(handle);

        assert!(sessions.upgrade().is_none());
        assert!(call_ids.upgrade().is_none());
        assert!(statistics.upgrade().is_none());
        assert!(answer_order.upgrade().is_none());
    }
}

#[tokio::test]
async fn reconfiguration_disconnects_a_removed_device_without_touching_its_peer() {
    let retained = definition_for("SEP001122334455");
    let removed = definition_for("SEP112233445566");
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [retained.clone(), removed.clone()])
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let protocol = ProtocolVersion::V22;

    let mut retained_phone = TcpStream::connect(address).await.unwrap();
    let mut retained_decoder = FrameDecoder::new();
    retained_phone
        .write_all(&register_bytes_for_device(
            protocol,
            115,
            retained.id.as_str(),
        ))
        .await
        .unwrap();
    read_until_message(
        &mut retained_phone,
        &mut retained_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));

    let mut removed_phone = TcpStream::connect(address).await.unwrap();
    let mut removed_decoder = FrameDecoder::new();
    removed_phone
        .write_all(&register_bytes_for_device(
            protocol,
            115,
            removed.id.as_str(),
        ))
        .await
        .unwrap();
    read_until_message(
        &mut removed_phone,
        &mut removed_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(_)
        }))
    ));

    let report = handle.reconfigure([retained.clone()]).await.unwrap();
    assert_eq!(report.removed, vec![removed.id.clone()]);
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), events.recv()).await,
        Ok(Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::Disconnected {} }))) if device_id == removed.id
    ));

    retained_phone
        .write_all(
            &Frame::new(protocol.wire(), wire_id::KEEP_ALIVE, Vec::new())
                .encode()
                .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(
        &mut retained_phone,
        &mut retained_decoder,
        wire_id::KEEP_ALIVE_ACK,
    )
    .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn malformed_pre_registration_frame_is_quietly_disconnected() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut peer = TcpStream::connect(address).await.unwrap();
    let mut malformed_header = [0_u8; 12];
    malformed_header[..4].copy_from_slice(&u32::MAX.to_le_bytes());

    peer.write_all(&malformed_header).await.unwrap();

    let mut byte = [0_u8; 1];
    let count = tokio::time::timeout(Duration::from_secs(1), peer.read(&mut byte))
        .await
        .expect("server did not close malformed pre-registration stream")
        .unwrap();
    assert_eq!(count, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "pre-registration scanner traffic emitted a public session error"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn standalone_server_registers_and_serves_line_status() {
    for protocol in [
        ProtocolVersion::V3,
        ProtocolVersion::V17,
        ProtocolVersion::V22,
    ] {
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
        let address = server.local_addr().unwrap();
        let task = tokio::spawn(server.run());
        let mut phone = TcpStream::connect(address).await.unwrap();
        let mut alarm_payload = vec![0; 2_000];
        let alarm = b"<?xml version=\"1.0\"?><x-cisco-alarm></x-cisco-alarm>";
        alarm_payload[..alarm.len()].copy_from_slice(alarm);
        phone
            .write_all(
                &Frame::new(0, wire_id::XML_ALARM, alarm_payload)
                    .encode()
                    .unwrap(),
            )
            .await
            .unwrap();
        phone.write_all(&register_bytes(protocol)).await.unwrap();
        let mut decoder = FrameDecoder::new();
        let mut buffer = [0_u8; 1024];
        let frames = read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
        let ack = frames
            .iter()
            .find(|frame| frame.message_id == wire_id::REGISTER_ACK)
            .cloned()
            .unwrap();
        assert_eq!(
            ServerMessage::decode(ack, protocol).unwrap(),
            ServerMessage::RegisterAck {
                keepalive_seconds: 30,
                secondary_keepalive_seconds: 30,
                protocol,
                features: PhoneFeatures::empty(),
                date_template: Default::default(),
            }
        );
        assert!(
            frames
                .iter()
                .any(|frame| frame.message_id == wire_id::CAPABILITIES_REQ)
        );
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Registered(_)
            }))
        ));

        let malformed = Frame::new(protocol.wire(), wire_id::IP_PORT, vec![0, 1])
            .encode()
            .unwrap();
        let keepalive = Frame::new(protocol.wire(), wire_id::KEEP_ALIVE, vec![0; 4])
            .encode()
            .unwrap();
        phone
            .write_all(&[malformed, keepalive].concat())
            .await
            .unwrap();
        let count = phone.read(&mut buffer).await.unwrap();
        let frames = decoder.push(&buffer[..count]).unwrap();
        assert!(
            frames
                .iter()
                .any(|frame| frame.message_id == wire_id::KEEP_ALIVE_ACK),
            "session did not survive a malformed application message"
        );
        assert!(matches!(
            events.recv().await,
            Some(Event::ProtocolWarning {
                message_id: wire_id::IP_PORT,
                ..
            })
        ));

        phone
            .write_all(
                &Frame::new(
                    protocol.wire(),
                    wire_id::LINE_STAT_REQ,
                    1_u32.to_le_bytes().to_vec(),
                )
                .encode()
                .unwrap(),
            )
            .await
            .unwrap();
        let line_message_id = if protocol >= ProtocolVersion::V17 {
            wire_id::LINE_STAT_DYNAMIC
        } else {
            wire_id::LINE_STAT
        };
        let frames = read_until_message(&mut phone, &mut decoder, line_message_id).await;
        let line = frames
            .iter()
            .find(|frame| frame.message_id == line_message_id)
            .unwrap();
        assert!(matches!(
            ServerMessage::decode(line.clone(), protocol).unwrap(),
            ServerMessage::LineStatus {
                directory_number,
                fully_qualified_display_name,
                display_label,
                ..
            } if directory_number == "1001"
                && fully_qualified_display_name == "Test phone"
                && display_label == "Desk 1001"
        ));

        phone
            .write_all(&ClientMessage::ServerRequest.encode(protocol).unwrap())
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, wire_id::SERVER_RES).await;
        let response = frames
            .into_iter()
            .find(|frame| frame.message_id == wire_id::SERVER_RES)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(response, protocol).unwrap(),
            ServerMessage::ServerResponse {
                servers: vec![SignalingServerEndpoint {
                    name: "sccp-protocol".into(),
                    address: address.ip(),
                    port: NonZeroU16::new(address.port()).unwrap(),
                }],
            }
        );

        let cancelled_call = CallId(9_001);
        handle
            .try_send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::CloseCall {
                    call_id: cancelled_call,
                },
            ))
            .unwrap();
        handle
            .try_offer_incoming_call_with_id(
                DeviceId::new("SEP001122334455").unwrap(),
                LineInstance::new(1),
                cancelled_call,
                CallInfo {
                    direction: crate::types::CallDirection::Inbound,
                    calling_name: "Cancelled caller".into(),
                    calling_number: "1009".into(),
                    called_name: "Desk".into(),
                    called_number: "1001".into(),
                    ..CallInfo::default()
                },
            )
            .unwrap();
        handle
            .try_send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::SetCallState {
                    call_id: cancelled_call,
                    state: CallState::Connected,
                },
            ))
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), phone.read(&mut buffer))
                .await
                .is_err(),
            "a call cancelled before its offer still rang the phone"
        );

        let incoming = handle
            .offer_incoming_call(
                DeviceId::new("SEP001122334455").unwrap(),
                LineInstance::new(1),
                CallInfo {
                    direction: crate::types::CallDirection::Inbound,
                    calling_name: "Caller".into(),
                    calling_number: "1002".into(),
                    called_name: "Desk".into(),
                    called_number: "1001".into(),
                    ..CallInfo::default()
                },
            )
            .await
            .unwrap();
        let frames = read_until_message(
            &mut phone,
            &mut decoder,
            if protocol >= ProtocolVersion::V8 {
                wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS
            } else {
                wire_id::DISPLAY_PROMPT_STATUS
            },
        )
        .await;
        assert!(
            frames
                .iter()
                .all(|frame| frame.message_id != wire_id::ACTIVATE_CALL_PLANE),
            "RingIn activated the call plane before answer"
        );

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Answer.wire_value(),
                    line_instance: 0,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_LAMP).await;
        let off_hook = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::CallState {
                        state: CallState::OffHook,
                        ..
                    })
                )
            })
            .expect("answer did not transition through OffHook");
        let activate = frames
            .iter()
            .position(|frame| frame.message_id == wire_id::ACTIVATE_CALL_PLANE)
            .expect("answer did not activate the call plane");
        assert!(
            off_hook < activate,
            "OffHook must precede call-plane activation"
        );
        let answer_event = events.recv().await;
        assert!(
            matches!(
                answer_event,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                    call_id: Some(answered),
                    soft_key: SoftKey::Answer,
                    ..
                } })) if answered == incoming
            ),
            "unexpected answer event: {answer_event:?}"
        );

        handle
            .send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::SetCallState {
                    call_id: incoming,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
        assert!(
            frames
                .iter()
                .any(|frame| frame.message_id == wire_id::SET_SPEAKER_MODE),
            "Connected did not enable the active audio accessory"
        );
        assert!(frames.iter().any(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::DisplayPrompt { text, .. }) if text == "Connected"
            )
        }));

        phone
            .write_all(
                &ClientMessage::OffHook {
                    line_instance: 1,
                    call_reference: incoming.0 as u32,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), phone.read_u8())
                .await
                .is_err(),
            "duplicate OffHook rewrote a connected call's handset UI"
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(50), events.recv())
                .await
                .is_err(),
            "duplicate OffHook emitted a second application event"
        );

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Hold.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                call_id: Some(held),
                soft_key: SoftKey::Hold,
                ..
            } })) if held == incoming
        ));
        handle
            .send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::SetCallState {
                    call_id: incoming,
                    state: CallState::Hold,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::Hold,
                ..
            })
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SetLamp {
                mode: LampMode::Wink,
                ..
            })
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SetSpeakerMode(SpeakerMode::Off))
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SelectSoftKeys {
                set: KeyMode::OnHold,
                ..
            })
        )));

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::Resume.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                call_id: Some(resumed),
                soft_key: SoftKey::Resume,
                ..
            } })) if resumed == incoming
        ));
        handle
            .send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::SetCallState {
                    call_id: incoming,
                    state: CallState::Connected,
                },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::Connected,
                ..
            })
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SetSpeakerMode(SpeakerMode::On))
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::SelectSoftKeys {
                set: KeyMode::Connected,
                ..
            })
        )));

        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: Digit::Number(5),
                    line_instance: 1,
                    call_reference: 0,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Digit {
                call_id,
                digit: Digit::Number(5),
                ..
            } })) if call_id == incoming
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), phone.read_u8())
                .await
                .is_err(),
            "connected DTMF emitted dial-collection UI"
        );

        phone
            .write_all(
                &ClientMessage::SoftKeyEvent {
                    event: SoftKey::EndCall.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
                call_id: Some(ended),
                soft_key: SoftKey::EndCall,
                ..
            } })) if ended == incoming
        ));

        handle
            .send(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::CloseCall { call_id: incoming },
            ))
            .await
            .unwrap();
        let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_RINGER).await;
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
            .expect("close did not send OnHook");
        let ringer_off = frames
            .iter()
            .position(|frame| {
                matches!(
                    ServerMessage::decode(frame.clone(), protocol),
                    Ok(ServerMessage::SetRinger {
                        mode: RingerMode::Off,
                        ..
                    })
                )
            })
            .expect("close did not stop the ringer");
        assert!(
            on_hook < ringer_off,
            "79x1 phones require OnHook before the final ringer-off indication"
        );

        let mut updated = definition();
        let crate::types::ButtonDefinition::Line(line) = &mut updated.buttons[0] else {
            panic!("expected line button");
        };
        line.label = Some("Updated desk".into());
        handle.reconfigure([updated]).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), events.recv()).await,
            Ok(Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::Disconnected {} })))
                if device_id.as_str() == "SEP001122334455"
        ));

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn injected_streams_enforce_station_transport_requirements() {
    for (requirement, transport, accepted) in [
        (
            StationTransportRequirement::Clear,
            StationTransport::Clear,
            true,
        ),
        (
            StationTransportRequirement::Clear,
            StationTransport::Secure,
            false,
        ),
        (
            StationTransportRequirement::Secure,
            StationTransport::Secure,
            true,
        ),
        (
            StationTransportRequirement::Secure,
            StationTransport::Clear,
            false,
        ),
        (
            StationTransportRequirement::Either,
            StationTransport::Clear,
            true,
        ),
        (
            StationTransportRequirement::Either,
            StationTransport::Secure,
            true,
        ),
    ] {
        let mut station = definition();
        station.transport = requirement;
        let config = ServerConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            advertised_address: Ipv4Addr::LOCALHOST,
            ..ServerConfig::default()
        };
        let (server, handle, mut events, ingress) =
            Server::with_ingress(config, [station]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        let peer = SocketAddr::from(([127, 0, 0, 1], 40_000));
        let local = SocketAddr::from(([127, 0, 0, 1], 2_000));
        ingress
            .accept(server_stream, peer, local, transport)
            .await
            .unwrap();
        phone
            .write_all(&register_bytes(ProtocolVersion::V22))
            .await
            .unwrap();

        let mut decoder = FrameDecoder::new();
        let expected = if accepted {
            wire_id::REGISTER_ACK
        } else {
            wire_id::REGISTER_REJECT
        };
        let frames = read_until_message(&mut phone, &mut decoder, expected).await;
        assert!(frames.iter().any(|frame| frame.message_id == expected));
        if accepted {
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _,
                    event: DeviceEventKind::Registered(registration),
                    ..
                })) if registration.transport == transport
            ));
        }

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn registration_tokens_apply_transport_priority_parity_and_configured_backoff() {
    let cases = [
        (
            RegistrationFallback::Reject,
            1,
            "SEP001122334455",
            StationTransport::Clear,
            false,
        ),
        (
            RegistrationFallback::ReturnToPrimary,
            1,
            "SEP001122334455",
            StationTransport::Clear,
            true,
        ),
        (
            RegistrationFallback::ReturnToPrimary,
            2,
            "SEP001122334455",
            StationTransport::Clear,
            false,
        ),
        (
            RegistrationFallback::DeviceIdOdd,
            2,
            "SEP001122334455",
            StationTransport::Clear,
            true,
        ),
        (
            RegistrationFallback::DeviceIdEven,
            2,
            "SEP001122334455",
            StationTransport::Clear,
            false,
        ),
    ];
    for (fallback, server_priority, device_id, transport, accepted) in cases {
        let station = definition();
        let config = ServerConfig {
            registration_tokens: RegistrationTokenPolicy {
                fallback,
                backoff: Duration::from_secs(75),
                server_priority,
            },
            ..ServerConfig::default()
        };
        let (server, handle, _events, ingress) = Server::with_ingress(config, [station]).unwrap();
        let task = tokio::spawn(server.run());
        let (server_stream, mut phone) = tokio::io::duplex(2_048);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_000)),
                SocketAddr::from(([127, 0, 0, 1], 2_000)),
                transport,
            )
            .await
            .unwrap();
        phone
            .write_all(
                &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                    device_id: DeviceId::new(device_id).unwrap(),
                    device_instance: 1,
                    address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                    device_type: DeviceType::from(115),
                    flags: 0,
                })
                .encode(ProtocolVersion::V17)
                .unwrap(),
            )
            .await
            .unwrap();
        let expected = if accepted {
            wire_id::REGISTER_TOKEN_ACK
        } else {
            wire_id::REGISTER_TOKEN_REJECT
        };
        let mut decoder = FrameDecoder::new();
        let frames = read_until_message(&mut phone, &mut decoder, expected).await;
        let response = frames
            .into_iter()
            .find(|frame| frame.message_id == expected)
            .unwrap();
        if accepted {
            assert_eq!(
                ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
                ServerMessage::RegisterTokenAck
            );
        } else {
            assert_eq!(
                ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
                ServerMessage::RegisterTokenReject {
                    backoff_seconds: 75,
                }
            );
        }
        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }

    let mut secure_station = definition();
    secure_station.transport = StationTransportRequirement::Secure;
    let config = ServerConfig {
        registration_tokens: RegistrationTokenPolicy {
            fallback: RegistrationFallback::ReturnToPrimary,
            backoff: Duration::from_secs(90),
            server_priority: 1,
        },
        ..ServerConfig::default()
    };
    let (server, handle, _events, ingress) =
        Server::with_ingress(config, [secure_station]).unwrap();
    let task = tokio::spawn(server.run());
    let (server_stream, mut phone) = tokio::io::duplex(2_048);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_001)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    phone
        .write_all(
            &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                device_instance: 1,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                device_type: DeviceType::from(115),
                flags: 0,
            })
            .encode(ProtocolVersion::V17)
            .unwrap(),
        )
        .await
        .unwrap();
    let mut decoder = FrameDecoder::new();
    let response = read_until_message(&mut phone, &mut decoder, wire_id::REGISTER_TOKEN_REJECT)
        .await
        .into_iter()
        .find(|frame| frame.message_id == wire_id::REGISTER_TOKEN_REJECT)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
        ServerMessage::RegisterTokenReject {
            backoff_seconds: 90,
        }
    );
    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn registration_token_parity_requires_a_canonical_sep_mac_identity() {
    let policy = |fallback| RegistrationTokenPolicy {
        fallback,
        server_priority: 2,
        ..RegistrationTokenPolicy::default()
    };
    assert!(
        policy(RegistrationFallback::DeviceIdOdd)
            .accepts(&DeviceId::new("SEP001122334455").unwrap())
    );
    assert!(
        policy(RegistrationFallback::DeviceIdEven)
            .accepts(&DeviceId::new("SEP001122334454").unwrap())
    );
    for device_id in ["ALICE1", "SEP1", "SEP00112233445Z"] {
        let device_id = DeviceId::new(device_id).unwrap();
        assert!(!policy(RegistrationFallback::DeviceIdOdd).accepts(&device_id));
        assert!(!policy(RegistrationFallback::DeviceIdEven).accepts(&device_id));
    }
    let return_to_primary = RegistrationTokenPolicy {
        fallback: RegistrationFallback::ReturnToPrimary,
        server_priority: 1,
        ..RegistrationTokenPolicy::default()
    };
    assert!(return_to_primary.accepts(&DeviceId::new("ALICE1").unwrap()));
}

#[tokio::test]
async fn failed_replacement_token_response_preserves_the_incumbent() {
    let config = ServerConfig {
        advertised_address: Ipv4Addr::LOCALHOST,
        registration_tokens: RegistrationTokenPolicy {
            fallback: RegistrationFallback::ReturnToPrimary,
            backoff: Duration::from_secs(75),
            server_priority: 1,
        },
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) =
        Server::with_ingress(config, [definition()]).unwrap();
    let task = tokio::spawn(server.run());
    let protocol = ProtocolVersion::V22;
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let (server_stream, mut phone) = tokio::io::duplex(8_192);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_080)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
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

    let (server_stream, mut contender) = tokio::io::duplex(8_192);
    ingress
        .accept(
            TokenRejectWriteFailure {
                inner: server_stream,
            },
            SocketAddr::from(([127, 0, 0, 1], 40_081)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    contender
        .write_all(
            &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                device_id: device_id.clone(),
                device_instance: 1,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                device_type: DeviceType::from(115),
                flags: 0,
            })
            .encode(ProtocolVersion::V17)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::SessionError { .. })
    ));

    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::BeginCall {
                line_instance: LineInstance::new(1),
                call_id: CallId(7_080),
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn duplicate_registration_token_retires_the_incumbent_before_retry() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        registration_tokens: RegistrationTokenPolicy {
            fallback: RegistrationFallback::ReturnToPrimary,
            backoff: Duration::from_secs(75),
            server_priority: 1,
        },
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let protocol = ProtocolVersion::V22;
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let call_id = CallId(7001);

    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
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
    let wire_call_reference = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE)
        .await
        .into_iter()
        .find_map(|frame| match ServerMessage::decode(frame, protocol) {
            Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
            _ => None,
        })
        .expect("begin call omitted its wire reference");
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
    let media_party = open_receive_request_party(&frames, protocol);
    phone
        .write_all(
            &ClientMessage::OpenReceiveChannelAck {
                status: MediaStatus::Ok,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                port: 4000,
                call_reference: wire_call_reference,
                passthrough_party_id: media_party,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _,
            event: DeviceEventKind::ReceiveChannelOpened {
                call_id: actual_call_id,
                ..
            },
            ..
        })) if actual_call_id == call_id
    ));

    let mut contender = TcpStream::connect(address).await.unwrap();
    let mut contender_decoder = FrameDecoder::new();
    contender
        .write_all(
            &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                device_id: device_id.clone(),
                device_instance: 1,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                device_type: DeviceType::from(115),
                flags: 0,
            })
            .encode(ProtocolVersion::V17)
            .unwrap(),
        )
        .await
        .unwrap();
    let response = read_until_message(
        &mut contender,
        &mut contender_decoder,
        wire_id::REGISTER_TOKEN_REJECT,
    )
    .await
    .into_iter()
    .find(|frame| frame.message_id == wire_id::REGISTER_TOKEN_REJECT)
    .unwrap();
    assert_eq!(
        ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
        ServerMessage::RegisterTokenReject {
            backoff_seconds: REPLACEMENT_REGISTRATION_BACKOFF_SECONDS,
        }
    );
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            device_id: actual_device_id,
            event: DeviceEventKind::Disconnected {},
            ..
        })) if actual_device_id == device_id
    ));
    let mut retry = TcpStream::connect(address).await.unwrap();
    let mut retry_decoder = FrameDecoder::new();
    retry
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut retry, &mut retry_decoder, wire_id::KEEP_ALIVE_ACK).await;
    retry
        .write_all(
            &ClientMessage::RegisterToken(crate::message::RegisterTokenMessage {
                device_id,
                device_instance: 1,
                address: IpAddr::V4(Ipv4Addr::LOCALHOST),
                device_type: DeviceType::from(115),
                flags: 0,
            })
            .encode(ProtocolVersion::V17)
            .unwrap(),
        )
        .await
        .unwrap();
    let response = read_until_message(&mut retry, &mut retry_decoder, wire_id::REGISTER_TOKEN_ACK)
        .await
        .into_iter()
        .find(|frame| frame.message_id == wire_id::REGISTER_TOKEN_ACK)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(response, ProtocolVersion::V17).unwrap(),
        ServerMessage::RegisterTokenAck
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn server_list_selects_ordered_endpoints_for_the_active_transport() {
    let config = ServerConfig {
        signaling_servers: vec![
            SignalingServerRoute {
                priority: 2,
                name: "backup".into(),
                address: "192.0.2.20".parse().unwrap(),
                clear_port: NonZeroU16::new(2001),
                secure_port: None,
            },
            SignalingServerRoute {
                priority: 1,
                name: "primary".into(),
                address: "192.0.2.10".parse().unwrap(),
                clear_port: NonZeroU16::new(2000),
                secure_port: NonZeroU16::new(2443),
            },
        ],
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) =
        Server::with_ingress(config, [definition()]).unwrap();
    let task = tokio::spawn(server.run());

    for (index, transport, expected) in [
        (
            0,
            StationTransport::Clear,
            vec![
                SignalingServerEndpoint {
                    name: "primary".into(),
                    address: "192.0.2.10".parse().unwrap(),
                    port: NonZeroU16::new(2000).unwrap(),
                },
                SignalingServerEndpoint {
                    name: "backup".into(),
                    address: "192.0.2.20".parse().unwrap(),
                    port: NonZeroU16::new(2001).unwrap(),
                },
            ],
        ),
        (
            1,
            StationTransport::Secure,
            vec![SignalingServerEndpoint {
                name: "primary".into(),
                address: "192.0.2.10".parse().unwrap(),
                port: NonZeroU16::new(2443).unwrap(),
            }],
        ),
    ] {
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 40_000 + index)),
                SocketAddr::from(([127, 0, 0, 1], if index == 0 { 2_000 } else { 2_443 })),
                transport,
            )
            .await
            .unwrap();
        let protocol = ProtocolVersion::V22;
        phone.write_all(&register_bytes(protocol)).await.unwrap();
        let mut decoder = FrameDecoder::new();
        read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
        loop {
            if matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    event: DeviceEventKind::Registered(_),
                    ..
                }))
            ) {
                break;
            }
        }
        phone
            .write_all(&ClientMessage::ServerRequest.encode(protocol).unwrap())
            .await
            .unwrap();
        let response = read_until_message(&mut phone, &mut decoder, wire_id::SERVER_RES)
            .await
            .into_iter()
            .find(|frame| frame.message_id == wire_id::SERVER_RES)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(response, protocol).unwrap(),
            ServerMessage::ServerResponse { servers: expected }
        );
    }

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn server_list_never_empties_when_routes_do_not_fit_the_session() {
    let config = ServerConfig {
        advertised_address: "192.0.2.99".parse().unwrap(),
        signaling_servers: vec![SignalingServerRoute {
            priority: 1,
            name: "secure-v6".into(),
            address: "2001:db8::20".parse().unwrap(),
            clear_port: None,
            secure_port: NonZeroU16::new(2443),
        }],
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) =
        Server::with_ingress(config, [definition()]).unwrap();
    let task = tokio::spawn(server.run());

    for (offset, transport, local_address, expected_address) in [
        (
            0,
            StationTransport::Clear,
            "192.0.2.30".parse().unwrap(),
            "192.0.2.30".parse().unwrap(),
        ),
        (
            1,
            StationTransport::Secure,
            "192.0.2.31".parse().unwrap(),
            "192.0.2.31".parse().unwrap(),
        ),
        (
            2,
            StationTransport::Secure,
            "2001:db8::30".parse().unwrap(),
            "192.0.2.99".parse().unwrap(),
        ),
    ] {
        let local = SocketAddr::new(
            local_address,
            if transport == StationTransport::Clear {
                2000
            } else {
                2443
            },
        );
        let (server_stream, mut phone) = tokio::io::duplex(8_192);
        ingress
            .accept(
                server_stream,
                SocketAddr::from(([127, 0, 0, 1], 41_000 + offset)),
                local,
                transport,
            )
            .await
            .unwrap();
        let protocol = ProtocolVersion::V3;
        phone.write_all(&register_bytes(protocol)).await.unwrap();
        let mut decoder = FrameDecoder::new();
        read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
        while !matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                event: DeviceEventKind::Registered(_),
                ..
            }))
        ) {}
        phone
            .write_all(&ClientMessage::ServerRequest.encode(protocol).unwrap())
            .await
            .unwrap();
        let response = read_until_message(&mut phone, &mut decoder, wire_id::SERVER_RES)
            .await
            .into_iter()
            .find(|frame| frame.message_id == wire_id::SERVER_RES)
            .unwrap();
        assert_eq!(
            ServerMessage::decode(response, protocol).unwrap(),
            ServerMessage::ServerResponse {
                servers: vec![SignalingServerEndpoint {
                    name: "sccp-protocol".into(),
                    address: expected_address,
                    port: NonZeroU16::new(local.port()).unwrap(),
                }]
            }
        );
    }

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn secondary_sessions_use_the_secondary_keepalive_deadline() {
    let config = ServerConfig {
        keepalive_seconds: 5,
        secondary_keepalive_seconds: 20,
        registration_tokens: RegistrationTokenPolicy {
            server_priority: 2,
            ..RegistrationTokenPolicy::default()
        },
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) =
        Server::with_ingress(config, [definition()]).unwrap();
    let (observation_tx, mut observations) = mpsc::channel(64);
    let server = server.with_observation_sender(observation_tx);
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
    let protocol = ProtocolVersion::V22;
    phone.write_all(&register_bytes(protocol)).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    let acknowledgement = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::REGISTER_ACK)
        .unwrap();
    assert_eq!(
        ServerMessage::decode(acknowledgement, protocol).unwrap(),
        ServerMessage::RegisterAck {
            keepalive_seconds: 5,
            secondary_keepalive_seconds: 20,
            protocol,
            features: PhoneFeatures::empty(),
            date_template: Default::default(),
        }
    );
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));

    tokio::time::advance(Duration::from_secs(59)).await;
    tokio::task::yield_now().await;
    assert!(events.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            event: DeviceEventKind::Disconnected {},
            ..
        }))
    ));
    assert_eq!(
        observed_disconnect_reason(&mut observations, 1).await,
        StationDisconnectReason::KeepaliveExpiry
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn observations_distinguish_peer_closure_from_io_failure() {
    let (server, handle, _events, ingress) =
        Server::with_ingress(ServerConfig::default(), [definition()]).unwrap();
    let (observation_tx, mut observations) = mpsc::channel(16);
    let server = server.with_observation_sender(observation_tx);
    let task = tokio::spawn(server.run());

    let (server_stream, phone) = tokio::io::duplex(64);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_000)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    drop(phone);
    assert_eq!(
        observed_disconnect_reason(&mut observations, 1).await,
        StationDisconnectReason::PeerClosure
    );

    ingress
        .accept(
            FailingStationIo,
            SocketAddr::from(([127, 0, 0, 1], 40_001)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    assert_eq!(
        observed_disconnect_reason(&mut observations, 2).await,
        StationDisconnectReason::IoFailure
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn observation_reports_deliberate_server_retirement() {
    let (server, handle, mut events, ingress) =
        Server::with_ingress(ServerConfig::default(), [definition()]).unwrap();
    let (observation_tx, mut observations) = mpsc::channel(64);
    let server = server.with_observation_sender(observation_tx);
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
    let protocol = ProtocolVersion::V22;
    phone.write_all(&register_bytes(protocol)).await.unwrap();
    let mut decoder = FrameDecoder::new();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));

    handle.shutdown().await.unwrap();
    assert_eq!(
        observed_disconnect_reason(&mut observations, 1).await,
        StationDisconnectReason::ServerRetirement
    );
    task.await.unwrap().unwrap();
}

async fn observed_disconnect_reason(
    observations: &mut mpsc::Receiver<ServerObservation>,
    expected_connection_id: u64,
) -> StationDisconnectReason {
    while let Some(observation) = observations.recv().await {
        if let ServerObservationKind::Disconnected {
            connection_id,
            reason,
        } = observation.kind
            && connection_id.get() == expected_connection_id
        {
            return reason;
        }
    }
    panic!("observation stream ended before connection {expected_connection_id} disconnected")
}

struct FailingStationIo;

impl tokio::io::AsyncRead for FailingStationIo {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        _buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "injected station read failure",
        )))
    }
}

impl tokio::io::AsyncWrite for FailingStationIo {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::task::Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}

#[tokio::test]
async fn unregister_acknowledges_then_retires_the_exact_session() {
    let config = ServerConfig {
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let device = definition();
    let device_id = device.id.clone();
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
    let protocol = ProtocolVersion::V22;
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

    phone
        .write_all(
            &ClientMessage::Unregister { reason: 0 }
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::UNREGISTER_ACK).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation,
            device_id: actual_device_id,
            event: DeviceEventKind::Disconnected {},
        })) if session_generation == generation && actual_device_id == device_id
    ));
    let mut byte = [0_u8; 1];
    assert_eq!(phone.read(&mut byte).await.unwrap(), 0);
    assert!(matches!(
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::ResetDevice {
                    reset_type: ResetType::Reset,
                },
            ))
            .await,
        Err(ServerError::CommandWrite(error))
            if error == ServerError::DeviceNotConnected(device_id).to_string()
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test(start_paused = true)]
async fn valid_station_traffic_refreshes_the_session_watchdog() {
    let config = ServerConfig {
        keepalive_seconds: 5,
        secondary_keepalive_seconds: 5,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) =
        Server::with_ingress(config, [definition()]).unwrap();
    let task = tokio::spawn(server.run());
    let (server_stream, mut phone) = tokio::io::duplex(8_192);
    ingress
        .accept(
            server_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_001)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    let protocol = ProtocolVersion::V22;
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

    tokio::time::advance(Duration::from_secs(14)).await;
    phone
        .write_all(&ClientMessage::TimeDateRequest.encode(protocol).unwrap())
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::DEFINE_TIME_DATE).await;
    tokio::time::advance(Duration::from_secs(14)).await;
    tokio::task::yield_now().await;
    assert!(events.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(2)).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Disconnected {},
            ..
        })) if session_generation == generation
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_phone_receives_typed_text_service_controls_and_priority() {
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

    let mut expected = CiscoIpPhoneText::new("Dispatch", "Read", "Café <ready> & waiting").unwrap();
    expected.soft_keys.push(CiscoIpPhoneSoftKeyItem {
        name: Some("Refresh".into()),
        position: PhoneSoftKeyPosition::new(1).unwrap(),
        url: Some("https://pbx.example/text?id=7&view=full".into()),
        url_down: None,
    });
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::ShowTextService {
                line_instance: LineInstance::new(2),
                call_reference: CallReference::new(42),
                transaction_id: TransactionId::new(73),
                priority: PhoneServicePriority::HIGH,
                document: expected.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    let message = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::USER_TO_DEVICE_DATA_V1)
        .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
        .unwrap();
    let ServerMessage::UserToDeviceDataV1(message) = message else {
        panic!("expected text application data");
    };
    assert_eq!(message.application_id, PHONE_TEXT_APPLICATION_ID);
    assert_eq!(message.line_instance, 2);
    assert_eq!(message.call_reference, 42);
    assert_eq!(message.transaction_id, 73);
    assert_eq!(message.sequence_flag, 2);
    assert_eq!(message.display_priority, 2);
    assert_eq!(CiscoIpPhoneText::from_xml(&message.data).unwrap(), expected);

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_phone_receives_typed_input_and_returns_ordered_submission() {
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

    let mut expected = CiscoIpPhoneInput::new(
        "Invite <guest>",
        "Enter details",
        "conference/44/invite",
        vec![
            CiscoIpPhoneInputItem {
                display_name: Some("Number".into()),
                parameter: PhoneInputParameterName::new("NUMBER").unwrap(),
                flags: PhoneInputFlags::Telephone,
                default_value: None,
            },
            CiscoIpPhoneInputItem {
                display_name: Some("Name".into()),
                parameter: PhoneInputParameterName::new("NAME").unwrap(),
                flags: PhoneInputFlags::Alphabetic,
                default_value: Some("François".into()),
            },
        ],
    )
    .unwrap();
    expected.soft_keys.push(CiscoIpPhoneSoftKeyItem {
        name: Some("Submit".into()),
        position: PhoneSoftKeyPosition::new(1).unwrap(),
        url: Some("SoftKey:Submit".into()),
        url_down: None,
    });
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::ShowInputService {
                line_instance: LineInstance::new(2),
                call_reference: CallReference::new(42),
                application_id: ApplicationId::new(9_092),
                transaction_id: TransactionId::new(73),
                priority: PhoneServicePriority::HIGH,
                document: expected.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    let message = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::USER_TO_DEVICE_DATA_V1)
        .map(|frame| ServerMessage::decode(frame, protocol).unwrap())
        .unwrap();
    let ServerMessage::UserToDeviceDataV1(message) = message else {
        panic!("expected input application data");
    };
    assert_eq!(message.application_id, 9_092);
    assert_eq!(message.line_instance, 2);
    assert_eq!(message.call_reference, 42);
    assert_eq!(message.transaction_id, 73);
    assert_eq!(message.sequence_flag, 2);
    assert_eq!(message.display_priority, 2);
    assert_eq!(
        CiscoIpPhoneInput::from_xml(&message.data).unwrap(),
        expected
    );

    phone
        .write_all(
            &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                application_id: 9_092,
                line_instance: 2,
                call_reference: 42,
                transaction_id: 73,
                sequence_flag: 2,
                display_priority: 2,
                conference_id: 42,
                application_instance_id: 9_092,
                routing: 1,
                data: b"conference/44/invite?NUMBER=555%2A12&NAME=Fran%C3%A7ois".to_vec(),
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id: _,
        event: DeviceEventKind::PhoneServiceResponse { response, .. },
    })) = events.recv().await
    else {
        panic!("expected typed input submission");
    };
    assert_eq!(response.routing.application_id, ApplicationId::new(9_092));
    assert_eq!(response.routing.line_instance, LineInstance::new(2));
    assert_eq!(response.routing.call_reference, CallReference::new(42));
    assert_eq!(response.routing.transaction_id, TransactionId::new(73));
    let PhoneServicePayload::Submission(submission) = response.payload else {
        panic!("expected typed input submission payload");
    };
    assert_eq!(submission.route, ["conference", "44", "invite"]);
    assert_eq!(
        submission.values_named("NUMBER").collect::<Vec<_>>(),
        ["555*12"]
    );
    assert_eq!(
        submission.values_named("NAME").collect::<Vec<_>>(),
        ["François"]
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_phone_receives_typed_background_selection_and_preview_commands() {
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

    let set = CiscoIpPhoneSetBackground::new(
        PhoneBackgroundHttpUrl::new("http://pbx.example/background.png").unwrap(),
        PhoneBackgroundHttpUrl::new("http://pbx.example/background-thumb.png").unwrap(),
    );
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::SetBackgroundImage {
                transaction_id: TransactionId::new(109),
                document: set.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::UserToDeviceDataV1(message))
            if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                && message.transaction_id == 109
                && CiscoIpPhoneSetBackground::from_xml(&message.data).unwrap() == set
    )));

    let preview = CiscoIpPhoneSetBackgroundPreview::new(
        PhoneBackgroundHttpUrl::new("http://pbx.example/background.png?preview=1").unwrap(),
    );
    handle
        .send(Command::new(
            device_id,
            CommandAction::PreviewBackgroundImage {
                transaction_id: TransactionId::new(110),
                document: preview.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::UserToDeviceDataV1(message))
            if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                && message.transaction_id == 110
                && CiscoIpPhoneSetBackgroundPreview::from_xml(&message.data).unwrap() == preview
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_phone_receives_typed_ringtone_command() {
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

    let document = CiscoIpPhoneSetRingTone::new(
        PhoneRingtoneUrl::new("http://pbx.example/ringtones/Classic.raw?locale=sv").unwrap(),
    );
    handle
        .send(Command::new(
            device_id,
            CommandAction::SetRingtone {
                transaction_id: TransactionId::new(112),
                document: document.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::UserToDeviceDataV1(message))
            if message.application_id == PHONE_RINGTONE_APPLICATION_ID
                && message.line_instance == 0
                && message.call_reference == 0
                && message.transaction_id == 112
                && message.sequence_flag == 2
                && message.display_priority == 0
                && message.conference_id == 0
                && message.application_instance_id == PHONE_RINGTONE_APPLICATION_ID
                && message.routing == 1
                && CiscoIpPhoneSetRingTone::from_xml(&message.data).unwrap() == document
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_phone_receives_typed_execute_image_status_tone_and_announcement_commands() {
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

    let execute = CiscoIpPhoneExecute::new(vec![
        CiscoIpPhoneExecuteItem::with_priority("App:Close:9093", PhoneExecutePriority::NORMAL)
            .unwrap(),
    ])
    .unwrap();
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::ExecutePhoneActions {
                line_instance: LineInstance::new(2),
                call_reference: CallReference::new(42),
                application_id: ApplicationId::new(9_093),
                transaction_id: TransactionId::new(73),
                priority: PhoneServicePriority::HIGH,
                document: execute.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::UserToDeviceDataV1(message))
            if message.application_id == 9_093
                && message.line_instance == 2
                && message.call_reference == 42
                && message.transaction_id == 73
                && message.display_priority == 2
                && CiscoIpPhoneExecute::from_xml(&message.data).unwrap() == execute
    )));

    let image = PhoneImageDocument::ImageFile(CiscoIpPhoneImageFile {
        keypad_target: None,
        application_id: Some("map".into()),
        on_focus_lost: None,
        on_focus_gained: None,
        on_minimized: None,
        on_closed: None,
        title: Some("Site map".into()),
        prompt: Some("Inspect".into()),
        soft_keys: Vec::new(),
        key_items: Vec::new(),
        location_x: Some(12),
        location_y: Some(8),
        url: PhoneImageUrl::new("https://pbx.example/site.png?view=all").unwrap(),
    });
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::ShowImageService {
                line_instance: LineInstance::new(2),
                call_reference: CallReference::new(42),
                application_id: ApplicationId::new(9_095),
                transaction_id: TransactionId::new(74),
                priority: PhoneServicePriority::LOW,
                document: image.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::UserToDeviceDataV1(message))
            if message.application_id == 9_095
                && message.line_instance == 2
                && message.call_reference == 42
                && message.transaction_id == 74
                && message.display_priority == 0
                && PhoneImageDocument::from_xml(&message.data).unwrap() == image
    )));

    let status = PhoneStatusDocument::File(CiscoIpPhoneStatusFile {
        text: Some("Queue ready".into()),
        timer_seconds: Some(10),
        location_x: Some(4),
        location_y: Some(8),
        url: PhoneImageUrl::new("https://pbx.example/status.png?queue=support").unwrap(),
    });
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::ShowStatusService {
                line_instance: LineInstance::new(2),
                call_reference: CallReference::new(42),
                application_id: ApplicationId::new(9_096),
                transaction_id: TransactionId::new(75),
                priority: PhoneServicePriority::NORMAL,
                document: status.clone(),
            },
        ))
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::UserToDeviceDataV1(message))
            if message.application_id == 9_096
                && message.line_instance == 2
                && message.call_reference == 42
                && message.transaction_id == 75
                && message.display_priority == 1
                && PhoneStatusDocument::from_xml(&message.data).unwrap() == status
    )));

    let call_id = CallId(7001);
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
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::StartTone {
                call_id,
                tone: Tone::RecorderWarning,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::START_TONE).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::StartTone {
            tone: Tone::RecorderWarning,
            direction: ToneDirection::User,
            line_instance: 1,
            call_reference: 7001,
        })
    )));

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetMicrophoneMode { enabled: false },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_MICROPHONE_MODE).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::SetMicrophoneMode(MicrophoneMode::Off))
    )));

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetRecordingStatus {
                call_id,
                active: true,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::RECORDING_STATUS).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::RecordingStatus {
            call_reference: 7001,
            active: true,
        })
    )));

    let conference_id = ConferenceId::new(44);
    let rejected = handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::StartAnnouncement {
                conference_id,
                announcements: vec![AnnouncementEntry {
                    locale: 1,
                    country: 46,
                    tone: Tone::Zip,
                }],
                end_of_ack: true,
                participant_ids: vec![ParticipantId::new(7), ParticipantId::new(9)],
                hearing_participant_mask: 0b11,
                play_mode: 2,
            },
        ))
        .await
        .unwrap_err();
    assert!(
        matches!(rejected, ServerError::CommandWrite(message) if message.contains("not a station command"))
    );

    // Rejecting a service-node message must not retire the handset
    // session or poison subsequent station UI delivery.
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetMicrophoneMode { enabled: true },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_MICROPHONE_MODE).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::SetMicrophoneMode(MicrophoneMode::On))
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_xml_alarms_route_typed_or_opaque_without_leaking_payloads() {
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

    let known = "<x-cisco-alarm><Alarm Name=\"LastOutOfServiceInformation\"><ParameterList><String name=\"DeviceName\">private-device-name</String><Enum name=\"ReasonForOutOfService\">25</Enum></ParameterList></Alarm></x-cisco-alarm>";
    phone
        .write_all(
            &ClientMessage::XmlAlarm(XmlAlarmMessage::from_xml(known).unwrap())
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id,
        event: DeviceEventKind::XmlAlarm { telemetry },
    })) = events.recv().await
    else {
        panic!("typed XML alarm event was not emitted");
    };
    assert_eq!(device_id, DeviceId::new("SEP001122334455").unwrap());
    assert_eq!(
        telemetry.summary(),
        Some(crate::phone::xml::PhoneAlarmSummary {
            kind: crate::phone::xml::PhoneAlarmKind::LastOutOfService,
            reason_for_out_of_service: Some(25),
        })
    );
    assert!(!format!("{telemetry:?}").contains("private-device-name"));

    let unknown = "<vendor-alarm><Credential>private-token</Credential></vendor-alarm>";
    phone
        .write_all(
            &ClientMessage::XmlAlarm(XmlAlarmMessage::from_xml(unknown).unwrap())
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id: _,
        event: DeviceEventKind::XmlAlarm { telemetry, .. },
    })) = events.recv().await
    else {
        panic!("opaque XML alarm event was not emitted");
    };
    assert!(telemetry.is_opaque());
    assert_eq!(telemetry.summary(), None);
    assert!(!format!("{telemetry:?}").contains("private-token"));

    phone
        .write_all(
            &ClientMessage::XmlAlarm(
                XmlAlarmMessage::from_xml(
                    "<x-cisco-alarm><Alarm Name=\"Unknown\">&undeclared;</Alarm></x-cisco-alarm>",
                )
                .unwrap(),
            )
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;
    assert!(
        frames
            .iter()
            .any(|frame| frame.message_id == wire_id::KEEP_ALIVE_ACK)
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn registered_location_information_routes_typed_or_opaque_without_leaking_fields() {
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

    let known = "<Interface1><wifi><BSSID>E8:ED:F3:10:29:FD</BSSID><SSID>private-network</SSID><APName>private-access-point</APName></wifi><OffPrem></OffPrem></Interface1>";
    phone
        .write_all(
            &ClientMessage::LocationInfo { xml: known.into() }
                .encode(protocol)
                .unwrap(),
        )
        .await
        .unwrap();
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id,
        event: DeviceEventKind::LocationInformation { telemetry },
    })) = events.recv().await
    else {
        panic!("typed location-information event was not emitted");
    };
    assert_eq!(device_id, DeviceId::new("SEP001122334455").unwrap());
    assert_eq!(
        telemetry.summary(),
        Some(crate::phone::xml::PhoneLocationSummary {
            kind: crate::phone::xml::PhoneLocationKind::WirelessInterface,
            off_premises: true,
        })
    );
    let crate::phone::xml::PhoneLocationTelemetry::WirelessInterface(location) = &telemetry else {
        panic!("known wireless location was not typed");
    };
    assert_eq!(
        location.wifi.bssid.octets(),
        [0xe8, 0xed, 0xf3, 0x10, 0x29, 0xfd]
    );
    assert_eq!(location.wifi.ssid, "private-network");
    assert_eq!(location.wifi.access_point_name, "private-access-point");
    let debug = format!("{telemetry:?}");
    assert!(!debug.contains("private-network"));
    assert!(!debug.contains("private-access-point"));
    assert!(!debug.contains("E8:ED:F3:10:29:FD"));

    let unknown = "<DeviceLocation><CivicAddress>private-building</CivicAddress></DeviceLocation>";
    phone
        .write_all(
            &ClientMessage::LocationInfo {
                xml: unknown.into(),
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id: _,
        event: DeviceEventKind::LocationInformation { telemetry, .. },
    })) = events.recv().await
    else {
        panic!("opaque location-information event was not emitted");
    };
    assert!(telemetry.is_opaque());
    assert_eq!(telemetry.summary(), None);
    assert!(!format!("{telemetry:?}").contains("private-building"));

    phone
        .write_all(
            &ClientMessage::LocationInfo {
                xml: "<Interface1>&undeclared;</Interface1>".into(),
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
    );

    phone
        .write_all(&ClientMessage::KeepAlive.encode(protocol).unwrap())
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::KEEP_ALIVE_ACK).await;
    assert!(
        frames
            .iter()
            .any(|frame| frame.message_id == wire_id::KEEP_ALIVE_ACK)
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}
