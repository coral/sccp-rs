use super::*;

const CONFIG: &str = r#"
        [general]
        bind = 0.0.0.0:2000
        advertised_address = 192.0.2.10
        disallow = all
        allow = ulaw
        allow = alaw

        [SEP001122334455]
        type = device
        description = Reception
        line = 1001

        [1001]
        type = line
        label = Reception
        context = from-sccp
        callerid = "Reception" <1001>
        mailbox = 1001@default
    "#;

#[test]
fn general_policy_views_use_runtime_duration_types_without_changing_defaults() {
    let general = GeneralConfig::default();
    assert_eq!(
        general.timing_policy(),
        GeneralTimingPolicy {
            keepalive: Duration::from_secs(30),
            secondary_keepalive: Duration::from_secs(30),
            first_digit_timeout: Duration::from_secs(10),
            interdigit_timeout: Duration::from_secs(5),
            call_waiting_repeat: Duration::ZERO,
        }
    );
    assert_eq!(
        general.station_policy(),
        GeneralStationPolicy {
            timezone_offset_minutes: 0,
            date_template: DateTemplate::default(),
            ring_type: RingerMode::Outside,
            call_waiting_tone: Some(Tone::CallWaiting),
        }
    );
}

#[test]
fn sample_configuration_stays_parseable() {
    let config = ModuleConfig::parse(include_str!("../../../sccp.conf.example")).unwrap();
    assert_eq!(config.devices.len(), 1);
    assert_eq!(config.lines.len(), 2);
    assert!(config.soft_key_profile("reception-softkeys").is_some());
    assert_eq!(config.general.listeners.clear.port(), 2000);
    assert_eq!(
        config
            .general
            .listeners
            .tls
            .as_ref()
            .expect("sample documents complete TLS policy")
            .bind
            .port(),
        2443
    );
    assert!(!config.general.network.acl.rules.is_empty());
    assert!(matches!(
        config.general.network.external,
        Some(ExternalAddress::Hostname {
            refresh_seconds: 60,
            ..
        })
    ));
    assert_eq!(config.general.date_template.as_str(), "D/M/Y");
    assert_eq!(config.general.timezone_offset_minutes, 0);
    assert_eq!(config.general.qos, QosPolicy::default());
    assert_eq!(config.general.registration.contexts.len(), 2);

    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let device = &config.devices[&device_id];
    assert_eq!(device.network.transport, TransportRequirement::Either);
    assert_eq!(device.network.qos, QosPolicy::default());
    assert_eq!(device.call_ui.mwi_lamp_mode, LampMode::On);
    assert!(!device.call_ui.mwi_on_call);
    assert!(device.conference.allowed);
    assert_eq!(device.conference.dialing.application_options, "Mac");
    assert_eq!(
        device.feature_defaults.forwarding,
        ForwardingDefaults::default()
    );
    assert!(
        device
            .buttons
            .iter()
            .any(|button| matches!(button, ButtonDefinition::AddonModule(_)))
    );

    let line = config.features_for_line("1001").unwrap();
    assert_eq!(line.media.video_mode, VideoMode::Auto);
    assert_eq!(line.conference.destination.as_deref(), Some("700"));
    assert_eq!(
        line.voicemail
            .number
            .as_ref()
            .map(VoicemailDestination::as_str),
        Some("600")
    );
    assert_eq!(line.registration.extensions.len(), 2);
}

#[test]
fn omitted_codec_policy_allows_every_mapped_audio_format() {
    let defaults = GeneralConfig::default();
    assert_eq!(defaults.codecs, mapped_audio_codecs());
    assert!(
        defaults
            .codecs
            .iter()
            .copied()
            .all(|codec| pbx_audio_format(codec).is_ok())
    );
}

#[test]
fn explicitly_unrepresentable_audio_codec_is_rejected() {
    for codec in ["isac", "aac", "amr", "g728", "activevoice"] {
        let input = CONFIG.replace(
            "disallow = all\n        allow = ulaw\n        allow = alaw",
            &format!("disallow = all\n        allow = {codec}"),
        );
        assert!(matches!(
            ModuleConfig::parse(&input),
            Err(ConfigError::InvalidValue { .. })
        ));
    }
}

#[test]
fn parses_native_configuration_and_builds_definitions() {
    let config = ModuleConfig::parse(CONFIG).unwrap();
    assert_eq!(
        config.general.advertised_address,
        "192.0.2.10".parse::<Ipv4Addr>().unwrap()
    );
    assert_eq!(
        config.general.codecs,
        [
            Codec::Pcmu,
            Codec::G711Ulaw56k,
            Codec::Pcma,
            Codec::G711Alaw56k,
        ]
    );
    assert_eq!(config.general.remote_hangup_tone, None);
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let profile = config.soft_key_profile_for_device(&device_id).unwrap();
    assert_eq!(profile.name, DEFAULT_SOFT_KEY_PROFILE);
    assert_eq!(profile.sets.len(), KeyMode::ALL_KNOWN.len());
    assert_eq!(profile.actions(KeyMode::OnHook), [SoftKey::NewCall]);
    assert_eq!(
        profile.actions(KeyMode::Connected),
        [SoftKey::Hold, SoftKey::EndCall, SoftKey::Transfer]
    );
    let features = config.feature_defaults_for_device(&device_id).unwrap();
    assert_eq!(features, &DeviceFeatureDefaults::default());
    let line_features = config.features_for_line("1001").unwrap();
    let mut expected_line_features = LineFeatureConfig::default();
    expected_line_features.registration.extensions = vec![RegistrationExtension {
        extension: "1001".into(),
        context: None,
    }];
    expected_line_features.media.codecs = config.general.codecs.clone();
    assert_eq!(line_features, &expected_line_features);
    assert!(config.registration_contexts().is_empty());
    assert!(
        config
            .registration_targets_for_line("1001")
            .unwrap()
            .is_empty()
    );
    let binding = config.line("1001").unwrap();
    assert_eq!(binding.device_id.as_str(), "SEP001122334455");
    assert_eq!(binding.line_instance, 1);
    assert_eq!(binding.line.caller_name, "Reception");
    assert_eq!(
        config.device_definitions()[0].first_line().unwrap().number,
        "1001"
    );
    assert_eq!(config.dial_target("1001"), Some(binding));
    assert_eq!(config.dial_target("SEP001122334455/1001"), Some(binding));
    assert!(config.dial_target("SEP000000000000/1001").is_none());
    assert_eq!(config.appearances_for_line("1001").count(), 1);
    assert_eq!(
        config
            .appearances_for_device(&binding.device_id)
            .map(|appearance| appearance.line_instance)
            .collect::<Vec<_>>(),
        [1]
    );
}

#[test]
fn device_description_and_line_button_label_remain_distinct() {
    let input = CONFIG
        .replace("description = Reception", "description = coral")
        .replace("line = 1001", "button = line, 1001, label=ATP");
    let config = ModuleConfig::parse(&input).unwrap();
    let definition = config.device_definitions().remove(0);
    let primary = definition.first_line().unwrap();

    assert_eq!(definition.description, "coral");
    assert_eq!(primary.number, "1001");
    assert_eq!(primary.display_label(), "ATP");
}

#[test]
fn parses_bounded_channel_metadata_with_exact_inheritance_and_order() {
    let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        language = sv\n        accountcode = general-private",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        setvar = DEVICE_CLASS=desk\n        setvar = __TRACE_ID=alpha",
            )
            .replace(
                "mailbox = 1001@default",
                "mailbox = 1001@default\n        language = en_GB\n        accountcode = line-private\n        setvar = LINE_CLASS=reception",
            );
    let config = ModuleConfig::parse(&input).unwrap();
    assert_eq!(config.general.language, "sv");
    assert_eq!(
        config.general.account_code.as_deref(),
        Some("general-private")
    );
    let device = config
        .devices
        .get(&DeviceId::new("SEP001122334455").unwrap())
        .unwrap();
    assert_eq!(
        device
            .channel_variables
            .iter()
            .map(|variable| variable.name())
            .collect::<Vec<_>>(),
        ["DEVICE_CLASS", "__TRACE_ID"]
    );
    let line = config.lines.get("1001").unwrap();
    assert_eq!(line.language, "en_GB");
    assert_eq!(line.account_code.as_deref(), Some("line-private"));
    assert_eq!(line.channel_variables[0].name(), "LINE_CLASS");
    let debug = format!("{config:?}");
    for private in [
        "general-private",
        "line-private",
        "DEVICE_CLASS",
        "desk",
        "TRACE_ID",
        "LINE_CLASS",
    ] {
        assert!(!debug.contains(private), "debug leaked {private}");
    }

    let inherited = ModuleConfig::parse(
            &CONFIG.replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        language = sv\n        accountcode = general-private",
            ),
        )
        .unwrap();
    let line = inherited.lines.get("1001").unwrap();
    assert_eq!(line.language, "sv");
    assert_eq!(line.account_code.as_deref(), Some("general-private"));
}

#[test]
fn channel_metadata_rejects_unsafe_or_duplicate_assignments_without_disclosure() {
    for assignment in [
        "FUNC(value)=private-one",
        "AUTHORIZATION_TOKEN=private-two",
        "DUPLICATE=one\n        setvar = DUPLICATE=two",
        "EMPTY=",
    ] {
        let input = CONFIG.replace(
            "description = Reception",
            &format!("description = Reception\n        setvar = {assignment}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("<redacted>"), "{error}");
        assert!(!error.contains("private-one"), "{error}");
        assert!(!error.contains("private-two"), "{error}");
    }

    let input = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        &format!(
            "advertised_address = 192.0.2.10\n        accountcode = {}",
            "s".repeat(MAX_ACCOUNT_CODE_BYTES + 1)
        ),
    );
    let error = ModuleConfig::parse(&input).unwrap_err().to_string();
    assert!(error.contains("<redacted>"), "{error}");
    assert!(!error.contains(&"s".repeat(MAX_ACCOUNT_CODE_BYTES + 1)));
}

#[test]
fn call_selection_and_station_history_policies_are_typed_with_safe_defaults() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    assert_eq!(defaults.call_answer_order(), CallAnswerOrder::OldestFirst);
    assert_eq!(
        defaults.call_ui_for_device(&device_id),
        Some(&DeviceCallUiConfig {
            redial_mode: RedialMode::LastNumber,
            hinted_ringing_notification: false,
            ..DeviceCallUiConfig::default()
        })
    );

    let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        callanswerorder = LastFirst",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        useRedialMenu = yes\n        allowRinginNotification = on\n        mwilamp = flash\n        mwioncall = yes\n        phonecodepage = ASCII",
            );
    let configured = ModuleConfig::parse(&input).unwrap();
    assert_eq!(configured.call_answer_order(), CallAnswerOrder::LastFirst);
    assert_eq!(
        configured.call_ui_for_device(&device_id),
        Some(&DeviceCallUiConfig {
            redial_mode: RedialMode::PlacedCallsMenu,
            hinted_ringing_notification: true,
            mwi_lamp_mode: LampMode::Flash,
            mwi_on_call: true,
            legacy_code_page: LegacyCodePage::Ascii,
        })
    );
}

#[test]
fn station_calendar_policy_is_typed_and_bounded() {
    let input = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        dateformat = Y.M.DA\n        tzoffset = -8",
    );
    let parsed = ModuleConfig::parse(&input).unwrap();
    assert_eq!(parsed.general.date_template.as_str(), "Y.M.DA");
    assert!(parsed.general.date_template.uses_twelve_hour_clock());
    assert_eq!(parsed.general.timezone_offset_minutes, -480);

    for setting in ["dateformat = MDY", "dateformat = D/M-M", "tzoffset = 15"] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {setting}"),
        );
        assert!(ModuleConfig::parse(&input).is_err(), "accepted {setting}");
    }
}

#[test]
fn ringing_waiting_and_incoming_limit_policies_are_typed_and_bounded() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert_eq!(defaults.general.ring_type, RingerMode::Outside);
    assert_eq!(defaults.general.call_waiting_tone, Some(Tone::CallWaiting));
    assert_eq!(defaults.general.call_waiting_interval_seconds, 0);
    assert_eq!(
        defaults.features_for_line("1001").unwrap().incoming_limit,
        6
    );

    let configured = ModuleConfig::parse(
            &CONFIG
                .replace(
                    "advertised_address = 192.0.2.10",
                    "advertised_address = 192.0.2.10\n        ringtype = Urgent\n        callwaitingtone = PriorityCallWaiting\n        callwaitinginterval = 12",
                )
                .replace("mailbox = 1001@default", "mailbox = 1001@default\n        incominglimit = 2"),
        )
        .unwrap();
    assert_eq!(configured.general.ring_type, RingerMode::Urgent);
    assert_eq!(
        configured.general.call_waiting_tone,
        Some(Tone::PriorityCallWaiting)
    );
    assert_eq!(configured.general.call_waiting_interval_seconds, 12);
    assert_eq!(
        configured.features_for_line("1001").unwrap().incoming_limit,
        2
    );

    let disabled = ModuleConfig::parse(&CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        callwaitingtone = 0",
    ))
    .unwrap();
    assert_eq!(disabled.general.call_waiting_tone, None);

    for setting in [
        "ringtype = emergency",
        "callwaitingtone = unknown",
        "callwaitinginterval = -1",
        "callwaitinginterval = 86401",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {setting}"),
        );
        assert!(ModuleConfig::parse(&input).is_err(), "accepted {setting}");
    }
    for setting in ["incominglimit = -1", "incominglimit = 256"] {
        let input = CONFIG.replace(
            "mailbox = 1001@default",
            &format!("mailbox = 1001@default\n        {setting}"),
        );
        assert!(ModuleConfig::parse(&input).is_err(), "accepted {setting}");
    }
}

