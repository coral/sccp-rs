use super::support::*;

#[tokio::test]
async fn parking_button_menu_and_selection_are_typed_end_to_end() {
    let mut device = definition();
    device
        .buttons
        .push(ButtonDefinition::Feature(FeatureDefinition {
            instance: 4,
            label: "Parking".into(),
            feature: ButtonType::ParkingLot,
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
                stimulus: Stimulus::ParkingLot,
                instance: 4,
                call_reference: 0,
                status: Some(0),
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ParkingLotButton {
            instance: LineInstance(4),
            call_id: None,
            line_instance: LineInstance(1),
        } })) if actual == device_id
    ));

    handle
        .send(Command::new(
            device_id.clone(),
            CommandAction::ShowParkingMenu {
                instance: LineInstance::new(4),
                transaction_id: TransactionId(17),
                lot: "east & west".into(),
                calls: vec![ParkingMenuEntry {
                    slot: 701,
                    caller_name: "Taylor <T>".into(),
                    caller_number: "2100".into(),
                    connected_name: "Desk".into(),
                    connected_number: "1001".into(),
                }],
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
    let ServerMessage::UserToDeviceDataV1(menu) = message else {
        panic!("expected parking menu application data");
    };
    assert_eq!(menu.application_id, PARKING_APPLICATION_ID);
    assert_eq!(menu.line_instance, 4);
    assert_eq!(menu.call_reference, 0);
    assert_eq!(menu.transaction_id, 17);
    let xml = String::from_utf8(menu.data).unwrap();
    assert!(xml.contains("Taylor &lt;T&gt;"));
    assert!(xml.contains("UserCallData:9090:4:0:17:"));
    assert!(xml.contains("retrieve/east%20%26%20west/701"));

    phone
        .write_all(
            &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                application_id: PARKING_APPLICATION_ID,
                line_instance: 4,
                call_reference: 0,
                transaction_id: 17,
                sequence_flag: 0,
                display_priority: 0,
                conference_id: 0,
                application_instance_id: 4,
                routing: 0,
                data: b"retrieve/east%20%26%20west/701".to_vec(),
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ParkingMenuSelection {
            lot,
            slot: 701,
        } })) if actual == device_id && lot == "east & west"
    ));
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id: actual,
        event: DeviceEventKind::PhoneServiceResponse { response },
    })) = events.recv().await
    else {
        panic!("expected typed phone-service response");
    };
    assert_eq!(actual, device_id);
    assert_eq!(response.kind, PhoneServiceMessageKind::Data);
    assert_eq!(response.routing.application_id, ApplicationId::new(9090));
    assert_eq!(response.routing.line_instance, LineInstance::new(4));
    assert_eq!(response.routing.call_reference, CallReference::new(0));
    assert_eq!(response.routing.transaction_id, TransactionId::new(17));
    let PhoneServicePayload::Submission(submission) = response.payload else {
        panic!("expected typed menu submission");
    };
    assert_eq!(submission.route, ["retrieve", "east & west", "701"]);
    assert!(submission.values.is_empty());

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn conference_list_uses_protocol_family_and_routes_typed_actions() {
    for (protocol, family) in [
        (ProtocolVersion::V3, ConferenceMenuFamily::Menu),
        (ProtocolVersion::V22, ConferenceMenuFamily::IconMenu),
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
        read_until_message(&mut phone, &mut decoder, wire_id::CALL_STATE).await;

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowConferenceList {
                    call_id: CallId(7001),
                    conference_id: ConferenceId::new(44),
                    participants: vec![ConferenceListEntry {
                        participant_id: crate::ParticipantId::new(7),
                        name: "Taylor <T>".into(),
                        number: "2100".into(),
                        moderator: false,
                        muted: false,
                    }],
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
        let ServerMessage::UserToDeviceDataV1(menu) = message else {
            panic!("expected conference-list application data");
        };
        assert_eq!(menu.application_id, ConferenceListAction::APPLICATION_ID);
        assert_eq!(menu.conference_id, 44);
        let document = ConferenceListDocument::from_xml(&menu.data, family).unwrap();
        assert_eq!(
            document.actions().collect::<Vec<_>>(),
            [
                ConferenceListAction::Participant {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
                ConferenceListAction::End {
                    conference_id: ConferenceId::new(44),
                },
            ]
        );
        assert!(
            String::from_utf8(menu.data)
                .unwrap()
                .contains("Taylor &lt;T&gt;")
        );

        phone
            .write_all(
                &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                    application_id: ConferenceListAction::APPLICATION_ID,
                    line_instance: 1,
                    call_reference: 7001,
                    transaction_id: 44,
                    sequence_flag: 0,
                    display_priority: 0,
                    conference_id: 44,
                    application_instance_id: 1,
                    routing: 0,
                    data: b"conference/44/participant/7".to_vec(),
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ConferenceListAction {
                action: ConferenceListAction::Participant {
                    conference_id,
                    participant_id,
                },
            } })) if actual == device_id
                && conference_id == ConferenceId::new(44)
                && participant_id == crate::ParticipantId::new(7)
        ));
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::PhoneServiceResponse { .. }
            }))
        ));

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowConferenceParticipantActions {
                    call_id: CallId(7001),
                    conference_id: ConferenceId::new(44),
                    participant: ConferenceListEntry {
                        participant_id: crate::ParticipantId::new(7),
                        name: "Taylor <T>".into(),
                        number: "2100".into(),
                        moderator: false,
                        muted: false,
                    },
                    removable: true,
                    demotable: false,
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
        let ServerMessage::UserToDeviceDataV1(menu) = message else {
            panic!("expected conference-participant action menu");
        };
        let document = ConferenceParticipantActionsDocument::from_xml(&menu.data, family).unwrap();
        assert_eq!(
            document.actions().collect::<Vec<_>>(),
            [
                ConferenceListAction::Mute {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
                ConferenceListAction::Remove {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
                ConferenceListAction::Promote {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
            ]
        );

        phone
            .write_all(
                &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                    application_id: ConferenceListAction::APPLICATION_ID,
                    line_instance: 1,
                    call_reference: 7001,
                    transaction_id: 44,
                    sequence_flag: 0,
                    display_priority: 0,
                    conference_id: 44,
                    application_instance_id: 1,
                    routing: 0,
                    data: b"conference/44/participant/7/remove".to_vec(),
                })
                .encode(protocol)
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ConferenceListAction {
                action: ConferenceListAction::Remove {
                    conference_id,
                    participant_id,
                },
            } })) if actual == device_id
                && conference_id == ConferenceId::new(44)
                && participant_id == crate::ParticipantId::new(7)
        ));
        assert!(matches!(
            events.recv().await,
            Some(Event::Device(DeviceEvent {
                session_generation: _,
                device_id: _,
                event: DeviceEventKind::PhoneServiceResponse { .. }
            }))
        ));
        for (route, expected) in [
            (
                b"conference/44/participant/7/mute".as_slice(),
                ConferenceListAction::Mute {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
            ),
            (
                b"conference/44/participant/7/unmute".as_slice(),
                ConferenceListAction::Unmute {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
            ),
            (
                b"conference/44/participant/7/promote".as_slice(),
                ConferenceListAction::Promote {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
            ),
            (
                b"conference/44/participant/7/demote".as_slice(),
                ConferenceListAction::Demote {
                    conference_id: ConferenceId::new(44),
                    participant_id: crate::ParticipantId::new(7),
                },
            ),
            (
                b"conference/44/end".as_slice(),
                ConferenceListAction::End {
                    conference_id: ConferenceId::new(44),
                },
            ),
        ] {
            phone
                .write_all(
                    &ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                        application_id: ConferenceListAction::APPLICATION_ID,
                        line_instance: 1,
                        call_reference: 7001,
                        transaction_id: 44,
                        sequence_flag: 0,
                        display_priority: 0,
                        conference_id: 44,
                        application_instance_id: 1,
                        routing: 0,
                        data: route.to_vec(),
                    })
                    .encode(protocol)
                    .unwrap(),
                )
                .await
                .unwrap();
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ConferenceListAction {
                    action,
                } })) if actual == device_id && action == expected
            ));
            assert!(matches!(
                events.recv().await,
                Some(Event::Device(DeviceEvent {
                    session_generation: _,
                    device_id: _,
                    event: DeviceEventKind::PhoneServiceResponse { .. }
                }))
            ));
        }

        handle
            .send(Command::new(
                device_id.clone(),
                CommandAction::ShowConferenceParticipantActions {
                    call_id: CallId(7001),
                    conference_id: ConferenceId::new(44),
                    participant: ConferenceListEntry {
                        participant_id: crate::ParticipantId::new(7),
                        name: "Taylor <T>".into(),
                        number: "2100".into(),
                        moderator: true,
                        muted: false,
                    },
                    removable: false,
                    demotable: true,
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
        let ServerMessage::UserToDeviceDataV1(menu) = message else {
            panic!("expected moderator conference-participant action menu");
        };
        let document = ConferenceParticipantActionsDocument::from_xml(&menu.data, family).unwrap();
        assert_eq!(
            document.actions().collect::<Vec<_>>(),
            [ConferenceListAction::Demote {
                conference_id: ConferenceId::new(44),
                participant_id: crate::ParticipantId::new(7),
            }]
        );

        handle.shutdown().await.unwrap();
        task.await.unwrap().unwrap();
    }
}

#[tokio::test]
async fn phone_service_responses_preserve_legacy_and_extended_routing() {
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

    phone
        .write_all(
            &ClientMessage::DeviceToUserDataResponseV1(UserDataV1Message {
                application_id: 9084,
                line_instance: 2,
                call_reference: 42,
                transaction_id: 73,
                sequence_flag: 1,
                display_priority: 2,
                conference_id: 51,
                application_instance_id: 6,
                routing: 4,
                data: br#"<CiscoIPPhoneResponse><ResponseItem Status="0" Data="ok &amp; ready" URL="Init:Services"/></CiscoIPPhoneResponse>"#.to_vec(),
            })
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let Some(Event::Device(DeviceEvent {
        session_generation: _,
        device_id: actual,
        event: DeviceEventKind::PhoneServiceResponse { response },
    })) = events.recv().await
    else {
        panic!("expected typed execute response");
    };
    assert_eq!(actual, device_id);
    assert_eq!(response.kind, PhoneServiceMessageKind::Response);
    assert_eq!(response.routing.application_id, ApplicationId::new(9084));
    assert_eq!(response.routing.line_instance, LineInstance::new(2));
    assert_eq!(response.routing.call_reference, CallReference::new(42));
    assert_eq!(response.routing.transaction_id, TransactionId::new(73));
    assert_eq!(
        response.extended,
        Some(PhoneServiceExtendedRouting {
            sequence_flag: 1,
            display_priority: 2,
            conference_id: 51,
            application_instance_id: 6,
            routing: 4,
        })
    );
    let PhoneServicePayload::ExecuteResponse(execute) = response.payload else {
        panic!("expected typed execute response payload");
    };
    assert_eq!(execute.items.len(), 1);
    assert_eq!(execute.items[0].status.get(), 0);
    assert_eq!(execute.items[0].data, "ok & ready");
    assert_eq!(execute.items[0].url, "Init:Services");

    phone
        .write_all(
            &ClientMessage::DeviceToUserData(crate::message::UserDataMessage {
                application_id: 9083,
                line_instance: 1,
                call_reference: 43,
                transaction_id: 74,
                data: b"invite?NUMBER=555%2A12&NUMBER=555%2A13&NAME=Fran%C3%A7ois".to_vec(),
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
    assert_eq!(response.extended, None);
    assert_eq!(response.routing.application_id, ApplicationId::new(9083));
    assert_eq!(response.routing.line_instance, LineInstance::new(1));
    assert_eq!(response.routing.call_reference, CallReference::new(43));
    assert_eq!(response.routing.transaction_id, TransactionId::new(74));
    let PhoneServicePayload::Submission(submission) = response.payload else {
        panic!("expected typed input submission payload");
    };
    assert_eq!(submission.route, ["invite"]);
    assert_eq!(
        submission.values_named("NUMBER").collect::<Vec<_>>(),
        ["555*12", "555*13"]
    );
    assert_eq!(
        submission.values_named("NAME").collect::<Vec<_>>(),
        ["François"]
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn parking_selection_requires_the_pending_envelope_and_survives_malformed_data() {
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
            CommandAction::ShowParkingMenu {
                instance: LineInstance::new(4),
                transaction_id: TransactionId(17),
                lot: "main".into(),
                calls: vec![],
            },
        ))
        .await
        .unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::USER_TO_DEVICE_DATA_V1).await;

    let response =
        |application_id, line_instance, call_reference, transaction_id, instance, data: &[u8]| {
            ClientMessage::DeviceToUserDataV1(UserDataV1Message {
                application_id,
                line_instance,
                call_reference,
                transaction_id,
                sequence_flag: 0,
                display_priority: 0,
                conference_id: 0,
                application_instance_id: instance,
                routing: 0,
                data: data.to_vec(),
            })
            .encode(protocol)
            .unwrap()
        };

    for (application_id, line_instance, call_reference, transaction_id, instance) in [
        (9083, 4, 0, 17, 4),
        (PARKING_APPLICATION_ID, 5, 0, 17, 4),
        (PARKING_APPLICATION_ID, 4, 9, 17, 4),
        (PARKING_APPLICATION_ID, 4, 0, 18, 4),
        (PARKING_APPLICATION_ID, 4, 0, 17, 5),
    ] {
        phone
            .write_all(&response(
                application_id,
                line_instance,
                call_reference,
                transaction_id,
                instance,
                b"retrieve/main/701",
            ))
            .await
            .unwrap();
        let Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event:
                DeviceEventKind::PhoneServiceResponse {
                    response: routed, ..
                },
        })) = events.recv().await
        else {
            panic!("expected mismatched response to remain generically routed");
        };
        assert_eq!(routed.routing.application_id.get(), application_id);
        assert_eq!(routed.routing.line_instance.get(), line_instance);
        assert_eq!(routed.routing.call_reference.get(), call_reference);
        assert_eq!(routed.routing.transaction_id.get(), transaction_id);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), events.recv())
                .await
                .is_err(),
            "mismatched envelope emitted a parking action"
        );
    }

    phone
        .write_all(&response(
            PARKING_APPLICATION_ID,
            4,
            0,
            17,
            4,
            b"retrieve/secret%GG/701",
        ))
        .await
        .unwrap();
    let Some(Event::ProtocolWarning {
        message_id, error, ..
    }) = events.recv().await
    else {
        panic!("expected malformed service-data warning");
    };
    assert_eq!(message_id, wire_id::DEVICE_TO_USER_DATA_V1);
    assert!(!error.contains("secret"));

    phone
        .write_all(&response(
            PARKING_APPLICATION_ID,
            4,
            0,
            17,
            4,
            b"retrieve/main/701",
        ))
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent { session_generation: _, device_id: actual, event: DeviceEventKind::ParkingMenuSelection {
            lot,
            slot: 701,
        } })) if actual == device_id && lot == "main"
    ));
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::PhoneServiceResponse { .. }
        }))
    ));

    phone
        .write_all(&response(
            PARKING_APPLICATION_ID,
            4,
            0,
            17,
            4,
            b"retrieve/main/701",
        ))
        .await
        .unwrap();
    assert!(matches!(
        events.recv().await,
        Some(Event::Device(DeviceEvent {
            session_generation: _,
            device_id: _,
            event: DeviceEventKind::PhoneServiceResponse { .. }
        }))
    ));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err(),
        "replayed selection emitted a second parking action"
    );

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}

