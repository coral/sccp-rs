use super::super::*;
use super::button::{parse_button, parse_line_button};

pub(in crate::config) fn parse_device(
    section: &RawSection,
    lines: &HashMap<String, LineConfig>,
    soft_key_profiles: &HashMap<String, SoftKeyProfile>,
    general: &GeneralConfig,
) -> Result<DeviceConfig, ConfigError> {
    let id = DeviceId::new(&section.name)
        .map_err(|_| ConfigError::InvalidDevice(section.name.clone()))?;
    let mut draft = DeviceSectionDraft::default();
    let mut section_values = SectionValues::new(section);

    for entry in deserialize_entries::<DeviceOption>(section)? {
        let key = &entry.source.key;
        let raw = &entry.source.value;
        let diagnostic = entry.source.diagnostic_key();
        let parsed = match entry.key {
            DeviceOption::Type | DeviceOption::Description => continue,
            DeviceOption::SoftkeyProfile => {
                if draft.soft_key_profile.is_some() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one soft-key profile reference",
                        false,
                    ));
                }
                let name = canonical::profile_name(raw);
                if name.is_empty() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "the name of a declared soft-key profile",
                        false,
                    ));
                }
                if !soft_key_profiles.contains_key(&name) {
                    return Err(ConfigError::UnknownSoftKeyProfile {
                        device: id.clone(),
                        profile: raw.clone(),
                    });
                }
                draft.soft_key_profile = Some(name);
                continue;
            }
            DeviceOption::ForwardAllEnabled => {
                set_once(
                    &mut draft.forward_all_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardBusyEnabled => {
                set_once(
                    &mut draft.forward_busy_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardNoAnswerEnabled => {
                set_once(
                    &mut draft.forward_no_answer_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardNoAnswerTimeout => {
                let timeout = parse::<u32>(&diagnostic, raw)?;
                if timeout == 0 || timeout > 86_400 {
                    return Err(ConfigError::InvalidValue {
                        key: diagnostic,
                        value: format!("{raw:?}; expected timeout seconds 1..86400"),
                    });
                }
                set_once(
                    &mut draft.forward_no_answer_timeout,
                    section,
                    key,
                    raw,
                    timeout,
                )?;
                continue;
            }
            DeviceOption::ForwardAll => {
                set_once(
                    &mut draft.forward_all,
                    section,
                    key,
                    raw,
                    parse_optional_forwarding_destination(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardBusy => {
                set_once(
                    &mut draft.forward_busy,
                    section,
                    key,
                    raw,
                    parse_optional_forwarding_destination(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForwardNoAnswer => {
                set_once(
                    &mut draft.forward_no_answer,
                    section,
                    key,
                    raw,
                    parse_optional_forwarding_destination(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::DndFeature => {
                set_once(
                    &mut draft.dnd_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Dnd => {
                set_once(
                    &mut draft.dnd,
                    section,
                    key,
                    raw,
                    parse_dnd_mode(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::DndSchedule => {
                if raw.trim().eq_ignore_ascii_case("none") {
                    if draft.dnd_schedule_cleared || !draft.dnd_schedules.is_empty() {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "none as the sole dnd_schedule value",
                            false,
                        ));
                    }
                    draft.dnd_schedule_cleared = true;
                    continue;
                }
                if draft.dnd_schedule_cleared {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "schedule entries without a dnd_schedule = none value",
                        false,
                    ));
                }
                let schedule = DndSchedule::parse(raw)
                    .map_err(|error| invalid_option(&diagnostic, raw, &error.to_string(), false))?;
                draft.dnd_schedules.push(schedule);
                continue;
            }
            DeviceOption::PrivacyFeature => {
                set_once(
                    &mut draft.privacy_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Privacy => {
                set_once(
                    &mut draft.privacy,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::FeatureDefault => {
                draft
                    .configured_feature_defaults
                    .push(parse_feature_default(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::SetVariable => {
                push_channel_variable(&mut draft.channel_variables, &diagnostic, raw)?;
                continue;
            }
            DeviceOption::Park => {
                set_once(
                    &mut draft.parking_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceAllow => {
                set_once(
                    &mut draft.conference_allowed,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceMusicOnHoldClass => {
                set_once(
                    &mut draft.conference_music_on_hold_class,
                    section,
                    key,
                    raw,
                    parse_empty_optional_setting(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferencePlayGeneralAnnounce => {
                set_once(
                    &mut draft.conference_play_general_announcements,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferencePlayParticipantAnnounce => {
                set_once(
                    &mut draft.conference_play_participant_announcements,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceMuteOnEntry => {
                set_once(
                    &mut draft.conference_mute_on_entry,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceShowList => {
                set_once(
                    &mut draft.conference_show_list,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceDialingEnabled => {
                set_once(
                    &mut draft.conference_dialing_enabled,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ConferenceOptions => {
                set_once(
                    &mut draft.conference_application_options,
                    section,
                    key,
                    raw,
                    parse_application_options(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::UseRedialMenu => {
                set_once(
                    &mut draft.use_redial_menu,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::AllowRinginNotification => {
                set_once(
                    &mut draft.allow_ringing_notification,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::MwiLamp => {
                let mode = match raw.trim().to_ascii_lowercase().as_str() {
                    "off" => LampMode::Off,
                    "on" => LampMode::On,
                    "wink" => LampMode::Wink,
                    "flash" => LampMode::Flash,
                    "blink" => LampMode::Blink,
                    _ => {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "off, on, wink, flash, or blink",
                            false,
                        ));
                    }
                };
                set_once(&mut draft.mwi_lamp_mode, section, key, raw, mode)?;
                continue;
            }
            DeviceOption::MwiOnCall => {
                set_once(
                    &mut draft.mwi_on_call,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::PhoneCodePage => {
                let code_page = match normalize_name(raw).as_str() {
                    "iso88591" | "latin1" => LegacyCodePage::Iso8859_1,
                    "ascii" | "usascii" => LegacyCodePage::Ascii,
                    _ => {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "ISO8859-1 or ASCII",
                            false,
                        ));
                    }
                };
                set_once(&mut draft.legacy_code_page, section, key, raw, code_page)?;
                continue;
            }
            DeviceOption::AllowOverlap => {
                set_once(
                    &mut draft.allow_overlap,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::ForceDtmfMode => {
                set_once(
                    &mut draft.dtmf_mode,
                    section,
                    key,
                    raw,
                    parse_dtmf_mode(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::DirectMedia => {
                set_once(
                    &mut draft.direct_media,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::EarlyMedia => {
                set_once(
                    &mut draft.early_media,
                    section,
                    key,
                    raw,
                    parse_early_media(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::AudioEncryption => {
                set_once(
                    &mut draft.audio_encryption,
                    section,
                    key,
                    raw,
                    parse_media_encryption_policy(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Deny | DeviceOption::Permit => {
                apply_acl_entry(
                    draft.acl_rules.get_or_insert_default(),
                    if matches!(entry.key, DeviceOption::Permit) {
                        AclAction::Permit
                    } else {
                        AclAction::Deny
                    },
                    &diagnostic,
                    raw,
                )?;
                continue;
            }
            DeviceOption::PermitHost => {
                let permitted_hosts = draft.permitted_hosts.get_or_insert_default();
                if raw.trim().is_empty() {
                    permitted_hosts.clear();
                } else {
                    let hostname = parse_hostname(&diagnostic, raw)?;
                    if permitted_hosts.contains(&hostname) {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "a unique permitted hostname",
                            false,
                        ));
                    }
                    permitted_hosts.push(hostname);
                }
                continue;
            }
            DeviceOption::Nat => {
                set_once(
                    &mut draft.nat,
                    section,
                    key,
                    raw,
                    parse_nat_mode(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::Transport => {
                set_once(
                    &mut draft.transport,
                    section,
                    key,
                    raw,
                    parse_transport_requirement(&diagnostic, raw)?,
                )?;
                continue;
            }
            DeviceOption::SignalingTos => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::SignalingDscp => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::SignalingCos => {
                section_values.claim_alias("signaling_cos", entry.source)?;
                draft.qos.signaling_cos = Some(parse_cos(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::AudioTos => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::AudioDscp => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::AudioCos => {
                section_values.claim_alias("audio_cos", entry.source)?;
                draft.qos.audio_cos = Some(parse_cos(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::VideoTos => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::VideoDscp => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_dscp(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::VideoCos => {
                section_values.claim_alias("video_cos", entry.source)?;
                draft.qos.video_cos = Some(parse_cos(&diagnostic, raw)?);
                continue;
            }
            DeviceOption::TrustPhoneIp | DeviceOption::ObsoleteDtmfMode => {
                return Err(invalid_option(
                    &diagnostic,
                    raw,
                    if matches!(entry.key, DeviceOption::TrustPhoneIp) {
                        "remove obsolete trustphoneip; peer addresses are always authoritative"
                    } else {
                        "remove obsolete dtmfmode and use force_dtmfmode"
                    },
                    false,
                ));
            }
            DeviceOption::Allow => {
                draft.codec_settings.push((true, raw.as_str()));
                continue;
            }
            DeviceOption::Disallow => {
                draft.codec_settings.push((false, raw.as_str()));
                continue;
            }
            DeviceOption::Line => parse_line_button(raw, &id, lines, &mut draft.instances)?,
            DeviceOption::Button => parse_button(raw, &id, lines, &mut draft.instances)?,
        };
        if let Some((instance, argument)) = parsed.feature_argument {
            draft.feature_arguments.insert(instance, argument);
        }
        if let Some((instance, target)) = parsed.blf_target {
            draft.blf_targets.insert(instance, target);
        }
        draft.buttons.push(parsed.definition);
    }

    for feature in draft.buttons.iter().filter_map(|button| match button {
        ButtonDefinition::Feature(feature) if feature.feature == ButtonType::DoNotDisturb => {
            Some(feature)
        }
        _ => None,
    }) {
        let Some(argument) = draft.feature_arguments.get_mut(&feature.instance) else {
            continue;
        };
        let mode = parse_dnd_button_mode(
            &format!("{}.button.feature.{}", section.name, feature.instance),
            argument,
        )?;
        *argument = mode
            .canonical()
            .expect("a DND feature argument is never cycle")
            .to_owned();
    }

    validate_dnd_schedules(&draft.dnd_schedules).map_err(|error| {
        invalid_option(
            section.diagnostic_key("dnd_schedule"),
            "<schedule list>",
            &error.to_string(),
            false,
        )
    })?;

    let line_names: Vec<_> = draft
        .buttons
        .iter()
        .filter_map(|button| match button {
            ButtonDefinition::Line(line) => Some(line.number.clone()),
            _ => None,
        })
        .collect();
    if line_names.is_empty() {
        return Err(ConfigError::DeviceWithoutLines(id));
    }

    let description = value(section, "description")
        .unwrap_or(id.as_str())
        .to_owned();
    let resolved_soft_key_profile = draft
        .soft_key_profile
        .unwrap_or_else(|| DEFAULT_SOFT_KEY_PROFILE.to_owned());
    DeviceDefinition {
        id: id.clone(),
        description: description.clone(),
        transport: StationTransportRequirement::Either,
        signaling_qos: None,
        buttons: draft.buttons.clone(),
        soft_keys: soft_key_profiles
            .get(&resolved_soft_key_profile)
            .expect("device soft-key profile was validated during parsing")
            .station_profile(),
        ui: StationUiPolicy::default(),
    }
    .validate()
    .map_err(|error| ConfigError::InvalidValue {
        key: format!("{}.button", section.name),
        value: error.to_string(),
    })?;

    let mut feature_defaults = DeviceFeatureDefaults::default();
    feature_defaults.forwarding.all_enabled = draft.forward_all_enabled.unwrap_or(true);
    feature_defaults.forwarding.busy_enabled = draft.forward_busy_enabled.unwrap_or(true);
    feature_defaults.forwarding.no_answer_enabled = draft.forward_no_answer_enabled.unwrap_or(true);
    feature_defaults.forwarding.no_answer_timeout_seconds =
        draft.forward_no_answer_timeout.unwrap_or(30);
    feature_defaults.forwarding.all = draft.forward_all.unwrap_or(None);
    feature_defaults.forwarding.busy = draft.forward_busy.unwrap_or(None);
    feature_defaults.forwarding.no_answer = draft.forward_no_answer.unwrap_or(None);
    feature_defaults.dnd_enabled = draft.dnd_enabled.unwrap_or(true);
    feature_defaults.dnd = draft.dnd.unwrap_or(DndMode::Off);
    feature_defaults.privacy_enabled = draft.privacy_enabled.unwrap_or(true);
    feature_defaults.privacy = draft.privacy.unwrap_or(false);
    for feature in draft.buttons.iter().filter_map(|button| match button {
        ButtonDefinition::Feature(feature) => Some(feature),
        _ => None,
    }) {
        feature_defaults.buttons.insert(feature.instance, false);
    }
    for (instance, enabled) in draft.configured_feature_defaults {
        let Some(value) = feature_defaults.buttons.get_mut(&instance) else {
            return Err(ConfigError::InvalidValue {
                key: format!("{}.feature_default", section.name),
                value: instance.to_string(),
            });
        };
        *value = enabled;
    }

    let mut parking = DeviceParkingConfig {
        enabled: draft.parking_enabled.unwrap_or(true),
        feature_buttons: HashMap::new(),
    };
    for feature in draft.buttons.iter().filter_map(|button| match button {
        ButtonDefinition::Feature(feature) if feature.feature == ButtonType::ParkingLot => {
            Some(feature)
        }
        _ => None,
    }) {
        let button = parse_parking_lot_button(
            &format!("{}.button.feature.{}", section.name, feature.instance),
            draft
                .feature_arguments
                .get(&feature.instance)
                .map(String::as_str),
        )?;
        parking.feature_buttons.insert(feature.instance, button);
    }

    let conference = DeviceConferenceConfig {
        allowed: draft.conference_allowed.unwrap_or(true),
        music_on_hold_class: draft
            .conference_music_on_hold_class
            .unwrap_or_else(|| Some("default".into())),
        play_general_announcements: draft.conference_play_general_announcements.unwrap_or(true),
        play_participant_announcements: draft
            .conference_play_participant_announcements
            .unwrap_or(true),
        mute_on_entry: draft.conference_mute_on_entry.unwrap_or(false),
        show_conference_list: draft.conference_show_list.unwrap_or(true),
        dialing: ConferenceDialingConfig {
            enabled: draft
                .conference_dialing_enabled
                .unwrap_or(general.conference_dialing.enabled),
            application_options: draft
                .conference_application_options
                .unwrap_or_else(|| general.conference_dialing.application_options.clone()),
        },
    };
    let call_ui = DeviceCallUiConfig {
        redial_mode: if draft.use_redial_menu.unwrap_or(false) {
            RedialMode::PlacedCallsMenu
        } else {
            RedialMode::LastNumber
        },
        hinted_ringing_notification: draft.allow_ringing_notification.unwrap_or(false),
        mwi_lamp_mode: draft.mwi_lamp_mode.unwrap_or(LampMode::On),
        mwi_on_call: draft.mwi_on_call.unwrap_or(false),
        legacy_code_page: draft.legacy_code_page.unwrap_or(LegacyCodePage::Iso8859_1),
    };
    let codecs = if draft.codec_settings.is_empty() {
        general.codecs.clone()
    } else {
        apply_codec_settings(
            Vec::new(),
            &draft.codec_settings,
            &format!("{}.codecs", section.name),
        )?
    };
    let media = DeviceMediaConfig {
        codecs,
        audio_encryption: draft
            .audio_encryption
            .unwrap_or_else(|| general.audio_encryption.clone()),
        dtmf_mode: draft.dtmf_mode.unwrap_or(DtmfMode::Auto),
        direct_media: draft.direct_media.unwrap_or(general.direct_media),
        early_media: draft.early_media.unwrap_or(general.early_media),
    };
    let transport = draft.transport.unwrap_or_default();
    if transport == TransportRequirement::Tls && general.listeners.tls.is_none() {
        return Err(invalid_option(
            section.diagnostic_key("transport"),
            "tls",
            "a configured general TLS listener and credentials",
            false,
        ));
    }
    let network = DeviceNetworkPolicy {
        acl: draft.acl_rules.map_or_else(
            || general.network.acl.clone(),
            |rules| AccessControlList { rules },
        ),
        permitted_hosts: draft.permitted_hosts.unwrap_or_default(),
        nat: draft.nat.unwrap_or(general.network.nat),
        qos: draft.qos.resolve(general.qos),
        transport,
    };

    Ok(DeviceConfig {
        id,
        description,
        lines: line_names,
        buttons: draft.buttons,
        feature_arguments: draft.feature_arguments,
        blf_targets: draft.blf_targets,
        channel_variables: draft.channel_variables,
        soft_key_profile: resolved_soft_key_profile,
        feature_defaults,
        dnd_schedules: draft.dnd_schedules,
        parking,
        conference,
        call_ui,
        allow_overlap: draft.allow_overlap.unwrap_or(general.allow_overlap),
        media,
        network,
    })
}