#[test]
fn fallback_registration_policy_has_safe_typed_defaults() {
    let config = ModuleConfig::parse(CONFIG).unwrap();

    assert_eq!(
        config.fallback_registration(),
        &FallbackRegistrationConfig {
            decision: FallbackDecision::Reject,
            backoff_seconds: 60,
            server_priority: 1,
        }
    );
}

#[test]
fn transfer_on_hangup_is_disabled_by_default_and_exactly_named() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert!(!defaults.general.transfer_on_hangup);

    let configured = ModuleConfig::parse(&CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        transfer_on_hangup = yes",
    ))
    .unwrap();
    assert!(configured.general.transfer_on_hangup);

    for settings in [
        "transfer-on-hangup = yes",
        "transferonhangup = yes",
        "transfer_on_hangup = maybe",
        "transfer_on_hangup = yes\n        transfer_on_hangup = no",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            ModuleConfig::parse(&input).is_err(),
            "accepted invalid transfer policy: {settings}",
        );
    }
}

#[test]
fn first_digit_timeout_is_distinct_bounded_and_exactly_named() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert_eq!(defaults.general.first_digit_timeout_ms, 10_000);

    let configured = ModuleConfig::parse(&CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        firstdigittimeout = 16",
    ))
    .unwrap();
    assert_eq!(configured.general.first_digit_timeout_ms, 16_000);

    for settings in [
        "firstdigittimeout = 0",
        "firstdigittimeout = 86401",
        "firstdigittimeout = -1",
        "firstdigittimeout = 10\n        firstdigittimeout = 11",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid first-digit timeout: {settings}"
        );
    }
}

#[test]
fn subsequent_digit_timeout_accepts_exact_seconds_or_milliseconds() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert_eq!(defaults.general.interdigit_timeout_ms, 5_000);

    for (setting, expected_ms) in [
        ("digittimeout = 8", 8_000),
        ("interdigit_timeout_ms = 1750", 1_750),
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {setting}"),
        );
        assert_eq!(
            ModuleConfig::parse(&input)
                .unwrap()
                .general
                .interdigit_timeout_ms,
            expected_ms
        );
    }

    for settings in [
        "digittimeout = 0",
        "digittimeout = 86401",
        "interdigit_timeout_ms = 249",
        "interdigit_timeout_ms = 86400001",
        "interdigittimeout = 5",
        "digittimeout = 5\n        interdigit_timeout_ms = 1500",
        "digittimeout = 5\n        digittimeout = 6",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid subsequent-digit timeout: {settings}"
        );
    }
}

#[test]
fn dial_terminator_is_typed_bounded_and_exactly_named() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert_eq!(
        defaults.general.dial_terminator,
        DialTerminatorConfig::default()
    );

    for (raw, expected) in [
        ("0", '0'),
        ("9", '9'),
        ("*", '*'),
        ("#", '#'),
        ("a", 'A'),
        ("D", 'D'),
    ] {
        let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!(
                    "advertised_address = 192.0.2.10\n        digittimeoutchar = {raw}\n        recorddigittimeoutchar = yes"
                ),
            );
        assert_eq!(
            ModuleConfig::parse(&input).unwrap().general.dial_terminator,
            DialTerminatorConfig {
                character: expected,
                record: true,
            }
        );
    }

    for settings in [
        "digittimeoutchar =",
        "digittimeoutchar = 12",
        "digittimeoutchar = E",
        "digittimeoutchar = +",
        "recorddigittimeoutchar = perhaps",
        "digittimeoutchar = #\n        digittimeoutchar = *",
        "recorddigittimeoutchar = yes\n        recorddigittimeoutchar = no",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid dial terminator policy: {settings}"
        );
    }
}

#[test]
fn simulated_enbloc_has_a_safe_exact_boolean_policy() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert!(defaults.general.simulate_enbloc);

    let disabled = ModuleConfig::parse(&CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        simulate_enbloc = no",
    ))
    .unwrap();
    assert!(!disabled.general.simulate_enbloc);

    for settings in [
        "simulateenbloc = yes",
        "simulate_enbloc = perhaps",
        "simulate_enbloc = yes\n        simulate_enbloc = no",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid simulated en-bloc policy: {settings}"
        );
    }
}

#[test]
fn speed_dial_further_digit_policy_is_explicit_and_disabled_by_default() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert!(!defaults.general.speed_dial_await_further_digits);
    assert!(
        defaults
            .device_definitions()
            .iter()
            .all(|device| !device.ui.speed_dial_await_further_digits)
    );

    let enabled = ModuleConfig::parse(&CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        SpeedDialAwaitFurtherDigits = yes",
    ))
    .unwrap();
    assert!(enabled.general.speed_dial_await_further_digits);
    assert!(
        enabled
            .device_definitions()
            .iter()
            .all(|device| device.ui.speed_dial_await_further_digits)
    );

    for settings in [
        "SpeedDialAwaitFurtherDigits = perhaps",
        "SpeedDialAwaitFurtherDigits = yes\n        SpeedDialAwaitFurtherDigits = no",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid speed-dial further-digit policy: {settings}"
        );
    }
}

#[test]
fn overlap_dialing_is_explicit_disabled_by_default_and_device_overridable() {
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert!(!defaults.general.allow_overlap);
    assert!(!defaults.devices[&device_id].allow_overlap);

    let enabled = ModuleConfig::parse(&CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        allowoverlap = yes",
    ))
    .unwrap();
    assert!(enabled.general.allow_overlap);
    assert!(enabled.devices[&device_id].allow_overlap);

    let overridden = ModuleConfig::parse(
        &CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        allowoverlap = yes",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        allowoverlap = no",
            ),
    )
    .unwrap();
    assert!(!overridden.devices[&device_id].allow_overlap);

    for settings in [
        "allowoverlap = perhaps",
        "allowoverlap = yes\n        allowoverlap = no",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted unsafe overlap setting: {settings}"
        );
    }
}

#[test]
fn line_dial_tones_are_typed_inherited_bounded_and_exactly_named() {
    let defaults = ModuleConfig::parse(CONFIG).unwrap();
    assert_eq!(
        defaults.features_for_line("1001").unwrap().dial_tones,
        LineDialToneConfig::default()
    );

    let configured = ModuleConfig::parse(&CONFIG.replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        initial_dialtone_tone = Recall Dial Tone\n        secondary_dialtone_digits = 9a#\n        secondary_dialtone_tone = 0x2a",
        ))
        .unwrap();
    assert_eq!(
        configured.features_for_line("1001").unwrap().dial_tones,
        LineDialToneConfig {
            initial: Tone::RecallDial,
            secondary_prefix: Some("9A#".into()),
            secondary: Tone::PartialDial,
        }
    );
    let station_line = configured
        .device_definitions()
        .into_iter()
        .flat_map(|device| device.buttons)
        .find_map(|button| match button {
            ButtonDefinition::Line(line) if line.number == "1001" => Some(line),
            _ => None,
        })
        .unwrap();
    assert_eq!(station_line.initial_tone, Tone::RecallDial);

    let cleared = ModuleConfig::parse(&CONFIG.replace(
        "mailbox = 1001@default",
        "mailbox = 1001@default\n        secondary_dialtone_digits =",
    ))
    .unwrap();
    assert_eq!(
        cleared
            .features_for_line("1001")
            .unwrap()
            .dial_tones
            .secondary_prefix,
        None
    );

    for setting in [
        "initialdialtonetone = Inside Dial Tone",
        "secondarydialtonedigits = 9",
        "secondarydialtonetone = Outside Dial Tone",
        "secondary_dialtone_digits = 1234567890",
        "secondary_dialtone_digits = 9+",
        "secondary_dialtone_tone = unknown tone",
        "secondary_dialtone_digits = 9\n        secondary_dialtone_digits = 8",
    ] {
        let input = CONFIG.replace(
            "mailbox = 1001@default",
            &format!("mailbox = 1001@default\n        {setting}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid line dial-tone setting: {setting}"
        );
    }
}

#[test]
fn parses_fallback_decision_priority_and_backoff() {
    for (raw, expected) in [
        ("yes", FallbackDecision::Accept),
        ("no", FallbackDecision::Reject),
        ("odd", FallbackDecision::DeviceIdOdd),
        ("even", FallbackDecision::DeviceIdEven),
    ] {
        let input = CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!(
                    "advertised_address = 192.0.2.10\n        fallback = {raw}\n        backoff_time = 90\n        server_priority = 2"
                ),
            );
        let config = ModuleConfig::parse(&input).unwrap();
        assert_eq!(
            config.fallback_registration(),
            &FallbackRegistrationConfig {
                decision: expected,
                backoff_seconds: 90,
                server_priority: 2,
            }
        );
    }
}

#[test]
fn rejects_invalid_duplicate_or_invented_fallback_settings() {
    for settings in [
        "fallback = sometimes",
        "fallback = /private/runner",
        "backoff_time = 29",
        "backoff_time = -1",
        "server_priority = 0",
        "server_priority = -1",
        "server_priority = 256",
        "fallback_mode = yes",
        "backoff-time = 60",
        "server-priority = 1",
        "fallback = yes\n        fallback = no",
        "backoff_time = 60\n        backoff_time = 90",
        "server_priority = 1\n        server_priority = 2",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid fallback settings: {settings}"
        );
    }

    let rejected_path = "/private/runner";
    let error = ModuleConfig::parse(&CONFIG.replace(
        "advertised_address = 192.0.2.10",
        &format!("advertised_address = 192.0.2.10\n        fallback = {rejected_path}"),
    ))
    .unwrap_err();
    assert!(!error.to_string().contains(rejected_path));
}

#[test]
fn parses_bounded_transport_specific_server_routes() {
    let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        secondary_keepalive = 45\n        signaling_server = 1, primary, 192.0.2.10, 2000, 2443\n        signaling_server = 2, backup, 2001:db8::20, 2001, none",
        );
    let config = ModuleConfig::parse(&input).unwrap();

    assert_eq!(config.general.secondary_keepalive_seconds, 45);
    assert_eq!(
        config.general.signaling_servers,
        [
            SignalingServerRoute {
                priority: 1,
                name: "primary".into(),
                address: "192.0.2.10".parse().unwrap(),
                clear_port: std::num::NonZeroU16::new(2000),
                secure_port: std::num::NonZeroU16::new(2443),
            },
            SignalingServerRoute {
                priority: 2,
                name: "backup".into(),
                address: "2001:db8::20".parse().unwrap(),
                clear_port: std::num::NonZeroU16::new(2001),
                secure_port: None,
            },
        ]
    );
}

#[test]
fn rejects_ambiguous_or_unusable_server_routes() {
    for settings in [
        "signaling_server = 1, primary, 192.0.2.10, none, none",
        "signaling_server = 1, primary, 192.0.2.10, 2000, none\n        signaling_server = 1, duplicate, 192.0.2.20, 2001, none",
        "server_priority = 2\n        signaling_server = 1, primary, 192.0.2.10, 2000, none",
        "signaling-server = 1, primary, 192.0.2.10, 2000, none",
        "secondarykeepalive = 45",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        assert!(ModuleConfig::parse(&input).is_err(), "accepted {settings}");
    }
}

#[test]
fn station_history_policies_follow_device_template_scalar_inheritance() {
    let input = CONFIG
            .replace(
                "[SEP001122334455]",
                "[station-ui](!)\n        type = device\n        useRedialMenu = yes\n        allowRinginNotification = yes\n\n        [SEP001122334455](station-ui)",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        useRedialMenu = no",
            );
    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    assert_eq!(
        config.call_ui_for_device(&device_id),
        Some(&DeviceCallUiConfig {
            redial_mode: RedialMode::LastNumber,
            hinted_ringing_notification: true,
            ..DeviceCallUiConfig::default()
        })
    );
}

#[test]
fn call_answer_order_rejects_documentation_typos_and_invented_values() {
    for value in [
        "lastestfirst",
        "latestfirst",
        "newestfirst",
        "oldest",
        "1",
        "",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        callanswerorder = {value}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("[general].callanswerorder"), "{error}");
        assert!(error.contains("OldestFirst or LastFirst"), "{error}");
    }
}

#[test]
fn call_ui_options_reject_invented_names_values_and_wrong_scopes() {
    for setting in [
        "redialmenu = yes",
        "ringing_notification = yes",
        "useRedialMenu = placedcalls",
        "allowRinginNotification = ringing",
        "callanswerorder = LastFirst",
    ] {
        let input = CONFIG.replace(
            "description = Reception",
            &format!("description = Reception\n        {setting}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("line "), "{setting} produced {error}");
        assert!(error.contains("expected"), "{setting} produced {error}");
    }

    let wrong_general_scope = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        useRedialMenu = yes",
    );
    let error = ModuleConfig::parse(&wrong_general_scope)
        .unwrap_err()
        .to_string();
    assert!(error.contains("[general].useRedialMenu"), "{error}");
    assert!(error.contains("expected"), "{error}");

    let invented_general_name = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        answer_call_order = LastFirst",
    );
    let error = ModuleConfig::parse(&invented_general_name)
        .unwrap_err()
        .to_string();
    assert!(error.contains("[general].answer_call_order"), "{error}");
    assert!(error.contains("unknown variant"), "{error}");
}