#[test]
fn parking_menu_xml_is_typed_round_trippable_and_size_bounded() {
    let calls = [ParkingMenuEntry {
        slot: 701,
        caller_name: "Taylor <T> & Co".into(),
        caller_number: "2100".into(),
        connected_name: "Desk".into(),
        connected_number: "1001".into(),
    }];
    let xml = parking_menu_xml(4, 17, "east & west", &calls).unwrap();
    assert!(xml.contains("Taylor &lt;T&gt; &amp; Co"));
    let decoded = CiscoIpPhoneMenu::from_xml_with_limit(xml.as_bytes(), 2_000).unwrap();
    assert_eq!(decoded.title.as_deref(), Some("Parked calls - east & west"));
    assert_eq!(decoded.items.len(), 1);
    assert_eq!(
        decoded.items[0].url.as_deref(),
        Some("UserCallData:9090:4:0:17:retrieve/east%20%26%20west/701")
    );

    let oversized = [ParkingMenuEntry {
        caller_name: "x".repeat(2100),
        ..calls[0].clone()
    }];
    assert!(matches!(
        parking_menu_xml(4, 17, "main", &oversized),
        Err(ServerError::PhoneXml(PhoneXmlError::InvalidField {
            field: "menu item name",
            ..
        }))
    ));
    let byte_oversized = vec![
        ParkingMenuEntry {
            caller_name: "x".repeat(45),
            ..calls[0].clone()
        };
        PARKING_MENU_MAX_ITEMS
    ];
    assert!(matches!(
        parking_menu_xml(4, 17, "main", &byte_oversized),
        Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
            kind: "phone XML document",
            maximum: 2_000,
            ..
        }))
    ));
    assert!(matches!(
        parking_menu_xml(
            4,
            17,
            "main",
            &vec![calls[0].clone(); PARKING_MENU_MAX_ITEMS + 1]
        ),
        Err(ServerError::PhoneXml(error)) if error.to_string().contains("maximum is 32")
    ));
    assert!(
        CiscoIpPhoneMenu::from_xml_with_limit(b"<CiscoIPPhoneMenu><Title>broken", 2_000,).is_err()
    );

    #[derive(Debug)]
    struct FailingWriter;

    impl std::fmt::Write for FailingWriter {
        fn write_str(&mut self, _value: &str) -> std::fmt::Result {
            Err(std::fmt::Error)
        }
    }

    assert!(matches!(
        phone_xml::to_writer(FailingWriter, &decoded, 2_000),
        Err(PhoneXmlError::Write(_))
    ));
}

