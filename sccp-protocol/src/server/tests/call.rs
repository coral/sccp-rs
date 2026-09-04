use super::support::*;

struct FinalPromptGate {
    inner: tokio::io::DuplexStream,
    armed: Arc<std::sync::atomic::AtomicBool>,
    blocked: Arc<std::sync::atomic::AtomicBool>,
    released: Arc<std::sync::atomic::AtomicBool>,
    fail: Arc<std::sync::atomic::AtomicBool>,
    waker: Arc<std::sync::Mutex<Option<std::task::Waker>>>,
}

impl tokio::io::AsyncRead for FinalPromptGate {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl tokio::io::AsyncWrite for FinalPromptGate {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        bytes: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        let message_id = bytes
            .get(8..12)
            .and_then(|bytes| <[u8; 4]>::try_from(bytes).ok())
            .map(u32::from_le_bytes);
        if self.armed.load(std::sync::atomic::Ordering::SeqCst)
            && message_id == Some(wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS)
        {
            if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
                return std::task::Poll::Ready(Err(std::io::Error::other(
                    "injected final-prompt write failure",
                )));
            }
            if self.released.load(std::sync::atomic::Ordering::SeqCst) {
                return std::pin::Pin::new(&mut self.inner).poll_write(context, bytes);
            }
            self.blocked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            *self.waker.lock().unwrap() = Some(context.waker().clone());
            return std::task::Poll::Pending;
        }
        std::pin::Pin::new(&mut self.inner).poll_write(context, bytes)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[test]
fn last_number_uses_the_configured_terminator_recording_policy() {
    let without_terminator = ServerConfig {
        dial_terminator: Digit::Star,
        record_dial_terminator: false,
        ..ServerConfig::default()
    };
    let with_terminator = ServerConfig {
        record_dial_terminator: true,
        ..without_terminator.clone()
    };

    assert_eq!(
        normalized_last_number(" 5551212* ", &without_terminator),
        Some("5551212".into())
    );
    assert_eq!(
        normalized_last_number(" 5551212* ", &with_terminator),
        Some("5551212*".into())
    );
    assert_eq!(
        normalized_last_number("5551212#", &without_terminator),
        Some("5551212#".into()),
        "non-terminator DTMF must remain part of the remembered number"
    );
    assert_eq!(normalized_last_number("***", &without_terminator), None);
}

#[tokio::test]
async fn invalid_server_dial_terminator_is_rejected_before_binding() {
    let result = Server::bind(
        ServerConfig {
            dial_terminator: Digit::Unknown(99),
            ..ServerConfig::default()
        },
        [definition()],
    )
    .await;
    assert!(matches!(result, Err(ServerError::InvalidConfig(_))));
}

#[test]
fn invalid_server_signaling_qos_is_rejected_for_external_ingress() {
    let result = Server::with_ingress(
        ServerConfig {
            signaling_qos: SignalingQos::new(64, 0),
            ..ServerConfig::default()
        },
        [definition()],
    );

    assert!(
        matches!(result, Err(ServerError::InvalidConfig(message)) if message.contains("DSCP 64"))
    );
}

#[test]
fn invalid_failover_policy_is_rejected_before_ingress_starts() {
    let route = |priority| SignalingServerRoute {
        priority,
        name: format!("node-{priority}"),
        address: IpAddr::V4(Ipv4Addr::new(192, 0, 2, priority)),
        clear_port: NonZeroU16::new(2000),
        secure_port: None,
    };
    let invalid = [
        ServerConfig {
            advertised_address: Ipv4Addr::UNSPECIFIED,
            ..ServerConfig::default()
        },
        ServerConfig {
            secondary_keepalive_seconds: 4,
            ..ServerConfig::default()
        },
        ServerConfig {
            registration_tokens: RegistrationTokenPolicy {
                backoff: Duration::from_secs(29),
                ..RegistrationTokenPolicy::default()
            },
            ..ServerConfig::default()
        },
        ServerConfig {
            signaling_servers: vec![route(1), route(1)],
            ..ServerConfig::default()
        },
        ServerConfig {
            signaling_servers: vec![route(2)],
            ..ServerConfig::default()
        },
        ServerConfig {
            signaling_servers: (1..=6).map(route).collect(),
            ..ServerConfig::default()
        },
    ];

    for config in invalid {
        assert!(matches!(
            Server::with_ingress(config, [definition()]),
            Err(ServerError::InvalidConfig(_))
        ));
    }
}

#[test]
fn expansion_module_reserves_exact_model_capacity_and_places_configured_keys() {
    let mut device = definition();
    device.buttons.extend([
        ButtonDefinition::AddonModule(crate::types::AddonModuleDefinition {
            slot: 1,
            device_type: DeviceType::CiscoAddon7914,
        }),
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
    ]);
    device.validate().unwrap();

    let layout = button_template(&device);
    assert_eq!(layout.len(), 15, "one base key plus fourteen sidecar keys");
    assert_eq!(
        &layout[..3],
        [
            ButtonTemplateEntry {
                instance: 1,
                button_type: ButtonType::Line,
            },
            ButtonTemplateEntry {
                instance: 1,
                button_type: ButtonType::SpeedDial,
            },
            ButtonTemplateEntry {
                instance: 1,
                button_type: ButtonType::DoNotDisturb,
            },
        ]
    );
    assert!(
        layout[3..]
            .iter()
            .all(|button| { button.instance == 0 && button.button_type == ButtonType::Unused })
    );

    let mut over_capacity = definition();
    over_capacity.buttons.push(ButtonDefinition::AddonModule(
        crate::types::AddonModuleDefinition {
            slot: 1,
            device_type: DeviceType::AddonSpa500s,
        },
    ));
    over_capacity
        .buttons
        .extend(std::iter::repeat_n(ButtonDefinition::Unused, 33));
    assert!(matches!(
        over_capacity.validate(),
        Err(CodecError::InvalidDefinition(message))
            if message.contains("more buttons than its addon module provides")
    ));
}

#[test]
fn shared_line_appearances_keep_distinct_instances_and_labels() {
    let logical_line = LineDefinition {
        number: "4100".into(),
        display_name: "Operations".into(),
    };
    let mut primary = LineAppearance::new(1, logical_line.clone());
    primary.label = Some("Operations primary".into());
    let mut shared = LineAppearance::new(2, logical_line);
    shared.label = Some("Operations shared".into());
    let device = DeviceDefinition {
        id: DeviceId::new("SEP00AABBCCDDEE").unwrap(),
        description: "Shared line phone".into(),
        transport: StationTransportRequirement::Either,
        signaling_qos: None,
        buttons: vec![
            ButtonDefinition::Line(primary),
            ButtonDefinition::Line(shared),
        ],
        soft_keys: SoftKeyProfile::default(),
        ui: Default::default(),
    };

    device.validate().unwrap();
    assert_eq!(
        line_status(&device, 1),
        Some(ServerMessage::LineStatus {
            instance: 1,
            directory_number: "4100".into(),
            fully_qualified_display_name: "Shared line phone".into(),
            display_label: "Operations primary".into(),
        })
    );
    assert_eq!(
        line_status(&device, 2),
        Some(ServerMessage::LineStatus {
            instance: 2,
            directory_number: "4100".into(),
            fully_qualified_display_name: "4100".into(),
            display_label: "Operations shared".into(),
        })
    );
    assert_eq!(line_status(&device, 3), None);
}

#[test]
fn primary_line_keeps_device_header_separate_from_button_label() {
    let mut appearance = LineAppearance::new(
        1,
        LineDefinition {
            number: "coral".into(),
            display_name: "ATP".into(),
        },
    );
    appearance.label = Some("ATP".into());
    let device = DeviceDefinition {
        id: DeviceId::new("SEP00AABBCCDDEE").unwrap(),
        description: "coral".into(),
        transport: StationTransportRequirement::Either,
        signaling_qos: None,
        buttons: vec![ButtonDefinition::Line(appearance)],
        soft_keys: SoftKeyProfile::default(),
        ui: Default::default(),
    };

    assert_eq!(
        line_status(&device, 1),
        Some(ServerMessage::LineStatus {
            instance: 1,
            directory_number: "coral".into(),
            fully_qualified_display_name: "coral".into(),
            display_label: "ATP".into(),
        })
    );
}

#[tokio::test]
async fn configured_speed_dial_creates_and_routes_an_exact_outbound_call() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let device = mixed_definition();
    let device_id = device.id.clone();
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
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::SpeedDial,
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