#[test]
fn duplicate_call_selection_and_station_history_settings_are_rejected() {
    let duplicate_general = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        callanswerorder = OldestFirst\n        CALLANSWERORDER = LastFirst",
        );
    let error = ModuleConfig::parse(&duplicate_general)
        .unwrap_err()
        .to_string();
    assert!(error.contains("[general].CALLANSWERORDER"), "{error}");
    assert!(error.contains("duplicates"), "{error}");

    for setting in [
        "useRedialMenu = yes\n        USEREDIALMENU = no",
        "allowRinginNotification = yes\n        ALLOWRINGINNOTIFICATION = no",
    ] {
        let input = CONFIG.replace(
            "description = Reception",
            &format!("description = Reception\n        {setting}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("[SEP001122334455]"), "{error}");
        assert!(error.contains("duplicates"), "{error}");
    }
}

#[test]
fn parses_typed_mobility_pin_contexts_extensions_and_resolved_targets() {
    let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        regcontext = registrations & backup-registrations",
            )
            .replace(
                "mailbox = 1001@default",
                "mailbox = 1001@default\n        pin = 0012345\n        regexten = 1001 & 91001@external-registrations",
            );
    let config = ModuleConfig::parse(&input).unwrap();

    assert_eq!(
        config.registration_contexts(),
        ["registrations", "backup-registrations"]
    );
    let mobility = config.mobility_for_line("1001").unwrap();
    let pin = mobility.pin.as_ref().unwrap();
    assert_eq!(pin.digits(), 7);
    assert!(pin.verify("0012345"));
    assert!(!pin.verify("12345"));
    assert_eq!(format!("{pin:?}"), "MobilityPin(<redacted>)");
    assert!(!format!("{config:?}").contains("0012345"));

    assert_eq!(
        config.registration_for_line("1001").unwrap().extensions,
        [
            RegistrationExtension {
                extension: "1001".into(),
                context: None,
            },
            RegistrationExtension {
                extension: "91001".into(),
                context: Some("external-registrations".into()),
            },
        ]
    );
    assert_eq!(
        config.registration_targets_for_line("1001").unwrap(),
        [
            RegistrationTarget {
                extension: "1001".into(),
                context: "registrations".into(),
            },
            RegistrationTarget {
                extension: "1001".into(),
                context: "backup-registrations".into(),
            },
            RegistrationTarget {
                extension: "91001".into(),
                context: "external-registrations".into(),
            },
        ]
    );
}

#[test]
fn omitted_or_cleared_registration_extension_uses_the_logical_line_number() {
    let input = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        regcontext = primary&secondary",
    );
    let config = ModuleConfig::parse(&input).unwrap();

    assert_eq!(
        config.registration_for_line("1001").unwrap().extensions,
        [RegistrationExtension {
            extension: "1001".into(),
            context: None,
        }]
    );
    assert_eq!(
        config.registration_targets_for_line("1001").unwrap(),
        [
            RegistrationTarget {
                extension: "1001".into(),
                context: "primary".into(),
            },
            RegistrationTarget {
                extension: "1001".into(),
                context: "secondary".into(),
            },
        ]
    );

    let cleared = ModuleConfig::parse(&input.replace(
        "type = line\n        label = Reception",
        "type = line\n        pin =\n        regexten =\n        label = Reception",
    ))
    .unwrap();
    assert_eq!(
        cleared.registration_for_line("1001").unwrap().extensions,
        config.registration_for_line("1001").unwrap().extensions
    );
    assert!(cleared.mobility_for_line("1001").unwrap().pin.is_none());
}

#[test]
fn mobility_and_registration_settings_follow_line_template_scalar_inheritance() {
    let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        regcontext = registrations",
            )
            .replace(
                "[1001]",
                "[mobile-line](!)\n        type = line\n        pin = 7654321\n        regexten = 91001\n\n        [1001](mobile-line)",
            );
    let inherited = ModuleConfig::parse(&input).unwrap();
    assert!(
        inherited
            .mobility_for_line("1001")
            .unwrap()
            .pin
            .as_ref()
            .unwrap()
            .verify("7654321")
    );
    assert_eq!(
        inherited.registration_targets_for_line("1001").unwrap(),
        [RegistrationTarget {
            extension: "91001".into(),
            context: "registrations".into(),
        }]
    );

    let cleared = ModuleConfig::parse(&input.replace(
        "[1001](mobile-line)",
        "[1001](mobile-line)\n        pin =\n        regexten =",
    ))
    .unwrap();
    assert!(cleared.mobility_for_line("1001").unwrap().pin.is_none());
    assert_eq!(
        cleared.registration_targets_for_line("1001").unwrap(),
        [RegistrationTarget {
            extension: "1001".into(),
            context: "registrations".into(),
        }]
    );
}

#[test]
fn mobility_pin_errors_are_located_and_never_disclose_the_value() {
    for pin in ["12A4", "12345678"] {
        let input = CONFIG.replace(
            "mailbox = 1001@default",
            &format!("mailbox = 1001@default\n        pin = {pin}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err();
        let text = error.to_string();
        assert!(text.contains("[1001].pin"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
        assert!(!text.contains(pin), "{text}");
    }
}

#[test]
fn mobility_pin_verification_checks_full_bound_and_all_diagnostics_are_redacted() {
    let pin = MobilityPin("0123456".into());
    assert!(pin.verify("0123456"));
    for candidate in [
        "1123456", "0023456", "0133456", "0124456", "0123556", "0123466", "0123457", "", "0",
        "012345", "01234567",
    ] {
        assert!(
            !pin.verify(candidate),
            "accepted mismatched candidate length {}",
            candidate.len()
        );
    }

    let debug = format!("{pin:?}");
    assert_eq!(debug, "MobilityPin(<redacted>)");
    assert!(!debug.contains("0123456"));
    let config = ModuleConfig::parse(&CONFIG.replace(
        "mailbox = 1001@default",
        "mailbox = 1001@default\n        pin = 0123456",
    ))
    .unwrap();
    let diagnostic = format!("{config:?}");
    assert!(!diagnostic.contains("0123456"));
    assert!(diagnostic.contains("MobilityPin(<redacted>)"));
}

#[test]
fn mobility_and_registration_options_reject_invented_aliases() {
    let general_alias = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        reg_context = registrations",
    );
    let text = ModuleConfig::parse(&general_alias).unwrap_err().to_string();
    assert!(text.contains("[general].reg_context"), "{text}");
    assert!(text.contains("unknown variant"), "{text}");

    let extension_alias = CONFIG.replace(
        "mailbox = 1001@default",
        "mailbox = 1001@default\n        reg_exten = 91001",
    );
    let text = ModuleConfig::parse(&extension_alias)
        .unwrap_err()
        .to_string();
    assert!(text.contains("[1001].reg_exten"), "{text}");
    assert!(text.contains("unknown variant"), "{text}");

    let pin_alias = CONFIG.replace(
        "mailbox = 1001@default",
        "mailbox = 1001@default\n        p-in = 7654321",
    );
    let text = ModuleConfig::parse(&pin_alias).unwrap_err().to_string();
    assert!(text.contains("[1001].p-in"), "{text}");
    assert!(text.contains("unknown variant"), "{text}");
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("7654321"), "{text}");
    assert!(text.contains("<redacted>"), "{text}");
    assert!(!text.contains("7654321"), "{text}");
}

#[test]
fn registration_lists_reject_empty_duplicate_oversized_and_unscoped_entries() {
    for contexts in [
        "primary&&secondary".to_owned(),
        "primary&primary".to_owned(),
        "primary context".to_owned(),
        "x".repeat(MAX_REGISTRATION_IDENTIFIER_BYTES + 1),
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        regcontext = {contexts}"),
        );
        let text = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(text.contains("[general].regcontext"), "{text}");
        assert!(text.contains("expected"), "{text}");
    }

    for extensions in [
        "1001&&2000".to_owned(),
        "@registrations".to_owned(),
        "1001@".to_owned(),
        "1001@one@two".to_owned(),
        "1001&1001".to_owned(),
        "1001&1001@registrations".to_owned(),
        "10 01".to_owned(),
        "x".repeat(MAX_REGISTRATION_IDENTIFIER_BYTES + 1),
        vec!["x".repeat(64); 4].join("&"),
    ] {
        let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        regcontext = registrations",
            )
            .replace(
                "mailbox = 1001@default",
                &format!("mailbox = 1001@default\n        regexten = {extensions}"),
            );
        let text = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(text.contains("[1001].regexten"), "{text}");
        assert!(text.contains("expected"), "{text}");
    }

    let unscoped = CONFIG.replace(
        "mailbox = 1001@default",
        "mailbox = 1001@default\n        regexten = 91001",
    );
    let text = ModuleConfig::parse(&unscoped).unwrap_err().to_string();
    assert!(text.contains("[1001].regexten"), "{text}");
    assert!(text.contains("general regcontext"), "{text}");
}

#[test]
fn duplicate_resolved_registration_targets_across_lines_are_rejected() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10
            regcontext = registrations

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002

            [1001]
            type = line
            regexten = shared

            [1002]
            type = line
            regexten = shared@registrations
        "#;
    let text = ModuleConfig::parse(input).unwrap_err().to_string();
    assert!(text.contains("[1002].regexten"), "{text}");
    assert!(text.contains("shared@registrations"), "{text}");
    assert!(text.contains("already used by [1001]"), "{text}");
}

#[test]
fn network_listener_and_qos_defaults_are_normalized() {
    let config = ModuleConfig::parse(CONFIG).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(config.listener_policy(), &ListenerPolicy::default());
    assert_eq!(config.qos_policy(), &QosPolicy::default());
    assert_eq!(
        config.qos_policy().signaling,
        QosClass {
            dscp: Dscp(24),
            cos: Cos(4),
        }
    );
    let mut expected_network = NetworkPolicy::default();
    expected_network.advertised.ipv4 = Some("192.0.2.10".parse().unwrap());
    assert_eq!(config.network_policy(), &expected_network);
    assert_eq!(
        config.network_for_device(&device_id),
        Some(&DeviceNetworkPolicy {
            acl: AccessControlList::default(),
            permitted_hosts: Vec::new(),
            nat: NatMode::Auto,
            qos: QosPolicy::default(),
            transport: TransportRequirement::Either,
        })
    );
}

#[test]
fn parses_ipv4_ipv6_acl_nat_qos_and_split_tls_policy() {
    let input = CONFIG
            .replace(
                "bind = 0.0.0.0:2000\n        advertised_address = 192.0.2.10",
                "bindaddr = ::\n        port = 2001\n        advertised_ipv4 = none\n        advertised_ipv6 = 2001:db8::10\n        deny = 0.0.0.0/0\n        permit = 192.0.2.99/255.255.255.0\n        permit = 2001:db8:1::99/64\n        localnet =\n        localnet = 10.10.99.1/16\n        localnet = 2001:db8:2::99/64\n        externhost = PBX.EXAMPLE.test\n        externrefresh = 120\n        nat = (auto)on\n        signaling_dscp = AF31\n        signaling_cos = 5\n        audio_tos = 0xb8\n        audio_cos = 6\n        video_dscp = CS4\n        video_cos = 4\n        tls_bind = [::]:2443\n        tls_certificate = /etc/asterisk/tls/server.crt\n        tls_private_key = /etc/asterisk/tls/server.key\n        tls_trust_store = /etc/asterisk/tls/ca.pem",
            )
            .replace(
                "description = Reception",
                "description = Reception\n        deny =\n        permit = 203.0.113.99/24\n        permit_host = PHONE.EXAMPLE.test\n        nat = off\n        audio_dscp = EF\n        audio_cos = 7\n        video_tos = 0x88\n        transport = tls",
            );
    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(config.listener_policy().clear, "[::]:2001".parse().unwrap());
    assert_eq!(
        config.listener_policy().tls,
        Some(TlsListener {
            bind: "[::]:2443".parse().unwrap(),
            credentials: TlsCredentials::SplitPem {
                certificate: PathBuf::from("/etc/asterisk/tls/server.crt"),
                private_key: PathBuf::from("/etc/asterisk/tls/server.key"),
                trust_store: Some(PathBuf::from("/etc/asterisk/tls/ca.pem")),
            },
        })
    );
    assert_eq!(
        config.network_policy().advertised,
        AdvertisedAddresses {
            ipv4: None,
            ipv6: Some("2001:db8::10".parse().unwrap()),
        }
    );
    assert_eq!(
        config.network_policy().external,
        Some(ExternalAddress::Hostname {
            name: "pbx.example.test".into(),
            refresh_seconds: 120,
        })
    );
    assert_eq!(config.network_policy().nat, NatMode::AutoOn);
    assert_eq!(
        config.network_policy().local_networks,
        [
            IpNetwork {
                address: "10.10.0.0".parse().unwrap(),
                prefix: 16,
            },
            IpNetwork {
                address: "2001:db8:2::".parse().unwrap(),
                prefix: 64,
            },
        ]
    );
    assert_eq!(
        config.qos_policy().signaling,
        QosClass {
            dscp: Dscp(26),
            cos: Cos(5)
        }
    );
    assert_eq!(
        config.qos_policy().audio,
        QosClass {
            dscp: Dscp(46),
            cos: Cos(6)
        }
    );
    assert_eq!(
        config.qos_policy().video,
        QosClass {
            dscp: Dscp(32),
            cos: Cos(4)
        }
    );

    let device = config.network_for_device(&device_id).unwrap();
    let station = config
        .device_definitions()
        .into_iter()
        .find(|definition| definition.id == device_id)
        .unwrap();
    assert_eq!(device.transport, TransportRequirement::Tls);
    assert_eq!(station.transport, StationTransportRequirement::Secure);
    assert_eq!(station.signaling_qos, Some(SignalingQos::new(26, 5)));
    assert_eq!(device.nat, NatMode::Off);
    assert_eq!(device.permitted_hosts, ["phone.example.test"]);
    assert_eq!(
        device.acl.rules,
        [AclRule {
            action: AclAction::Permit,
            network: IpNetwork {
                address: "203.0.113.0".parse().unwrap(),
                prefix: 24,
            },
        }]
    );
    assert_eq!(
        device.qos.audio,
        QosClass {
            dscp: Dscp(46),
            cos: Cos(7)
        }
    );
    assert_eq!(device.qos.video.dscp, Dscp(34));
}