#[test]
fn text_service_delivery_types_priority_and_segments_only_modern_documents() {
    let short = CiscoIpPhoneText::new("Sender", "Read", "Hello & goodbye").unwrap();
    let legacy = text_service_messages(
        LineInstance::new(3),
        CallReference::new(71),
        TransactionId::new(99),
        PhoneServicePriority::NORMAL,
        &short,
        ProtocolVersion::V17,
    )
    .unwrap();
    assert!(matches!(
        legacy.as_slice(),
        [ServerMessage::UserToDeviceDataV1(message)]
            if message.application_id == PHONE_TEXT_APPLICATION_ID
                && message.line_instance == 3
                && message.call_reference == 71
                && message.transaction_id == 99
                && message.sequence_flag == 2
                && message.display_priority == 1
                && message.conference_id == 71
                && message.application_instance_id == PHONE_TEXT_APPLICATION_ID
                && message.routing == 1
                && CiscoIpPhoneText::from_xml(&message.data).unwrap() == short
    ));

    let legacy_oversized = CiscoIpPhoneText::new(
        "Sender",
        "Read",
        "x".repeat(PHONE_TEXT_LEGACY_MAX_CHARS + 1),
    )
    .unwrap();
    assert!(matches!(
        text_service_messages(
            LineInstance::new(0),
            CallReference::new(1),
            TransactionId::new(1),
            PhoneServicePriority::LOW,
            &legacy_oversized,
            ProtocolVersion::V17,
        ),
        Err(ServerError::PhoneXml(PhoneXmlError::InvalidField {
            field: "legacy phone text body",
            ..
        }))
    ));

    let modern = CiscoIpPhoneText::new("Sender", "Read", "&".repeat(3_000)).unwrap();
    let messages = text_service_messages(
        LineInstance::new(0),
        CallReference::new(1),
        TransactionId::new(100),
        PhoneServicePriority::HIGH,
        &modern,
        ProtocolVersion::V18,
    )
    .unwrap();
    assert!(messages.len() > 2);
    let mut reassembled = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let ServerMessage::UserToDeviceDataV1(message) = message else {
            panic!("expected text application-data segment");
        };
        assert!(message.data.len() <= 2_000);
        assert_eq!(message.display_priority, 2);
        assert_eq!(
            message.sequence_flag,
            if index == 0 {
                0
            } else if index + 1 == messages.len() {
                2
            } else {
                1
            }
        );
        reassembled.extend_from_slice(&message.data);
    }
    assert_eq!(CiscoIpPhoneText::from_xml(&reassembled).unwrap(), modern);
}