    let (call_id, line_instance) = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            device_id: actual_device,
            event:
                DeviceEventKind::OffHook {
                    call_id,
                    line_instance,
                },
            ..
        })) if actual_device == device_id => (call_id, line_instance),
        other => panic!("unexpected speed-dial setup event: {other:?}"),
    };
    assert_eq!(line_instance, LineInstance::new(1));
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            device_id: actual_device,
            event: DeviceEventKind::SpeedDial {
                call_id: actual_call,
                line_instance: LineInstance(1),
                ref number,
                await_further_digits: false,
            },
            ..
        })) if actual_device == device_id
            && actual_call == call_id
            && number == "2001"
    ));

    // Once the call is already collecting digits, another speed dial
    // targets that same exact call rather than creating a second call.
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::BlfSpeedDial,
                instance: 2,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            device_id: actual_device,
            event: DeviceEventKind::SpeedDial {
                call_id: actual_call,
                line_instance: LineInstance(1),
                ref number,
                await_further_digits: false,
            },
            ..
        })) if actual_device == device_id
            && actual_call == call_id
            && number == "2002"
    ));

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
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::SpeedDial,
                instance: 1,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), events.recv())
            .await
            .is_err(),
        "a station without feature bit 30 created a call beside an active call"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn multiple_active_call_feature_allows_speed_dial_beside_connected_call() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let device = mixed_definition();
    let device_id = device.id.clone();
    let (server, handle, mut events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    let protocol = ProtocolVersion::V22;

    phone
        .write_all(&register_bytes_with_features(
            protocol,
            PhoneFeatures::MULTIPLE_ACTIVE_CALLS,
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::SpeedDial,
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
    let first_call = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::OffHook { call_id, .. },
            ..
        })) => call_id,
        other => panic!("unexpected first speed-dial setup event: {other:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::SpeedDial {
                call_id,
                ref number,
                await_further_digits: false,
                ..
            },
            ..
        })) if call_id == first_call && number == "2001"
    ));
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id: first_call,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::BlfSpeedDial,
                instance: 2,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    let second_call = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::OffHook { call_id, .. },
            ..
        })) => call_id,
        other => panic!("unexpected additional speed-dial setup event: {other:?}"),
    };
    assert_ne!(second_call, first_call);
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::SpeedDial {
                call_id,
                ref number,
                await_further_digits: false,
                ..
            },
            ..
        })) if call_id == second_call && number == "2002"
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn speed_dial_await_further_digits_keeps_the_call_in_digit_collection() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let mut device = mixed_definition();
    device.ui.speed_dial_await_further_digits = true;
    let device_id = device.id.clone();
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
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::SpeedDial,
                instance: 1,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::DIALED_NUMBER).await;
    assert!(frames.iter().any(|frame| {
        ServerMessage::decode(frame.clone(), protocol).is_ok_and(|message| {
            matches!(
                message,
                ServerMessage::DialedNumber {
                    ref number,
                    ..
                } if number == "2001"
            )
        })
    }));
    let call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            device_id: actual_device,
            event: DeviceEventKind::OffHook { call_id, .. },
            ..
        })) if actual_device == device_id => call_id,
        other => panic!("unexpected awaiting speed-dial setup event: {other:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::SpeedDial {
                call_id: actual_call,
                ref number,
                await_further_digits: true,
                ..
            },
            ..
        })) if actual_call == call_id && number == "2001"
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn begin_call_creates_the_reserved_retrieval_identity() {
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
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance(1),
                call_id: CallId(7001),
                codec: Codec::Pcma,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::CallState {
            state: CallState::OffHook,
            line_instance: 1,
            call_reference: 7001,
        })
    )));

    let info = CallInfo {
        direction: crate::types::CallDirection::Inbound,
        calling_name: "Caller".into(),
        calling_number: "2100".into(),
        called_name: "Park 701".into(),
        called_number: "701".into(),
        original_called_name: "Reception".into(),
        original_called_number: "2000".into(),
        last_redirecting_name: "Front Desk".into(),
        last_redirecting_number: "2050".into(),
        original_redirect_reason: 2,
        last_redirect_reason: 4,
        party_restrictions: 0xf,
    };
    handle
        .send(Command::new(
            device_id,
            CommandAction::SetCallInfo {
                call_id: CallId(7001),
                info: info.clone(),
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_INFO_DYNAMIC).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::CallInfo {
            info: actual,
            line_instance: 1,
            call_reference: 7001,
        }) if actual == info
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn one_way_intercom_uses_restricted_keys_active_identity_and_microphone_frame() {
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

    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let call_id = CallId(7010);
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
                state: CallState::IntercomOneWay,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::CallState {
            state: CallState::IntercomOneWay,
            line_instance: 1,
            call_reference: 7010,
        })
    )));
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SelectSoftKeys {
            line_instance: 1,
            call_reference: 7010,
            set: KeyMode::OffHook,
            valid_mask: 1,
        })
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
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err()
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
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::SoftKey {
            call_id: Some(CallId(7010)),
            line_instance: LineInstance(1),
            soft_key: SoftKey::EndCall,
        } })) if actual_device == device_id
    ));

    handle
        .send_confirmed(Command::new(
            device_id,
            CommandAction::SetMicrophoneMode { enabled: false },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_MICROPHONE_MODE).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::SetMicrophoneMode(MicrophoneMode::Off))
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn unknown_device_type_receives_the_configured_generic_layout() {
    let device = mixed_definition();
    let expected = button_template(&device);
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
        .write_all(&register_bytes_for_device_type(
            ProtocolVersion::V22,
            0xffff_fffe,
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
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

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn on_hook_enbloc_creates_one_call_and_fieldless_on_hook_ends_it() {
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

    phone
        .write_all(
            &ClientMessage::EnblocCall {
                called_party: "8675309".into(),
                line_instance: 1,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let initial = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(initial.iter().any(|frame| {
        matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::OffHook,
                ..
            })
        )
    }));
    assert!(
        initial
            .iter()
            .all(|frame| frame.message_id != wire_id::DIALED_NUMBER)
    );

    let call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event:
                DeviceEventKind::OffHook {
                    call_id,
                    line_instance: LineInstance(1),
                    ..
                },
        })) => call_id,
        event => {
            panic!("expected addressable off-hook call before en-bloc routing, got {event:?}")
        }
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall {
            call_id: routed_call_id,
            line_instance: LineInstance(1),
            ref number,
            ..
        } })) if routed_call_id == call_id && number == "8675309"
    ));
    handle
        .send_confirmed(Command::new(
            DeviceId::new("SEP001122334455").unwrap(),
            CommandAction::CommitOutboundCall {
                call_id,
                info: CallInfo {
                    direction: crate::CallDirection::Outbound,
                    called_number: "8675309".into(),
                    ..CallInfo::default()
                },
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.message_id == wire_id::DIALED_NUMBER)
            .count(),
        1
    );
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "8675309"
    )));

    phone
        .write_all(
            &Frame::new(protocol.wire(), wire_id::ON_HOOK, Vec::new())
                .encode()
                .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_RINGER).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::CallState {
            state: CallState::OnHook,
            line_instance: 1,
            ..
        })
    )));
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OnHook {
                call_id: ended_call_id,
                line_instance: LineInstance(1),
            },
        })) if ended_call_id == call_id
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn redial_reuses_the_last_completed_number_on_the_selected_line() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let mut device = definition();
    device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::Redial, SoftKey::NewCall]);
    let ButtonDefinition::Line(line) = &mut device.buttons[0] else {
        panic!("test station lost its line button");
    };
    line.initial_tone = Tone::RecallDial;
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
            &ClientMessage::OffHook {
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    let speaker = frames
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::SetSpeakerMode(SpeakerMode::On))
            )
        })
        .expect("physical OffHook did not enable the speaker");
    let line_lamp = frames
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::SetLamp {
                    stimulus: ButtonType::Line,
                    mode: LampMode::On,
                    ..
                })
            )
        })
        .expect("physical OffHook did not enable the line lamp");
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
        .expect("physical OffHook did not publish OffHook");
    let activate = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::ACTIVATE_CALL_PLANE)
        .expect("physical OffHook did not activate the call plane");
    let prompt = frames
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::DisplayPrompt { ref text, .. }) if text == "Enter number"
            )
        })
        .expect("physical OffHook did not prompt for digits");
    let dial_tone = frames
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::StartTone {
                    tone: Tone::RecallDial,
                    ..
                })
            )
        })
        .expect("physical OffHook did not start dial tone");
    let soft_keys = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::SELECT_SOFT_KEYS)
        .expect("physical OffHook did not select off-hook keys");
    assert!(
        speaker < line_lamp
            && line_lamp < off_hook
            && off_hook < activate
            && activate < prompt
            && prompt < dial_tone
            && dial_tone < soft_keys
    );
    let call_reference = frames
        .iter()
        .find_map(
            |frame| match ServerMessage::decode(frame.clone(), protocol) {
                Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
                _ => None,
            },
        )
        .unwrap();
    let first_call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("unexpected first redial OffHook event: {event:?}"),
    };

    phone
        .write_all(
            &ClientMessage::EnblocCall {
                called_party: "5551212".into(),
                line_instance: 1,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall { ref number, .. } })) if number == "5551212"
    ));
    handle
        .send_confirmed(Command::new(
            DeviceId::new("SEP001122334455").unwrap(),
            CommandAction::CommitOutboundCall {
                call_id: first_call_id,
                info: CallInfo {
                    direction: crate::CallDirection::Outbound,
                    called_number: "5551212".into(),
                    ..CallInfo::default()
                },
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;

    phone
        .write_all(
            &ClientMessage::OnHook {
                line_instance: 1,
                call_reference,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SET_RINGER).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OnHook { .. }
        }))
    ));

    phone
        .write_all(
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::Redial.wire_value(),
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let initial = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    let redial_call_reference =
        initial.iter().find_map(
            |frame| match ServerMessage::decode(frame.clone(), protocol) {
                Ok(ServerMessage::CallState {
                    state: CallState::OffHook,
                    call_reference,
                    ..
                }) => Some(call_reference),
                _ => None,
            },
        );
    let redial_call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("unexpected redial OffHook event: {event:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall { ref number, .. } })) if number == "5551212"
    ));
    handle
        .send_confirmed(Command::new(
            DeviceId::new("SEP001122334455").unwrap(),
            CommandAction::CommitOutboundCall {
                call_id: redial_call_id,
                info: CallInfo {
                    direction: crate::CallDirection::Outbound,
                    called_number: "5551212".into(),
                    ..CallInfo::default()
                },
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "5551212"
    )));

    phone
        .write_all(
            &ClientMessage::OnHook {
                line_instance: 1,
                call_reference: redial_call_reference.unwrap(),
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SET_RINGER).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OnHook { .. }
        }))
    ));

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::LastNumberRedial,
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
    let stimulus_call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("unexpected stimulus-redial OffHook event: {event:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::EnblocCall { ref number, .. } })) if number == "5551212"
    ));
    handle
        .send_confirmed(Command::new(
            DeviceId::new("SEP001122334455").unwrap(),
            CommandAction::CommitOutboundCall {
                call_id: stimulus_call_id,
                info: CallInfo {
                    direction: crate::CallDirection::Outbound,
                    called_number: "5551212".into(),
                    ..CallInfo::default()
                },
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "5551212"
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn new_call_key_and_stimulus_support_dial_and_backspace() {
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
    let call_reference = frames
        .into_iter()
        .find_map(|frame| match ServerMessage::decode(frame, protocol) {
            Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
            _ => None,
        })
        .unwrap();
    let new_call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("unexpected new-call event: {event:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::SoftKey {
                call_id: Some(_),
                soft_key: SoftKey::NewCall,
                ..
            }
        }))
    ));

    for (index, digit) in [Digit::Number(1), Digit::Number(2)].into_iter().enumerate() {
        phone
            .write_all(
                &ClientMessage::KeypadButton {
                    button: digit,
                    line_instance: 1,
                    call_reference,
                    wire_layout: None,
                }
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        if index == 0 {
            let frames =
                read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
            assert!(
                frames
                    .iter()
                    .any(|frame| frame.message_id == wire_id::STOP_TONE)
            );
            assert!(
                frames
                    .iter()
                    .all(|frame| frame.message_id != wire_id::DIALED_NUMBER)
            );
        }
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::Digit { .. }
            }))
        ));
        if index == 1 {
            assert!(
                tokio::time::timeout(Duration::from_millis(50), phone.read_u8())
                    .await
                    .is_err(),
                "a repeated digit emitted redundant station UI"
            );
        }
    }

    phone
        .write_all(
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::Backspace.wire_value(),
                line_instance: 1,
                call_reference,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::BACKSPACE_RESPONSE).await;
    assert!(
        frames
            .iter()
            .all(|frame| frame.message_id != wire_id::DIALED_NUMBER)
    );
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::SoftKey {
                soft_key: SoftKey::Backspace,
                ..
            }
        }))
    ));

    phone
        .write_all(
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::Dial.wire_value(),
                line_instance: 1,
                call_reference,
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
            event: DeviceEventKind::SoftKey {
                soft_key: SoftKey::Dial,
                ..
            }
        }))
    ));

    handle
        .send(Command::new(
            DeviceId::new("SEP001122334455").unwrap(),
            CommandAction::CommitOutboundCall {
                call_id: new_call_id,
                info: CallInfo {
                    direction: crate::CallDirection::Outbound,
                    called_number: "1".into(),
                    ..CallInfo::default()
                },
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    let stop_tone = frames
        .iter()
        .position(|frame| frame.message_id == wire_id::STOP_TONE)
        .expect("dial commit did not stop tone");
    let dialed_number = frames
        .iter()
        .position(|frame| {
            matches!(
                ServerMessage::decode(frame.clone(), protocol),
                Ok(ServerMessage::DialedNumber { ref number, .. }) if number == "1"
            )
        })
        .expect("dial commit did not publish the complete number");
    let proceed = frames
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
        .expect("dial commit did not publish Proceed");
    assert!(stop_tone < dialed_number && dialed_number < proceed);
    assert_eq!(
        frames
            .iter()
            .filter(|frame| frame.message_id == wire_id::DIALED_NUMBER)
            .count(),
        1
    );

    phone
        .write_all(
            &ClientMessage::OnHook {
                line_instance: 1,
                call_reference,
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
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OnHook { .. }
        }))
    ));

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::NewCall,
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
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::SoftKey {
                call_id: Some(_),
                soft_key: SoftKey::NewCall,
                ..
            }
        }))
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn pickup_key_and_stimulus_create_an_addressable_call_before_dispatch() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let mut device = definition();
    device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::Pickup, SoftKey::GroupPickup]);
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
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::Pickup.wire_value(),
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    let call_reference = frames
        .into_iter()
        .find_map(|frame| match ServerMessage::decode(frame, protocol) {
            Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
            _ => None,
        })
        .unwrap();
    let call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("expected pickup OffHook event, got {event:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
            call_id: Some(event_call_id),
            soft_key: SoftKey::Pickup,
            ..
        } })) if event_call_id == call_id
    ));

    phone
        .write_all(
            &ClientMessage::OnHook {
                line_instance: 1,
                call_reference,
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
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OnHook { .. }
        }))
    ));

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::GroupCallPickup,
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
    let call_id = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("expected group-pickup OffHook event, got {event:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
            call_id: Some(event_call_id),
            soft_key: SoftKey::GroupPickup,
            ..
        } })) if event_call_id == call_id
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn meetme_key_and_stimulus_reserve_a_distinct_addressable_call() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let mut device = definition();
    device.soft_keys = SoftKeyProfile::new(
        KeyMode::ALL_KNOWN
            .iter()
            .copied()
            .map(|mode| (mode, vec![SoftKey::MeetMe])),
    )
    .unwrap();
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
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::MeetMe.wire_value(),
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    let first_reference = frames
        .into_iter()
        .find_map(|frame| match ServerMessage::decode(frame, protocol) {
            Ok(ServerMessage::CallState { call_reference, .. }) => Some(call_reference),
            _ => None,
        })
        .unwrap();
    let first_call = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("expected conference-destination OffHook event, got {event:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::MeetMe,
            ..
        } })) if call_id == first_call
    ));

    handle
        .send(Command::new(
            DeviceId::new("SEP001122334455").unwrap(),
            CommandAction::SetCallState {
                call_id: first_call,
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::MeetMeConference,
                instance: 1,
                call_reference: first_reference,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    let second_call = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook { call_id, .. },
        })) => call_id,
        event => panic!("expected a new conference-destination OffHook event, got {event:?}"),
    };
    assert_ne!(second_call, first_call);
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::MeetMe,
            ..
        } })) if call_id == second_call
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn omitted_call_reference_uses_active_then_configured_answer_order() {
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
    state.active_key_mode = KeyMode::RingIn;
    let first = insert_call(
        &mut state,
        CallId(10),
        1,
        Codec::Pcmu,
        CallState::CallWaiting,
    );
    let last = insert_call(&mut state, CallId(20), 2, Codec::Pcma, CallState::RingIn);

    assert_eq!(
        find_answer_call(&state, 0, 0, CallSelectionOrder::OldestFirst).map(|call| call.call_id),
        Some(CallId(10))
    );
    assert_eq!(
        find_answer_call(&state, 0, 0, CallSelectionOrder::LastFirst).map(|call| call.call_id),
        Some(CallId(20))
    );
    assert_eq!(
        find_answer_call(
            &state,
            first.wire_reference,
            1,
            CallSelectionOrder::LastFirst,
        )
        .map(|call| call.call_id),
        Some(CallId(10))
    );
    assert!(
        find_answer_call(
            &state,
            last.wire_reference,
            1,
            CallSelectionOrder::LastFirst,
        )
        .is_none()
    );
    assert_eq!(
        find_answer_call(&state, 0, 1, CallSelectionOrder::LastFirst).map(|call| call.call_id),
        Some(CallId(10))
    );

    state.active_call_id = Some(last.call_id);
    assert_eq!(
        find_call(&state, 0).map(|call| call.call_id),
        Some(CallId(20))
    );
    assert_eq!(
        find_answer_call(&state, 0, 0, CallSelectionOrder::OldestFirst).map(|call| call.call_id),
        Some(CallId(20))
    );
    remove_call(&mut state, CallId(20));
    assert_eq!(state.active_call_id, None);
    assert_eq!(
        find_call(&state, 0).map(|call| call.call_id),
        Some(CallId(10))
    );
}