#[test]
fn device_network_policy_inherits_scalars_and_clears_ordered_rules() {
    let input = CONFIG.replace(
            "[SEP001122334455]",
            "[network-base](!)\n        type = device\n        deny = 0.0.0.0/0\n        permit = 10.0.0.0/8\n        permit_host = old.example.test\n        audio_cos = 2\n        transport = clear\n\n        [network-site](!, network-base)\n        deny =\n        permit = 2001:db8:5::1/64\n        permit_host =\n        permit_host = phone.example.test\n        audio_cos = 7\n\n        [SEP001122334455](network-site)",
        );
    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let policy = config.network_for_device(&device_id).unwrap();

    assert_eq!(policy.transport, TransportRequirement::Clear);
    assert_eq!(policy.qos.audio.cos, Cos(7));
    assert_eq!(policy.permitted_hosts, ["phone.example.test"]);
    assert_eq!(policy.acl.rules.len(), 1);
    assert_eq!(policy.acl.rules[0].action, AclAction::Permit);
    assert_eq!(
        policy.acl.rules[0].network.address,
        "2001:db8:5::".parse::<IpAddr>().unwrap()
    );
}

#[test]
fn combined_pem_and_accepted_transport_aliases_are_typed() {
    let input = CONFIG
            .replace(
                "advertised_address = 192.0.2.10",
                "advertised_address = 192.0.2.10\n        secbindaddr = 0.0.0.0\n        secport = 2443\n        certfile = /etc/asterisk/tls/server.pem",
            )
            .replace("description = Reception", "description = Reception\n        transport_requirement = secure");
    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(
        config.listener_policy().tls.as_ref().unwrap().credentials,
        TlsCredentials::CombinedPem(PathBuf::from("/etc/asterisk/tls/server.pem"))
    );
    assert_eq!(
        config.network_for_device(&device_id).unwrap().transport,
        TransportRequirement::Tls
    );
}

#[test]
fn single_advertised_address_alias_selects_exactly_one_ip_family() {
    let input = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 2001:db8::20",
    );
    let config = ModuleConfig::parse(&input).unwrap();

    assert_eq!(
        config.network_policy().advertised,
        AdvertisedAddresses {
            ipv4: None,
            ipv6: Some("2001:db8::20".parse().unwrap()),
        }
    );
}

#[test]
fn accepted_nat_and_transport_spellings_have_one_typed_result_each() {
    for (raw, expected) in [
        ("auto", NatMode::Auto),
        ("off", NatMode::Off),
        ("(auto)off", NatMode::AutoOff),
        ("on", NatMode::On),
        ("(auto)on", NatMode::AutoOn),
    ] {
        assert_eq!(parse_nat_mode("nat", raw).unwrap(), expected);
    }
    for (raw, expected) in [
        ("clear", TransportRequirement::Clear),
        ("tcp", TransportRequirement::Clear),
        ("tls", TransportRequirement::Tls),
        ("secure", TransportRequirement::Tls),
        ("either", TransportRequirement::Either),
        ("any", TransportRequirement::Either),
    ] {
        assert_eq!(
            parse_transport_requirement("transport", raw).unwrap(),
            expected
        );
    }
}

#[test]
fn rejects_invalid_acl_nat_qos_listener_and_external_ranges() {
    for (setting, expected) in [
        ("permit = 192.0.2.0/33", "prefix 0..32"),
        ("permit = 192.0.2.0/255.0.255.0", "contiguous IPv4 netmask"),
        ("permit = 2001:db8::/129", "IPv6 prefix 0..128"),
        ("nat = sometimes", "auto, off"),
        ("audio_dscp = 64", "DSCP 0..63"),
        ("video_cos = 8", "COS priority 0..7"),
        ("port = 0", "TCP port 1..65535"),
        (
            "tls_bind = 0.0.0.0:0\n        certfile = /tls.pem",
            "TLS listener port",
        ),
        ("externrefresh = 0", "1..86400"),
        ("externip = 0.0.0.0", "non-unspecified"),
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {setting}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("line "), "missing line in {error}");
        assert!(
            error.contains("[general]."),
            "missing section/key in {error}"
        );
        assert!(error.contains("expected"), "missing expectation in {error}");
        assert!(
            error.contains(expected),
            "{error} did not contain {expected}"
        );
    }
}

#[test]
fn rejects_network_listener_and_tls_contradictions() {
    for settings in [
        "bind = 0.0.0.0:2001\n        bindaddr = ::",
        "advertised_ipv4 = none\n        advertised_ipv6 = none",
        "advertised_address = 192.0.2.20\n        advertised_ipv4 = 192.0.2.21",
        "externip = 192.0.2.20\n        externhost = pbx.example.test",
        "externip = 192.0.2.20\n        externrefresh = 60",
        "tls_bind = 0.0.0.0:2000\n        certfile = /tls.pem",
        "certfile = /tls.pem\n        tls_certificate = /tls.crt\n        tls_private_key = /tls.key",
        "tls_certificate = /tls.crt",
        "tls_private_key = /tls.key",
        "audio_tos = 0xb8\n        audio_dscp = EF",
    ] {
        let input = CONFIG.replace(
            "bind = 0.0.0.0:2000\n        advertised_address = 192.0.2.10",
            settings,
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("[general]"), "{settings} produced {error}");
        assert!(error.contains("expected"), "{settings} produced {error}");
    }

    let input = CONFIG.replace(
        "description = Reception",
        "description = Reception\n        transport = tls",
    );
    let error = ModuleConfig::parse(&input).unwrap_err().to_string();
    assert!(error.contains("[SEP001122334455].transport"));
    assert!(error.contains("configured general TLS listener"));
}

#[test]
fn tls_errors_report_locations_without_leaking_private_paths() {
    let secret = "/do/not/expose/private-server-key.pem";
    let input = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        &format!("advertised_address = 192.0.2.10\n        tls_private_key = {secret}"),
    );
    let error = ModuleConfig::parse(&input).unwrap_err().to_string();

    assert!(error.contains("line 2 [general]"));
    assert!(error.contains("expected tls_certificate together with tls_private_key"));
    assert!(error.contains("<redacted>"));
    assert!(!error.contains(secret));
}

#[test]
fn tls_policy_debug_output_redacts_credential_paths() {
    let listener = TlsListener {
        bind: "127.0.0.1:2443".parse().unwrap(),
        credentials: TlsCredentials::SplitPem {
            certificate: PathBuf::from("/private/server-certificate.pem"),
            private_key: PathBuf::from("/private/server-key.pem"),
            trust_store: Some(PathBuf::from("/private/client-roots.pem")),
        },
    };

    let debug = format!("{listener:?}");
    assert!(debug.contains("<redacted>"));
    for private in ["server-certificate", "server-key", "client-roots"] {
        assert!(!debug.contains(private), "debug leaked {private}");
    }
}

#[test]
fn inherited_errors_retain_the_original_template_location() {
    let input = CONFIG.replace(
            "[SEP001122334455]",
            "[bad-network-template](!)\n        type = device\n        audio_cos = 9\n\n        [SEP001122334455](bad-network-template)",
        );
    let error = ModuleConfig::parse(&input).unwrap_err().to_string();

    assert!(error.contains("[bad-network-template].audio_cos"));
    assert!(!error.contains("[SEP001122334455].audio_cos"));
    assert!(error.contains("expected COS priority 0..7"));
}

#[test]
fn rejects_obsolete_and_wrong_scope_network_options_with_guidance() {
    for (scope, setting, guidance) in [
        (
            "general",
            "trustphoneip = yes",
            "peer addresses are always authoritative",
        ),
        (
            "device",
            "trustphoneip = yes",
            "peer addresses are always authoritative",
        ),
        ("device", "dtmfmode = rfc2833", "use force_dtmfmode"),
        ("line", "permit = 192.0.2.0/24", "unknown variant"),
        ("line", "audio_dscp = EF", "unknown variant"),
    ] {
        let input = match scope {
            "general" => CONFIG.replace(
                "advertised_address = 192.0.2.10",
                &format!("advertised_address = 192.0.2.10\n        {setting}"),
            ),
            "device" => CONFIG.replace(
                "description = Reception",
                &format!("description = Reception\n        {setting}"),
            ),
            "line" => CONFIG.replace(
                "label = Reception",
                &format!("label = Reception\n        {setting}"),
            ),
            _ => unreachable!(),
        };
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("line "), "{error}");
        assert!(error.contains(guidance), "{error}");
    }
}

#[test]
fn permits_one_logical_line_on_multiple_devices() {
    let config = format!("{CONFIG}\n[SEP112233445566]\ntype=device\nline=1001\n");
    let config = ModuleConfig::parse(&config).unwrap();
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP112233445566").unwrap();

    assert_eq!(config.line_appearance_count("1001"), 2);
    assert_eq!(config.appearances_for_line("1001").count(), 2);
    assert_eq!(
        config
            .dial_target("SEP001122334455/1001")
            .unwrap()
            .device_id,
        first
    );
    assert_eq!(
        config
            .dial_target("SEP112233445566/1001")
            .unwrap()
            .device_id,
        second
    );
}

#[test]
fn resolves_multilevel_device_and_line_templates_before_typing() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10

            [desk-keys]
            type = softkey_profile
            on_hook = redial, new_call

            [device-base](!)
            type = device
            description = Base phone
            softkey_profile = desk-keys
            button = speed_dial, Helpdesk, 2000

            [device-model](!, device-base)
            description = Model phone
            button = blf, Warehouse, 2001, 2001@internal

            [device-site](!)
            type = device
            description = Site phone
            button = empty

            [SEP001122334455](device-model, device-site)
            description = Reception phone
            button = line, 1001

            [SEP112233445566](device-model, device-site)
            button = line, 1001, label=Shared side desk

            [line-base](!)
            type = line
            label = Base line
            context = from-base
            callerid = "Base caller" <91001>
            mailbox = base@default

            [line-site](!, line-base)
            context = from-site
            callerid = "Site caller" <92001>

            [1001](line-site)
            label = Reception
            mailbox = 1001@default
        "#;

    let config = ModuleConfig::parse(input).unwrap();
    assert_eq!(config.devices.len(), 2);
    assert_eq!(config.lines.len(), 1);
    let first = config
        .devices
        .get(&DeviceId::new("SEP001122334455").unwrap())
        .unwrap();
    assert_eq!(first.description, "Reception phone");
    assert_eq!(first.soft_key_profile, "desk-keys");
    assert!(matches!(
        &first.buttons[0],
        ButtonDefinition::SpeedDial(speed) if speed.instance == 1 && speed.number == "2000"
    ));
    assert!(matches!(
        &first.buttons[1],
        ButtonDefinition::BlfSpeedDial(speed)
            if speed.instance == 1 && speed.number == "2001"
    ));
    assert_eq!(
        first.blf_targets.get(&1).map(ToString::to_string),
        Some("2001@internal".into())
    );
    assert!(matches!(&first.buttons[2], ButtonDefinition::Unused));
    assert!(matches!(
        &first.buttons[3],
        ButtonDefinition::Line(line) if line.instance == 1 && line.number == "1001"
    ));

    let second = config
        .devices
        .get(&DeviceId::new("SEP112233445566").unwrap())
        .unwrap();
    assert_eq!(second.description, "Site phone");
    assert!(matches!(
        &second.buttons[3],
        ButtonDefinition::Line(line)
            if line.instance == 1 && line.label.as_deref() == Some("Shared side desk")
    ));

    let line = config.lines.get("1001").unwrap();
    assert_eq!(line.label, "Reception");
    assert_eq!(line.context, "from-site");
    assert_eq!(line.caller_name, "Site caller");
    assert_eq!(line.caller_number, "92001");
    assert_eq!(line.mailbox.as_deref(), Some("1001@default"));
}

#[test]
fn inheritance_rejects_cycles_missing_and_invalid_parents() {
    let cycle = r#"
            [device-a](!, device-b)
            type = device
            [device-b](!, device-c)
            [device-c](!, device-a)
        "#;
    assert!(matches!(
        ModuleConfig::parse(cycle),
        Err(ConfigError::InheritanceCycle(path))
            if path == "device-a -> device-b -> device-c -> device-a"
    ));

    let missing = r#"
            [SEP001122334455](missing-device)
            button = line, 1001
            [1001]
            type = line
        "#;
    assert!(matches!(
        ModuleConfig::parse(missing),
        Err(ConfigError::MissingTemplate { section, parent })
            if section == "SEP001122334455" && parent == "missing-device"
    ));

    let concrete_parent = r#"
            [SEP112233445566]
            type = device
            button = line, 1001
            [SEP001122334455](SEP112233445566)
            button = line, 1001
            [1001]
            type = line
        "#;
    assert!(matches!(
        ModuleConfig::parse(concrete_parent),
        Err(ConfigError::ParentIsNotTemplate { section, parent })
            if section == "SEP001122334455" && parent == "SEP112233445566"
    ));
}