#[test]
fn input_service_delivery_preserves_typed_fields_and_modern_segmentation() {
    let short = CiscoIpPhoneInput::new(
        "Invite",
        "Enter number",
        "conference/44/invite",
        vec![CiscoIpPhoneInputItem {
            display_name: Some("Number".into()),
            parameter: PhoneInputParameterName::new("NUMBER").unwrap(),
            flags: PhoneInputFlags::Telephone,
            default_value: Some("5550100".into()),
        }],
    )
    .unwrap();
    let legacy = input_service_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_092),
        TransactionId::new(99),
        PhoneServicePriority::NORMAL,
        &short,
        ProtocolVersion::V17,
    )
    .unwrap();
    assert!(matches!(
        legacy.as_slice(),
        [ServerMessage::UserToDeviceDataV1(message)]
            if message.application_id == 9_092
                && message.line_instance == 3
                && message.call_reference == 71
                && message.transaction_id == 99
                && message.sequence_flag == 2
                && message.display_priority == 1
                && message.conference_id == 71
                && message.application_instance_id == 9_092
                && message.routing == 1
                && CiscoIpPhoneInput::from_xml(&message.data).unwrap() == short
    ));

    let mut large = short;
    large.key_items = (0..32)
        .map(|index| CiscoIpPhoneKeyItem {
            key: PhoneXmlKey::NavBack,
            url: Some(format!("{}-{index:02}", "x".repeat(252))),
            url_down: Some(format!("{}-{index:02}", "y".repeat(252))),
        })
        .collect();
    assert!(matches!(
        input_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_092),
            TransactionId::new(100),
            PhoneServicePriority::HIGH,
            &large,
            ProtocolVersion::V17,
        ),
        Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
            maximum: 2_000,
            ..
        }))
    ));
    let messages = input_service_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_092),
        TransactionId::new(100),
        PhoneServicePriority::HIGH,
        &large,
        ProtocolVersion::V18,
    )
    .unwrap();
    assert!(messages.len() > 2);
    let mut reassembled = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let ServerMessage::UserToDeviceDataV1(message) = message else {
            panic!("expected input application-data segment");
        };
        assert!(message.data.len() <= 2_000);
        assert_eq!(message.application_id, 9_092);
        assert_eq!(message.display_priority, 2);
        assert_eq!(
            message.sequence_flag,
            if index == 0 {
                0
            } else if index + 1 == messages.len() {
                2
            } else {
                1
            }
        );
        reassembled.extend_from_slice(&message.data);
    }
    assert_eq!(CiscoIpPhoneInput::from_xml(&reassembled).unwrap(), large);
}