#[test]
fn distinct_and_urgent_ring_modes_preserve_exact_waiting_semantics() {
    assert_eq!(
        incoming_ringer(Some(IncomingRing::default()), CallState::RingIn),
        Some(IncomingRing {
            mode: RingerMode::Inside,
            duration: RingDuration::Normal,
        })
    );
    assert_eq!(
        incoming_ringer(
            Some(IncomingRing {
                mode: RingerMode::Bellcore4,
                duration: RingDuration::Normal,
            }),
            CallState::RingIn,
        ),
        Some(IncomingRing {
            mode: RingerMode::Bellcore4,
            duration: RingDuration::Normal,
        })
    );
    assert_eq!(
        incoming_ringer(
            Some(IncomingRing {
                mode: RingerMode::Bellcore4,
                duration: RingDuration::Normal,
            }),
            CallState::CallWaiting,
        ),
        Some(IncomingRing {
            mode: RingerMode::Silent,
            duration: RingDuration::Single,
        })
    );
    assert_eq!(
        incoming_ringer(
            Some(IncomingRing {
                mode: RingerMode::Urgent,
                duration: RingDuration::Normal,
            }),
            CallState::CallWaiting,
        ),
        Some(IncomingRing {
            mode: RingerMode::Urgent,
            duration: RingDuration::Single,
        })
    );
    assert_eq!(incoming_ringer(None, CallState::CallWaiting), None);
    assert!(ringer_is_audible(IncomingRing::default()));
    assert!(!ringer_is_audible(IncomingRing {
        mode: RingerMode::Silent,
        duration: RingDuration::Single,
    }));
}