#[test]
fn inheritance_rejects_wrong_or_untyped_template_kinds() {
    let wrong_kind = r#"
            [line-defaults](!)
            type = line
            context = from-sccp
            [SEP001122334455](line-defaults)
            type = device
            button = line, 1001
            [1001]
            type = line
        "#;
    assert!(matches!(
        ModuleConfig::parse(wrong_kind),
        Err(ConfigError::WrongTemplateKind {
            section,
            child_kind,
            parent,
            parent_kind,
        }) if section == "SEP001122334455"
            && child_kind == "device"
            && parent == "line-defaults"
            && parent_kind == "line"
    ));

    let mixed_parents = r#"
            [device-defaults](!)
            type = device
            [line-defaults](!)
            type = line
            [mixed](!, device-defaults, line-defaults)
        "#;
    assert!(matches!(
        ModuleConfig::parse(mixed_parents),
        Err(ConfigError::WrongTemplateKind {
            section,
            child_kind,
            parent,
            parent_kind,
        }) if section == "mixed"
            && child_kind == "device"
            && parent == "line-defaults"
            && parent_kind == "line"
    ));

    let untyped = "[defaults](!)\ndescription = no kind\n";
    assert!(matches!(
        ModuleConfig::parse(untyped),
        Err(ConfigError::InvalidTemplateKind { section, kind })
            if section == "defaults" && kind == "missing"
    ));
}

#[test]
fn inheritance_header_rejects_duplicate_and_empty_entries() {
    for (header, message) in [
        ("[child](!, base, BASE)", "duplicate parent template [BASE]"),
        ("[child](!, !)", "duplicate template marker"),
        ("[child]()", "empty inheritance entry"),
        ("[child](base, )", "empty inheritance entry"),
    ] {
        assert!(matches!(
            parse_sections(header),
            Err(ConfigError::Syntax { message: actual, .. }) if actual == message
        ));
    }
}

#[test]
fn rejects_unknown_options() {
    let config = CONFIG.replace("keepalive = 30", "unknown = value");
    // Add a guaranteed unknown because the fixture relies on the default keepalive.
    let config = config.replace("bind = 0.0.0.0:2000", "bind = 0.0.0.0:2000\nwat = no");
    assert!(matches!(
        ModuleConfig::parse(&config),
        Err(ConfigError::InvalidValue { key, value })
            if key.contains("[general].wat") && value.contains("expected")
    ));
}

#[test]
fn parses_typed_device_feature_defaults() {
    let input = CONFIG.replace(
        "description = Reception\n        line = 1001",
        r#"description = Reception
            cfwdall = no
            forward_busy_enabled = yes
            cfwdnoanswer = on
            forward_no_answer_timeout = 45
            forward_all = 2000
            forward_busy = none
            forward_no_answer = 2001
            dnd_feature = no
            dnd = reject
            privacy_feature = yes
            privacy = on
            button = feature, Do not disturb, dnd
            button = feature, Forward all, forward_all
            feature_default = 2, yes
            line = 1001"#,
    );
    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let defaults = config.feature_defaults_for_device(&device_id).unwrap();

    assert!(!defaults.forwarding.all_enabled);
    assert!(defaults.forwarding.busy_enabled);
    assert!(defaults.forwarding.no_answer_enabled);
    assert_eq!(defaults.forwarding.no_answer_timeout_seconds, 45);
    assert_eq!(
        defaults
            .forwarding
            .all
            .as_ref()
            .map(ForwardingDestination::as_str),
        Some("2000")
    );
    assert_eq!(defaults.forwarding.busy, None);
    assert_eq!(
        defaults
            .forwarding
            .no_answer
            .as_ref()
            .map(ForwardingDestination::as_str),
        Some("2001")
    );
    assert!(!defaults.dnd_enabled);
    assert_eq!(defaults.dnd, DndMode::Reject);
    assert!(defaults.privacy_enabled);
    assert!(defaults.privacy);
    assert_eq!(defaults.buttons, HashMap::from([(1, false), (2, true)]));
}

#[test]
fn dnd_feature_button_modes_are_typed_and_canonical() {
    let input = CONFIG.replace(
        "description = Reception",
        r#"description = Reception
            button = feature, Cycle DND, dnd
            button = feature, Silent DND, dnd, silent
            button = feature, Reject DND, dnd, busy"#,
    );
    let config = ModuleConfig::parse(&input).unwrap();
    let device = DeviceId::new("SEP001122334455").unwrap();
    assert_eq!(
        [1, 2, 3].map(|instance| config.dnd_button_mode(&device, instance)),
        [
            Some(DndButtonMode::Cycle),
            Some(DndButtonMode::Silent),
            Some(DndButtonMode::Reject),
        ]
    );
    assert_eq!(
        config.dnd_buttons_for_device(&device).collect::<Vec<_>>(),
        [
            (1, DndButtonMode::Cycle),
            (2, DndButtonMode::Silent),
            (3, DndButtonMode::Reject),
        ]
    );
    assert_eq!(
        config.devices[&device]
            .feature_arguments
            .get(&3)
            .map(String::as_str),
        Some("reject")
    );

    let invalid = CONFIG.replace(
        "description = Reception",
        "description = Reception\n        button = feature, DND, dnd, invented",
    );
    let error = ModuleConfig::parse(&invalid).unwrap_err();
    assert!(
        matches!(
            &error,
            ConfigError::InvalidValue { key, value }
                if key == "line 12 [SEP001122334455].button"
                    && value == "\"invented\"; expected silent or reject"
        ),
        "unexpected error: {error:?}"
    );
}

#[test]
fn parses_voicemail_and_pickup_groups_into_line_features() {
    let input = CONFIG.replace(
        "mailbox = 1001@default",
        r#"mailbox = 1001@default
            voicemail_number = 600
            voicemail_transfer = 61001
            call_group = 0, 2-4, 63
            pickup_group = 1, 5-6
            named_call_group = reception, front desk
            named_pickup_group = sales, support
            directed_pickup = no
            directed_pickup_context = pickup-internal
            pickup_mode_answer = off"#,
    );
    let config = ModuleConfig::parse(&input).unwrap();
    let line = config.lines.get("1001").unwrap();
    let features = config.features_for_line("1001").unwrap();

    assert_eq!(line.mailbox.as_deref(), Some("1001@default"));
    assert_eq!(
        features
            .voicemail
            .number
            .as_ref()
            .map(|value| value.as_str()),
        Some("600")
    );
    assert_eq!(
        features
            .voicemail
            .transfer_destination
            .as_ref()
            .map(|value| value.as_str()),
        Some("61001")
    );
    assert_eq!(
        features
            .voicemail
            .divert_destination()
            .map(VoicemailDestination::as_str),
        Some("61001")
    );
    assert_eq!(
        features.pickup.call_groups,
        BTreeSet::from([0, 2, 3, 4, 63])
    );
    assert_eq!(features.pickup.pickup_groups, BTreeSet::from([1, 5, 6]));
    assert_eq!(
        features.pickup.named_call_groups,
        BTreeSet::from(["front desk".into(), "reception".into()])
    );
    assert_eq!(
        features.pickup.named_pickup_groups,
        BTreeSet::from(["sales".into(), "support".into()])
    );
    assert!(!features.pickup.directed);
    assert_eq!(
        features.pickup.directed_context.as_deref(),
        Some("pickup-internal")
    );
    assert!(!features.pickup.answer_directed);
}

#[test]
fn divert_actions_require_trnsfvm_and_never_fall_back_to_vmnum() {
    let voicemail = VoicemailDefaults {
        number: Some(VoicemailDestination::new("private-mailbox").unwrap()),
        transfer_destination: None,
    };
    assert!(voicemail.divert_destination().is_none());

    let voicemail = VoicemailDefaults {
        number: Some(VoicemailDestination::new("private-mailbox").unwrap()),
        transfer_destination: Some(VoicemailDestination::new("private-divert-target").unwrap()),
    };
    assert_eq!(
        voicemail
            .divert_destination()
            .map(VoicemailDestination::as_str),
        Some("private-divert-target")
    );
    assert!(!format!("{voicemail:?}").contains("private-divert-target"));
}

#[test]
fn feature_and_pickup_defaults_follow_template_merge_semantics() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10

            [device-base](!)
            type = device
            forward_all = 2000
            dnd = silent
            button = feature, Do not disturb, dnd
            feature_default = 1, yes

            [device-child](!, device-base)
            forward_all = none
            dnd = reject
            button = feature, Forward all, forward_all
            feature_default = 1, no
            feature_default = 2, yes

            [SEP001122334455](device-child)
            button = line, 1001

            [line-base](!)
            type = line
            context = from-sccp
            vmnum = 600
            callgroup = 1-3
            namedpickupgroup = sales
            directed_pickup_context = inherited-pickup

            [line-child](!, line-base)
            voicemail_number = 700
            call_group =
            named_pickup_group = support
            directed_pickup_context = none

            [1001](line-child)
            label = Reception
        "#;
    let config = ModuleConfig::parse(input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let defaults = config.feature_defaults_for_device(&device_id).unwrap();
    assert_eq!(defaults.forwarding.all, None);
    assert_eq!(defaults.dnd, DndMode::Reject);
    assert_eq!(defaults.buttons, HashMap::from([(1, false), (2, true)]));

    let line = config.features_for_line("1001").unwrap();
    assert_eq!(
        line.voicemail.number.as_ref().map(|value| value.as_str()),
        Some("700")
    );
    assert!(line.pickup.call_groups.is_empty());
    assert_eq!(
        line.pickup.named_pickup_groups,
        BTreeSet::from(["support".into()])
    );
    assert_eq!(line.pickup.directed_context, None);
}

#[test]
fn parking_and_conference_defaults_are_fully_normalized() {
    let config = ModuleConfig::parse(CONFIG).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(
        config.general.conference_dialing,
        ConferenceDialingConfig::default()
    );
    assert_eq!(
        config.parking_for_device(&device_id),
        Some(&DeviceParkingConfig::default())
    );
    assert_eq!(
        config.conference_for_device(&device_id),
        Some(&DeviceConferenceConfig::default())
    );
    assert_eq!(
        config.parking_for_line("1001"),
        Some(&LineParkingConfig::default())
    );
    assert_eq!(
        config.conference_for_line("1001"),
        Some(&LineConferenceConfig::default())
    );
    assert_eq!(
        config
            .conference_dialing_for_appearance(&device_id, 1)
            .unwrap(),
        ResolvedConferenceDialing {
            enabled: true,
            destination: None,
            application_options: "qxd".into(),
        }
    );
}

#[test]
fn parses_typed_parking_and_conference_policies_with_inheritance() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10
            meetme = no
            meetmeopts = qxd

            [device-base](!)
            type = device
            park = no
            conf_allow = no
            conf_music_on_hold_class =
            conf_play_general_announce = no
            conf_play_part_announce = no
            conf_mute_on_entry = yes
            conf_show_conflist = no
            meetme = no
            meetmeopts = qd
            button = feature, Main parking, parkinglot

            [device-site](!, device-base)
            park = yes
            conf_allow = yes
            conf_music_on_hold_class = office
            meetme = yes
            meetmeopts = Mac
            button = feature, Executive parking, parkinglot, executive, AlwaysShowMenu

            [SEP001122334455](device-site)
            line = 1001

            [line-base](!)
            type = line
            context = from-sccp
            parkinglot = default
            meetme = yes
            meetmenum = 700
            meetmeopts = qxd

            [1001](line-base)
            parkinglot = executive
            meetmeopts = M(acme_bridge)
        "#;
    let config = ModuleConfig::parse(input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let parking = config.parking_for_device(&device_id).unwrap();

    assert!(parking.enabled);
    assert_eq!(
        parking.feature_buttons.get(&1),
        Some(&ParkingLotButtonConfig {
            lot: "default".into(),
            retrieval: ParkingRetrievalBehavior::RetrieveSingle,
        })
    );
    assert_eq!(
        config.parking_lot_for_button(&device_id, 2),
        Some(&ParkingLotButtonConfig {
            lot: "executive".into(),
            retrieval: ParkingRetrievalBehavior::AlwaysShowMenu,
        })
    );
    assert_eq!(
        config.parking_for_line("1001").unwrap().lot.as_deref(),
        Some("executive")
    );

    let conference = config.conference_for_device(&device_id).unwrap();
    assert!(conference.allowed);
    assert_eq!(conference.music_on_hold_class.as_deref(), Some("office"));
    assert!(!conference.play_general_announcements);
    assert!(!conference.play_participant_announcements);
    assert!(conference.mute_on_entry);
    assert!(!conference.show_conference_list);
    assert_eq!(
        conference.dialing,
        ConferenceDialingConfig {
            enabled: true,
            application_options: "Mac".into(),
        }
    );

    let line = config.conference_for_line("1001").unwrap();
    assert_eq!(line.enabled, Some(true));
    assert_eq!(line.destination.as_deref(), Some("700"));
    assert_eq!(line.application_options.as_deref(), Some("M(acme_bridge)"));
    assert_eq!(
        config
            .conference_dialing_for_appearance(&device_id, 1)
            .unwrap(),
        ResolvedConferenceDialing {
            enabled: true,
            destination: Some("700".into()),
            application_options: "M(acme_bridge)".into(),
        }
    );
}

