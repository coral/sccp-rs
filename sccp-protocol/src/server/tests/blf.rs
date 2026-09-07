use super::support::*;

#[test]
fn blf_updates_map_every_state_to_native_feature_status() {
    let cases = [
        (BlfState::Idle, BusyLampFieldState::Idle),
        (BlfState::Ringing, BusyLampFieldState::Alerting),
        (BlfState::Busy, BusyLampFieldState::InUse),
        (BlfState::Held, BusyLampFieldState::InUse),
        (BlfState::DoNotDisturb, BusyLampFieldState::DoNotDisturb),
        (BlfState::Unavailable, BusyLampFieldState::UnknownState),
        (BlfState::Unknown, BusyLampFieldState::UnknownState),
    ];

    let device = mixed_definition();
    for (state, expected_icon) in cases {
        let feature = blf_status_message(&device, 2, state).unwrap();
        assert!(matches!(
            feature,
            ServerMessage::FeatureStatus {
                instance: 2,
                button_type: ButtonType::BlfSpeedDial,
                ref label,
                state,
            } if label == "Warehouse" && state == expected_icon.wire_value()
        ));
    }
    assert_eq!(blf_status_message(&device, 99, BlfState::Idle), None);
}

#[test]
fn hinted_ringing_policy_adds_only_a_ringing_notification() {
    let caller = BlfCallerInfo {
        name: "Taylor".into(),
        number: "5550100".into(),
    };
    let mut disabled = mixed_definition();
    assert_eq!(
        hinted_ringing_notification(&disabled, "Dispatch", Some(&caller), BlfState::Ringing,),
        None
    );

    disabled.ui.hinted_ringing_notification = true;
    assert_eq!(
        hinted_ringing_notification(&disabled, "Dispatch", Some(&caller), BlfState::Ringing,),
        Some(HandsetStatusMessage::Display {
            text: "Dispatch is ringing: Taylor (5550100)".into(),
            timeout_seconds: 5,
            priority: None,
        })
    );
    for state in [
        BlfState::Idle,
        BlfState::Busy,
        BlfState::Held,
        BlfState::DoNotDisturb,
        BlfState::Unavailable,
        BlfState::Unknown,
    ] {
        assert_eq!(
            hinted_ringing_notification(&disabled, "Dispatch", Some(&caller), state),
            None,
            "non-ringing BLF state {state:?} must not be replaced by a notification"
        );
        assert!(blf_status_message(&disabled, 2, state).is_some());
    }
}

#[test]
fn simultaneous_blf_alerts_replace_and_clear_without_hiding_survivors() {
    let alert = |text: &str| HandsetStatusMessage::Display {
        text: text.into(),
        timeout_seconds: 5,
        priority: None,
    };
    let mut active = BTreeMap::new();
    let mut visible = None;

    assert_eq!(
        reconcile_blf_alert(2, Some(alert("Two")), &mut active, &mut visible),
        Some(alert("Two"))
    );
    assert_eq!(
        reconcile_blf_alert(3, Some(alert("Three")), &mut active, &mut visible),
        None
    );
    assert_eq!(
        reconcile_blf_alert(2, None, &mut active, &mut visible),
        Some(alert("Three"))
    );
    assert_eq!(
        reconcile_blf_alert(3, None, &mut active, &mut visible),
        Some(HandsetStatusMessage::Clear { priority: None })
    );
}

#[test]
fn blf_feature_status_keeps_the_static_configured_label() {
    let device = mixed_definition();
    let without_caller = blf_status_message(&device, 2, BlfState::Ringing).unwrap();
    assert!(matches!(
        without_caller,
        ServerMessage::FeatureStatus { label, .. } if label == "Warehouse"
    ));
}

#[tokio::test]
async fn native_blf_update_is_validated_cached_and_replayed_as_feature_status_only() {
    let device = mixed_definition();
    let device_id = device.id.clone();
    let config = ServerConfig {
        bind: "127.0.0.1:0".parse().unwrap(),
        advertised_address: Ipv4Addr::LOCALHOST,
        ..ServerConfig::default()
    };
    let (server, handle, _events) = Server::bind(config, [device]).await.unwrap();
    let address = server.local_addr().unwrap();
    let task = tokio::spawn(server.run());
    let protocol = ProtocolVersion::V22;
    let mut phone = TcpStream::connect(address).await.unwrap();
    let mut decoder = FrameDecoder::new();
    phone.write_all(&register_bytes(protocol)).await.unwrap();
    read_until_message(&mut phone, &mut decoder, wire_id::CAPABILITIES_REQ).await;

    handle
        .send_confirmed(Command::new(
            device_id.clone(),
            CommandAction::SetBlfStatus {
                instance: LineInstance::new(2),
                state: BlfState::Busy,
                caller: Some(BlfCallerInfo {
                    name: "Must not replace label".into(),
                    number: "5550100".into(),
                }),
            },
        ))
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::FEATURE_STAT).await;
    assert_eq!(
        frames
            .iter()
            .filter(|frame| matches!(
                frame.message_id,
                wire_id::FEATURE_STAT | wire_id::FEATURE_STAT_DYNAMIC
            ))
            .count(),
        1
    );
    assert!(frames.iter().all(|frame| !matches!(
        frame.message_id,
        wire_id::SPEED_DIAL_STAT | wire_id::SPEED_DIAL_STAT_DYNAMIC | wire_id::SET_LAMP
    )));
    let feature = frames
        .into_iter()
        .find(|frame| {
            matches!(
                frame.message_id,
                wire_id::FEATURE_STAT | wire_id::FEATURE_STAT_DYNAMIC
            )
        })
        .unwrap();
    assert!(matches!(
        ServerMessage::decode(feature, protocol).unwrap(),
        ServerMessage::FeatureStatus {
            instance: 2,
            button_type: ButtonType::BlfSpeedDial,
            label,
            state,
        } if label == "Warehouse" && state == BusyLampFieldState::InUse.wire_value()
    ));

    phone
        .write_all(
            &ClientMessage::FeatureStatusRequest {
                index: 2,
                capabilities: Some(0),
            }
            .encode(protocol)
            .unwrap(),
        )
        .await
        .unwrap();
    let frames = read_until_message(&mut phone, &mut decoder, wire_id::FEATURE_STAT).await;
    let feature = frames
        .into_iter()
        .find(|frame| frame.message_id == wire_id::FEATURE_STAT)
        .unwrap();
    assert!(matches!(
        ServerMessage::decode(feature, protocol).unwrap(),
        ServerMessage::FeatureStatus { state, .. }
            if state == BusyLampFieldState::InUse.wire_value()
    ));

    assert!(matches!(
        handle
            .send_confirmed(Command::new(
                device_id,
                CommandAction::SetBlfStatus {
                    instance: LineInstance::new(99),
                    state: BlfState::Idle,
                    caller: None,
                },
            ))
            .await,
        Err(ServerError::CommandWrite(message)) if message.contains("no BLF feature button instance 99")
    ));

    handle.shutdown().await.unwrap();
    task.await.unwrap().unwrap();
}