#[test]
fn call_waiting_never_owns_an_audible_ring_and_promotes_after_active_cleanup() {
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
    let active = insert_call(&mut state, CallId(1), 1, Codec::Pcmu, CallState::Connected);
    let waiting = insert_call(
        &mut state,
        CallId(2),
        1,
        Codec::Pcmu,
        CallState::CallWaiting,
    );
    state.calls_by_id.get_mut(&waiting.call_id).unwrap().ringer = Some(IncomingRing::default());
    state.ringer_owner = Some(CallId(9));

    let waiting_projection = incoming_ringer(
        state.calls_by_id[&waiting.call_id].ringer,
        CallState::CallWaiting,
    )
    .unwrap();
    assert!(!ringer_is_audible(waiting_projection));
    assert_eq!(state.ringer_owner, Some(CallId(9)));
    assert_eq!(
        incoming_successor(&state, active.call_id, CallSelectionOrder::OldestFirst),
        Some((waiting.call_id, true))
    );
    assert!(ringer_is_audible(
        incoming_ringer(
            state.calls_by_id[&waiting.call_id].ringer,
            CallState::RingIn,
        )
        .unwrap()
    ));
}

#[test]
fn answer_order_reload_updates_the_shared_policy_without_replacing_sessions() {
    let (command_tx, _command_rx) = mpsc::channel(1);
    let order = Arc::new(RwLock::new(CallSelectionOrder::OldestFirst));
    let handle = ServerHandle {
        command_tx,
        next_call_id: Arc::new(AtomicU64::new(1)),
        latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
        call_answer_order: Arc::clone(&order),
    };
    handle.set_call_answer_order(CallSelectionOrder::LastFirst);
    assert_eq!(
        *order.read().expect("test answer-order lock poisoned"),
        CallSelectionOrder::LastFirst
    );
}

#[test]
fn calendar_conversion_is_stable() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(19_358), (2023, 1, 1));
    assert_eq!(
        time_date_message_at(UNIX_EPOCH + Duration::from_secs(23 * 3_600 + 45 * 60), 30),
        ServerMessage::TimeDate {
            year: 1970,
            month: 1,
            weekday: 6,
            day: 2,
            hour: 0,
            minute: 15,
            second: 0,
            milliseconds: 0,
            unix_seconds: 87_300,
        }
    );
}

#[test]
fn call_history_distinguishes_answered_missed_and_elsewhere_answered() {
    assert_eq!(
        updated_history_disposition(CallHistoryDisposition::Missed, CallState::Connected),
        CallHistoryDisposition::Received
    );
    assert_eq!(
        updated_history_disposition(CallHistoryDisposition::Missed, CallState::OnHook),
        CallHistoryDisposition::Missed
    );
    assert_eq!(
        updated_history_disposition(CallHistoryDisposition::Missed, CallState::RemoteMultiline,),
        CallHistoryDisposition::Ignore
    );
    assert_eq!(
        updated_history_disposition(CallHistoryDisposition::Placed, CallState::Connected),
        CallHistoryDisposition::Placed
    );
}

#[test]
fn server_response_uses_the_accepted_local_interface_with_configured_fallback() {
    assert_eq!(
        server_response_address(
            "10.20.30.40".parse().unwrap(),
            "192.0.2.10".parse().unwrap(),
            Some("2001:db8::10".parse().unwrap()),
        ),
        "10.20.30.40".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        server_response_address(
            "2001:db8::20".parse().unwrap(),
            "192.0.2.10".parse().unwrap(),
            Some("2001:db8::10".parse().unwrap()),
        ),
        "2001:db8::20".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        server_response_address(
            "0.0.0.0".parse().unwrap(),
            "192.0.2.10".parse().unwrap(),
            Some("2001:db8::10".parse().unwrap()),
        ),
        "192.0.2.10".parse::<IpAddr>().unwrap()
    );
    assert_eq!(
        server_response_address(
            "::".parse().unwrap(),
            "192.0.2.10".parse().unwrap(),
            Some("2001:db8::10".parse().unwrap()),
        ),
        "2001:db8::10".parse::<IpAddr>().unwrap()
    );
}

#[test]
fn synchronous_offer_and_hangup_commands_cannot_overtake_each_other() {
    let (command_tx, mut command_rx) = mpsc::channel(4);
    let handle = ServerHandle {
        command_tx,
        next_call_id: Arc::new(AtomicU64::new(1)),
        latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
        call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
    };
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let call_id = CallId(42);
    handle
        .try_offer_incoming_call_with_id(
            device_id.clone(),
            LineInstance::new(1),
            call_id,
            CallInfo {
                direction: crate::types::CallDirection::Inbound,
                calling_name: "Caller".into(),
                calling_number: "1002".into(),
                called_name: "Desk".into(),
                called_number: "1001".into(),
                ..CallInfo::default()
            },
        )
        .unwrap();
    handle
        .try_send(Command::new(
            device_id.clone(),
            CommandAction::CloseCall { call_id },
        ))
        .unwrap();

    assert!(matches!(
        command_rx.try_recv().unwrap(),
        ServerCommand::OfferIncoming {
            device_id: offered_device,
            call_id: offered_call,
            ..
        } if offered_device == device_id && offered_call == call_id
    ));
    assert!(matches!(
        command_rx.try_recv().unwrap(),
        ServerCommand::Public(command)
            if matches!(command.as_ref(), Command {
                device_id: closed_device,
                action: CommandAction::CloseCall {
                call_id: closed_call,
            } } if closed_device == &device_id && *closed_call == call_id)
    ));
}