#[test]
fn empty_parking_and_conference_strings_have_exact_clear_semantics() {
    let input = CONFIG
        .replace(
            "description = Reception",
            "description = Reception\n        conf_music_on_hold_class =",
        )
        .replace(
            "mailbox = 1001@default",
            "mailbox = 1001@default\n        parkinglot =\n        meetmeopts =",
        );
    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(
        config
            .conference_for_device(&device_id)
            .unwrap()
            .music_on_hold_class,
        None
    );
    assert_eq!(config.parking_for_line("1001").unwrap().lot, None);
    assert_eq!(
        config
            .conference_for_line("1001")
            .unwrap()
            .application_options
            .as_deref(),
        Some("")
    );
    assert_eq!(
        config
            .conference_dialing_for_appearance(&device_id, 1)
            .unwrap()
            .application_options,
        ""
    );
}

#[test]
fn rejects_malformed_parking_retrieval_behavior() {
    for button in [
        "button = feature, Parking, parkinglot, default, SometimesShowMenu",
        "button = feature, Parking, parkinglot, default, RetrieveSingle, extra",
        "button = feature, Parking, parkinglot, , RetrieveSingle",
        "button = feature, Parking, parkinglot, default,",
    ] {
        let input = CONFIG.replace("line = 1001", &format!("{button}\n        line = 1001"));
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {button}"
        );
    }
}

#[test]
fn rejects_invalid_or_contradictory_conference_settings() {
    for setting in [
        "conf_allow = perhaps",
        "conf_play_general_announce = perhaps",
        "conf_play_part_announce = perhaps",
        "conf_mute_on_entry = perhaps",
        "conf_show_conflist = perhaps",
        "meetme = perhaps",
        "conf_allow = yes\n        conf-allow = no",
    ] {
        let input = CONFIG.replace(
            "description = Reception",
            &format!("description = Reception\n        {setting}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {setting}"
        );
    }

    for setting in [
        "meetme = yes",
        "meetme = no\n        meetmenum = 700",
        "meetme = no\n        meetmeopts = Mac",
    ] {
        let input = CONFIG.replace(
            "mailbox = 1001@default",
            &format!("mailbox = 1001@default\n        {setting}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {setting}"
        );
    }

    let inherited_clear = r#"
            [general]
            advertised_address = 192.0.2.10
            [SEP001122334455]
            type = device
            line = 1001
            [line-base](!)
            type = line
            meetme = yes
            meetmenum = 700
            [1001](line-base)
            meetmenum =
        "#;
    assert!(matches!(
        ModuleConfig::parse(inherited_clear),
        Err(ConfigError::InvalidValue { key, value })
            if key.contains("[1001].meetmenum") && value.contains("expected")
    ));
}

#[test]
fn auto_answer_hotline_and_media_defaults_are_normalized() {
    let config = ModuleConfig::parse(CONFIG).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(config.auto_answer(), &AutoAnswerConfig::default());
    assert_eq!(config.guest_hotline(), &GuestHotlineConfig::default());
    assert!(!config.guest_hotline().enabled);
    assert_eq!(config.general.jitter_buffer, JitterBufferConfig::default());
    assert_eq!(
        config.hotline_for_line("1001"),
        Some(&LineHotlineConfig::default())
    );
    assert_eq!(
        config.media_for_device(&device_id).unwrap(),
        &DeviceMediaConfig {
            codecs: config.general.codecs.clone(),
            audio_encryption: MediaEncryptionPolicy::default(),
            dtmf_mode: DtmfMode::Auto,
            direct_media: false,
            early_media: true,
        }
    );
    assert_eq!(
        config.media_for_line("1001").unwrap(),
        &LineMediaConfig {
            codecs: config.general.codecs.clone(),
            audio_encryption: MediaEncryptionPolicy::default(),
            video_mode: VideoMode::Auto,
            audio_processing: AudioProcessingPolicy::default(),
        }
    );
    assert_eq!(
        config.media_for_appearance(&device_id, 1).unwrap(),
        ResolvedMediaConfig {
            codecs: config.general.codecs,
            audio_encryption: MediaEncryptionPolicy::default(),
            dtmf_mode: DtmfMode::Auto,
            direct_media: false,
            early_media: true,
            video_mode: VideoMode::Auto,
            audio_processing: AudioProcessingPolicy::default(),
        }
    );
}

#[test]
fn echo_cancellation_and_silence_suppression_resolve_per_line() {
    let config = ModuleConfig::parse(
        r#"
            [general]
            advertised_address = 192.0.2.10
            echocancel = no
            silencesuppression = yes

            [line-base](!)
            type = line
            echocancel = yes

            [1001](line-base)
            silencesuppression = no

            [1002]
            type = line

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002
            "#,
    )
    .unwrap();
    let device = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(
        config.general.audio_processing,
        AudioProcessingPolicy {
            echo_cancellation: EchoCancellation::Off,
            silence_suppression: SilenceSuppression::On,
        }
    );
    assert_eq!(
        config
            .media_for_appearance(&device, 1)
            .unwrap()
            .audio_processing,
        AudioProcessingPolicy {
            echo_cancellation: EchoCancellation::On,
            silence_suppression: SilenceSuppression::Off,
        }
    );
    assert_eq!(
        config
            .media_for_appearance(&device, 2)
            .unwrap()
            .audio_processing,
        config.general.audio_processing
    );

    for invalid in [
        "[general]\nadvertised_address = 192.0.2.10\nechocancel = maybe",
        "[general]\nadvertised_address = 192.0.2.10\nsilencesuppression = yes\nsilencesuppression = no",
        "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\nechocancel = maybe",
        "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\n[SEP001122334455]\ntype = device\nline = 1001\nechocancel = yes",
    ] {
        assert!(
            matches!(
                ModuleConfig::parse(invalid),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid audio-processing policy: {invalid}"
        );
    }
}

#[test]
fn parses_exact_global_jitter_buffer_policy() {
    let config = ModuleConfig::parse(
        r#"
            [general]
            advertised_address = 192.0.2.10
            jbenable = yes
            jbforce = yes
            jblog = yes
            jbmaxsize = 320
            jbresyncthreshold = 1500
            jbimpl = adaptive

            [1001]
            type = line

            [SEP001122334455]
            type = device
            line = 1001
            "#,
    )
    .unwrap();

    assert_eq!(
        config.general.jitter_buffer,
        JitterBufferConfig {
            enabled: true,
            forced: true,
            log_frames: true,
            max_size_ms: 320,
            resync_threshold_ms: 1_500,
            implementation: JitterBufferImplementation::Adaptive,
        }
    );

    let forced_without_enabled = ModuleConfig::parse(
        "[general]\nadvertised_address = 192.0.2.10\njbforce = yes\n\
             [1001]\ntype = line\n\
             [SEP001122334455]\ntype = device\nline = 1001",
    )
    .unwrap();
    assert!(!forced_without_enabled.general.jitter_buffer.enabled);
    assert!(forced_without_enabled.general.jitter_buffer.forced);

    let mut policy = JitterBufferConfig::default();
    assert!(!policy.should_configure_channel(false));
    policy.enabled = true;
    assert!(policy.should_configure_channel(false));
    assert!(!policy.should_configure_channel(true));
    policy.forced = true;
    assert!(policy.should_configure_channel(true));
    policy.enabled = false;
    assert!(!policy.should_configure_channel(false));
}

#[test]
fn rejects_invalid_scoped_or_invented_jitter_buffer_policy() {
    for invalid in [
        "[general]\nadvertised_address = 192.0.2.10\njbenable = maybe",
        "[general]\nadvertised_address = 192.0.2.10\njbmaxsize = 0",
        "[general]\nadvertised_address = 192.0.2.10\njbresyncthreshold = 2147483648",
        "[general]\nadvertised_address = 192.0.2.10\njbimpl = dynamic",
        "[general]\nadvertised_address = 192.0.2.10\njbenable = yes\njbenable = no",
        "[general]\nadvertised_address = 192.0.2.10\njbtargetextra = 40",
        "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\njbenable = yes",
        "[general]\nadvertised_address = 192.0.2.10\n[1001]\ntype = line\n[SEP001122334455]\ntype = device\nline = 1001\njbforce = yes",
    ] {
        assert!(
            matches!(
                ModuleConfig::parse(invalid),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted invalid jitter-buffer policy: {invalid}"
        );
    }
}

#[test]
fn parses_auto_answer_guest_hotline_and_line_hotline() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10
            autoanswer_ring_time = 7
            autoanswer_tone = 0x31
            remotehangup_tone = 0
            hotline_enabled = yes
            hotline_extension = 9911
            hotline_context = emergency
            hotline_label = Emergency only

            [SEP001122334455]
            type = device
            line = 1001

            [line-base](!)
            type = line
            adhocNumber = 912

            [1001](line-base)
            adhoc_number = 911
        "#;
    let config = ModuleConfig::parse(input).unwrap();

    assert_eq!(
        config.auto_answer(),
        &AutoAnswerConfig {
            ring_time_seconds: 7,
            tone: Tone::ZipZip,
        }
    );
    assert_eq!(config.general.remote_hangup_tone, None);
    assert_eq!(
        config.guest_hotline(),
        &GuestHotlineConfig {
            enabled: true,
            extension: Some(HotlineDestination::new("9911").unwrap()),
            context: "emergency".into(),
            label: "Emergency only".into(),
        }
    );
    assert_eq!(
        config
            .hotline_for_line("1001")
            .unwrap()
            .destination
            .as_ref()
            .map(HotlineDestination::as_str),
        Some("911")
    );
    let debug = format!("{:?}", config.guest_hotline());
    assert!(!debug.contains("9911"));
    let debug = format!("{:?}", config.hotline_for_line("1001"));
    assert!(!debug.contains("911"));

    let configured_id = DeviceId::new("SEP001122334455").unwrap();
    let configured = config.line_for_device(&configured_id, 1).unwrap();
    assert_eq!(
        config
            .hotline_destination_for_binding(configured)
            .map(HotlineDestination::as_str),
        Some("911")
    );
    let guest_id = DeviceId::new("SEPFFEEDDCCBBAA").unwrap();
    let guest = config.guest_hotline_binding(&guest_id, 1).unwrap();
    assert_eq!(guest.line.number, "hotline");
    assert_eq!(guest.line.context, "emergency");
    assert_eq!(guest.appearance.display_label(), "Emergency only");
    assert_eq!(
        config
            .hotline_destination_for_binding(&guest)
            .map(HotlineDestination::as_str),
        Some("9911")
    );
    assert!(config.guest_hotline_binding(&guest_id, 2).is_none());
    assert!(config.guest_hotline_binding(&configured_id, 1).is_none());
}

#[test]
fn disabled_guest_hotline_allows_cleared_identity_fields() {
    let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        hotline_enabled = no\n        hotline_extension =\n        hotline_context =\n        hotline_label =",
        );
    let config = ModuleConfig::parse(&input).unwrap();

    assert_eq!(
        config.guest_hotline(),
        &GuestHotlineConfig {
            enabled: false,
            extension: None,
            context: "".into(),
            label: "".into(),
        }
    );
}

#[test]
fn rejects_invalid_auto_answer_and_hotline_ranges() {
    for setting in [
        "autoanswer_ring_time = -1".to_owned(),
        "autoanswer_ring_time = 4294967296".to_owned(),
        "autoanswer_tone = teleport".to_owned(),
        "autoanswer_tone = 0x100".to_owned(),
        "remotehanguptone = Zip".to_owned(),
        "remotehangup_tone = teleport".to_owned(),
        "remotehangup_tone = Zip\nremotehangup_tone = ZipZip".to_owned(),
        "hotline_enabled = perhaps".to_owned(),
        "hotline_enabled = yes\nhotline_extension =".to_owned(),
        "hotline_enabled = yes\nhotline_context =".to_owned(),
        "hotline_enabled = yes\nhotline_label =".to_owned(),
        format!(
            "hotline_extension = {}",
            "1".repeat(MAX_HOTLINE_FIELD_BYTES + 1)
        ),
        format!(
            "hotline_context = {}",
            "c".repeat(MAX_HOTLINE_FIELD_BYTES + 1)
        ),
        format!(
            "hotline_label = {}",
            "l".repeat(MAX_HOTLINE_FIELD_BYTES + 1)
        ),
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {setting}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {setting}"
        );
    }

    let oversized = "9".repeat(MAX_HOTLINE_FIELD_BYTES + 1);
    let input = CONFIG.replace(
        "mailbox = 1001@default",
        &format!("mailbox = 1001@default\n        adhocNumber = {oversized}"),
    );
    assert!(matches!(
        ModuleConfig::parse(&input),
        Err(ConfigError::InvalidValue { key, value })
            if key.contains("[1001].adhocNumber") && value.contains("expected")
    ));
}

#[test]
fn parses_codec_dtmf_early_direct_and_video_policy_with_inheritance() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10
            disallow = all
            allow = ulaw, g729
            allow = h264
            directrtp = yes
            earlyrtp = none

            [device-base](!)
            type = device
            disallow = all
            allow = ulaw, g729
            force_dtmfmode = rfc2833
            directrtp = yes
            earlyrtp = none

            [device-site](!, device-base)
            disallow = g729
            allow = alaw
            force_dtmfmode = skinny
            directrtp = no
            earlyrtp = progress

            [SEP001122334455](device-site)
            line = 1001

            [line-base](!)
            type = line
            disallow = all
            allow = opus, h264
            videomode = auto

            [1001](line-base)
            videomode = user
        "#;
    let config = ModuleConfig::parse(input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(
        config.general.codecs,
        [
            Codec::Pcmu,
            Codec::G711Ulaw56k,
            Codec::G729,
            Codec::G729A,
            Codec::G729B,
            Codec::G729Ab,
            Codec::G729AnnexB,
            Codec::H264,
            Codec::H264Svc,
            Codec::H264Fec,
            Codec::H264Uc,
        ]
    );
    assert!(config.general.direct_media);
    assert!(!config.general.early_media);

    let device = config.media_for_device(&device_id).unwrap();
    assert_eq!(
        device.codecs,
        [
            Codec::Pcmu,
            Codec::G711Ulaw56k,
            Codec::Pcma,
            Codec::G711Alaw56k,
        ]
    );
    assert_eq!(device.dtmf_mode, DtmfMode::Skinny);
    assert!(!device.direct_media);
    assert!(device.early_media);

    let line = config.media_for_line("1001").unwrap();
    assert_eq!(
        line.codecs,
        [
            Codec::Opus,
            Codec::H264,
            Codec::H264Svc,
            Codec::H264Fec,
            Codec::H264Uc,
        ]
    );
    assert_eq!(line.video_mode, VideoMode::User);
    assert_eq!(
        config.media_for_appearance(&device_id, 1).unwrap(),
        ResolvedMediaConfig {
            codecs: line.codecs.clone(),
            audio_encryption: MediaEncryptionPolicy::default(),
            dtmf_mode: DtmfMode::Skinny,
            direct_media: false,
            early_media: true,
            video_mode: VideoMode::User,
            audio_processing: AudioProcessingPolicy::default(),
        }
    );
}

#[test]
fn general_media_defaults_apply_regardless_of_section_order() {
    let input = r#"
            [SEP001122334455]
            type = device
            line = 1001
            [1001]
            type = line
            [general]
            advertised_address = 192.0.2.10
            disallow = all
            allow = opus
            directrtp = yes
            earlyrtp = no
        "#;
    let config = ModuleConfig::parse(input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(config.media_for_line("1001").unwrap().codecs, [Codec::Opus]);
    let device = config.media_for_device(&device_id).unwrap();
    assert_eq!(device.codecs, [Codec::Opus]);
    assert!(device.direct_media);
    assert!(!device.early_media);
}

#[test]
fn appearance_codec_preferences_resolve_line_then_device_then_general() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10
            disallow = all
            allow = ulaw

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002
            disallow = all
            allow = alaw

            [1001]
            type = line

            [1002]
            type = line
            disallow = all
            allow = g722
        "#;
    let config = ModuleConfig::parse(input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(
        config.media_for_appearance(&device_id, 1).unwrap().codecs,
        [Codec::Pcma, Codec::G711Alaw56k]
    );
    assert_eq!(
        config.media_for_appearance(&device_id, 2).unwrap().codecs,
        [Codec::G72264k, Codec::G72256k, Codec::G72248k]
    );
}

#[test]
fn audio_encryption_resolves_as_one_policy_line_then_device_then_general() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10
            audio_encryption = required,aes-128-hmac-sha1-80

            [SEP001122334455]
            type = device
            line = 1001
            line = 1002
            audio_encryption = optional,aead-aes-128-gcm

            [SEP001122334466]
            type = device
            line = 1003

            [1001]
            type = line

            [1002]
            type = line
            audio_encryption = required,aead-aes-256-gcm,aes-128-hmac-sha1-32

            [1003]
            type = line
        "#;
    let config = ModuleConfig::parse(input).unwrap();
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP001122334466").unwrap();

    assert_eq!(
        config
            .media_for_appearance(&first, 1)
            .unwrap()
            .audio_encryption,
        MediaEncryptionPolicy::new(
            MediaEncryptionRequirement::Optional,
            [MediaEncryptionProfile::AEAD_AES_128_GCM]
        )
        .unwrap()
    );
    assert_eq!(
        config
            .media_for_appearance(&first, 2)
            .unwrap()
            .audio_encryption,
        MediaEncryptionPolicy::new(
            MediaEncryptionRequirement::Required,
            [
                MediaEncryptionProfile::AEAD_AES_256_GCM,
                MediaEncryptionProfile::AES_128_HMAC_SHA1_32,
            ]
        )
        .unwrap()
    );
    assert_eq!(
        config
            .media_for_appearance(&second, 1)
            .unwrap()
            .audio_encryption,
        MediaEncryptionPolicy::new(
            MediaEncryptionRequirement::Required,
            [MediaEncryptionProfile::AES_128_HMAC_SHA1_80]
        )
        .unwrap()
    );
}

#[test]
fn audio_encryption_rejects_incomplete_or_unknown_policy() {
    for value in [
        "enabled",
        "off,aes-128-hmac-sha1-80",
        "optional",
        "required",
        "required,future-profile",
        "optional,aes-128-hmac-sha1-80,",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        audio_encryption = {value}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {value}"
        );
    }
}

