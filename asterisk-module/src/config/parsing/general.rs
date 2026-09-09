use super::super::*;

pub(in crate::config) fn parse_general(
    config: &mut GeneralConfig,
    section: &RawSection,
) -> Result<(), ConfigError> {
    let mut draft = GeneralSectionDraft::default();
    let mut section_values = SectionValues::new(section);
    for entry in deserialize_entries::<GeneralOption>(section)? {
        let key = &entry.source.key;
        let raw = &entry.source.value;
        let diagnostic = entry.source.diagnostic_key();
        match entry.key {
            GeneralOption::ConfigurationSource => set_once(
                &mut draft.configuration_source,
                section,
                key,
                raw,
                parse_configuration_source(&diagnostic, raw)?,
            )?,
            GeneralOption::DateFormat => set_once(
                &mut draft.date_template,
                section,
                key,
                raw,
                DateTemplate::new(raw.trim())
                    .map_err(|error| invalid_option(&diagnostic, raw, &error.to_string(), false))?,
            )?,
            GeneralOption::Timezone => set_once(
                &mut draft.timezone,
                section,
                key,
                raw,
                raw.trim().parse::<sccp_protocol::TimeZone>().map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "an IANA timezone, e.g. America/Los_Angeles",
                        false,
                    )
                })?,
            )?,
            GeneralOption::TimezoneOffset => {
                let hours = raw
                    .trim()
                    .parse::<i16>()
                    .ok()
                    .filter(|hours| (-14..=14).contains(hours))
                    .ok_or_else(|| {
                        invalid_option(&diagnostic, raw, "UTC offset -14..14 hours", false)
                    })?;
                set_once(
                    &mut draft.timezone_offset_minutes,
                    section,
                    key,
                    raw,
                    hours * 60,
                )?;
            }
            GeneralOption::Bind => {
                let address = parse::<SocketAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "an IPv4/IPv6 socket address with port",
                        false,
                    )
                })?;
                if address.port() == 0 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "clear listener port 1..65535",
                        false,
                    ));
                }
                set_once(&mut draft.clear_bind, section, key, raw, address)?;
            }
            GeneralOption::BindAddress => {
                let address = parse::<IpAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(&diagnostic, raw, "an IPv4 or IPv6 address", false)
                })?;
                set_once(&mut draft.clear_address, section, key, raw, address)?;
            }
            GeneralOption::Port => {
                let port = parse::<u16>(&diagnostic, raw)
                    .map_err(|_| invalid_option(&diagnostic, raw, "TCP port 1..65535", false))?;
                if port == 0 {
                    return Err(invalid_option(&diagnostic, raw, "TCP port 1..65535", false));
                }
                set_once(&mut draft.clear_port, section, key, raw, port)?;
            }
            GeneralOption::AdvertisedAddress => {
                if draft.advertised_alias_seen
                    || draft.advertised_ipv4.is_some()
                    || draft.advertised_ipv6.is_some()
                {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one advertised_address or explicit advertised_ipv4/advertised_ipv6 values",
                        false,
                    ));
                }
                draft.advertised_alias_seen = true;
                let address: IpAddr = parse(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "a non-unspecified IPv4 or IPv6 address",
                        false,
                    )
                })?;
                if address.is_unspecified() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "a non-unspecified IPv4 or IPv6 address",
                        false,
                    ));
                }
                match address {
                    IpAddr::V4(address) => {
                        draft.advertised_ipv4 = Some(Some(address));
                        draft.advertised_ipv6 = Some(None);
                    }
                    IpAddr::V6(address) => {
                        draft.advertised_ipv4 = Some(None);
                        draft.advertised_ipv6 = Some(Some(address));
                    }
                }
            }
            GeneralOption::AdvertisedIpv4 => {
                if draft.advertised_alias_seen || draft.advertised_ipv4.is_some() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one value for the advertised IPv4 address",
                        false,
                    ));
                }
                let value = raw.trim();
                draft.advertised_ipv4 =
                    Some(if value.is_empty() || value.eq_ignore_ascii_case("none") {
                        None
                    } else {
                        let address: Ipv4Addr = parse(&diagnostic, value).map_err(|_| {
                            invalid_option(&diagnostic, raw, "an IPv4 address or none", false)
                        })?;
                        if address.is_unspecified() {
                            return Err(invalid_option(
                                &diagnostic,
                                raw,
                                "a non-unspecified IPv4 address or none",
                                false,
                            ));
                        }
                        Some(address)
                    });
            }
            GeneralOption::AdvertisedIpv6 => {
                if draft.advertised_alias_seen || draft.advertised_ipv6.is_some() {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "one value for the advertised IPv6 address",
                        false,
                    ));
                }
                let value = raw.trim();
                draft.advertised_ipv6 =
                    Some(if value.is_empty() || value.eq_ignore_ascii_case("none") {
                        None
                    } else {
                        let address: Ipv6Addr = parse(&diagnostic, value).map_err(|_| {
                            invalid_option(&diagnostic, raw, "an IPv6 address or none", false)
                        })?;
                        if address.is_unspecified() {
                            return Err(invalid_option(
                                &diagnostic,
                                raw,
                                "a non-unspecified IPv6 address or none",
                                false,
                            ));
                        }
                        Some(address)
                    });
            }
            GeneralOption::TlsBind => {
                let address = parse::<SocketAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "an IPv4/IPv6 TLS socket address with port",
                        false,
                    )
                })?;
                if address.port() == 0 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "TLS listener port 1..65535",
                        false,
                    ));
                }
                set_once(&mut draft.tls_bind, section, key, raw, address)?;
            }
            GeneralOption::TlsBindAddress => {
                let address = parse::<IpAddr>(&diagnostic, raw).map_err(|_| {
                    invalid_option(&diagnostic, raw, "an IPv4 or IPv6 TLS bind address", false)
                })?;
                set_once(&mut draft.tls_address, section, key, raw, address)?;
            }
            GeneralOption::TlsPort => {
                let port = parse::<u16>(&diagnostic, raw)
                    .map_err(|_| invalid_option(&diagnostic, raw, "TLS port 1..65535", false))?;
                if port == 0 {
                    return Err(invalid_option(&diagnostic, raw, "TLS port 1..65535", false));
                }
                set_once(&mut draft.tls_port, section, key, raw, port)?;
            }
            GeneralOption::TlsCombinedPem => {
                let path = parse_path(&diagnostic, raw, true)?;
                set_once(&mut draft.combined_pem, section, key, "<redacted>", path)?;
            }
            GeneralOption::TlsCertificate => {
                let path = parse_path(&diagnostic, raw, false)?;
                set_once(&mut draft.tls_certificate, section, key, raw, path)?;
            }
            GeneralOption::TlsPrivateKey => {
                let path = parse_path(&diagnostic, raw, true)?;
                set_once(&mut draft.tls_private_key, section, key, "<redacted>", path)?;
            }
            GeneralOption::TlsTrustStore => {
                let path = parse_path(&diagnostic, raw, true)?;
                set_once(&mut draft.tls_trust_store, section, key, "<redacted>", path)?;
            }
            GeneralOption::Deny | GeneralOption::Permit => {
                apply_acl_entry(
                    draft.acl_rules.get_or_insert_default(),
                    if matches!(entry.key, GeneralOption::Permit) {
                        AclAction::Permit
                    } else {
                        AclAction::Deny
                    },
                    &diagnostic,
                    raw,
                )?;
            }
            GeneralOption::LocalNetwork => {
                let local_networks = draft.local_networks.get_or_insert_default();
                if raw.trim().is_empty() {
                    local_networks.clear();
                } else {
                    local_networks.extend(parse_ip_networks(&diagnostic, raw)?);
                }
            }
            GeneralOption::ExternalAddress => {
                let value = raw.trim();
                let address = if value.is_empty() || value.eq_ignore_ascii_case("none") {
                    None
                } else {
                    let address: IpAddr = parse(&diagnostic, value).map_err(|_| {
                        invalid_option(&diagnostic, raw, "an IPv4/IPv6 address or none", false)
                    })?;
                    if address.is_unspecified() {
                        return Err(invalid_option(
                            &diagnostic,
                            raw,
                            "a non-unspecified IPv4/IPv6 address or none",
                            false,
                        ));
                    }
                    Some(address)
                };
                set_once(&mut draft.external_address, section, key, raw, address)?;
            }
            GeneralOption::ExternalHost => {
                let value = raw.trim();
                let hostname = if value.is_empty() || value.eq_ignore_ascii_case("none") {
                    None
                } else {
                    Some(parse_hostname(&diagnostic, value)?)
                };
                set_once(&mut draft.external_hostname, section, key, raw, hostname)?;
            }
            GeneralOption::ExternalRefresh => {
                let refresh = parse::<u32>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "external DNS refresh interval 1..86400 seconds",
                        false,
                    )
                })?;
                if !(1..=MAX_EXTERNAL_REFRESH_SECONDS).contains(&refresh) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "external DNS refresh interval 1..86400 seconds",
                        false,
                    ));
                }
                set_once(&mut draft.external_refresh, section, key, raw, refresh)?;
            }
            GeneralOption::Nat => {
                let mode = parse_nat_mode(&diagnostic, raw)?;
                set_once(&mut draft.nat, section, key, raw, mode)?;
            }
            GeneralOption::SignalingTos => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
            }
            GeneralOption::SignalingDscp => {
                section_values.claim_alias("signaling_dscp", entry.source)?;
                draft.qos.signaling_dscp = Some(parse_dscp(&diagnostic, raw)?);
            }
            GeneralOption::SignalingCos => {
                section_values.claim_alias("signaling_cos", entry.source)?;
                draft.qos.signaling_cos = Some(parse_cos(&diagnostic, raw)?);
            }
            GeneralOption::AudioTos => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
            }
            GeneralOption::AudioDscp => {
                section_values.claim_alias("audio_dscp", entry.source)?;
                draft.qos.audio_dscp = Some(parse_dscp(&diagnostic, raw)?);
            }
            GeneralOption::AudioCos => {
                section_values.claim_alias("audio_cos", entry.source)?;
                draft.qos.audio_cos = Some(parse_cos(&diagnostic, raw)?);
            }
            GeneralOption::VideoTos => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_tos_as_dscp(&diagnostic, raw)?);
            }
            GeneralOption::VideoDscp => {
                section_values.claim_alias("video_dscp", entry.source)?;
                draft.qos.video_dscp = Some(parse_dscp(&diagnostic, raw)?);
            }
            GeneralOption::VideoCos => {
                section_values.claim_alias("video_cos", entry.source)?;
                draft.qos.video_cos = Some(parse_cos(&diagnostic, raw)?);
            }
            GeneralOption::TrustPhoneIp => {
                return Err(invalid_option(
                    &diagnostic,
                    raw,
                    "remove obsolete trustphoneip; peer addresses are always authoritative",
                    false,
                ));
            }
            GeneralOption::ServerName => config.server_name.clone_from(raw),
            GeneralOption::Language => set_once(
                &mut draft.language,
                section,
                key,
                raw,
                parse_metadata_required(&diagnostic, raw, MAX_LANGUAGE_BYTES, false)?,
            )?,
            GeneralOption::AccountCode => set_once(
                &mut draft.account_code,
                section,
                key,
                "<redacted>",
                parse_metadata_optional(&diagnostic, raw, MAX_ACCOUNT_CODE_BYTES, true)?,
            )?,
            GeneralOption::Keepalive => config.keepalive_seconds = parse(&diagnostic, raw)?,
            GeneralOption::SecondaryKeepalive => {
                config.secondary_keepalive_seconds = parse(&diagnostic, raw)?;
            }
            GeneralOption::SignalingServer => {
                config
                    .signaling_servers
                    .push(parse_signaling_server(&diagnostic, raw)?);
            }
            GeneralOption::FirstDigitTimeout => {
                let seconds = parse::<u64>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "first-digit timeout 1..86400 seconds",
                        false,
                    )
                })?;
                if !(1..=86_400).contains(&seconds) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "first-digit timeout 1..86400 seconds",
                        false,
                    ));
                }
                set_once(
                    &mut draft.first_digit_timeout,
                    section,
                    key,
                    raw,
                    seconds * 1_000,
                )?;
            }
            GeneralOption::InterdigitTimeoutMs => {
                let milliseconds = parse::<u64>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 250..86400000 milliseconds",
                        false,
                    )
                })?;
                if !(250..=86_400_000).contains(&milliseconds) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 250..86400000 milliseconds",
                        false,
                    ));
                }
                set_once(
                    &mut draft.interdigit_timeout,
                    section,
                    key,
                    raw,
                    milliseconds,
                )?;
            }
            GeneralOption::DigitTimeout => {
                let seconds = parse::<u64>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 1..86400 seconds",
                        false,
                    )
                })?;
                if !(1..=86_400).contains(&seconds) {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "subsequent-digit timeout 1..86400 seconds",
                        false,
                    ));
                }
                set_once(
                    &mut draft.interdigit_timeout,
                    section,
                    key,
                    raw,
                    seconds * 1_000,
                )?;
            }
            GeneralOption::DigitTimeoutChar => {
                set_once(
                    &mut draft.dial_terminator,
                    section,
                    key,
                    raw,
                    parse_dial_terminator(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::RecordDigitTimeoutChar => {
                set_once(
                    &mut draft.record_dial_terminator,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::SimulateEnbloc => {
                set_once(
                    &mut draft.simulate_enbloc,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::SpeedDialAwaitFurtherDigits => {
                set_once(
                    &mut draft.speed_dial_await_further_digits,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::AllowOverlap => {
                set_once(
                    &mut draft.allow_overlap,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::TransferOnHangup => {
                set_once(
                    &mut draft.transfer_on_hangup,
                    section,
                    key,
                    raw,
                    parse_bool(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::CallAnswerOrder => {
                set_once(
                    &mut draft.call_answer_order,
                    section,
                    key,
                    raw,
                    parse_call_answer_order(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::RingType => {
                set_once(
                    &mut draft.ring_type,
                    section,
                    key,
                    raw,
                    parse_ringer_mode(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::CallWaitingTone => {
                let tone = if raw.trim() == "0" {
                    None
                } else {
                    Some(parse_tone(&diagnostic, raw)?)
                };
                set_once(&mut draft.call_waiting_tone, section, key, raw, tone)?;
            }
            GeneralOption::CallWaitingInterval => {
                set_once(
                    &mut draft.call_waiting_interval,
                    section,
                    key,
                    raw,
                    raw.trim()
                        .parse::<u32>()
                        .ok()
                        .filter(|seconds| *seconds <= 86_400)
                        .ok_or_else(|| {
                            invalid_option(
                                &diagnostic,
                                raw,
                                "call-waiting interval 0..86400 seconds",
                                false,
                            )
                        })?,
                )?;
            }
            GeneralOption::Fallback => {
                set_once(
                    &mut draft.fallback_decision,
                    section,
                    key,
                    raw,
                    parse_fallback_decision(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::BackoffTime => {
                let seconds = parse::<u32>(&diagnostic, raw).map_err(|_| {
                    invalid_option(
                        &diagnostic,
                        raw,
                        "registration-token backoff of at least 30 seconds",
                        false,
                    )
                })?;
                if seconds < 30 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "registration-token backoff of at least 30 seconds",
                        false,
                    ));
                }
                set_once(&mut draft.fallback_backoff, section, key, raw, seconds)?;
            }
            GeneralOption::ServerPriority => {
                let priority = parse::<u8>(&diagnostic, raw).map_err(|_| {
                    invalid_option(&diagnostic, raw, "positive fallback-server priority", false)
                })?;
                if priority == 0 {
                    return Err(invalid_option(
                        &diagnostic,
                        raw,
                        "positive fallback-server priority",
                        false,
                    ));
                }
                set_once(
                    &mut draft.fallback_server_priority,
                    section,
                    key,
                    raw,
                    priority,
                )?;
            }
            GeneralOption::Allow => draft.codec_settings.push((true, raw.as_str())),
            GeneralOption::Disallow => draft.codec_settings.push((false, raw.as_str())),
            GeneralOption::ConferenceEnabled => set_once(
                &mut draft.conference_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::ConferenceOptions => set_once(
                &mut draft.conference_options,
                section,
                key,
                raw,
                parse_application_options(&diagnostic, raw)?,
            )?,
            GeneralOption::AutoanswerRingTime => set_once(
                &mut draft.auto_answer_ring_time,
                section,
                key,
                raw,
                parse::<u32>(&diagnostic, raw)?,
            )?,
            GeneralOption::AutoanswerTone => set_once(
                &mut draft.auto_answer_tone,
                section,
                key,
                raw,
                parse_tone(&diagnostic, raw)?,
            )?,
            GeneralOption::RemoteHangupTone => {
                let tone = if raw.trim() == "0" {
                    None
                } else {
                    Some(parse_tone(&diagnostic, raw)?)
                };
                set_once(&mut draft.remote_hangup_tone, section, key, raw, tone)?;
            }
            GeneralOption::HotlineEnabled => set_once(
                &mut draft.hotline_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::HotlineExtension => set_once(
                &mut draft.hotline_extension,
                section,
                key,
                "<redacted>",
                parse_optional_hotline_destination(&diagnostic, raw)?,
            )?,
            GeneralOption::HotlineContext => set_once(
                &mut draft.hotline_context,
                section,
                key,
                raw,
                parse_bounded_setting_allow_empty(&diagnostic, raw, MAX_HOTLINE_FIELD_BYTES)?,
            )?,
            GeneralOption::HotlineLabel => set_once(
                &mut draft.hotline_label,
                section,
                key,
                raw,
                parse_bounded_setting_allow_empty(&diagnostic, raw, MAX_HOTLINE_FIELD_BYTES)?,
            )?,
            GeneralOption::DirectMedia => set_once(
                &mut draft.direct_media,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::EarlyMedia => set_once(
                &mut draft.early_media,
                section,
                key,
                raw,
                parse_early_media(&diagnostic, raw)?,
            )?,
            GeneralOption::AudioEncryption => set_once(
                &mut draft.audio_encryption,
                section,
                key,
                raw,
                parse_media_encryption_policy(&diagnostic, raw)?,
            )?,
            GeneralOption::EchoCancel => set_once(
                &mut draft.echo_cancellation,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::SilenceSuppression => set_once(
                &mut draft.silence_suppression,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbEnable => set_once(
                &mut draft.jitter_enabled,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbForce => set_once(
                &mut draft.jitter_forced,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbLog => set_once(
                &mut draft.jitter_log_frames,
                section,
                key,
                raw,
                parse_bool(&diagnostic, raw)?,
            )?,
            GeneralOption::JbMaxSize => set_once(
                &mut draft.jitter_max_size_ms,
                section,
                key,
                raw,
                parse_positive_jitter_millis(&diagnostic, raw)?,
            )?,
            GeneralOption::JbResyncThreshold => set_once(
                &mut draft.jitter_resync_threshold_ms,
                section,
                key,
                raw,
                parse_positive_jitter_millis(&diagnostic, raw)?,
            )?,
            GeneralOption::JbImplementation => set_once(
                &mut draft.jitter_implementation,
                section,
                key,
                raw,
                parse_jitter_buffer_implementation(&diagnostic, raw)?,
            )?,
            GeneralOption::RegistrationContext => {
                set_once(
                    &mut draft.registration_contexts,
                    section,
                    key,
                    raw,
                    parse_registration_contexts(&diagnostic, raw)?,
                )?;
            }
            GeneralOption::DeviceTable => set_once(
                &mut draft.device_table,
                section,
                key,
                raw,
                parse_realtime_family(&diagnostic, raw)?,
            )?,
            GeneralOption::LineTable => set_once(
                &mut draft.line_table,
                section,
                key,
                raw,
                parse_realtime_family(&diagnostic, raw)?,
            )?,
        }
    }
    if let Some(enabled) = draft.conference_enabled {
        config.conference_dialing.enabled = enabled;
    }
    config.configuration_source = draft.configuration_source.unwrap_or_default();
    if let Some(order) = draft.call_answer_order {
        config.call_answer_order = order;
    }
    if draft.timezone.is_some() && draft.timezone_offset_minutes.is_some() {
        return Err(invalid_option(
            "timezone",
            "timezone + tzoffset",
            "only one of timezone or tzoffset",
            false,
        ));
    }
    if let Some(timezone) = draft.timezone {
        config.timezone = Some(timezone);
    }
    if let Some(offset) = draft.timezone_offset_minutes {
        config.timezone_offset_minutes = offset;
    }
    if let Some(template) = draft.date_template {
        config.date_template = template;
    }
    if let Some(mode) = draft.ring_type {
        config.ring_type = mode;
    }
    if let Some(tone) = draft.call_waiting_tone {
        config.call_waiting_tone = tone;
    }
    if let Some(seconds) = draft.call_waiting_interval {
        config.call_waiting_interval_seconds = seconds;
    }
    if let Some(timeout_ms) = draft.first_digit_timeout {
        config.first_digit_timeout_ms = timeout_ms;
    }
    if let Some(timeout_ms) = draft.interdigit_timeout {
        config.interdigit_timeout_ms = timeout_ms;
    }
    if let Some(character) = draft.dial_terminator {
        config.dial_terminator.character = character;
    }
    if let Some(record) = draft.record_dial_terminator {
        config.dial_terminator.record = record;
    }
    if let Some(enabled) = draft.simulate_enbloc {
        config.simulate_enbloc = enabled;
    }
    if let Some(enabled) = draft.speed_dial_await_further_digits {
        config.speed_dial_await_further_digits = enabled;
    }
    if let Some(enabled) = draft.allow_overlap {
        config.allow_overlap = enabled;
    }
    if let Some(enabled) = draft.transfer_on_hangup {
        config.transfer_on_hangup = enabled;
    }
    if let Some(decision) = draft.fallback_decision {
        config.fallback_registration.decision = decision;
    }
    if let Some(seconds) = draft.fallback_backoff {
        config.fallback_registration.backoff_seconds = seconds;
    }
    if let Some(priority) = draft.fallback_server_priority {
        config.fallback_registration.server_priority = priority;
    }
    if let Some(options) = draft.conference_options {
        config.conference_dialing.application_options = options;
    }
    if let Some(contexts) = draft.registration_contexts {
        config.registration.contexts = contexts;
    }
    if !draft.codec_settings.is_empty() {
        config.codecs = apply_codec_settings(Vec::new(), &draft.codec_settings, "general.codecs")?;
    }
    if let Some(ring_time_seconds) = draft.auto_answer_ring_time {
        config.auto_answer.ring_time_seconds = ring_time_seconds;
    }
    if let Some(tone) = draft.auto_answer_tone {
        config.auto_answer.tone = tone;
    }
    if let Some(tone) = draft.remote_hangup_tone {
        config.remote_hangup_tone = tone;
    }
    if let Some(enabled) = draft.hotline_enabled {
        config.guest_hotline.enabled = enabled;
    }
    if let Some(extension) = draft.hotline_extension {
        config.guest_hotline.extension = extension;
    }
    if let Some(context) = draft.hotline_context {
        config.guest_hotline.context = context;
    }
    if let Some(label) = draft.hotline_label {
        config.guest_hotline.label = label;
    }
    if config.guest_hotline.enabled
        && (config.guest_hotline.extension.is_none()
            || config.guest_hotline.context.is_empty()
            || config.guest_hotline.label.is_empty())
    {
        return Err(ConfigError::InvalidValue {
            key: "general.hotline_enabled".into(),
            value: "enabled guest hotline requires extension, context, and label".into(),
        });
    }
    if draft.clear_bind.is_some() && (draft.clear_address.is_some() || draft.clear_port.is_some()) {
        return Err(invalid_option(
            section.section_location(),
            "clear listener aliases",
            "either bind/clear_bind or bindaddr+port, not both",
            false,
        ));
    }
    let clear = draft.clear_bind.unwrap_or_else(|| {
        SocketAddr::new(
            draft.clear_address.unwrap_or(config.listeners.clear.ip()),
            draft.clear_port.unwrap_or(config.listeners.clear.port()),
        )
    });
    if clear.port() == 0 {
        return Err(invalid_option(
            section.section_location(),
            &clear.to_string(),
            "clear listener port 1..65535",
            false,
        ));
    }
    config.bind = clear;
    config.listeners.clear = clear;

    if let Some(ipv4) = draft.advertised_ipv4 {
        config.network.advertised.ipv4 = ipv4;
        if let Some(ipv4) = ipv4 {
            config.advertised_address = ipv4;
        }
    }
    if let Some(ipv6) = draft.advertised_ipv6 {
        config.network.advertised.ipv6 = ipv6;
    }
    if config.network.advertised.ipv4.is_none() && config.network.advertised.ipv6.is_none() {
        return Err(invalid_option(
            section.section_location(),
            "none",
            "at least one advertised IPv4 or IPv6 address",
            false,
        ));
    }

    if let Some(rules) = draft.acl_rules {
        config.network.acl.rules = rules;
    }
    if let Some(local_networks) = draft.local_networks {
        config.network.local_networks = local_networks;
    }
    config.network.nat = draft.nat.unwrap_or(NatMode::Auto);

    let external_address = draft.external_address.take().flatten();
    let external_hostname = draft.external_hostname.take().flatten();
    if external_address.is_some() && external_hostname.is_some() {
        return Err(invalid_option(
            section.section_location(),
            "externip + externhost",
            "exactly one external address source: externip or externhost",
            false,
        ));
    }
    if draft.external_refresh.is_some() && external_hostname.is_none() {
        return Err(invalid_option(
            section.section_location(),
            "externrefresh without externhost",
            "externrefresh only together with externhost",
            false,
        ));
    }
    config.network.external = if let Some(address) = external_address {
        Some(ExternalAddress::Address(address))
    } else {
        external_hostname.map(|name| ExternalAddress::Hostname {
            name,
            refresh_seconds: draft.external_refresh.unwrap_or(60),
        })
    };

    if draft.tls_bind.is_some() && (draft.tls_address.is_some() || draft.tls_port.is_some()) {
        return Err(invalid_option(
            section.section_location(),
            "TLS listener aliases",
            "either tls_bind or secbindaddr+secport, not both",
            false,
        ));
    }
    let split_credentials_requested = draft.tls_certificate.is_some()
        || draft.tls_private_key.is_some()
        || draft.tls_trust_store.is_some();
    if draft.combined_pem.is_some() && split_credentials_requested {
        return Err(invalid_option(
            section.section_location(),
            "<redacted TLS credentials>",
            "either certfile/combined PEM or split certificate+private key+optional trust store",
            true,
        ));
    }
    let tls_requested = draft.tls_bind.is_some()
        || draft.tls_address.is_some()
        || draft.tls_port.is_some()
        || draft.combined_pem.is_some()
        || split_credentials_requested;
    config.listeners.tls = if tls_requested {
        let credentials = if let Some(path) = draft.combined_pem {
            TlsCredentials::CombinedPem(path)
        } else {
            let certificate = draft.tls_certificate.ok_or_else(|| {
                invalid_option(
                    section.section_location(),
                    "<redacted TLS credentials>",
                    "tls_certificate together with tls_private_key",
                    true,
                )
            })?;
            let private_key = draft.tls_private_key.ok_or_else(|| {
                invalid_option(
                    section.section_location(),
                    "<redacted TLS credentials>",
                    "tls_private_key together with tls_certificate",
                    true,
                )
            })?;
            TlsCredentials::SplitPem {
                certificate,
                private_key,
                trust_store: draft.tls_trust_store,
            }
        };
        let bind = draft.tls_bind.unwrap_or_else(|| {
            SocketAddr::new(
                draft
                    .tls_address
                    .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
                draft.tls_port.unwrap_or(2443),
            )
        });
        if bind.port() == 0 {
            return Err(invalid_option(
                section.section_location(),
                &bind.to_string(),
                "TLS listener port 1..65535",
                false,
            ));
        }
        if bind == clear {
            return Err(invalid_option(
                section.section_location(),
                &bind.to_string(),
                "distinct clear and TLS listener socket addresses",
                false,
            ));
        }
        Some(TlsListener { bind, credentials })
    } else {
        None
    };
    config.qos = draft.qos.resolve(config.qos);
    if let Some(language) = draft.language {
        config.language = language;
    }
    if let Some(account_code) = draft.account_code {
        config.account_code = account_code;
    }
    config.direct_media = draft.direct_media.unwrap_or(false);
    config.early_media = draft.early_media.unwrap_or(true);
    config.audio_encryption = draft.audio_encryption.unwrap_or_default();
    config.audio_processing = AudioProcessingPolicy {
        echo_cancellation: if draft.echo_cancellation.unwrap_or(true) {
            EchoCancellation::On
        } else {
            EchoCancellation::Off
        },
        silence_suppression: if draft.silence_suppression.unwrap_or(false) {
            SilenceSuppression::On
        } else {
            SilenceSuppression::Off
        },
    };
    config.jitter_buffer = JitterBufferConfig {
        enabled: draft.jitter_enabled.unwrap_or(false),
        forced: draft.jitter_forced.unwrap_or(false),
        log_frames: draft.jitter_log_frames.unwrap_or(false),
        max_size_ms: draft.jitter_max_size_ms.unwrap_or(200),
        resync_threshold_ms: draft.jitter_resync_threshold_ms.unwrap_or(1_000),
        implementation: draft.jitter_implementation.unwrap_or_default(),
    };
    config.realtime_tables = match (draft.device_table, draft.line_table) {
        (None, None) => None,
        (Some(device_family), Some(line_family)) if device_family != line_family => {
            Some(RealtimeTableConfig {
                device_family,
                line_family,
            })
        }
        (Some(device_family), Some(_line_family)) => {
            return Err(invalid_option(
                section.section_location(),
                &device_family,
                "different devicetable and linetable family names",
                false,
            ));
        }
        (Some(_), None) => {
            return Err(invalid_option(
                section.diagnostic_key("devicetable"),
                "devicetable without linetable",
                "devicetable and linetable together",
                false,
            ));
        }
        (None, Some(_)) => {
            return Err(invalid_option(
                section.diagnostic_key("linetable"),
                "linetable without devicetable",
                "devicetable and linetable together",
                false,
            ));
        }
    };
    if config.configuration_source == ConfigurationSource::Sorcery
        && config.realtime_tables.is_some()
    {
        return Err(invalid_option(
            section.section_location(),
            "configuration_source=sorcery with realtime tables",
            "sorcery without devicetable or linetable",
            false,
        ));
    }
    Ok(())
}

fn parse_configuration_source(key: &str, raw: &str) -> Result<ConfigurationSource, ConfigError> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "file" => Ok(ConfigurationSource::File),
        "sorcery" => Ok(ConfigurationSource::Sorcery),
        _ => Err(invalid_option(key, raw, "file or sorcery", false)),
    }
}