#[tokio::test]
async fn exact_session_offer_receipts_distinguish_stale_and_missing_sessions() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let device = definition();
    let device_id = device.id.clone();
    let (server, handle, _events) = Server::bind(config, [device]).await.unwrap();
    let (session_tx, _session_rx) = mpsc::channel(4);
    server.sessions.lock().await.insert(
        device_id.clone(),
        SessionSender {
            generation: SessionGeneration::new(2).unwrap(),
            anonymous_hotline: false,
            tx: session_tx,
            admission: Arc::new(SessionAdmission::new()),
        },
    );
    let task = tokio::spawn(server.run());
    let info = CallInfo {
        direction: crate::types::CallDirection::Inbound,
        calling_name: "Caller".into(),
        calling_number: "1002".into(),
        called_name: "Desk".into(),
        called_number: "1001".into(),
        ..CallInfo::default()
    };

    let stale = handle
        .try_offer_incoming_call_for_session(
            StationSessionTarget::new(device_id.clone(), SessionGeneration::new(1).unwrap()),
            LineInstance::new(1),
            CallId(40),
            info.clone(),
            IncomingPresentation::RingIn,
            Some(IncomingRing::default()),
        )
        .unwrap();
    assert_eq!(
        stale.wait().await.unwrap(),
        IncomingOfferDelivery::SessionStale {
            actual_generation: SessionGeneration::new(2).unwrap(),
        }
    );

    let missing = handle
        .try_offer_incoming_call_for_session(
            StationSessionTarget::new(
                DeviceId::new("SEPAABBCCDDEEFF").unwrap(),
                SessionGeneration::new(1).unwrap(),
            ),
            LineInstance::new(1),
            CallId(41),
            info,
            IncomingPresentation::CallWaiting,
            Some(IncomingRing::default()),
        )
        .unwrap();
    assert_eq!(
        missing.wait().await.unwrap(),
        IncomingOfferDelivery::SessionMissing
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn retirement_rejects_a_command_that_reserved_queue_capacity_first() {
    let device_id = definition().id;
    let (tx, mut rx) = mpsc::channel(1);
    let session = SessionSender {
        generation: SessionGeneration::new(1).unwrap(),
        anonymous_hotline: false,
        tx,
        admission: Arc::new(SessionAdmission::new()),
    };
    let tx = session.tx.clone();
    let permit = tx.reserve().await.unwrap();
    session.retire();
    assert!(
        session
            .admission
            .commit(
                permit,
                SessionCommand::Public(Box::new(Command::new(
                    device_id,
                    CommandAction::SetMicrophoneMode { enabled: true },
                )))
            )
            .is_err()
    );
    assert!(matches!(
        rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
}

#[tokio::test]
async fn replacement_resolves_an_admitted_exact_offer_as_stale() {
    let config = ServerConfig {
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) =
        Server::with_ingress(config, [definition()]).unwrap();
    let task = tokio::spawn(server.run());
    let (inner, mut phone) = tokio::io::duplex(8_192);
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waker = Arc::new(std::sync::Mutex::new(None));
    ingress
        .accept(
            FinalPromptGate {
                inner,
                armed: Arc::clone(&armed),
                blocked: Arc::clone(&blocked),
                released: Arc::clone(&released),
                fail,
                waker: Arc::clone(&waker),
            },
            SocketAddr::from(([127, 0, 0, 1], 40_082)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    let protocol = ProtocolVersion::V22;
    let device_id = definition().id;
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
    let info = CallInfo {
        direction: crate::types::CallDirection::Inbound,
        ..CallInfo::default()
    };

    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let presenting = handle
        .try_offer_incoming_call_for_session(
            StationSessionTarget::new(device_id.clone(), generation),
            LineInstance::new(1),
            CallId(60),
            info.clone(),
            IncomingPresentation::RingIn,
            Some(IncomingRing::default()),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !blocked.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let queued = handle
        .try_offer_incoming_call_for_session(
            StationSessionTarget::new(device_id, generation),
            LineInstance::new(1),
            CallId(61),
            info,
            IncomingPresentation::CallWaiting,
            Some(IncomingRing::default()),
        )
        .unwrap();

    let (replacement_stream, mut replacement) = tokio::io::duplex(8_192);
    ingress
        .accept(
            replacement_stream,
            SocketAddr::from(([127, 0, 0, 1], 40_083)),
            SocketAddr::from(([127, 0, 0, 1], 2_000)),
            StationTransport::Clear,
        )
        .await
        .unwrap();
    let mut replacement_decoder = FrameDecoder::new();
    replacement
        .write_all(&register_bytes(protocol))
        .await
        .unwrap();
    read_until_message(
        &mut replacement,
        &mut replacement_decoder,
        wire_id::CAPABILITIES_REQ,
    )
    .await;
    let replacement_generation = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Registered(_),
            ..
        })) => session_generation,
        event => panic!("expected replacement registration, got {event:?}"),
    };

    released.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(waker) = waker.lock().unwrap().take() {
        waker.wake();
    }
    assert_eq!(
        presenting.wait().await.unwrap(),
        IncomingOfferDelivery::Presented
    );
    assert_eq!(
        queued.wait().await.unwrap(),
        IncomingOfferDelivery::SessionStale {
            actual_generation: replacement_generation,
        }
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn incoming_offer_receipt_waits_for_final_prompt_and_reports_tombstones() {
    let config = ServerConfig {
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events, ingress) =
        Server::with_ingress(config, [definition()]).unwrap();
    let task = tokio::spawn(server.run());
    let (inner, mut phone) = tokio::io::duplex(8_192);
    let armed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let blocked = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let fail = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let waker = Arc::new(std::sync::Mutex::new(None));
    ingress
        .accept(
            FinalPromptGate {
                inner,
                armed: Arc::clone(&armed),
                blocked: Arc::clone(&blocked),
                released: Arc::clone(&released),
                fail: Arc::clone(&fail),
                waker: Arc::clone(&waker),
            },
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
    let generation = match events.recv().await {
        Some(Event::Device(DeviceEvent {
            session_generation,
            event: DeviceEventKind::Registered(_),
            ..
        })) => session_generation,
        event => panic!("expected registration, got {event:?}"),
    };
    let info = CallInfo {
        direction: crate::types::CallDirection::Inbound,
        calling_name: "Caller".into(),
        calling_number: "1002".into(),
        called_name: "Desk".into(),
        called_number: "1001".into(),
        ..CallInfo::default()
    };
    let device_id = definition().id;

    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let mut receipt = handle
        .try_offer_incoming_call_for_session(
            StationSessionTarget::new(device_id.clone(), generation),
            LineInstance::new(1),
            CallId(50),
            info.clone(),
            IncomingPresentation::RingIn,
            Some(IncomingRing::default()),
        )
        .unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !blocked.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(receipt.try_recv().unwrap(), None);
    released.store(true, std::sync::atomic::Ordering::SeqCst);
    if let Some(waker) = waker.lock().unwrap().take() {
        waker.wake();
    }
    assert_eq!(
        receipt.wait().await.unwrap(),
        IncomingOfferDelivery::Presented
    );
    read_until_message(
        &mut phone,
        &mut decoder,
        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
    )
    .await;

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::CloseCall {
                call_id: CallId(51),
            },
        ))
        .await
        .unwrap();
    let cancelled = handle
        .try_offer_incoming_call_for_session(
            StationSessionTarget::new(device_id.clone(), generation),
            LineInstance::new(1),
            CallId(51),
            info,
            IncomingPresentation::CallWaiting,
            Some(IncomingRing::default()),
        )
        .unwrap();
    assert_eq!(
        cancelled.wait().await.unwrap(),
        IncomingOfferDelivery::CancelledBeforePresentation
    );

    fail.store(true, std::sync::atomic::Ordering::SeqCst);
    let failed = handle
        .try_offer_incoming_call_for_session(
            StationSessionTarget::new(device_id.clone(), generation),
            LineInstance::new(1),
            CallId(52),
            CallInfo {
                direction: crate::types::CallDirection::Inbound,
                ..CallInfo::default()
            },
            IncomingPresentation::RingIn,
            Some(IncomingRing::default()),
        )
        .unwrap();
    assert_eq!(
        failed.wait().await.unwrap(),
        IncomingOfferDelivery::WriteFailed
    );
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            device_id: actual_device_id,
            session_generation,
            event: DeviceEventKind::Disconnected {},
        })) if actual_device_id == device_id && session_generation == generation
    ));
    assert!(matches!(
        events.recv().await,
        Some(Event::SessionError { .. })
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn synchronous_command_queue_reports_saturation_and_recovers_after_drain() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let handle = ServerHandle {
        command_tx,
        next_call_id: Arc::new(AtomicU64::new(1)),
        latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
        call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
    };
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    handle
        .try_send(Command::new(
            device_id.clone(),
            CommandAction::DisconnectDevice {},
        ))
        .unwrap();
    assert!(matches!(
        handle.try_send(Command::new(
            device_id.clone(),
            CommandAction::DisconnectDevice {}
        )),
        Err(ServerError::CommandQueueFull)
    ));
    assert!(matches!(
        handle.try_offer_incoming_call_with_id(
            device_id.clone(),
            LineInstance::new(1),
            CallId(42),
            CallInfo {
                direction: crate::types::CallDirection::Inbound,
                calling_name: "Caller".into(),
                calling_number: "1002".into(),
                called_name: "Desk".into(),
                called_number: "1001".into(),
                ..CallInfo::default()
            },
        ),
        Err(ServerError::CommandQueueFull)
    ));

    assert!(matches!(
        command_rx.try_recv().unwrap(),
        ServerCommand::Public(command)
            if matches!(command.as_ref(), Command {
                device_id: queued_device,
                action: CommandAction::DisconnectDevice { .. },
            } if queued_device == &device_id)
    ));
    handle
        .try_send(Command::new(
            device_id.clone(),
            CommandAction::DisconnectDevice {},
        ))
        .unwrap();
    assert!(matches!(
        command_rx.try_recv().unwrap(),
        ServerCommand::Public(command)
            if matches!(command.as_ref(), Command {
                device_id: queued_device,
                action: CommandAction::DisconnectDevice { .. },
            } if queued_device == &device_id)
    ));
}

#[tokio::test]
async fn confirmed_command_waits_for_device_write_and_propagates_failure() {
    let (command_tx, mut command_rx) = mpsc::channel(2);
    let handle = ServerHandle {
        command_tx,
        next_call_id: Arc::new(AtomicU64::new(1)),
        latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
        call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
    };
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    let success = tokio::spawn({
        let handle = handle.clone();
        let device_id = device_id.clone();
        async move {
            handle
                .send_confirmed(Command::new(
                    device_id,
                    CommandAction::StopAnnouncement {
                        conference_id: ConferenceId::new(44),
                    },
                ))
                .await
        }
    });
    let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
        panic!("expected a confirmed command")
    };
    assert!(!success.is_finished());
    written.send(Ok(())).unwrap();
    assert!(success.await.unwrap().is_ok());

    let failure = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .send_confirmed(Command::new(
                    device_id,
                    CommandAction::SetMicrophoneMode { enabled: false },
                ))
                .await
        }
    });
    let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
        panic!("expected a confirmed command")
    };
    written.send(Err("socket closed".into())).unwrap();
    assert!(matches!(
        failure.await.unwrap(),
        Err(ServerError::CommandWrite(message)) if message == "socket closed"
    ));
}