#[test]
fn execute_action_delivery_preserves_envelope_order_and_protocol_bounds() {
    let short = CiscoIpPhoneExecute::new(vec![
        CiscoIpPhoneExecuteItem::with_priority(
            "Key:Directories?view=all&side=west",
            PhoneExecutePriority::LOW,
        )
        .unwrap(),
        CiscoIpPhoneExecuteItem::new("Application:PlacedCalls").unwrap(),
    ])
    .unwrap();
    let legacy = execute_phone_action_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_093),
        TransactionId::new(99),
        PhoneServicePriority::NORMAL,
        &short,
        ProtocolVersion::V17,
    )
    .unwrap();
    assert!(matches!(
        legacy.as_slice(),
        [ServerMessage::UserToDeviceDataV1(message)]
            if message.application_id == 9_093
                && message.line_instance == 3
                && message.call_reference == 71
                && message.transaction_id == 99
                && message.sequence_flag == 2
                && message.display_priority == 1
                && message.routing == 1
                && CiscoIpPhoneExecute::from_xml(&message.data).unwrap() == short
    ));

    let large = CiscoIpPhoneExecute::new(
        (0..PHONE_EXECUTE_MAX_ITEMS)
            .map(|_| {
                CiscoIpPhoneExecuteItem::with_priority("\"".repeat(256), PhoneExecutePriority::HIGH)
                    .unwrap()
            })
            .collect(),
    )
    .unwrap();
    assert!(matches!(
        execute_phone_action_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_093),
            TransactionId::new(100),
            PhoneServicePriority::HIGH,
            &large,
            ProtocolVersion::V17,
        ),
        Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
            maximum: 2_000,
            ..
        }))
    ));
    let messages = execute_phone_action_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_093),
        TransactionId::new(100),
        PhoneServicePriority::HIGH,
        &large,
        ProtocolVersion::V18,
    )
    .unwrap();
    assert!(messages.len() > 2);
    let mut reassembled = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let ServerMessage::UserToDeviceDataV1(message) = message else {
            panic!("expected execute application-data segment");
        };
        assert!(message.data.len() <= 2_000);
        assert_eq!(message.display_priority, 2);
        assert_eq!(
            message.sequence_flag,
            if index == 0 {
                0
            } else if index + 1 == messages.len() {
                2
            } else {
                1
            }
        );
        reassembled.extend_from_slice(&message.data);
    }
    assert_eq!(CiscoIpPhoneExecute::from_xml(&reassembled).unwrap(), large);
}

