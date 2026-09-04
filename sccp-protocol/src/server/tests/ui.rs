use super::support::*;

#[test]
fn call_count_response_advertises_the_configured_lines_and_standard_defaults() {
    assert_eq!(
        call_count_response(&definition()).unwrap(),
        ServerMessage::CallCountResponse(CallCountResponse {
            total_configured_lines: 1,
            starting_line_instance: 1,
            line_data: vec![CallCountLineData {
                max_calls: DEFAULT_MAX_CALLS_PER_LINE,
                busy_trigger: DEFAULT_BUSY_TRIGGER_PER_LINE,
            }],
        })
    );
}

#[test]
fn every_call_state_and_soft_key_has_an_explicit_availability_result() {
    let expected_modes = [
        (CallState::OffHook, KeyMode::OffHook),
        (CallState::OnHook, KeyMode::OnHook),
        (CallState::RingOut, KeyMode::RingOut),
        (CallState::RingIn, KeyMode::RingIn),
        (CallState::Connected, KeyMode::Connected),
        (CallState::Busy, KeyMode::OffHook),
        (CallState::Congestion, KeyMode::OffHook),
        (CallState::Hold, KeyMode::OnHold),
        (CallState::CallWaiting, KeyMode::RingIn),
        (CallState::Transfer, KeyMode::ConnectedTransfer),
        (CallState::Park, KeyMode::OnHook),
        (CallState::Proceed, KeyMode::RingOut),
        (CallState::RemoteMultiline, KeyMode::OnHookStealable),
        (CallState::InvalidNumber, KeyMode::OffHook),
        (CallState::HoldYellow, KeyMode::OnHold),
        (CallState::IntercomOneWay, KeyMode::OffHook),
        (CallState::HoldRed, KeyMode::OnHold),
    ];
    assert_eq!(CallState::ALL_KNOWN.len(), expected_modes.len());
    assert_eq!(
        CallState::ALL_KNOWN,
        expected_modes
            .iter()
            .map(|(state, _)| *state)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        key_mode_for_call_state(CallState::Unknown(99)),
        KeyMode::OnHook
    );

    let profile = SoftKeyProfile::built_in();
    for (state, expected_mode) in expected_modes {
        let mode = key_mode_for_call_state(state);
        assert_eq!(mode, expected_mode, "unexpected key mode for {state:?}");
        let expected_actions: &[SoftKey] = match mode {
            KeyMode::OnHook => &[SoftKey::NewCall],
            KeyMode::Connected | KeyMode::ConnectedTransfer => {
                &[SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer]
            }
            KeyMode::OnHold | KeyMode::OffHookFeature | KeyMode::HoldConference => {
                &[SoftKey::Resume, SoftKey::NewCall, SoftKey::EndCall]
            }
            KeyMode::RingIn => &[SoftKey::Answer, SoftKey::EndCall],
            KeyMode::OffHook | KeyMode::RingOut => &[SoftKey::EndCall],
            KeyMode::DigitsFollowing => &[SoftKey::Backspace, SoftKey::EndCall, SoftKey::Dial],
            KeyMode::ConnectedConference => &[SoftKey::Hold, SoftKey::EndCall],
            KeyMode::OnHookStealable => &[SoftKey::Intercept, SoftKey::NewCall],
            KeyMode::InUseHint | KeyMode::Empty | KeyMode::Unknown(_) => &[],
        };
        for &soft_key in SoftKey::ALL_KNOWN {
            assert_eq!(
                profile.allows(mode, soft_key),
                expected_actions.contains(&soft_key),
                "state={state:?} mode={mode:?} soft_key={soft_key:?}"
            );
        }
    }
}