#[tokio::test(start_paused = true)]
async fn ordering_acknowledgement_timeout_bounds_a_stalled_writer_and_retires_sender() {
    let (command_tx, mut command_rx) = mpsc::channel(1);
    let handle = ServerHandle {
        command_tx,
        next_call_id: Arc::new(AtomicU64::new(1)),
        latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
        call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
    };
    let pending = tokio::spawn(async move {
        handle
            .send_confirmed(Command::new(
                DeviceId::new("SEP001122334455").unwrap(),
                CommandAction::SetMicrophoneMode { enabled: false },
            ))
            .await
    });
    let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
        panic!("expected a confirmed command")
    };

    tokio::time::advance(ORDERING_ACKNOWLEDGEMENT_TIMEOUT).await;
    assert!(matches!(
        pending.await.unwrap(),
        Err(ServerError::CommandAcknowledgementTimeout)
    ));
    assert!(written.send(Ok(())).is_err());
}

#[tokio::test(start_paused = true)]
async fn expired_confirmed_commands_are_retired_at_both_queue_boundaries() {
    let device = definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (mut server, handle, _events) = Server::bind(config, [device]).await.unwrap();
    let (session_tx, mut session_rx) = mpsc::channel(2);
    server.sessions.lock().await.insert(
        device_id.clone(),
        SessionSender {
            generation: SessionGeneration::new(1).unwrap(),
            anonymous_hotline: false,
            tx: session_tx,
            admission: Arc::new(SessionAdmission::new()),
        },
    );

    let server_queued = tokio::spawn({
        let handle = handle.clone();
        let device_id = device_id.clone();
        async move {
            handle
                .send_confirmed(Command::new(
                    device_id,
                    CommandAction::SetMicrophoneMode { enabled: false },
                ))
                .await
        }
    });
    let ServerCommand::Confirmed {
        command,
        written,
        expires_at,
    } = server.command_rx.recv().await.unwrap()
    else {
        panic!("expected a server-queued confirmed command")
    };
    tokio::time::advance(ORDERING_ACKNOWLEDGEMENT_TIMEOUT).await;
    assert!(matches!(
        server_queued.await.unwrap(),
        Err(ServerError::CommandAcknowledgementTimeout)
    ));
    server
        .dispatch_confirmed(command, written, expires_at)
        .await;
    assert!(matches!(
        session_rx.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));

    let session_queued = tokio::spawn({
        let handle = handle.clone();
        async move {
            handle
                .send_confirmed(Command::new(
                    device_id,
                    CommandAction::SetMicrophoneMode { enabled: true },
                ))
                .await
        }
    });
    let ServerCommand::Confirmed {
        command,
        written,
        expires_at,
    } = server.command_rx.recv().await.unwrap()
    else {
        panic!("expected another server-queued confirmed command")
    };
    server
        .dispatch_confirmed(command, written, expires_at)
        .await;
    let queued = session_rx.recv().await.unwrap();
    tokio::time::advance(ORDERING_ACKNOWLEDGEMENT_TIMEOUT).await;
    assert!(matches!(
        session_queued.await.unwrap(),
        Err(ServerError::CommandAcknowledgementTimeout)
    ));
    assert!(prepare_session_command(queued).is_none());
}

#[tokio::test]
async fn forwarding_collection_commands_propagate_confirmed_writer_failures() {
    let (command_tx, mut command_rx) = mpsc::channel(3);
    let handle = ServerHandle {
        command_tx,
        next_call_id: Arc::new(AtomicU64::new(1)),
        latest_media_statistics: Arc::new(RwLock::new(HashMap::new())),
        call_answer_order: Arc::new(RwLock::new(CallSelectionOrder::OldestFirst)),
    };
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let commands = [
        Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance(1),
                call_id: CallId(42),
                codec: Codec::Pcmu,
            },
        ),
        Command::new(
            device_id.clone(),
            CommandAction::DisplayPrompt {
                call_id: CallId(42),
                timeout_seconds: 0,
                text: "Enter forwarding destination".into(),
            },
        ),
        Command::new(
            device_id,
            CommandAction::CloseCall {
                call_id: CallId(42),
            },
        ),
    ];

    for (index, command) in commands.into_iter().enumerate() {
        let pending = tokio::spawn({
            let handle = handle.clone();
            async move { handle.send_confirmed(command).await }
        });
        let ServerCommand::Confirmed { written, .. } = command_rx.recv().await.unwrap() else {
            panic!("expected a confirmed forwarding command")
        };
        assert!(!pending.is_finished());
        written
            .send(Err(format!("forwarding writer failed at stage {index}")))
            .unwrap();
        assert!(matches!(
            pending.await.unwrap(),
            Err(ServerError::CommandWrite(message))
                if message == format!("forwarding writer failed at stage {index}")
        ));
    }
}