#[test]
fn image_service_delivery_preserves_family_envelope_and_protocol_bounds() {
    let short = PhoneImageDocument::ImageFile(CiscoIpPhoneImageFile {
        keypad_target: None,
        application_id: Some("maps".into()),
        on_focus_lost: None,
        on_focus_gained: None,
        on_minimized: None,
        on_closed: Some("Notify:maps/closed".into()),
        title: Some("Floor map".into()),
        prompt: Some("Inspect".into()),
        soft_keys: Vec::new(),
        key_items: Vec::new(),
        location_x: Some(-1),
        location_y: Some(167),
        url: PhoneImageUrl::new("https://pbx.example/map.png?floor=2&site=east").unwrap(),
    });
    let legacy = image_service_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_095),
        TransactionId::new(101),
        PhoneServicePriority::NORMAL,
        &short,
        ProtocolVersion::V17,
    )
    .unwrap();
    assert!(matches!(
        legacy.as_slice(),
        [ServerMessage::UserToDeviceDataV1(message)]
            if message.application_id == 9_095
                && message.line_instance == 3
                && message.call_reference == 71
                && message.transaction_id == 101
                && message.sequence_flag == 2
                && message.display_priority == 1
                && message.routing == 1
                && PhoneImageDocument::from_xml(&message.data).unwrap() == short
    ));

    let large = PhoneImageDocument::GraphicFileMenu(CiscoIpPhoneGraphicFileMenu {
        keypad_target: None,
        application_id: Some("map-regions".into()),
        on_focus_lost: None,
        on_focus_gained: None,
        on_minimized: None,
        on_closed: None,
        title: Some("Map regions".into()),
        prompt: Some("Choose".into()),
        soft_keys: Vec::new(),
        key_items: Vec::new(),
        location_x: Some(0),
        location_y: Some(0),
        url: PhoneImageUrl::new("https://pbx.example/map.png").unwrap(),
        items: (0..crate::phone::xml::PHONE_GRAPHIC_FILE_MENU_MAX_ITEMS)
            .map(|index| CiscoIpPhoneTouchAreaMenuItem {
                name: Some(format!("Region {index}")),
                url: Some("x".repeat(256)),
                touch_area: Some(PhoneTouchArea {
                    x1: index as u16,
                    y1: index as u16,
                    x2: index as u16 + 1,
                    y2: index as u16 + 1,
                }),
            })
            .collect(),
    });
    assert!(matches!(
        image_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_095),
            TransactionId::new(102),
            PhoneServicePriority::HIGH,
            &large,
            ProtocolVersion::V17,
        ),
        Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
            maximum: 2_000,
            ..
        }))
    ));
    let messages = image_service_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_095),
        TransactionId::new(102),
        PhoneServicePriority::HIGH,
        &large,
        ProtocolVersion::V18,
    )
    .unwrap();
    assert!(messages.len() > 2);
    let mut reassembled = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        let ServerMessage::UserToDeviceDataV1(message) = message else {
            panic!("expected image application-data segment");
        };
        assert!(message.data.len() <= 2_000);
        assert_eq!(message.display_priority, 2);
        assert_eq!(
            message.sequence_flag,
            if index == 0 {
                0
            } else if index + 1 == messages.len() {
                2
            } else {
                1
            }
        );
        reassembled.extend_from_slice(&message.data);
    }
    assert_eq!(PhoneImageDocument::from_xml(&reassembled).unwrap(), large);
}