#[test]
fn accepted_early_media_values_normalize_to_boolean_policy() {
    for (value, expected) in [
        ("yes", true),
        ("no", false),
        ("none", false),
        ("offhook", true),
        ("immediate", true),
        ("dial", true),
        ("ringout", true),
        ("progress", true),
    ] {
        let input = CONFIG.replace(
            "description = Reception",
            &format!("description = Reception\n        earlyrtp = {value}"),
        );
        let config = ModuleConfig::parse(&input).unwrap();
        let device_id = DeviceId::new("SEP001122334455").unwrap();
        assert_eq!(
            config.media_for_device(&device_id).unwrap().early_media,
            expected
        );
    }
}

#[test]
fn rejects_invalid_or_unsafe_media_policy() {
    for setting in [
        "disallow = all".to_owned(),
        "disallow = all\n        allow = h264".to_owned(),
        "disallow = all\n        allow = unknown".to_owned(),
        "disallow = all\n        allow = ulaw,,alaw".to_owned(),
        "disallow = all\n        allow = all, g722".to_owned(),
        "directrtp = perhaps".to_owned(),
        "earlyrtp = perhaps".to_owned(),
    ] {
        let input = CONFIG.replace(
            "disallow = all\n        allow = ulaw\n        allow = alaw",
            &setting,
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {setting}"
        );
    }

    for setting in [
        "force_dtmfmode = inband",
        "dtmfmode = skinny",
        "earlyrtp = perhaps",
        "directrtp = perhaps",
    ] {
        let input = CONFIG.replace(
            "description = Reception",
            &format!("description = Reception\n        {setting}"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {setting}"
        );
    }

    let input = CONFIG.replace(
        "mailbox = 1001@default",
        "mailbox = 1001@default\n        videomode = immediate",
    );
    assert!(matches!(
        ModuleConfig::parse(&input),
        Err(ConfigError::InvalidValue { key, value })
            if key.contains("[1001].videomode") && value.contains("expected")
    ));
}

#[test]
fn rejects_invalid_device_feature_defaults() {
    for setting in [
        "cfwdall = perhaps",
        "forward_no_answer_timeout = 0",
        "forward_no_answer_timeout = 86401",
        "dnd = user",
        "privacy = full",
        "feature_default = missing-fields",
        "feature_default = 0, yes",
        "feature_default = 2, yes",
    ] {
        let input = CONFIG.replace(
            "description = Reception",
            &format!(
                "description = Reception\n        button = feature, DND, dnd\n        {setting}"
            ),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {setting}"
        );
    }
}

#[test]
fn rejects_invalid_voicemail_and_pickup_settings() {
    for setting in [
        "mailbox = @default",
        "mailbox = 1001@default@extra",
        "mailbox = desk one@default",
        "callgroup = 64",
        "callgroup = 4-2",
        "callgroup = 1,,2",
        "callgroup = 1,1",
        "namedcallgroup = sales,,support",
        "namedpickupgroup = sales,sales",
        "directed_pickup = perhaps",
        "pickup_mode_answer = perhaps",
    ] {
        let input = CONFIG.replace("mailbox = 1001@default", setting);
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {setting}"
        );
    }
    for (setting, redacted) in [
        (format!("voicemail_number = {}", "6".repeat(80)), true),
        ("voicemail_transfer = 61\u{7}001".into(), false),
    ] {
        let input = CONFIG.replace("mailbox = 1001@default", &setting);
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        if redacted {
            assert!(error.contains("<redacted>"));
            assert!(!error.contains(&"6".repeat(80)));
        }
    }
}

#[test]
fn parses_ordered_mixed_button_layout() {
    let input = r#"
            [general]
            advertised_address = 192.0.2.10

            [SEP001122334455]
            type = device
            description = Reception
            button = line, 1001, label=Shared main, caller_name=Shared desk, caller_number=91001, ring=silent, subscription=1001@internal, privacy=yes
            button = empty
            button = speed_dial, Helpdesk, 2000
            button = blf, Warehouse, 2001, 2001@internal
            button = feature, Do not disturb, dnd, silent
            button = service, Directory, http://pbx.test/directory?view=all,compact
            button = addon, 1, 7914
            line = 1002

            [1001]
            type = line
            label = Main

            [1002]
            type = line
            label = Private
        "#;

    let config = ModuleConfig::parse(input).unwrap();
    let device = config
        .devices
        .get(&DeviceId::new("SEP001122334455").unwrap())
        .unwrap();
    assert_eq!(device.lines, ["1001", "1002"]);
    assert_eq!(
        device.feature_arguments.get(&2).map(String::as_str),
        Some("silent")
    );
    assert_eq!(
        config.dnd_button_mode(&device.id, 2),
        Some(DndButtonMode::Silent)
    );
    assert!(matches!(
        &device.buttons[0],
        ButtonDefinition::Line(line)
            if line.instance == 1
                && line.number == "1001"
                && line.display_name == "Main"
                && line.label.as_deref() == Some("Shared main")
                && line.caller_id.name.as_deref() == Some("Shared desk")
                && line.caller_id.number.as_deref() == Some("91001")
                && line.ring_mode == AppearanceRingMode::Silent
                && line.subscription_identity.as_deref() == Some("1001@internal")
                && line.privacy
    ));
    assert!(matches!(&device.buttons[1], ButtonDefinition::Unused));
    assert!(matches!(
        &device.buttons[2],
        ButtonDefinition::SpeedDial(speed_dial)
            if speed_dial.instance == 1
                && speed_dial.display_name == "Helpdesk"
                && speed_dial.number == "2000"
    ));
    assert!(matches!(
        &device.buttons[3],
        ButtonDefinition::BlfSpeedDial(blf)
            if blf.instance == 1
    ));
    assert_eq!(
        device.blf_targets.get(&1).map(ToString::to_string),
        Some("2001@internal".into())
    );
    assert!(matches!(
        &device.buttons[4],
        ButtonDefinition::Feature(feature)
            if feature.instance == 2 && feature.feature == ButtonType::DoNotDisturb
    ));
    assert!(matches!(
        &device.buttons[5],
        ButtonDefinition::Service(service)
            if service.instance == 1
                && service.url == "http://pbx.test/directory?view=all,compact"
    ));
    assert!(matches!(
        &device.buttons[6],
        ButtonDefinition::AddonModule(addon)
            if addon.slot == 1 && addon.device_type == DeviceType::CiscoAddon7914
    ));
    assert!(matches!(
        &device.buttons[7],
        ButtonDefinition::Line(line)
            if line.instance == 2 && line.number == "1002"
    ));
    assert_eq!(
        config.line_for_device(&device.id, 2).unwrap().line.number,
        "1002"
    );
    assert_eq!(
        config
            .appearances_for_device(&device.id)
            .map(|appearance| appearance.line.number.as_str())
            .collect::<HashSet<_>>(),
        HashSet::from(["1001", "1002"])
    );
    assert_eq!(config.device_definitions()[0].buttons, device.buttons);
}

#[test]
fn parses_canonical_recording_buttons_as_typed_mirrored_controls() {
    let input = CONFIG.replace(
        "line = 1001",
        "line = 1001\n        button = feature, Record calls, monitor\n        button = feature, Record backup, monitor",
    );

    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let device = &config.devices[&device_id];

    assert!(matches!(
        &device.buttons[1],
        ButtonDefinition::Recording(recording)
            if recording.instance == 1 && recording.label == "Record calls"
    ));
    assert!(matches!(
        &device.buttons[2],
        ButtonDefinition::Recording(recording)
            if recording.instance == 2 && recording.label == "Record backup"
    ));
    assert_eq!(config.device_definitions()[0].buttons, device.buttons);
    assert!(!device.feature_arguments.contains_key(&1));
    assert!(!device.feature_arguments.contains_key(&2));
    assert_eq!(
        config
            .recording_buttons_for_device(&device_id)
            .map(|recording| recording.label.as_str())
            .collect::<Vec<_>>(),
        ["Record calls", "Record backup"]
    );
}

#[test]
fn recording_button_rejects_missing_or_extra_arguments() {
    for button in [
        "button = feature, , monitor",
        "button = feature, Record calls, monitor, armed",
        "button = feature, Record calls, monitor, ",
    ] {
        let input = CONFIG.replace("line = 1001", &format!("line = 1001\n        {button}"));
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { ref key, .. }) if key.ends_with(".button")
            ),
            "accepted {button}"
        );
    }
}