#[tokio::test]
async fn terminal_failure_states_reach_the_handset_with_a_visible_prompt() {
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
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));

    let call_id = CallId(77);
    handle
        .send_confirmed(Command::new(
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

    for (state, expected_prompt) in [
        (CallState::Busy, "Busy"),
        (CallState::Congestion, "Network congestion"),
        (CallState::InvalidNumber, "Unknown number"),
    ] {
        handle
            .send_confirmed(Command::new(
                device_id.clone(),
                CommandAction::SetCallState { call_id, state },
            ))
            .await
            .unwrap();
        let frames = read_until_message(
            &mut phone,
            &mut decoder,
            wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
        )
        .await;
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::CallState {
                state: actual,
                call_reference,
                ..
            }) if actual == state && call_reference == 77
        )));
        assert!(frames.iter().any(|frame| matches!(
            ServerMessage::decode(frame.clone(), protocol),
            Ok(ServerMessage::DisplayPrompt {
                ref text,
                call_reference,
                ..
            }) if text == expected_prompt && call_reference == 77
        )));
    }

    // SCCP has no wire-level unavailable state.  A configured but unreachable
    // destination retains the congestion/reorder state and replaces only its
    // presentation with the more accurate call-scoped prompt.
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 5,
                text: "Unavailable".into(),
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(
        &mut phone,
        &mut decoder,
        wire_id::DISPLAY_DYNAMIC_PROMPT_STATUS,
    )
    .await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::DisplayPrompt {
            ref text,
            timeout_seconds: 5,
            call_reference: 77,
            ..
        }) if text == "Unavailable"
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn do_not_disturb_defaults_to_off() {
    assert_eq!(DoNotDisturbMode::default(), DoNotDisturbMode::Off);
}

fn recording_button_definition() -> DeviceDefinition {
    let mut device = definition();
    device.buttons.extend([
        ButtonDefinition::Recording(crate::types::RecordingButtonDefinition {
            instance: 1,
            label: "Record calls".into(),
        }),
        ButtonDefinition::Recording(crate::types::RecordingButtonDefinition {
            instance: 2,
            label: "Second recorder".into(),
        }),
    ]);
    device
}

#[test]
fn recording_button_template_follows_protocol_and_model_capabilities() {
    let device = recording_button_definition();
    let projected_types = |protocol, device_type| {
        button_template_for_station(&device, protocol, device_type)
            .into_iter()
            .skip(1)
            .map(|entry| entry.button_type)
            .collect::<Vec<_>>()
    };

    assert_eq!(
        projected_types(ProtocolVersion::V15, DeviceType::Cisco7925),
        vec![ButtonType::Feature; 2]
    );
    assert_eq!(
        projected_types(ProtocolVersion::V16, DeviceType::Cisco7925),
        vec![ButtonType::MultiblinkFeature; 2]
    );
    for device_type in [DeviceType::Cisco8941, DeviceType::Cisco8945] {
        assert_eq!(
            projected_types(ProtocolVersion::V22, device_type),
            vec![ButtonType::Feature; 2]
        );
    }
}