#[test]
fn background_control_delivery_uses_reserved_application_envelope_and_typed_xml() {
    let set = CiscoIpPhoneSetBackground::new(
        PhoneBackgroundHttpUrl::new("http://pbx.example/background.png?site=east").unwrap(),
        PhoneBackgroundHttpUrl::new("http://pbx.example/background-thumb.png").unwrap(),
    );
    let message = background_control_message(
        TransactionId::new(107),
        &PhoneBackgroundControlDocument::Set(set.clone()),
    )
    .unwrap();
    assert!(matches!(
        message,
        ServerMessage::UserToDeviceDataV1(message)
            if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                && message.line_instance == 0
                && message.call_reference == 0
                && message.transaction_id == 107
                && message.sequence_flag == 2
                && message.display_priority == 0
                && message.conference_id == 0
                && message.application_instance_id == PHONE_BACKGROUND_APPLICATION_ID
                && message.routing == 1
                && CiscoIpPhoneSetBackground::from_xml(&message.data).unwrap() == set
    ));

    let preview = CiscoIpPhoneSetBackgroundPreview::new(
        PhoneBackgroundHttpUrl::new("http://pbx.example/background.png").unwrap(),
    );
    let message = background_control_message(
        TransactionId::new(108),
        &PhoneBackgroundControlDocument::Preview(preview.clone()),
    )
    .unwrap();
    assert!(matches!(
        message,
        ServerMessage::UserToDeviceDataV1(message)
            if message.application_id == PHONE_BACKGROUND_APPLICATION_ID
                && message.transaction_id == 108
                && CiscoIpPhoneSetBackgroundPreview::from_xml(&message.data).unwrap() == preview
    ));
}