#[test]
fn recording_buttons_follow_ordered_device_template_inheritance() {
    let input = CONFIG.replace(
        "[SEP001122334455]",
        r#"[recording-device](!)
        type = device
        button = feature, Record calls, monitor

        [SEP001122334455](recording-device)"#,
    );

    let config = ModuleConfig::parse(&input).unwrap();
    let device = &config.devices[&DeviceId::new("SEP001122334455").unwrap()];
    assert!(matches!(
        &device.buttons[0],
        ButtonDefinition::Recording(recording)
            if recording.instance == 1 && recording.label == "Record calls"
    ));
    assert!(matches!(
        &device.buttons[1],
        ButtonDefinition::Line(line) if line.instance == 1 && line.number == "1001"
    ));
}

#[test]
fn recording_soft_key_remains_opt_in_to_connected_modes() {
    let default = ModuleConfig::parse(CONFIG).unwrap();
    let default_profile = default.soft_key_profile(DEFAULT_SOFT_KEY_PROFILE).unwrap();
    for mode in [
        KeyMode::Connected,
        KeyMode::ConnectedTransfer,
        KeyMode::ConnectedConference,
    ] {
        assert!(!default_profile.actions(mode).contains(&SoftKey::Monitor));
    }

    let input = CONFIG
        .replace(
            "[SEP001122334455]",
            r#"[recording-keys]
        type = softkey_profile
        connected = hold, monitor, end_call
        connected_transfer = monitor, transfer
        connected_conference = conference_list, monitor

        [SEP001122334455]"#,
        )
        .replace(
            "description = Reception",
            "description = Reception\n        softkey_profile = recording-keys",
        );
    let config = ModuleConfig::parse(&input).unwrap();
    let profile = config
        .soft_key_profile_for_device(&DeviceId::new("SEP001122334455").unwrap())
        .unwrap();
    for mode in [
        KeyMode::Connected,
        KeyMode::ConnectedTransfer,
        KeyMode::ConnectedConference,
    ] {
        assert!(profile.actions(mode).contains(&SoftKey::Monitor));
    }
}

#[test]
fn speed_dial_hint_builds_a_blf_button() {
    let input = CONFIG.replace(
        "line = 1001",
        "button = line, 1001\nbutton = speeddial, Helpdesk, 2000, 2000@internal",
    );
    let buttons = &ModuleConfig::parse(&input).unwrap().device_definitions()[0].buttons;
    assert!(matches!(
        &buttons[1],
        ButtonDefinition::BlfSpeedDial(blf)
            if blf.instance == 1
                && blf.number == "2000"
    ));
    let device = ModuleConfig::parse(&input)
        .unwrap()
        .devices
        .remove(&DeviceId::new("SEP001122334455").unwrap())
        .unwrap();
    assert_eq!(
        device.blf_targets.get(&1).map(ToString::to_string),
        Some("2000@internal".into())
    );
}

#[test]
fn parses_reusable_soft_key_profile_for_every_key_mode() {
    let input = CONFIG
        .replace(
            "[SEP001122334455]",
            r#"[Reception-Keys]
                type = softkey_profile
                on_hook = redial, new_call
                connected = hold, end_call, transfer
                on_hold = resume, new_call, end_call
                ring_in = answer, immediate_divert
                off_hook = end_call
                connected_transfer = direct_transfer, end_call
                digits_following = backspace, dial
                connected_conference = conference_list, join
                ring_out = callback, end_call
                off_hook_feature = pickup, group_pickup
                in_use_hint = barge
                on_hook_stealable = intercept, new_call
                hold_conference = select, conference
                empty =

                [SEP001122334455]"#,
        )
        .replace(
            "description = Reception",
            "description = Reception\n        softkey_profile = Reception-Keys",
        );

    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();
    let device = config.devices.get(&device_id).unwrap();
    assert_eq!(device.soft_key_profile, "reception-keys");
    let profile = config.soft_key_profile_for_device(&device_id).unwrap();
    assert_eq!(profile.name, "reception-keys");
    assert_eq!(profile.sets.len(), KeyMode::ALL_KNOWN.len());
    assert_eq!(
        profile.actions(KeyMode::OnHook),
        [SoftKey::Redial, SoftKey::NewCall]
    );
    assert_eq!(
        profile.actions(KeyMode::ConnectedTransfer),
        [SoftKey::DirectTransfer, SoftKey::EndCall]
    );
    assert_eq!(
        profile.actions(KeyMode::OffHookFeature),
        [SoftKey::Pickup, SoftKey::GroupPickup]
    );
    assert!(profile.actions(KeyMode::Empty).is_empty());
    assert_eq!(config.soft_key_profile("RECEPTION-KEYS"), Some(profile));
    let station = config.device_definitions().remove(0);
    assert_eq!(
        station.soft_keys.actions(KeyMode::OnHook),
        [SoftKey::Redial, SoftKey::NewCall]
    );
    assert_eq!(
        station.soft_keys.actions(KeyMode::ConnectedTransfer),
        [SoftKey::DirectTransfer, SoftKey::EndCall]
    );
}

#[test]
fn parses_every_named_soft_key_in_declared_order() {
    let names = [
        "redial",
        "new_call",
        "hold",
        "transfer",
        "forward_all",
        "forward_busy",
        "forward_no_answer",
        "backspace",
        "end_call",
        "resume",
        "answer",
        "info",
        "conference",
        "park",
        "join",
        "meet_me",
        "pickup",
        "group_pickup",
        "monitor",
        "callback",
        "barge",
        "do_not_disturb",
        "conference_list",
        "select",
        "private",
        "transfer_to_voicemail",
        "direct_transfer",
        "immediate_divert",
        "video_mode",
        "intercept",
        "empty",
        "dial",
    ];
    let input = CONFIG.replace(
            "[SEP001122334455]",
            &format!(
                "[all-actions]\ntype = softkey_profile\non_hook = {}\nconnected = {}\n\n[SEP001122334455]",
                names[..16].join(", "),
                names[16..].join(", ")
            ),
        );
    let config = ModuleConfig::parse(&input).unwrap();
    let profile = config.soft_key_profile("all-actions").unwrap();
    let actions: Vec<_> = profile
        .actions(KeyMode::OnHook)
        .iter()
        .chain(profile.actions(KeyMode::Connected))
        .copied()
        .collect();
    assert_eq!(actions, SoftKey::ALL_KNOWN);
}

#[test]
fn soft_key_profiles_reject_unknown_and_duplicate_entries() {
    for (setting, expected_key) in [
        ("type = softkey_profile", "[bad-keys].type"),
        ("waiting = answer", "[bad-keys].waiting"),
        ("on_hook = teleport", "[bad-keys].on_hook"),
        ("on_hook = new_call\non-hook = redial", "[bad-keys].on-hook"),
        ("on_hook = dnd, do_not_disturb", "[bad-keys].on_hook"),
        ("on_hook = hold, , transfer", "[bad-keys].on_hook"),
        (
            "on_hook = redial, new_call, hold, transfer, forward_all, forward_busy, forward_no_answer, backspace, end_call, resume, answer, info, conference, park, join, meet_me, pickup",
            "[bad-keys].on_hook",
        ),
    ] {
        let input = CONFIG.replace(
            "[SEP001122334455]",
            &format!("[bad-keys]\ntype = softkey_profile\n{setting}\n\n[SEP001122334455]"),
        );
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { key, value })
                    if key.contains(expected_key) && value.contains("expected")
            ),
            "accepted {setting}"
        );
    }
}

#[test]
fn soft_key_profile_references_are_required_to_resolve_once() {
    let unknown = CONFIG.replace(
        "description = Reception",
        "description = Reception\n        softkey_profile = missing",
    );
    assert!(matches!(
        ModuleConfig::parse(&unknown),
        Err(ConfigError::UnknownSoftKeyProfile { device, profile })
            if device.as_str() == "SEP001122334455" && profile == "missing"
    ));

    let duplicate = CONFIG.replace(
            "description = Reception",
            "description = Reception\n        softkey_profile = default\n        softkey_profile = DEFAULT",
        );
    assert!(matches!(
        ModuleConfig::parse(&duplicate),
        Err(ConfigError::InvalidValue { key, value })
            if key.contains("[SEP001122334455].softkey_profile")
                && value.contains("expected")
    ));
}

#[test]
fn configured_default_profile_replaces_the_builtin_default() {
    let input = CONFIG.replace(
        "[SEP001122334455]",
        "[default]\ntype = softkey_profile\non_hook = redial\n\n[SEP001122334455]",
    );
    let config = ModuleConfig::parse(&input).unwrap();
    let device_id = DeviceId::new("SEP001122334455").unwrap();

    assert_eq!(
        config
            .soft_key_profile_for_device(&device_id)
            .unwrap()
            .actions(KeyMode::OnHook),
        [SoftKey::Redial]
    );
}

#[test]
fn rejects_malformed_and_unknown_buttons() {
    for button in [
        "button = speed_dial, Missing number",
        "button = line, 1001, ring=occasionally",
        "button = line, 1001, privacy=perhaps",
        "button = line, 1001, label=One, label=Two",
        "button = blf, Desk, 2000",
        "button = blf, Desk, 2000, missing-context",
        "button = feature, DND, unknown-feature",
        "button = service, Directory",
        "button = empty, extra",
        "button = addon, 0, 7914",
        "button = addon, 57, 7914",
    ] {
        let input = CONFIG.replace("line = 1001", &format!("line = 1001\n{button}"));
        assert!(
            matches!(
                ModuleConfig::parse(&input),
                Err(ConfigError::InvalidValue { .. })
            ),
            "accepted {button}"
        );
    }
}

#[test]
fn rejects_duplicate_lines_and_oversized_button_layouts() {
    let duplicate = CONFIG.replace("line = 1001", "line = 1001\nbutton = line, 1001");
    assert!(matches!(
        ModuleConfig::parse(&duplicate),
        Err(ConfigError::InvalidValue { key, value })
            if key == "SEP001122334455.line" && value == "1001"
    ));

    let duplicate_addon = CONFIG.replace(
        "line = 1001",
        "line = 1001\nbutton = addon, 1, 7914\nbutton = addon, 1, 7914",
    );
    assert!(matches!(
        ModuleConfig::parse(&duplicate_addon),
        Err(ConfigError::InvalidValue { key, value })
            if key.contains("[SEP001122334455].button")
                && value.contains("repeats addon module button instance 1")
                && value.contains("expected")
    ));

    let empty_buttons = "button = empty\n".repeat(256);
    let oversized = CONFIG.replace(
        "line = 1001",
        &format!("button = line, 1001\n{empty_buttons}"),
    );
    assert!(matches!(
        ModuleConfig::parse(&oversized),
        Err(ConfigError::InvalidValue { key, value })
            if key.contains("[SEP001122334455].button")
            && value.contains("logical layout limit is 256")
                && value.contains("expected")
    ));
}

#[test]
fn realtime_table_pair_normalizes_without_changing_file_sections() {
    let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            "advertised_address = 192.0.2.10\n        devicetable = sccp_devices\n        linetable = sccp_lines",
        );
    let config = ModuleConfig::parse(&input).unwrap();

    assert_eq!(
        config.realtime_tables(),
        Some(&RealtimeTableConfig {
            device_family: "sccp_devices".into(),
            line_family: "sccp_lines".into(),
        })
    );
    assert_eq!(config.devices.len(), 1);
    assert_eq!(config.lines.len(), 1);
}

#[test]
fn realtime_table_pair_is_complete_distinct_and_safely_named() {
    for settings in [
        "devicetable = sccp_devices",
        "linetable = sccp_lines",
        "devicetable = same\n        linetable = same",
        "devicetable = device-table\n        linetable = sccp_lines",
        "devicetable = \n        linetable = sccp_lines",
    ] {
        let input = CONFIG.replace(
            "advertised_address = 192.0.2.10",
            &format!("advertised_address = 192.0.2.10\n        {settings}"),
        );
        let error = ModuleConfig::parse(&input).unwrap_err().to_string();
        assert!(error.contains("line "), "{settings} produced {error}");
        assert!(error.contains("expected"), "{settings} produced {error}");
    }
}

#[test]
fn canonical_schema_is_strict_while_runtime_matching_follows_asterisk_casing() {
    let mixed = CONFIG
        .replace("advertised_address =", "AdVeRtIsEd_AdDrEsS =")
        .replace("type = device", "TyPe = device")
        .replace("type = line", "TYPE = line");
    ModuleConfig::parse(&mixed).unwrap();
    let error = ModuleConfig::check_canonical(&mixed)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("canonical option name advertised_address"),
        "{error}"
    );

    let punctuation = CONFIG.replace(
        "advertised_address = 192.0.2.10",
        "advertised_address = 192.0.2.10\n        direct-media = yes",
    );
    let error = ModuleConfig::parse(&punctuation).unwrap_err().to_string();
    assert!(error.contains("unknown variant `direct-media`"), "{error}");
}

#[test]
fn canonical_serialization_is_deterministic_semantic_and_quote_safe() {
    let source = CONFIG.replace("description = Reception", "description = \"Desk; west\"");
    let expected = ModuleConfig::parse(&source).unwrap();
    let first = ModuleConfig::to_canonical_string(&source).unwrap();
    let second = ModuleConfig::to_canonical_string(&first).unwrap();

    assert_eq!(first, second);
    assert_eq!(ModuleConfig::parse(&first).unwrap(), expected);
    assert!(first.contains("description = \"Desk; west\""));
    ModuleConfig::check_canonical(&first).unwrap();
}