#[test]
fn recording_button_projects_exact_four_state_words_and_bounded_active_labels() {
    let device = recording_button_definition();
    let states = [
        (RecordingButtonState::Off, 0, LampMode::Off),
        (RecordingButtonState::Armed, 0x02_03_02, LampMode::On),
        (RecordingButtonState::Active, 0x03_02_03, LampMode::Wink),
        (
            RecordingButtonState::ArmedActive,
            0x03_02_05,
            LampMode::Blink,
        ),
    ];
    for (state, expected_word, expected_lamp) in states {
        let [status, lamp] = recording_button_state_messages(
            &device,
            1,
            state,
            ProtocolVersion::V22,
            DeviceType::Cisco7925,
            PhoneFeatures::empty(),
        )
        .unwrap();
        assert!(matches!(
            status,
            ServerMessage::FeatureStatus {
                button_type: ButtonType::MultiblinkFeature,
                state: actual_word,
                ..
            } if actual_word == expected_word
        ));
        assert!(matches!(
            lamp,
            ServerMessage::SetLamp { mode, .. } if mode == expected_lamp
        ));
    }

    assert_eq!(
        recording_button_label(
            "1234567890123456789012345678",
            RecordingButtonState::Active,
            PhoneFeatures::empty(),
        ),
        "1234567890123456789012345678"
    );
    assert_eq!(
        recording_button_label(
            "1234567890123456789012345678",
            RecordingButtonState::Active,
            PhoneFeatures::DYNAMIC_MESSAGES,
        ),
        "1234567890123456789012345678 (Recording)"
    );

    let [legacy_status, legacy_lamp] = recording_button_state_messages(
        &device,
        1,
        RecordingButtonState::ArmedActive,
        ProtocolVersion::V22,
        DeviceType::Cisco8945,
        PhoneFeatures::empty(),
    )
    .unwrap();
    assert!(matches!(
        legacy_status,
        ServerMessage::FeatureStatus {
            button_type: ButtonType::Feature,
            state: 1,
            ref label,
            ..
        } if label == "Record calls (Recording)"
    ));
    assert!(matches!(
        legacy_lamp,
        ServerMessage::SetLamp {
            stimulus: ButtonType::Feature,
            mode: LampMode::Blink,
            ..
        }
    ));
}

#[test]
fn button_template_uses_ordered_semantic_device_buttons() {
    let device = mixed_definition();

    assert_eq!(
        button_template(&device),
        vec![
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
            ButtonTemplateEntry {
                instance: 1,
                button_type: ButtonType::ServiceUrl,
            },
            ButtonTemplateEntry {
                instance: 0,
                button_type: ButtonType::Unused,
            },
            ButtonTemplateEntry {
                instance: 2,
                button_type: ButtonType::BlfSpeedDial,
            },
        ]
    );
}

#[test]
fn button_template_chunks_preserve_global_offset_count_and_total() {
    for total in [BUTTON_TEMPLATE_ENTRIES_PER_CHUNK, 43, 56, 84] {
        let mut device = definition();
        device
            .buttons
            .extend(std::iter::repeat_n(ButtonDefinition::Unused, total - 1));
        device.validate().unwrap();

        let messages = button_template_messages(&device).unwrap();
        assert_eq!(
            messages.len(),
            total.div_ceil(BUTTON_TEMPLATE_ENTRIES_PER_CHUNK)
        );
        let mut next_offset = 0_u32;
        for (index, message) in messages.into_iter().enumerate() {
            let ServerMessage::ButtonTemplate {
                offset,
                total: message_total,
                ref buttons,
            } = message
            else {
                unreachable!("button template helper returns only template chunks")
            };
            assert_eq!(offset, next_offset);
            assert_eq!(message_total, total as u32);
            assert_eq!(
                buttons.len(),
                (total - index * BUTTON_TEMPLATE_ENTRIES_PER_CHUNK)
                    .min(BUTTON_TEMPLATE_ENTRIES_PER_CHUNK)
            );
            if index == 0 {
                assert_eq!(buttons[0].button_type, ButtonType::Line);
            }
            next_offset += buttons.len() as u32;

            let bytes = message.encode(ProtocolVersion::V22).unwrap();
            let frame = FrameDecoder::new().push(&bytes).unwrap().remove(0);
            assert_eq!(frame.payload.len(), 96);
            assert_eq!(
                ServerMessage::decode(frame, ProtocolVersion::V22).unwrap(),
                message
            );
        }
        assert_eq!(next_offset, total as u32);
    }
}