#[tokio::test]
async fn two_phone_shared_offer_honors_ring_policy_and_remote_control_events() {
    let protocol = ProtocolVersion::V22;
    let first_id = DeviceId::new("SEP001122334455").unwrap();
    let second_id = DeviceId::new("SEP112233445566").unwrap();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let mut first_definition = definition_for(first_id.as_str());
    let mut second_definition = definition_for(second_id.as_str());
    for definition in [&mut first_definition, &mut second_definition] {
        definition.soft_keys = profile_with(
            KeyMode::OnHookStealable,
            vec![
                SoftKey::Intercept,
                SoftKey::Barge,
                SoftKey::Conference,
                SoftKey::NewCall,
            ],
        );
    }
    let stealable_mask = second_definition
        .soft_keys
        .valid_mask(KeyMode::OnHookStealable);
    let (server, handle, mut events) = Server::bind(config, [first_definition, second_definition])
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut first = TcpStream::connect(address).await.unwrap();
    let mut second = TcpStream::connect(address).await.unwrap();
    let mut first_decoder = FrameDecoder::new();
    let mut second_decoder = FrameDecoder::new();
    first
        .write_all(&register_bytes_for_device(protocol, 115, first_id.as_str()))
        .await
        .unwrap();
    second
        .write_all(&register_bytes_for_device(
            protocol,
            115,
            second_id.as_str(),
        ))
        .await
        .unwrap();
    read_until_message(&mut first, &mut first_decoder, wire_id::REGISTER_ACK).await;
    read_until_message(&mut second, &mut second_decoder, wire_id::REGISTER_ACK).await;
    let mut registered = HashSet::new();
    while registered.len() < 2 {
        if let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::Registered(registration),
        })) = events.recv().await
        {
            registered.insert(registration.id);
        }
    }
    assert_eq!(
        registered,
        HashSet::from([first_id.clone(), second_id.clone()])
    );

    let info = CallInfo {
        direction: crate::types::CallDirection::Inbound,
        calling_name: "Caller".into(),
        calling_number: "1002".into(),
        called_name: "Shared desk".into(),
        called_number: "1001".into(),
        ..CallInfo::default()
    };
    let first_call = CallId(101);
    let second_call = CallId(102);
    handle
        .try_offer_incoming_call_with_id_and_ring(
            first_id.clone(),
            LineInstance::new(1),
            first_call,
            info.clone(),
            true,
        )
        .unwrap();
    handle
        .try_offer_incoming_call_with_id_and_ring(
            second_id.clone(),
            LineInstance::new(1),
            second_call,
            info,
            false,
        )
        .unwrap();
    let first_frames = read_until_message(
        &mut first,
        &mut first_decoder,
        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
    )
    .await;
    let second_frames = read_until_message(
        &mut second,
        &mut second_decoder,
        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
    )
    .await;
    assert!(first_frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SetRinger {
            mode: RingerMode::Inside,
            ..
        })
    )));
    assert!(!second_frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SetRinger {
            mode: RingerMode::Inside,
            ..
        })
    )));

    for (device_id, call_id) in [
        (first_id.clone(), first_call),
        (second_id.clone(), second_call),
    ] {
        handle
            .send(Command::new(
                device_id,
                CommandAction::SetCallState {
                    call_id,
                    state: CallState::RemoteMultiline,
                },
            ))
            .await
            .unwrap();
    }
    let first_frames =
        read_until_message(&mut first, &mut first_decoder, wire_id::SELECT_SOFT_KEYS).await;
    let second_frames =
        read_until_message(&mut second, &mut second_decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(first_frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SetRinger {
            mode: RingerMode::Off,
            ..
        })
    )));
    assert!(first_frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SetLamp {
            stimulus: ButtonType::Line,
            mode: LampMode::On,
            ..
        })
    )));
    assert!(second_frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::SelectSoftKeys {
            set: KeyMode::OnHookStealable,
            valid_mask,
            ..
        }) if valid_mask == stealable_mask
    )));

    second
        .write_all(
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::Intercept.wire_value(),
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
        Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Intercept,
            ..
        } })) if device_id == second_id && call_id == second_call
    ));
    second
        .write_all(
            &ClientMessage::SoftKeyEvent {
                event: SoftKey::Barge.wire_value(),
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
        Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Barge,
            ..
        } })) if device_id == second_id && call_id == second_call
    ));
    second
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::Conference,
                instance: 1,
                call_reference: 0,
                status: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Conference,
            ..
        } })) if device_id == second_id && call_id == second_call
    ));
    second
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
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::LineButton {
            call_id: Some(call_id),
            ..
        } })) if device_id == second_id && call_id == second_call
    ));

    second
        .write_all(
            &ClientMessage::OnHook {
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    read_until_message(&mut second, &mut second_decoder, wire_id::DEFINE_TIME_DATE).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::OnHook {
            call_id,
            ..
        } })) if device_id == second_id && call_id == second_call
    ));
    handle
        .send(Command::new(
            second_id.clone(),
            CommandAction::SetCallState {
                call_id: second_call,
                state: CallState::RemoteMultiline,
            },
        ))
        .await
        .unwrap();
    let restored =
        read_until_message(&mut second, &mut second_decoder, wire_id::SELECT_SOFT_KEYS).await;
    assert!(restored.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::CallState {
            state: CallState::RemoteMultiline,
            ..
        })
    )));

    for (device_id, call_id) in [
        (first_id.clone(), first_call),
        (second_id.clone(), second_call),
    ] {
        handle
            .send(Command::new(
                device_id,
                CommandAction::CloseCall { call_id },
            ))
            .await
            .unwrap();
    }
    let first_close = read_until_message(&mut first, &mut first_decoder, wire_id::CALL_STATE).await;
    let second_close =
        read_until_message(&mut second, &mut second_decoder, wire_id::CALL_STATE).await;
    for frames in [first_close, second_close] {
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: CallState::OnHook,
                ..
            })
        )));
    }

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn headset_and_accessory_changes_are_typed_and_duplicate_stable() {
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
    phone
        .write_all(
            &ClientMessage::HeadsetStatus { enabled: true }
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
            event: DeviceEventKind::HeadsetStatusChanged { enabled: true, .. }
        }))
    ));
    phone
        .write_all(
            &ClientMessage::MediaPathEvent {
                path: crate::MediaPathId::Speaker,
                event: crate::MediaPathEvent::On,
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
            event: DeviceEventKind::MediaPathChanged {
                path: crate::MediaPathId::Speaker,
                event: crate::MediaPathEvent::On,
                ..
            }
        }))
    ));
    phone
        .write_all(
            &ClientMessage::MediaPathEvent {
                path: crate::MediaPathId::Speaker,
                event: crate::MediaPathEvent::On,
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

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn ipv6_signaling_requires_extended_layouts_and_preserves_station_addresses() {
    let config = ServerConfig {
        bind: "[::1]:0".parse().unwrap(),
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    assert!(address.is_ipv6());
    let task = tokio::spawn(server.run());

    let mut legacy = TcpStream::connect(address).await.unwrap();
    legacy
        .write_all(&register_bytes(ProtocolVersion::V3))
        .await
        .unwrap();
    let mut legacy_decoder = FrameDecoder::new();
    let rejection = read_until_message(&mut legacy, &mut legacy_decoder, wire_id::REGISTER_REJECT)
        .await
        .into_iter()
        .find(|frame| frame.message_id == wire_id::REGISTER_REJECT)
        .unwrap();
    assert!(matches!(
        ServerMessage::decode(rejection, ProtocolVersion::V3).unwrap(),
        ServerMessage::RegisterReject { reason } if reason == "IPv6 requires protocol v17"
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

    let protocol = ProtocolVersion::V22;
    let reported_ipv6: Ipv6Addr = "2001:db8::42".parse().unwrap();
    let registration = ClientMessage::Register(RegistrationMessage {
        device_id: DeviceId::new("SEP001122334455").unwrap(),
        reported_address: None,
        reported_ipv6_address: Some(reported_ipv6),
        device_type: DeviceType::Cisco7962,
        advertised_protocol: Some(protocol.wire()),
        features: PhoneFeatures::empty(),
        firmware: "test-load".into(),
        configuration_version_stamp: crate::message::BoundedBytes::default(),
        wire: None,
    });
    let mut phone = TcpStream::connect(address).await.unwrap();
    phone
        .write_all(&registration.encode(protocol).unwrap())
        .await
        .unwrap();
    let mut decoder = FrameDecoder::new();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::Registered(DeviceRegistration {
            peer,
            transport: StationTransport::Clear,
            reported_address: None,
            reported_ipv6_address: Some(reported),
            ..
        }),
            ..
        })) if peer.is_ipv6() && reported == reported_ipv6
    ));

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
                address: address.ip(),
                port: NonZeroU16::new(address.port()).unwrap(),
            }],
        }
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn status_messages_preserve_persistence_timeout_priority_and_phone_family() {
    let mut persistent = false;
    assert!(matches!(
        status_message_frames(
            HandsetStatusMessage::Display {
                text: "Persistent".into(),
                timeout_seconds: 0,
                priority: None,
            },
            DeviceType::Cisco7960,
            &mut persistent,
        )
        .as_slice(),
        [ServerMessage::DisplayPrompt {
            timeout_seconds: 0,
            line_instance: 0,
            call_reference: 0,
            text,
        }] if text == "Persistent"
    ));
    assert!(persistent);
    assert_eq!(
        status_message_frames(
            HandsetStatusMessage::Clear { priority: None },
            DeviceType::Cisco7960,
            &mut persistent,
        ),
        [
            ServerMessage::ClearPrompt {
                line_instance: 0,
                call_reference: 0,
            },
            ServerMessage::ClearPriorityNotify {
                priority: NotificationPriority::Timed,
            },
        ]
    );
    assert!(!persistent);

    assert!(matches!(
        status_message_frames(
            HandsetStatusMessage::Display {
                text: "Timed".into(),
                timeout_seconds: 9,
                priority: None,
            },
            DeviceType::Cisco7960,
            &mut persistent,
        )
        .as_slice(),
        [ServerMessage::DisplayPriorityNotify {
            timeout_seconds: 9,
            priority: NotificationPriority::Timed,
            text,
        }] if text == "Timed"
    ));
    assert!(matches!(
        status_message_frames(
            HandsetStatusMessage::Display {
                text: "Timed".into(),
                timeout_seconds: 9,
                priority: None,
            },
            DeviceType::Cisco6945,
            &mut persistent,
        )
        .as_slice(),
        [ServerMessage::DisplayPrompt {
            timeout_seconds: 9,
            line_instance: 0,
            call_reference: 0,
            ..
        }]
    ));
}