#[test]
fn ringtone_control_delivery_uses_reserved_application_envelope_and_typed_xml() {
    let document = CiscoIpPhoneSetRingTone::new(
        PhoneRingtoneUrl::new("http://pbx.example/ringtones/Classic.raw?locale=sv").unwrap(),
    );
    let message = ringtone_control_message(TransactionId::new(111), &document).unwrap();
    assert!(matches!(
        message,
        ServerMessage::UserToDeviceDataV1(message)
            if message.application_id == PHONE_RINGTONE_APPLICATION_ID
                && message.line_instance == 0
                && message.call_reference == 0
                && message.transaction_id == 111
                && message.sequence_flag == 2
                && message.display_priority == 0
                && message.conference_id == 0
                && message.application_instance_id == PHONE_RINGTONE_APPLICATION_ID
                && message.routing == 1
                && CiscoIpPhoneSetRingTone::from_xml(&message.data).unwrap() == document
    ));
}

#[test]
fn status_service_delivery_preserves_items_icons_timers_and_envelope() {
    let bitmap = PhoneStatusDocument::Bitmap(CiscoIpPhoneStatus {
        text: Some("Calls waiting".into()),
        timer_seconds: Some(30),
        location_x: Some(-1),
        location_y: Some(20),
        width: 106,
        height: 21,
        depth: 2,
        data: Some(PhoneBitmapData::new(vec![0x5a; PHONE_STATUS_BITMAP_MAX_BYTES]).unwrap()),
    });
    let legacy = status_service_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_096),
        TransactionId::new(103),
        PhoneServicePriority::HIGH,
        &bitmap,
        ProtocolVersion::V17,
    )
    .unwrap();
    assert!(matches!(
        legacy.as_slice(),
        [ServerMessage::UserToDeviceDataV1(message)]
            if message.application_id == 9_096
                && message.line_instance == 3
                && message.call_reference == 71
                && message.transaction_id == 103
                && message.sequence_flag == 2
                && message.display_priority == 2
                && message.routing == 1
                && PhoneStatusDocument::from_xml(&message.data).unwrap() == bitmap
    ));

    let file = PhoneStatusDocument::File(CiscoIpPhoneStatusFile {
        text: Some("Map status".into()),
        timer_seconds: Some(0),
        location_x: Some(261),
        location_y: Some(49),
        url: PhoneImageUrl::new("https://pbx.example/status.png?site=east").unwrap(),
    });
    let modern = status_service_messages(
        LineInstance::new(3),
        CallReference::new(71),
        ApplicationId::new(9_096),
        TransactionId::new(104),
        PhoneServicePriority::LOW,
        &file,
        ProtocolVersion::V22,
    )
    .unwrap();
    assert!(matches!(
        modern.as_slice(),
        [ServerMessage::UserToDeviceDataV1(message)]
            if message.sequence_flag == 2
                && message.display_priority == 0
                && PhoneStatusDocument::from_xml(&message.data).unwrap() == file
    ));

    let invalid = PhoneStatusDocument::Bitmap(CiscoIpPhoneStatus {
        text: None,
        timer_seconds: None,
        location_x: None,
        location_y: None,
        width: 1,
        height: 1,
        depth: 1,
        data: Some(PhoneBitmapData::new(vec![0; PHONE_STATUS_BITMAP_MAX_BYTES + 1]).unwrap()),
    });
    assert!(matches!(
        status_service_messages(
            LineInstance::new(3),
            CallReference::new(71),
            ApplicationId::new(9_096),
            TransactionId::new(105),
            PhoneServicePriority::NORMAL,
            &invalid,
            ProtocolVersion::V22,
        ),
        Err(ServerError::PhoneXml(PhoneXmlError::LimitExceeded {
            kind: "phone status bitmap bytes",
            maximum: PHONE_STATUS_BITMAP_MAX_BYTES,
            ..
        }))
    ));
}