#[test]
fn static_button_statuses_use_typed_instances_and_safe_unknowns() {
    let device = mixed_definition();

    assert_eq!(
        speed_dial_status(&device, 1),
        ServerMessage::SpeedDialStatus {
            instance: 1,
            number: "2001".into(),
            display_name: "Reception".into(),
        }
    );
    assert_eq!(
        speed_dial_status(&device, 99),
        ServerMessage::SpeedDialStatus {
            instance: 99,
            number: String::new(),
            display_name: String::new(),
        }
    );
    assert_eq!(
        feature_status(&device, 1, 0),
        Some(ServerMessage::FeatureStatus {
            instance: 1,
            button_type: ButtonType::DoNotDisturb,
            label: "DND".into(),
            state: 0,
        })
    );
    assert_eq!(feature_status(&device, 99, 0), None);
    assert_eq!(
        speed_dial_status(&device, 2),
        ServerMessage::SpeedDialStatus {
            instance: 2,
            number: String::new(),
            display_name: String::new(),
        }
    );
    assert_eq!(
        feature_status(&device, 2, 0),
        Some(ServerMessage::FeatureStatus {
            instance: 2,
            button_type: ButtonType::BlfSpeedDial,
            label: "Warehouse".into(),
            state: BusyLampFieldState::UnknownState.wire_value(),
        })
    );
    assert_eq!(feature_status(&device, 2, 1), feature_status(&device, 2, 0));
    assert_eq!(
        service_url_status(&device, 1),
        Some(ServerMessage::ServiceUrlStatus {
            index: 1,
            url: "http://services.invalid/directory".into(),
            label: "Directory".into(),
            extension_text: String::new(),
        })
    );
    assert_eq!(service_url_status(&device, 99), None);
}

#[test]
fn do_not_disturb_status_preserves_exact_mode_and_button_behavior() {
    let device = mixed_definition();

    for (mode, state, lamp) in [
        (DoNotDisturbMode::Off, 0x010000, LampMode::Off),
        (DoNotDisturbMode::Reject, 0x020202, LampMode::On),
        (DoNotDisturbMode::Silent, 0x030302, LampMode::Blink),
    ] {
        assert_eq!(
            do_not_disturb_state_messages(
                &device,
                1,
                mode,
                DoNotDisturbButtonMode::Cycle,
                ProtocolVersion::V22,
            ),
            Some([
                ServerMessage::FeatureStatus {
                    instance: 1,
                    button_type: ButtonType::MultiblinkFeature,
                    label: "DND".into(),
                    state,
                },
                ServerMessage::SetLamp {
                    stimulus: ButtonType::DoNotDisturb,
                    instance: 1,
                    mode: lamp,
                },
            ])
        );
    }

    for (button_mode, mode, enabled, lamp) in [
        (
            DoNotDisturbButtonMode::Silent,
            DoNotDisturbMode::Silent,
            1,
            LampMode::Blink,
        ),
        (
            DoNotDisturbButtonMode::Silent,
            DoNotDisturbMode::Reject,
            0,
            LampMode::Off,
        ),
        (
            DoNotDisturbButtonMode::Reject,
            DoNotDisturbMode::Reject,
            1,
            LampMode::On,
        ),
        (
            DoNotDisturbButtonMode::Reject,
            DoNotDisturbMode::Silent,
            0,
            LampMode::Off,
        ),
    ] {
        let [feature, lamp_message] =
            do_not_disturb_state_messages(&device, 1, mode, button_mode, ProtocolVersion::V22)
                .unwrap();
        assert!(matches!(
            feature,
            ServerMessage::FeatureStatus {
                button_type: ButtonType::DoNotDisturb,
                state,
                ..
            } if state == enabled
        ));
        assert!(matches!(
            lamp_message,
            ServerMessage::SetLamp { mode, .. } if mode == lamp
        ));
    }

    let [legacy_feature, legacy_lamp] = do_not_disturb_state_messages(
        &device,
        1,
        DoNotDisturbMode::Silent,
        DoNotDisturbButtonMode::Cycle,
        ProtocolVersion::V15,
    )
    .unwrap();
    assert!(matches!(
        legacy_feature,
        ServerMessage::FeatureStatus {
            button_type: ButtonType::DoNotDisturb,
            state: 1,
            ..
        }
    ));
    assert!(matches!(
        legacy_lamp,
        ServerMessage::SetLamp {
            mode: LampMode::Blink,
            ..
        }
    ));

    assert!(
        do_not_disturb_state_messages(
            &device,
            99,
            DoNotDisturbMode::Reject,
            DoNotDisturbButtonMode::Cycle,
            ProtocolVersion::V22,
        )
        .is_none()
    );
}

