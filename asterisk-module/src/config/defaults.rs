//! Semantic defaults for context-free configuration policy.

use super::*;

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            configuration_source: ConfigurationSource::File,
            bind: SocketAddr::from(([0, 0, 0, 0], 2000)),
            advertised_address: Ipv4Addr::LOCALHOST,
            server_name: "Asterisk SCCP".into(),
            language: "en".into(),
            account_code: None,
            keepalive_seconds: 30,
            secondary_keepalive_seconds: 30,
            signaling_servers: Vec::new(),
            first_digit_timeout_ms: 10_000,
            interdigit_timeout_ms: 5_000,
            dial_terminator: DialTerminatorConfig::default(),
            simulate_enbloc: true,
            speed_dial_await_further_digits: false,
            allow_overlap: false,
            transfer_on_hangup: false,
            call_answer_order: CallAnswerOrder::default(),
            timezone_offset_minutes: 0,
            timezone: None,
            date_template: DateTemplate::default(),
            ring_type: RingerMode::Outside,
            call_waiting_tone: Some(Tone::CallWaiting),
            call_waiting_interval_seconds: 0,
            // This is an allow-set, not a hidden preference imposed on a
            // registered station. Runtime negotiation preserves the phone's
            // advertised order and lets Asterisk choose the PBX format.
            codecs: mapped_audio_codecs(),
            audio_encryption: MediaEncryptionPolicy::default(),
            conference_dialing: ConferenceDialingConfig::default(),
            auto_answer: AutoAnswerConfig::default(),
            remote_hangup_tone: None,
            guest_hotline: GuestHotlineConfig::default(),
            direct_media: false,
            early_media: true,
            audio_processing: AudioProcessingPolicy::default(),
            jitter_buffer: JitterBufferConfig::default(),
            registration: RegistrationConfig::default(),
            fallback_registration: FallbackRegistrationConfig::default(),
            network: NetworkPolicy::default(),
            qos: QosPolicy::default(),
            listeners: ListenerPolicy::default(),
            realtime_tables: None,
        }
    }
}

impl Default for DialTerminatorConfig {
    fn default() -> Self {
        Self {
            character: '#',
            record: false,
        }
    }
}

impl Default for FallbackRegistrationConfig {
    fn default() -> Self {
        Self {
            decision: FallbackDecision::Reject,
            backoff_seconds: 60,
            server_priority: 1,
        }
    }
}

impl Default for AdvertisedAddresses {
    fn default() -> Self {
        Self {
            ipv4: Some(Ipv4Addr::LOCALHOST),
            ipv6: None,
        }
    }
}

impl Default for NetworkPolicy {
    fn default() -> Self {
        Self {
            acl: AccessControlList::default(),
            local_networks: internal_networks(),
            external: None,
            advertised: AdvertisedAddresses::default(),
            nat: NatMode::Auto,
        }
    }
}

impl Default for QosPolicy {
    fn default() -> Self {
        Self {
            signaling: QosClass {
                dscp: Dscp(24),
                cos: Cos(4),
            },
            audio: QosClass {
                dscp: Dscp(46),
                cos: Cos(6),
            },
            video: QosClass {
                dscp: Dscp(34),
                cos: Cos(5),
            },
        }
    }
}

impl Default for ListenerPolicy {
    fn default() -> Self {
        Self {
            clear: SocketAddr::from(([0, 0, 0, 0], 2000)),
            tls: None,
        }
    }
}

impl Default for ForwardingDefaults {
    fn default() -> Self {
        Self {
            all_enabled: true,
            busy_enabled: true,
            no_answer_enabled: true,
            no_answer_timeout_seconds: 30,
            all: None,
            busy: None,
            no_answer: None,
        }
    }
}

impl Default for DeviceFeatureDefaults {
    fn default() -> Self {
        Self {
            forwarding: ForwardingDefaults::default(),
            dnd_enabled: true,
            dnd: DndMode::Off,
            privacy_enabled: true,
            privacy: false,
            buttons: HashMap::new(),
        }
    }
}

impl Default for PickupConfig {
    fn default() -> Self {
        Self {
            call_groups: BTreeSet::new(),
            pickup_groups: BTreeSet::new(),
            named_call_groups: BTreeSet::new(),
            named_pickup_groups: BTreeSet::new(),
            directed: true,
            directed_context: None,
            answer_directed: true,
        }
    }
}

impl Default for DeviceParkingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            feature_buttons: HashMap::new(),
        }
    }
}

impl Default for ConferenceDialingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            application_options: "qxd".into(),
        }
    }
}

impl Default for DeviceConferenceConfig {
    fn default() -> Self {
        Self {
            allowed: true,
            music_on_hold_class: Some("default".into()),
            play_general_announcements: true,
            play_participant_announcements: true,
            mute_on_entry: false,
            show_conference_list: true,
            dialing: ConferenceDialingConfig::default(),
        }
    }
}

impl Default for AutoAnswerConfig {
    fn default() -> Self {
        Self {
            ring_time_seconds: 1,
            tone: Tone::Zip,
        }
    }
}

impl Default for GuestHotlineConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            extension: Some(
                HotlineDestination::new("111").expect("built-in guest-hotline extension is valid"),
            ),
            context: "default".into(),
            label: "hotline".into(),
        }
    }
}

impl Default for LineDialToneConfig {
    fn default() -> Self {
        Self {
            initial: Tone::InsideDial,
            secondary_prefix: None,
            secondary: Tone::OutsideDial,
        }
    }
}

impl Default for JitterBufferConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            forced: false,
            log_frames: false,
            max_size_ms: 200,
            resync_threshold_ms: 1_000,
            implementation: JitterBufferImplementation::Fixed,
        }
    }
}

impl Default for DeviceMediaConfig {
    fn default() -> Self {
        Self {
            codecs: GeneralConfig::default().codecs,
            audio_encryption: MediaEncryptionPolicy::default(),
            dtmf_mode: DtmfMode::Auto,
            direct_media: false,
            early_media: true,
        }
    }
}

impl Default for LineMediaConfig {
    fn default() -> Self {
        Self {
            codecs: GeneralConfig::default().codecs,
            audio_encryption: MediaEncryptionPolicy::default(),
            video_mode: VideoMode::Auto,
            audio_processing: AudioProcessingPolicy::default(),
        }
    }
}

impl Default for DeviceCallUiConfig {
    fn default() -> Self {
        Self {
            redial_mode: RedialMode::LastNumber,
            hinted_ringing_notification: false,
            mwi_lamp_mode: LampMode::On,
            mwi_on_call: false,
            legacy_code_page: LegacyCodePage::Iso8859_1,
        }
    }
}

impl Default for LineFeatureConfig {
    fn default() -> Self {
        Self {
            incoming_limit: 6,
            voicemail: VoicemailDefaults::default(),
            pickup: PickupConfig::default(),
            parking: LineParkingConfig::default(),
            conference: LineConferenceConfig::default(),
            hotline: LineHotlineConfig::default(),
            dial_tones: LineDialToneConfig::default(),
            mobility: LineMobilityConfig::default(),
            registration: LineRegistrationConfig::default(),
            media: LineMediaConfig::default(),
        }
    }
}