#[test]
fn every_status_priority_round_trips_through_typed_frames() {
    for priority in NotificationPriority::ALL_KNOWN {
        let mut persistent = false;
        assert_eq!(
            status_message_frames(
                HandsetStatusMessage::Display {
                    text: "Priority".into(),
                    timeout_seconds: 5,
                    priority: Some(*priority),
                },
                DeviceType::Cisco7960,
                &mut persistent,
            ),
            [ServerMessage::DisplayPriorityNotify {
                timeout_seconds: 5,
                priority: *priority,
                text: "Priority".into(),
            }]
        );
        assert_eq!(
            status_message_frames(
                HandsetStatusMessage::Clear {
                    priority: Some(*priority),
                },
                DeviceType::Cisco7960,
                &mut persistent,
            ),
            [ServerMessage::ClearPriorityNotify {
                priority: *priority,
            }]
        );
    }
}

#[test]
fn announcement_command_mapping_preserves_typed_ids_and_wire_bounds() {
    let message = start_announcement_message(
        ConferenceId::new(44),
        vec![AnnouncementEntry {
            locale: 1,
            country: 46,
            tone: Tone::Zip,
        }],
        true,
        vec![ParticipantId::new(7), ParticipantId::new(9)],
        0b11,
        2,
    );
    assert!(matches!(
        message,
        ServerMessage::StartAnnouncement {
            conference_id: 44,
            end_of_ack: 1,
            ref matrix_conference_party_ids,
            ..
        } if matrix_conference_party_ids == &[7, 9]
    ));
    assert!(matches!(
        message.encode(ProtocolVersion::V22),
        Err(CodecError::UnexpectedRoute {
            actual: crate::MessageRoute::IntraControl,
            ..
        })
    ));
    let control = ControlMessage::StartAnnouncement {
        announcements: vec![AnnouncementEntry {
            locale: 1,
            country: 46,
            tone: Tone::Zip,
        }],
        end_of_ack: EndOfAnnouncementAck::Required,
        conference_id: 44,
        matrix_conference_party_ids: vec![7, 9],
        hearing_conference_party_mask: 0b11,
        play_mode: AnnouncementPlayMode::Continuous,
    };
    assert!(control.encode(ProtocolVersion::V22).is_ok());

    let too_many_announcements = start_announcement_message(
        ConferenceId::new(44),
        vec![
            AnnouncementEntry {
                locale: 1,
                country: 46,
                tone: Tone::Zip,
            };
            33
        ],
        false,
        Vec::new(),
        0,
        0,
    );
    assert!(matches!(
        too_many_announcements.encode(ProtocolVersion::V22),
        Err(CodecError::CountTooLarge {
            field: "announcements",
            maximum: 32,
            ..
        })
    ));

    let too_many_participants = start_announcement_message(
        ConferenceId::new(44),
        Vec::new(),
        false,
        (1..=17).map(ParticipantId::new).collect(),
        0,
        0,
    );
    assert!(matches!(
        too_many_participants.encode(ProtocolVersion::V22),
        Err(CodecError::CountTooLarge {
            field: "matrix conference party identifiers",
            maximum: 16,
            ..
        })
    ));
}

#[tokio::test]
async fn transfer_presentation_marks_source_and_keeps_consultation_active() {
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
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance(1),
                call_id: CallId(10),
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    for state in [CallState::Connected, CallState::Hold] {
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState {
                    call_id: CallId(10),
                    state,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    }

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::BeginTransfer {
                source_call_id: CallId(10),
                consultation_line_instance: LineInstance(1),
                consultation_call_id: CallId(20),
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::SetLamp {
                stimulus: ButtonType::Transfer,
                mode: LampMode::Flash,
                ..
            }
        )
    })
    .await;
    let states = messages
        .iter()
        .filter_map(|message| match message {
            ServerMessage::CallState {
                state,
                call_reference,
                ..
            } => Some((*state, *call_reference)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        vec![(CallState::Transfer, 10), (CallState::OffHook, 20)]
    );
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::SelectSoftKeys {
            call_reference: 20,
            set: KeyMode::OffHookFeature,
            ..
        }
    )));
    assert!(!messages.iter().any(|message| matches!(
        message,
        ServerMessage::CallState {
            state: CallState::Transfer,
            call_reference: 20,
            ..
        }
    )));

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetCallState {
                call_id: CallId(20),
                state: CallState::Connected,
            },
        ))
        .await
        .unwrap();
    let messages = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::SelectSoftKeys {
                call_reference: 20,
                set: KeyMode::ConnectedTransfer,
                ..
            }
        )
    })
    .await;
    assert!(messages.iter().any(|message| matches!(
        message,
        ServerMessage::SelectSoftKeys {
            call_reference: 20,
            set: KeyMode::ConnectedTransfer,
            ..
        }
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn active_call_selection_and_hook_flash_use_exact_session_identity() {
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
    for call_id in [CallId(10), CallId(20)] {
        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::BeginCall {
                    line_instance: LineInstance(1),
                    call_id,
                    codec: Codec::Pcmu,
                },
            ))
            .await
            .unwrap();
        read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
    }

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::SetCallSelected {
                call_id: CallId(10),
                selected: true,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_SELECT_STAT).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::CallSelectStatus {
            status: 1,
            call_reference: 10,
            line_instance: 1,
        })
    )));

    phone
        .write_all(
            &ClientMessage::HookFlash {
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
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::HookFlash {
                call_id: Some(CallId(20)),
                line_instance: LineInstance(1),
                ..
            }
        }))
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn omitted_answer_uses_live_policy_and_skips_an_offer_closed_before_input() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        call_answer_order: CallSelectionOrder::LastFirst,
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
    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::BeginCall {
                line_instance: LineInstance(1),
                call_id: CallId(1),
                codec: Codec::Pcmu,
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::SELECT_SOFT_KEYS).await;
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
    read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;

    let info = CallInfo {
        direction: crate::types::CallDirection::Inbound,
        calling_name: "Caller".into(),
        calling_number: "1002".into(),
        called_name: "Desk".into(),
        called_number: "1001".into(),
        ..CallInfo::default()
    };
    for call_id in [CallId(10), CallId(20)] {
        handle
            .offer_incoming_call_with_id(
                device_id.clone(),
                LineInstance::new(1),
                call_id,
                info.clone(),
            )
            .await
            .unwrap();
        read_until_message(
            &mut phone,
            &mut decoder,
            wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
        )
        .await;
    }
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
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::SoftKey {
                call_id: Some(CallId(20)),
                soft_key: SoftKey::Answer,
                ..
            }
        }))
    ));

    handle
        .try_offer_incoming_call_with_id(device_id.clone(), LineInstance::new(1), CallId(30), info)
        .unwrap();
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::CloseCall {
                call_id: CallId(30),
            },
        ))
        .await
        .unwrap();
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::CloseCall {
                call_id: CallId(20),
            },
        ))
        .await
        .unwrap();
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
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::OffHook {
                call_id: CallId(10),
                line_instance: LineInstance(1),
                ..
            }
        }))
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn server_messages_are_decodeable_frames() {
    let bytes = ServerMessage::CapabilitiesRequest
        .encode(ProtocolVersion::V22)
        .unwrap();
    assert_eq!(
        FrameDecoder::new().push(&bytes).unwrap()[0].message_id,
        wire_id::CAPABILITIES_REQ
    );
    assert!(matches!(
        ClientMessage::decode(Frame::new(0, wire_id::KEEP_ALIVE, Vec::new())).unwrap(),
        ClientMessage::KeepAlive
    ));
}