#[tokio::test]
async fn mutable_forwarding_and_feature_state_is_published_and_answered() {
    let protocol = ProtocolVersion::V22;
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [mixed_definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::REGISTER_ACK).await;
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
            CommandAction::SetForwardStatus {
                line_instance: LineInstance(1),
                forward_all: Some("9000".into()),
                forward_busy: None,
                forward_no_answer: Some("9001".into()),
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::FORWARD_STAT).await;
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::ForwardStatus {
            line_instance: 1,
            ref forward_all,
            forward_busy: None,
            ref forward_no_answer,
        }) if forward_all.as_deref() == Some("9000")
            && forward_no_answer.as_deref() == Some("9001")
    )));

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::SetDoNotDisturbStatus {
                instance: LineInstance::new(1),
                mode: DoNotDisturbMode::Reject,
                button_mode: DoNotDisturbButtonMode::Cycle,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SET_LAMP).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::FeatureStatus {
            instance: 1,
            button_type: ButtonType::MultiblinkFeature,
            state: 0x020202,
            ..
        })
    )));
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::SetLamp {
            stimulus: ButtonType::DoNotDisturb,
            instance: 1,
            mode: LampMode::On,
        })
    )));

    phone
        .write_all(
            &[
                ClientMessage::ForwardStatusRequest { line_instance: 1 }
                    .encode(protocol)
                    .unwrap(),
                ClientMessage::FeatureStatusRequest {
                    index: 1,
                    capabilities: 0,
                }
                .encode(protocol)
                .unwrap(),
            ]
            .concat(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::FEATURE_STAT).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::ForwardStatus { ref forward_all, .. })
            if forward_all.as_deref() == Some("9000")
    )));
    assert!(frames.into_iter().any(|frame| matches!(
        ServerMessage::decode(frame, protocol),
        Ok(ServerMessage::FeatureStatus {
            instance: 1,
            button_type: ButtonType::MultiblinkFeature,
            state: 0x020202,
            ..
        })
    )));

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::DoNotDisturb,
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
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual_device, event: DeviceEventKind::DoNotDisturbButton {
            instance: LineInstance(1),
        } })) if actual_device == device_id
    ));
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::DoNotDisturb,
                instance: 99,
                call_reference: 0,
                status: 0,
            }
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

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn generic_feature_button_emits_only_for_the_configured_instance() {
    let mut device = definition();
    device
        .buttons
        .push(ButtonDefinition::Feature(FeatureDefinition {
            instance: 1,
            label: "Night service".into(),
            feature: ButtonType::Feature,
        }));
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
                stimulus: Stimulus::Privacy,
                instance: 99,
                call_reference: 0,
                status: 0,
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
            &ClientMessage::Stimulus {
                stimulus: Stimulus::Privacy,
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
        Some(Event::Device(DeviceEvent { session_generation: _, device_id, event: DeviceEventKind::FeatureButton {
            instance: LineInstance(1),
        } })) if device_id == DeviceId::new("SEP001122334455").unwrap()
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn recording_buttons_accept_both_stimuli_and_mirror_device_wide_state() {
    let protocol = ProtocolVersion::V22;
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, mut events) = Server::bind(config, [recording_button_definition()])
        .await
        .unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    phone
        .write_all(&register_bytes_for_device_type(
            protocol,
            DeviceType::Cisco7925.wire_value(),
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::REGISTER_ACK).await;
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            event: DeviceEventKind::Registered(_),
            ..
        }))
    ));

    for (stimulus, instance) in [(Stimulus::Privacy, 1), (Stimulus::MultiblinkFeature, 2)] {
        phone
            .write_all(
                &ClientMessage::Stimulus {
                    stimulus,
                    instance,
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
                event: DeviceEventKind::RecordingButton { instance: actual },
                ..
            })) if actual == LineInstance(instance)
        ));
    }

    handle
        .send(Command::new(
            device_id,
            CommandAction::SetRecordingButtonStatus {
                state: RecordingButtonState::ArmedActive,
            },
        ))
        .await
        .unwrap();
    let statuses = read_until_server_message(&mut phone, &mut decoder, protocol, |message| {
        matches!(
            message,
            ServerMessage::SetLamp {
                instance: 2,
                stimulus: ButtonType::MultiblinkFeature,
                ..
            }
        )
    })
    .await;
    let mirrored = statuses
        .iter()
        .filter_map(|message| match message {
            ServerMessage::FeatureStatus {
                instance,
                button_type: ButtonType::MultiblinkFeature,
                label,
                state: 0x03_02_05,
            } => Some((*instance, label.as_str())),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        mirrored,
        vec![
            (1, "Record calls (Recording)"),
            (2, "Second recorder (Recording)")
        ]
    );

    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::MultiblinkFeature,
                instance: 99,
                call_reference: 0,
                status: 0,
            }
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

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn mobility_candidate_rebuilds_every_slot_in_physical_button_order() {
    let mut configured = definition();
    configured
        .buttons
        .push(ButtonDefinition::Feature(FeatureDefinition {
            instance: 4,
            label: "Mobility A".into(),
            feature: ButtonType::Mobility,
        }));
    configured
        .buttons
        .push(ButtonDefinition::Feature(FeatureDefinition {
            instance: 5,
            label: "Mobility B".into(),
            feature: ButtonType::Mobility,
        }));
    let first = LineAppearance::new(
        2,
        LineDefinition {
            number: "9001".into(),
            display_name: "Roaming 9001".into(),
        },
    );
    let second = LineAppearance::new(
        3,
        LineDefinition {
            number: "9002".into(),
            display_name: "Roaming 9002".into(),
        },
    );

    let first_map = HashMap::from([(4, first.clone())]);
    let with_first = mobility_device_candidate(&configured, &HashMap::new(), &first_map)
        .expect("first roaming appearance is valid");
    let both_map = HashMap::from([(5, second.clone()), (4, first)]);
    let with_both = mobility_device_candidate(&with_first, &first_map, &both_map)
        .expect("both roaming appearances are valid");
    let projected = with_both
        .buttons
        .iter()
        .filter_map(|button| match button {
            ButtonDefinition::Feature(feature) if feature.feature == ButtonType::Mobility => {
                Some(("mobility", feature.instance))
            }
            ButtonDefinition::Line(line) if line.instance > 1 => Some(("line", line.instance)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        projected,
        vec![("mobility", 4), ("line", 2), ("mobility", 5), ("line", 3),]
    );
    assert!(matches!(
        line_status(&with_both, 2),
        Some(ServerMessage::LineStatus { directory_number, .. }) if directory_number == "9001"
    ));
    assert!(matches!(
        line_status(&with_both, 3),
        Some(ServerMessage::LineStatus { directory_number, .. }) if directory_number == "9002"
    ));

    let second_map = HashMap::from([(5, second)]);
    let without_first = mobility_device_candidate(&with_both, &both_map, &second_map)
        .expect("removing one roaming appearance preserves the other");
    assert!(line_status(&without_first, 2).is_none());
    assert!(matches!(
        line_status(&without_first, 3),
        Some(ServerMessage::LineStatus { directory_number, .. }) if directory_number == "9002"
    ));
}

#[tokio::test]
async fn mobility_button_and_live_appearance_refresh_preserve_the_session_call() {
    let mut device = definition();
    device
        .buttons
        .push(ButtonDefinition::Feature(FeatureDefinition {
            instance: 4,
            label: "Mobility".into(),
            feature: ButtonType::Mobility,
        }));
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
    phone
        .write_all(
            &ClientMessage::Stimulus {
                stimulus: Stimulus::Mobility,
                instance: 4,
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
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::MobilityButton {
                instance: LineInstance(4),
                ..
            }
        }))
    ));

    let call_id = CallId(77);
    handle
        .send_confirmed(Command::new(
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

    let roaming = LineAppearance::new(
        2,
        LineDefinition {
            number: "9001".into(),
            display_name: "Roaming 9001".into(),
        },
    );
    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetMobilityAppearance {
                mobility_instance: LineInstance::new(4),
                appearance: Some(roaming.clone()),
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::LINE_STAT_DYNAMIC).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::ButtonTemplate { ref buttons, .. })
            if buttons.iter().any(|button| button.instance == 2 && button.button_type == ButtonType::Line)
    )));
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::LineStatus { instance: 2, ref directory_number, .. })
            if directory_number == "9001"
    )));

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetMobilityAppearance {
                mobility_instance: LineInstance::new(4),
                appearance: None,
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::LINE_STAT_DYNAMIC).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::LineStatus { instance: 2, ref directory_number, .. })
            if directory_number.is_empty()
    )));
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
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;
    assert!(frames.iter().any(|frame| matches!(
        ServerMessage::decode(frame.clone(), protocol),
        Ok(ServerMessage::CallState {
            state: CallState::Connected,
            ..
        })
    )));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn unavailable_soft_key_events_and_stimuli_preserve_on_hook_state() {
    let mut device = definition();
    device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::Redial]);
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
                ClientMessage::SoftKeyEvent {
                    event: SoftKey::NewCall.wire_value(),
                    line_instance: 1,
                    call_reference: 0,
                }
                .encode(protocol)
                .unwrap(),
                ClientMessage::Stimulus {
                    stimulus: Stimulus::NewCall,
                    instance: 1,
                    call_reference: 0,
                    status: 0,
                }
                .encode(protocol)
                .unwrap(),
            ]
            .concat(),
        )
        .await
        .unwrap();
    let mut buffer = [0_u8; 256];
    assert!(
        tokio::time::timeout(Duration::from_millis(50), phone.read(&mut buffer))
            .await
            .is_err(),
        "unavailable actions unexpectedly changed the handset UI"
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "unavailable actions unexpectedly emitted an application event"
    );

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

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn configured_redial_menu_uses_typed_native_action_with_legacy_fallback_policy() {
    assert!(!placed_calls_menu_supported(ProtocolVersion::V3));
    assert!(placed_calls_menu_supported(ProtocolVersion::V8));
    assert!(placed_calls_menu_supported(ProtocolVersion::V22));

    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let mut device = definition();
    device.soft_keys = profile_with(KeyMode::OnHook, vec![SoftKey::Redial]);
    device.ui.placed_calls_redial_menu = true;
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
                event: SoftKey::Redial.wire_value(),
                line_instance: 1,
                call_reference: 0,
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames =
        read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;
    let message = frames
        .into_iter()
        .find_map(|frame| match ServerMessage::decode(frame, protocol) {
            Ok(ServerMessage::UserToDeviceDataV1(message)) => Some(message),
            _ => None,
        })
        .expect("placed-calls execute envelope");
    let document = CiscoIpPhoneExecute::from_xml(&message.data).unwrap();
    assert_eq!(
        document,
        CiscoIpPhoneExecute::new(vec![
            CiscoIpPhoneExecuteItem::new("Application:PlacedCalls").unwrap()
        ])
        .unwrap()
    );
    assert_eq!(message.line_instance, 1);
    assert_eq!(message.call_reference, 0);
    assert!(
        tokio::time::timeout(Duration::from_millis(50), events.recv())
            .await
            .is_err(),
        "opening the native placed-calls menu must not create or route a call"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

async fn assert_dedicated_messages_key_creates_an_exact_line_call(stimulus: Stimulus) {
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
            &ClientMessage::Stimulus {
                stimulus,
                // Dedicated Messages keys are not line-template buttons and
                // the captured 7965G reports instance zero.
                instance: 0,
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
            event:
                DeviceEventKind::OffHook {
                    call_id,
                    line_instance: LineInstance(1),
                    ..
                },
        })) => call_id,
        event => panic!("expected voicemail OffHook event, got {event:?}"),
    };
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: _, event: DeviceEventKind::VoicemailButton {
            call_id: routed_call,
            line_instance: LineInstance(1),
            ..
        } })) if routed_call == call_id
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dedicated_legacy_voicemail_key_routes_without_a_programmable_button() {
    assert_dedicated_messages_key_creates_an_exact_line_call(Stimulus::Voicemail).await;
}

#[tokio::test]
async fn dedicated_messages_key_routes_without_a_programmable_button() {
    assert_dedicated_messages_key_creates_an_exact_line_call(Stimulus::Messages).await;
}

#[tokio::test]
async fn legacy_phone_receives_static_button_status_layouts() {
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, _events) = Server::bind(config, [mixed_definition()]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();

    phone
        .write_all(&register_bytes(ProtocolVersion::V3))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;
    let requests = [
        ClientMessage::SpeedDialStatusRequest {
            speed_dial_instance: 1,
        }
        .encode(ProtocolVersion::V3)
        .unwrap(),
        ClientMessage::FeatureStatusRequest {
            index: 1,
            capabilities: 0,
        }
        .encode(ProtocolVersion::V3)
        .unwrap(),
        ClientMessage::ServiceUrlStatusRequest { index: 1 }
            .encode(ProtocolVersion::V3)
            .unwrap(),
    ]
    .concat();
    phone.write_all(&requests).await.unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::SERVICE_URL_STAT).await;

    for (message_id, expected) in [
        (
            wire_id::SPEED_DIAL_STAT,
            ServerMessage::SpeedDialStatus {
                instance: 1,
                number: "2001".into(),
                display_name: "Reception".into(),
            },
        ),
        (
            wire_id::FEATURE_STAT,
            ServerMessage::FeatureStatus {
                instance: 1,
                button_type: ButtonType::DoNotDisturb,
                label: "DND".into(),
                state: 0,
            },
        ),
        (
            wire_id::SERVICE_URL_STAT,
            ServerMessage::ServiceUrlStatus {
                index: 1,
                url: "http://services.invalid/directory".into(),
                label: "Directory".into(),
                extension_text: String::new(),
            },
        ),
    ] {
        let frame = frames
            .iter()
            .find(|frame| frame.message_id == message_id)
            .cloned()
            .unwrap();
        assert_eq!(
            ServerMessage::decode(frame, ProtocolVersion::V3).unwrap(),
            expected
        );
    }

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn mwi_policy_projects_configured_cadence_and_on_call_visibility() {
    let hidden_on_call = crate::types::StationUiPolicy {
        mwi_lamp_mode: LampMode::Flash,
        mwi_on_call: false,
        ..Default::default()
    };
    assert_eq!(
        projected_mwi_lamp(hidden_on_call, false, true),
        LampMode::Flash
    );
    assert_eq!(
        projected_mwi_lamp(hidden_on_call, true, true),
        LampMode::Off
    );
    assert_eq!(
        projected_mwi_lamp(hidden_on_call, false, false),
        LampMode::Off
    );

    let visible_on_call = crate::types::StationUiPolicy {
        mwi_lamp_mode: LampMode::Blink,
        mwi_on_call: true,
        ..Default::default()
    };
    assert_eq!(
        projected_mwi_lamp(visible_on_call, true, true),
        LampMode::Blink
    );
}
