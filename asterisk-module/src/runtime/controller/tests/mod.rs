/// Runs one pure controller transition and drops the mutex guard before
/// returning its owned result to adapter code. Adapter I/O belongs after this
/// function returns.
#[cfg(any(test, feature = "asterisk-22", feature = "asterisk-latest"))]
pub(crate) fn controller_step<T>(
    controller: &Mutex<Controller>,
    step: impl FnOnce(&mut Controller) -> T,
) -> T {
    let mut controller = controller.lock().expect("SCCP controller lock poisoned");
    step(&mut controller)
}

use super::*;
use crate::call::transfer::TransferExecutionProgress;
use crate::config::LineConfig;
use crate::media::formats::{PbxVideoFormat, negotiate_video_owned};
use sccp_protocol::{IpAddressType, ReceiveTransmit, VideoCapability, VideoLevelPreference};

fn binding() -> LineBinding {
    binding_for("SEP001122334455", 1)
}

fn assert_outbound_route(effects: &[DriverEffect], expected_destination: &str) {
    assert!(matches!(
        effects,
        [
            DriverEffect::Handset(HandsetEffect::CommitOutboundCall { info, .. }),
            DriverEffect::Backend(PbxEffect::StartRouting { destination, .. })
        ] if info.called_number == expected_destination
            && destination == expected_destination
    ));
}

fn test_media_endpoint(codec: Codec) -> MediaEndpoint {
    MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 20_000,
        rtcp_port: 20_001,
        codec,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 101,
    }
}

fn test_video_endpoint(port: u16) -> MediaEndpointAddress {
    MediaEndpointAddress {
        address: "192.0.2.30".parse().unwrap(),
        port,
    }
}

fn test_video_plan(controller: &Controller, mode: VideoMode) -> VideoPlan {
    let station = controller.registered_device(&binding().device_id).unwrap();
    let session_generation = station.session_generation;
    let protocol = station.registration.protocol;
    let capabilities = StationMediaCapabilities::new(
        Vec::new(),
        vec![VideoCapability {
            codec: Codec::H264,
            direction: ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
            level_preferences: vec![VideoLevelPreference {
                transmit_preference: 1,
                format: 4,
                max_bit_rate: 384,
                min_bit_rate: 64,
                minimum_picture_interval: 1,
                service_number: 0,
            }],
            codec_parameters: vec![64, 43, 40_500, 1_620, 8_100, 10_000],
            encryption_capability: None,
            address_type: Some(IpAddressType::Ipv4),
        }],
    );
    let negotiated = negotiate_video_owned(
        &[Codec::H264],
        capabilities,
        &[PbxVideoFormat::H264],
        ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
    )
    .unwrap();
    let payload = negotiated
        .multimedia_payload(PbxVideoFormat::H264.payload_type().unwrap())
        .unwrap();
    VideoPlan {
        session_generation,
        protocol,
        mode,
        negotiated,
        payload,
        local_endpoint: test_video_endpoint(30_000),
    }
}

fn forwarding(value: &str) -> ForwardingDestination {
    ForwardingDestination::new(value).unwrap()
}

fn voicemail_target(value: &str) -> VoicemailTarget {
    VoicemailTarget::new("from-sccp", value).unwrap()
}

pub(super) fn binding_for(device: &str, line_instance: u32) -> LineBinding {
    binding_with_ring(device, line_instance, AppearanceRingMode::Normal)
}

fn binding_with_ring(
    device: &str,
    line_instance: u32,
    ring_mode: AppearanceRingMode,
) -> LineBinding {
    let mut binding = LineBinding {
        device_id: DeviceId::new(device).unwrap(),
        line_instance,
        appearance: sccp_protocol::LineAppearance::new(
            line_instance,
            sccp_protocol::LineDefinition {
                number: "1001".into(),
                display_name: "Desk".into(),
            },
        ),
        line: LineConfig {
            number: "1001".into(),
            label: "Desk".into(),
            context: "from-sccp".into(),
            caller_name: "Desk".into(),
            caller_number: "1001".into(),
            mailbox: None,
            language: "en".into(),
            account_code: None,
            channel_variables: Vec::new(),
        },
    };
    binding.appearance.ring_mode = ring_mode;
    binding
}

fn registration() -> DeviceRegistration {
    registration_for("SEP001122334455")
}

fn registration_for(device: &str) -> DeviceRegistration {
    DeviceRegistration {
        id: DeviceId::new(device).unwrap(),
        peer: "192.0.2.10:2000".parse().unwrap(),
        transport: sccp_protocol::StationTransport::Clear,
        reported_address: Some("192.0.2.10".parse().unwrap()),
        reported_ipv6_address: None,
        device_type: sccp_protocol::DeviceType::Cisco7962,
        protocol: sccp_protocol::ProtocolVersion::V22,
        firmware: "SCCP-test".into(),
    }
}

pub(super) fn shared_inbound_controller() -> Controller {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration_for("SEP001122334455"));
    controller.registered(registration_for("SEP112233445566"));
    assert_eq!(
        controller
            .offer_inbound_call(
                PbxCallId(8),
                [
                    InboundAppearance {
                        call_id: CallId(2),
                        binding: binding_for("SEP001122334455", 1),
                        codec: Codec::Pcma,
                    },
                    InboundAppearance {
                        call_id: CallId(3),
                        binding: binding_for("SEP112233445566", 2),
                        codec: Codec::Pcmu,
                    },
                ],
            )
            .len(),
        2
    );
    controller
}

fn enable_barge_capabilities(controller: &mut Controller, device: &str, codec: Codec) {
    controller.capabilities(
        &DeviceId::new(device).unwrap(),
        vec![MediaCapability {
            codec,
            max_packet_ms: 80,
            codec_parameters: [0; 8],
        }],
    );
}

#[derive(Default)]
struct FakeHandsets {
    effects: Vec<HandsetEffect>,
    announcements: Vec<ConferenceAnnouncementOperation>,
}

impl FakeHandsets {
    fn apply(&mut self, effects: &[DriverEffect]) {
        for effect in effects {
            match effect {
                DriverEffect::Handset(effect) => self.effects.push(effect.clone()),
                DriverEffect::Backend(PbxEffect::ConferenceAnnouncement { operation }) => {
                    self.announcements.push(operation.clone());
                }
                DriverEffect::Backend(_) => {}
            }
        }
    }

    fn media_winners(&self) -> Vec<CallId> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                HandsetEffect::BeginMedia { call_id, .. }
                | HandsetEffect::BeginAnswerMedia { call_id, .. } => Some(*call_id),
                _ => None,
            })
            .collect()
    }

    fn call_states(&self) -> Vec<(CallId, HandsetCallState, bool)> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                HandsetEffect::SetCallState {
                    call_id,
                    state,
                    stop_media,
                    ..
                } => Some((*call_id, *state, *stop_media)),
                _ => None,
            })
            .collect()
    }

    fn call_info(&self, call_id: CallId) -> Vec<CallInfo> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                HandsetEffect::SetCallInfo {
                    call_id: actual,
                    info,
                    ..
                } if *actual == call_id => Some(info.clone()),
                _ => None,
            })
            .collect()
    }

    fn tones(&self, call_id: CallId) -> Vec<Tone> {
        self.effects
            .iter()
            .filter_map(|effect| match effect {
                HandsetEffect::StartTone {
                    call_id: actual,
                    tone,
                    ..
                } if *actual == call_id => Some(*tone),
                _ => None,
            })
            .collect()
    }

    fn announcements(
        &self,
    ) -> Vec<(
        ConferenceId,
        Vec<ParticipantId>,
        Vec<PbxCallId>,
        ConferenceAnnouncement,
    )> {
        self.announcements
            .iter()
            .map(|operation| {
                (
                    operation.conference_id,
                    operation
                        .targets
                        .iter()
                        .map(|target| target.participant_id)
                        .collect(),
                    operation
                        .targets
                        .iter()
                        .map(|target| target.call_id)
                        .collect(),
                    operation.announcement,
                )
            })
            .collect()
    }

    fn clear(&mut self) {
        self.effects.clear();
        self.announcements.clear();
    }
}

#[test]
fn adapter_callback_steps_release_the_controller_before_external_work() {
    let controller = Mutex::new(Controller::new(Duration::from_secs(1)));
    let observed = Mutex::new(Vec::new());
    let probe = |phase: &'static str| {
        assert!(
            controller.try_lock().is_ok(),
            "{phase} external work observed a held controller lock"
        );
        observed.lock().unwrap().push(phase);
    };

    controller_step(&controller, |controller| {
        controller.registered(registration_for("SEP001122334455"));
        controller.registered(registration_for("SEP112233445566"));
    });
    probe("registration");
    probe("blf callback");

    controller_step(&controller, |controller| {
        controller.begin_phone_call(
            CallId(10),
            binding_for("SEP001122334455", 1),
            Codec::Pcmu,
            Instant::now(),
        )
    });
    probe("phone event");
    controller_step(&controller, |controller| controller.hangup(CallId(10)));
    probe("phone effect execution");

    controller_step(&controller, |controller| {
        controller.disconnected(&DeviceId::new("SEP001122334455").unwrap())
    });
    probe("disconnect");
    controller_step(&controller, |controller| {
        controller.registered(registration_for("SEP001122334455"))
    });

    let offers = controller_step(&controller, |controller| {
        controller.offer_inbound_call(
            PbxCallId(20),
            [
                InboundAppearance {
                    call_id: CallId(21),
                    binding: binding_for("SEP001122334455", 1),
                    codec: Codec::Pcmu,
                },
                InboundAppearance {
                    call_id: CallId(22),
                    binding: binding_for("SEP112233445566", 2),
                    codec: Codec::Pcmu,
                },
            ],
        )
    });
    assert_eq!(offers.len(), 2);
    probe("inbound request and fanout");

    controller_step(&controller, |controller| {
        controller.pbx_answer(PbxCallId(20))
    });
    probe("PBX indication");
    controller_step(&controller, |controller| {
        controller.pbx_hangup_with_effects(PbxCallId(20))
    });
    probe("PBX hangup");

    // Reload does not enter the controller at all; the probe guards that
    // its phone reconfiguration and subscription work starts unlocked.
    probe("reload");
    let recording_callback = || probe("recording callback");
    recording_callback();

    assert_eq!(
        *observed.lock().unwrap(),
        [
            "registration",
            "blf callback",
            "phone event",
            "phone effect execution",
            "disconnect",
            "inbound request and fanout",
            "PBX indication",
            "PBX hangup",
            "reload",
            "recording callback",
        ]
    );
}

#[test]
fn asterisk_adapter_uses_typed_controller_operations_and_snapshots() {
    let source = concat!(
        include_str!("../../../asterisk/mod.rs"),
        include_str!("../../../asterisk/runtime/management.rs"),
        include_str!("../../../asterisk/runtime/lifecycle.rs"),
        include_str!("../../../asterisk/runtime/services.rs"),
        include_str!("../../../asterisk/phone/calls.rs"),
        include_str!("../../../asterisk/phone/parking.rs"),
        include_str!("../../../asterisk/phone/features.rs"),
        include_str!("../../../asterisk/phone/conference.rs"),
        include_str!("../../../asterisk/runtime/backend.rs"),
        include_str!("../../../asterisk/runtime/channel.rs"),
        include_str!("../../../asterisk/runtime/media.rs"),
        include_str!("../../../asterisk/runtime/presence.rs"),
        include_str!("../../../asterisk/runtime/native_support.rs"),
        include_str!("../../../asterisk/exports.rs"),
    );
    let compact: String = source
        .chars()
        .filter(|character| !character.is_whitespace())
        .collect();

    assert!(!compact.contains("controller_step("));
    assert!(
        compact.contains("pubcontroller:crate::runtime::controller::ownership::ControllerHandle")
    );
    assert!(!compact.contains("Mutex<Controller>"));
    assert!(
        !compact.contains(".controller.lock("),
        "adapter code acquired controller state instead of using its owner"
    );
}

#[test]
fn phone_call_collects_digits_then_starts_dialplan() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    let actions = controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    assert!(matches!(
        actions[0],
        DriverEffect::Backend(PbxEffect::CreateChannel { .. })
    ));
    assert_eq!(actions.len(), 1);
    assert!(
        controller
            .digit(CallId(7), Digit::Number(1), now)
            .is_empty()
    );
    assert!(
        controller
            .digit(CallId(7), Digit::Number(2), now)
            .is_empty()
    );
    let actions = controller.digit(CallId(7), Digit::Pound, now);
    assert_outbound_route(&actions, "12");
    assert_eq!(controller.pbx_call(PbxCallId(1)).unwrap().digits, "12");
}

#[test]
fn speed_dial_immediate_and_awaiting_modes_have_distinct_commit_boundaries() {
    let now = Instant::now();

    let mut immediate = Controller::new(Duration::from_secs(5));
    immediate.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    assert_outbound_route(
        &immediate.speed_dial(CallId(7), "2001".into(), false, now),
        "2001",
    );

    let mut awaiting = Controller::new(Duration::from_secs(5));
    awaiting.begin_phone_call(CallId(8), binding(), Codec::Pcmu, now);
    assert!(
        awaiting
            .speed_dial(CallId(8), "300".into(), true, now)
            .is_empty()
    );
    let call = awaiting.pbx_call(PbxCallId(1)).unwrap();
    assert_eq!(call.digits, "300");
    assert_eq!(call.digit_deadline, Some(now + Duration::from_secs(5)));
    assert!(!call.simulated_enbloc_eligible);

    assert!(
        awaiting
            .digit(CallId(8), Digit::Number(1), now + Duration::from_secs(1))
            .is_empty()
    );
    assert_eq!(awaiting.pbx_call(PbxCallId(1)).unwrap().digits, "3001");
    assert_outbound_route(
        &awaiting.expire_digits(now + Duration::from_secs(7)),
        "3001",
    );
}

#[test]
fn awaiting_speed_dial_honors_the_configured_terminator() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(5));
    controller.set_dial_terminator('*');
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

    assert_outbound_route(
        &controller.speed_dial(CallId(7), "2001*".into(), true, now),
        "2001",
    );
}

#[test]
fn configured_conference_destination_commits_one_typed_application_effect() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

    let effects = controller
        .begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(7),
            destination: "700".into(),
            application_options: "Mac".into(),
        })
        .unwrap();
    assert!(matches!(
        effects.as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::SetCallInfo {
                call_id: CallId(7),
                info,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::StartTone {
                call_id: CallId(7),
                tone: Tone::Silence,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(7),
                state: HandsetCallState::Proceed,
                stop_media: false,
                ..
            }),
            DriverEffect::Backend(PbxEffect::StartConferenceDestination {
                operation: ConferenceDestinationOperation {
                    call_id: PbxCallId(1),
                    destination,
                    application_options,
                    ..
                },
            }),
        ] if info.called_name == "Conference"
            && info.called_number == "700"
            && destination == "700"
            && application_options == "Mac"
    ));
    assert_eq!(
        controller.call(CallId(7)).unwrap().state,
        CallState::Calling
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn conference_mutation_tokens_reject_stale_and_repeated_completion() {
    let mut first = active_three_party_conference();
    let conference_id = first.conference_session(CallId(4)).unwrap().id;
    let token = first.claim_conference_mutation(CallId(4)).unwrap();
    assert!(first.conference_mutation_is_active(token));
    assert!(first.pbx_hangup_with_effects(PbxCallId(10)).is_some());
    assert!(!first.conference_mutation_is_active(token));
    assert!(!first.complete_conference_mutation(token));

    let mut second = active_three_party_conference();
    let second_id = second.conference_session(CallId(4)).unwrap().id;
    let second_token = second.claim_conference_mutation_by_id(second_id).unwrap();
    assert!(second.conference_mutation_is_active(second_token));
    assert!(second.complete_conference_mutation(second_token));
    assert!(!second.complete_conference_mutation(second_token));
    assert_eq!(conference_id, second_id);
}

#[test]
fn conference_destination_holds_an_ordinary_call_and_rejects_reentry() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    controller.enbloc(CallId(7), "2000".into());
    controller.pbx_answer(PbxCallId(1));
    controller.begin_phone_call(CallId(8), binding(), Codec::Pcmu, now);

    let effects = controller
        .begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(8),
            destination: "701".into(),
            application_options: String::new(),
        })
        .unwrap();
    let mutation = controller
        .conference_destination_mutation(CallId(8))
        .unwrap();
    assert!(matches!(
        effects.first(),
        Some(DriverEffect::Backend(PbxEffect::Hold {
            call_id: PbxCallId(1)
        }))
    ));
    assert_eq!(controller.call(CallId(7)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(8)).unwrap().state,
        CallState::Calling
    );
    assert_eq!(
        controller.begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(8),
            destination: "702".into(),
            application_options: "Mac".into(),
        }),
        Err(ConferenceDestinationRejection::Conflict)
    );
    assert_eq!(
        controller.call(CallId(8)).unwrap().info.called_number,
        "701"
    );
    let rollback = controller.conference_destination_failed(
        mutation,
        CallId(8),
        &[PbxCallId(1)],
        &[PbxCallId(1)],
    );
    assert!(matches!(
        rollback.first(),
        Some(DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(2)
        }))
    ));
    assert!(rollback.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Resume {
            call_id: PbxCallId(1)
        })
    )));
    assert!(controller.call(CallId(8)).is_none());
    assert_eq!(
        controller.call(CallId(7)).unwrap().state,
        CallState::Connected
    );
    assert!(
        controller
            .conference_destination_failed(mutation, CallId(8), &[PbxCallId(1)], &[PbxCallId(1)],)
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn conference_destination_rejects_missing_or_non_collecting_calls_without_mutation() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    let before_pbx_id = controller.call(CallId(7)).unwrap().pbx_id;
    assert_eq!(
        controller.begin_conference_destination(ConferenceDestinationRequest {
            device_id: DeviceId::new("SEP112233445566").unwrap(),
            handset_call_id: CallId(7),
            destination: "700".into(),
            application_options: "Mac".into(),
        }),
        Err(ConferenceDestinationRejection::Unavailable)
    );
    let unchanged = controller.call(CallId(7)).unwrap();
    assert_eq!(unchanged.pbx_id, before_pbx_id);
    assert_eq!(unchanged.state, CallState::Collecting);
    assert!(unchanged.digits.is_empty());
    assert_eq!(
        controller.begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(7),
            destination: String::new(),
            application_options: "Mac".into(),
        }),
        Err(ConferenceDestinationRejection::Unavailable)
    );
    controller.digit(CallId(7), Digit::Number(1), now);
    assert_eq!(
        controller.begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(7),
            destination: "700".into(),
            application_options: "Mac".into(),
        }),
        Err(ConferenceDestinationRejection::Conflict)
    );
    assert_eq!(controller.call(CallId(7)).unwrap().digits, "1");
    assert_eq!(
        controller.call(CallId(7)).unwrap().state,
        CallState::Collecting
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn conference_destination_failed_hold_restores_state_without_an_unexecuted_resume() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    controller.enbloc(CallId(7), "2000".into());
    controller.pbx_answer(PbxCallId(1));
    controller.begin_phone_call(CallId(8), binding(), Codec::Pcmu, now);
    controller
        .begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(8),
            destination: "700".into(),
            application_options: "Mac".into(),
        })
        .unwrap();
    let mutation = controller
        .conference_destination_mutation(CallId(8))
        .unwrap();

    let rollback =
        controller.conference_destination_failed(mutation, CallId(8), &[PbxCallId(1)], &[]);
    assert!(
        rollback
            .iter()
            .all(|effect| !matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
    );
    assert!(matches!(
        rollback.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(2)
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(8),
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            })
        ]
    ));
    assert_eq!(
        controller.call(CallId(7)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.call(CallId(8)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn configured_initial_and_secondary_dial_tones_follow_exact_prefixes() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.set_line_dial_tones([(
        "1001".into(),
        LineDialToneConfig {
            initial: Tone::RecallDial,
            secondary_prefix: Some("9".into()),
            secondary: Tone::OutsideDial,
        },
    )]);

    let effects = controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    assert!(matches!(
        effects.as_slice(),
        [DriverEffect::Backend(PbxEffect::CreateChannel { .. })]
    ));

    assert_eq!(
        controller.digit(CallId(7), Digit::Number(9), now),
        [DriverEffect::Handset(HandsetEffect::StartTone {
            device_id: binding().device_id,
            call_id: CallId(7),
            tone: Tone::OutsideDial,
        })]
    );
    assert!(
        controller
            .digit(CallId(7), Digit::Number(1), now)
            .is_empty(),
        "the secondary tone must only start on the exact configured prefix"
    );
}

#[test]
fn configured_dial_terminator_routes_without_entering_the_destination() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.set_dial_terminator('*');
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

    assert!(
        controller
            .digit(CallId(7), Digit::Number(1), now)
            .is_empty()
    );
    assert!(controller.digit(CallId(7), Digit::Pound, now).is_empty());
    assert_outbound_route(&controller.digit(CallId(7), Digit::Star, now), "1#");
}

#[test]
fn first_digit_deadline_is_independent_from_subsequent_digits() {
    let now = Instant::now();
    let mut controller =
        Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(2));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    assert_eq!(
        controller.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
        Some(now + Duration::from_secs(10))
    );
    assert!(
        controller
            .expire_digits(now + Duration::from_secs(9))
            .is_empty()
    );

    controller.digit(CallId(7), Digit::Number(1), now + Duration::from_secs(9));
    assert_eq!(
        controller.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
        Some(now + Duration::from_secs(11))
    );
    assert!(
        controller
            .expire_digits(now + Duration::from_secs(10))
            .is_empty()
    );
    assert_outbound_route(
        &controller.expire_digits(now + Duration::from_secs(12)),
        "1",
    );
}

#[test]
fn simulated_enbloc_accelerates_fast_keypad_entry_but_not_slow_entry() {
    let now = Instant::now();
    let mut fast = Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(5));
    fast.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    for (index, digit) in [1, 2, 3, 4].into_iter().enumerate() {
        fast.digit(
            CallId(7),
            Digit::Number(digit),
            now + Duration::from_millis(index as u64 * 100),
        );
    }
    assert_eq!(
        fast.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
        Some(now + Duration::from_millis(2_300))
    );

    let mut slow = Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(5));
    slow.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    slow.digit(CallId(7), Digit::Number(1), now);
    slow.digit(
        CallId(7),
        Digit::Number(2),
        now + Duration::from_millis(500),
    );
    slow.digit(
        CallId(7),
        Digit::Number(3),
        now + Duration::from_millis(600),
    );
    slow.digit(
        CallId(7),
        Digit::Number(4),
        now + Duration::from_millis(700),
    );
    assert_eq!(
        slow.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
        Some(now + Duration::from_millis(5_700))
    );
}

#[test]
fn simulated_enbloc_can_be_disabled_without_changing_direct_enbloc_routing() {
    let now = Instant::now();
    let mut controller =
        Controller::with_digit_timeouts(Duration::from_secs(10), Duration::from_secs(5));
    controller.set_simulated_enbloc(false);
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    for (index, digit) in [1, 2, 3, 4].into_iter().enumerate() {
        controller.digit(
            CallId(7),
            Digit::Number(digit),
            now + Duration::from_millis(index as u64 * 100),
        );
    }
    assert_eq!(
        controller.pbx_call(PbxCallId(1)).unwrap().digit_deadline,
        Some(now + Duration::from_millis(5_300))
    );
    assert_outbound_route(&controller.enbloc(CallId(7), "8675309".into()), "8675309");
}

#[test]
fn explicit_overlap_starts_on_the_first_digit_and_forwards_the_remainder() {
    let now = Instant::now();
    let binding = binding();
    let mut controller = Controller::new(Duration::from_secs(5));
    controller.set_overlap_devices([binding.device_id.clone()]);
    controller.begin_phone_call(CallId(7), binding, Codec::Pcmu, now);

    assert_outbound_route(&controller.digit(CallId(7), Digit::Number(1), now), "1");
    assert_eq!(
        controller.digit(CallId(7), Digit::Number(2), now),
        [DriverEffect::Backend(PbxEffect::SendDigit {
            call_id: PbxCallId(1),
            digit: '2',
        })]
    );
    assert_eq!(
        controller.digit(CallId(7), Digit::Pound, now),
        [DriverEffect::Backend(PbxEffect::SendDigit {
            call_id: PbxCallId(1),
            digit: '#',
        })]
    );
    assert_eq!(
        controller.pbx_call(PbxCallId(1)).unwrap().state,
        CallState::Calling
    );
    assert_eq!(controller.pbx_call(PbxCallId(1)).unwrap().digits, "1");
}

#[test]
fn overlap_disabled_keeps_collecting_until_an_explicit_completion() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(5));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

    assert!(
        controller
            .digit(CallId(7), Digit::Number(1), now)
            .is_empty()
    );
    assert_eq!(
        controller.pbx_call(PbxCallId(1)).unwrap().state,
        CallState::Collecting
    );
}

#[test]
fn pre_dial_codec_change_is_guarded_and_updates_the_snapshot() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);

    assert_eq!(
        controller.set_pre_dial_codec(PbxCallId(1), Codec::G72264k),
        Ok(Codec::Pcmu)
    );
    assert_eq!(controller.call(CallId(7)).unwrap().codec, Codec::G72264k);

    controller.digit(CallId(7), Digit::Number(1), now);
    controller.digit(CallId(7), Digit::Pound, now);
    assert_eq!(
        controller.set_pre_dial_codec(PbxCallId(1), Codec::Pcma),
        Ok(Codec::G72264k)
    );
    assert_eq!(controller.call(CallId(7)).unwrap().codec, Codec::Pcma);

    controller.pbx_progress(PbxCallId(1), true);
    assert_eq!(
        controller.set_pre_dial_codec(PbxCallId(1), Codec::Pcmu),
        Err(CodecPreferenceRejection::NotPreDial)
    );
    assert_eq!(controller.call(CallId(7)).unwrap().codec, Codec::Pcma);
    assert_eq!(
        controller.set_pre_dial_codec(PbxCallId(999), Codec::Pcmu),
        Err(CodecPreferenceRejection::Unavailable)
    );
}

#[test]
fn held_codec_change_is_limited_to_the_closed_active_appearance() {
    let mut controller = connected_outbound_controller();

    assert_eq!(
        controller.set_held_codec(PbxCallId(1), CallId(1), Codec::Wideband256k),
        None
    );
    controller.hold(CallId(1));
    assert_eq!(
        controller.set_held_codec(PbxCallId(1), CallId(1), Codec::Wideband256k),
        Some(Codec::Pcmu)
    );
    assert_eq!(
        controller.call(CallId(1)).unwrap().codec,
        Codec::Wideband256k
    );
    assert!(matches!(
        controller.resume(CallId(1)).last(),
        Some(DriverEffect::Handset(HandsetEffect::BeginMedia {
            codec: Codec::Wideband256k,
            ..
        }))
    ));
    assert_eq!(
        controller.set_held_codec(PbxCallId(1), CallId(1), Codec::Pcma),
        None
    );
    assert_eq!(
        controller.set_held_codec(PbxCallId(99), CallId(1), Codec::Pcma),
        None
    );
}

#[test]
fn call_snapshots_are_derived_from_current_call_and_appearance_state() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    let before = controller.call(CallId(7)).unwrap();

    assert!(
        controller
            .digit(CallId(7), Digit::Number(4), now)
            .is_empty()
    );
    let mut info = controller.call_info(CallId(7)).unwrap().clone();
    info.called_name = "Destination".into();
    info.called_number = "4001".into();
    controller.set_call_info(CallId(7), info.clone());
    let metadata = CallMetadata {
        account_code: Some("sales".into()),
        ..CallMetadata::default()
    };
    assert_eq!(
        controller.set_call_metadata(PbxCallId(1), metadata.clone()),
        Ok(true)
    );
    assert_eq!(
        controller.set_pre_dial_codec(PbxCallId(1), Codec::G72264k),
        Ok(Codec::Pcmu)
    );

    let after = controller.call(CallId(7)).unwrap();
    assert_eq!(before.digits, "");
    assert_eq!(before.info.called_number, "");
    assert!(before.metadata.account_code.is_none());
    assert_eq!(before.codec, Codec::Pcmu);
    assert_eq!(after.digits, "4");
    assert_eq!(after.info, info);
    assert!(after.metadata == metadata);
    assert_eq!(after.codec, Codec::G72264k);

    let by_pbx = controller.primary_call_by_pbx(PbxCallId(1)).unwrap();
    assert_eq!(by_pbx.digits, after.digits);
    assert_eq!(by_pbx.info, after.info);
    assert!(by_pbx.metadata == after.metadata);
    assert_eq!(controller.calls().next().unwrap().codec, after.codec);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn early_media_modes_are_explicit_and_answer_reuses_the_stream() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    controller.digit(CallId(7), Digit::Number(1), now);
    controller.digit(CallId(7), Digit::Pound, now);

    assert!(controller.pbx_progress(PbxCallId(1), false).is_empty());
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio,
        MediaStreamState::Closed
    );
    assert_eq!(
        controller.pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled,),
        [DriverEffect::Handset(HandsetEffect::BeginOutboundMedia {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            call_id: CallId(7),
            codec: Codec::Pcmu,
        })]
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio,
        MediaStreamState::Opening
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );
    assert!(
        controller
            .pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled,)
            .is_empty()
    );

    let endpoint = MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 20_000,
        rtcp_port: 20_001,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 101,
    };
    assert!(matches!(
        controller.media_opened(CallId(7), endpoint).as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::StartTone {
                tone: Tone::Silence,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
            DriverEffect::Backend(PbxEffect::ConfigureMediaOnly { .. })
        ]
    ));
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );
    let transmit_endpoint = MediaEndpoint {
        address: "192.0.2.21".parse().unwrap(),
        rtp_port: 20_002,
        rtcp_port: 20_003,
        ..endpoint
    };
    assert!(
        controller
            .media_transmission_started(CallId(7), transmit_endpoint)
            .is_empty()
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Open(transmit_endpoint)
    );
    let stale_endpoint = MediaEndpoint {
        rtp_port: 20_004,
        ..transmit_endpoint
    };
    controller.media_transmission_started(CallId(7), stale_endpoint);
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Open(transmit_endpoint)
    );

    assert_eq!(
        controller.media_retarget_started(CallId(7)),
        Some(transmit_endpoint)
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );
    assert!(controller.media_retarget_enqueue_failed(CallId(7), transmit_endpoint));
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Open(transmit_endpoint)
    );

    assert_eq!(
        controller.media_retarget_started(CallId(7)),
        Some(transmit_endpoint)
    );
    let retargeted_endpoint = MediaEndpoint {
        address: "192.0.2.22".parse().unwrap(),
        rtp_port: 20_006,
        rtcp_port: 20_007,
        ..transmit_endpoint
    };
    controller.media_transmission_started(CallId(7), retargeted_endpoint);
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Open(retargeted_endpoint)
    );
    let previous = controller
        .media_retarget_compensation_started(CallId(7))
        .unwrap();
    assert_eq!(previous, MediaStreamState::Open(retargeted_endpoint));
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );
    assert!(controller.media_retarget_compensation_enqueue_failed(CallId(7), previous));
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Open(retargeted_endpoint)
    );
    assert!(!controller.media_retarget_enqueue_failed(CallId(7), transmit_endpoint));
    assert_eq!(
        controller.pbx_answer(PbxCallId(1)),
        [DriverEffect::Handset(HandsetEffect::SetCallState {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            call_id: CallId(7),
            state: HandsetCallState::Connected,
            stop_media: false,
        })]
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio,
        MediaStreamState::Open(endpoint)
    );
    assert!(controller.pbx_progress(PbxCallId(1), true).is_empty());
    assert!(controller.invariant_error().is_none());

    let mut answer_race = Controller::new(Duration::from_secs(1));
    answer_race.begin_phone_call(CallId(8), binding(), Codec::Pcmu, now);
    answer_race.digit(CallId(8), Digit::Number(2), now);
    answer_race.digit(CallId(8), Digit::Pound, now);
    answer_race.pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled);
    assert_eq!(
        answer_race.pbx_answer(PbxCallId(1)),
        [DriverEffect::Handset(HandsetEffect::SetCallState {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            call_id: CallId(8),
            state: HandsetCallState::Connected,
            stop_media: false,
        })]
    );
    assert!(answer_race.pbx_answer(PbxCallId(1)).is_empty());
    assert_eq!(
        answer_race.call(CallId(8)).unwrap().audio,
        MediaStreamState::Opening
    );
    assert_eq!(
        answer_race.call(CallId(8)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );
    assert!(matches!(
        answer_race.media_opened(CallId(8), endpoint).as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::StartTone {
                tone: Tone::Silence,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
            DriverEffect::Backend(PbxEffect::ConfigureMediaOnly { .. })
        ]
    ));
    assert!(answer_race.invariant_error().is_none());

    let mut no_early_media = Controller::new(Duration::from_secs(1));
    no_early_media.begin_phone_call(CallId(9), binding(), Codec::Pcmu, now);
    no_early_media.digit(CallId(9), Digit::Number(2), now);
    no_early_media.digit(CallId(9), Digit::Pound, now);
    assert!(matches!(
        no_early_media.pbx_answer(PbxCallId(1)).as_slice(),
        [DriverEffect::Handset(HandsetEffect::BeginMedia {
            call_id: CallId(9),
            ..
        })]
    ));
    assert!(matches!(
        no_early_media.media_opened(CallId(9), endpoint).as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
            DriverEffect::Backend(PbxEffect::ConfigureMedia { .. })
        ]
    ));
    assert!(no_early_media.invariant_error().is_none());

    let mut staged = Controller::new(Duration::from_secs(1));
    staged.begin_phone_call(CallId(10), binding(), Codec::Pcmu, now);
    staged.digit(CallId(10), Digit::Number(2), now);
    staged.digit(CallId(10), Digit::Pound, now);
    assert!(matches!(
        staged
            .pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Staged,)
            .as_slice(),
        [DriverEffect::Handset(HandsetEffect::BeginEarlyMedia {
            call_id: CallId(10),
            ..
        })]
    ));
    assert_eq!(
        staged.call(CallId(10)).unwrap().audio_transmit,
        MediaStreamState::Closed
    );
    assert!(matches!(
        staged.pbx_answer(PbxCallId(1)).as_slice(),
        [DriverEffect::Handset(HandsetEffect::SetCallState {
            state: HandsetCallState::Connected,
            ..
        })]
    ));
    assert!(matches!(
        staged.media_opened(CallId(10), endpoint).as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
            DriverEffect::Backend(PbxEffect::ConfigureMedia { .. })
        ]
    ));
    assert_eq!(
        staged.call(CallId(10)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );

    let mut staged_answer = Controller::new(Duration::from_secs(1));
    staged_answer.begin_phone_call(CallId(11), binding(), Codec::Pcmu, now);
    staged_answer.digit(CallId(11), Digit::Number(2), now);
    staged_answer.digit(CallId(11), Digit::Pound, now);
    assert!(matches!(
        staged_answer.pbx_answer(PbxCallId(1)).as_slice(),
        [DriverEffect::Handset(HandsetEffect::BeginMedia {
            call_id: CallId(11),
            ..
        })]
    ));
    assert!(!staged_answer.coupled_outbound_media_pending(CallId(11)));
    let wrong_device = DeviceId::new("SEP112233445566").unwrap();
    assert!(
        staged_answer
            .media_opened_for_device(&wrong_device, CallId(11), endpoint)
            .is_empty()
    );
    assert_eq!(
        staged_answer.call(CallId(11)).unwrap().audio,
        MediaStreamState::Opening
    );
    let owner = binding().device_id;
    assert!(matches!(
        staged_answer
            .media_opened_for_device(&owner, CallId(11), endpoint)
            .as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
            DriverEffect::Backend(PbxEffect::ConfigureMedia { .. })
        ]
    ));
    assert!(
        staged_answer
            .media_transmission_started_for_device(&wrong_device, CallId(11), endpoint)
            .is_empty()
    );
    assert_eq!(
        staged_answer.call(CallId(11)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );
    staged_answer.media_transmission_started_for_device(&owner, CallId(11), endpoint);
    assert_eq!(
        staged_answer.call(CallId(11)).unwrap().audio_transmit,
        MediaStreamState::Open(endpoint)
    );
    assert!(staged_answer.invariant_error().is_none());
}

#[test]
fn coupled_media_keeps_an_explicit_transmit_ack_open_when_receive_ack_arrives_later() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    controller.digit(CallId(7), Digit::Number(2), now);
    controller.digit(CallId(7), Digit::Pound, now);
    assert!(matches!(
        controller
            .pbx_progress_with_media_mode(PbxCallId(1), true, OutboundMediaMode::Coupled)
            .as_slice(),
        [DriverEffect::Handset(
            HandsetEffect::BeginOutboundMedia { .. }
        )]
    ));

    let transmit_endpoint = MediaEndpoint {
        address: "192.0.2.21".parse().unwrap(),
        rtp_port: 20_002,
        rtcp_port: 20_003,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 101,
    };
    assert!(
        controller
            .media_transmission_started(CallId(7), transmit_endpoint)
            .is_empty()
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Open(transmit_endpoint)
    );

    let receive_endpoint = MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 20_000,
        rtcp_port: 20_001,
        ..transmit_endpoint
    };
    assert!(matches!(
        controller
            .media_opened(CallId(7), receive_endpoint)
            .as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::StartTone {
                tone: Tone::Silence,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::SetCallInfo { .. }),
            DriverEffect::Backend(PbxEffect::ConfigureMediaOnly { .. })
        ]
    ));
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio,
        MediaStreamState::Open(receive_endpoint)
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().audio_transmit,
        MediaStreamState::Open(transmit_endpoint)
    );
    assert!(!controller.coupled_outbound_media_pending(CallId(7)));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn outbound_signalling_advances_monotonically_without_regressing_proceed() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    assert_outbound_route(&controller.enbloc(CallId(7), "2200".into()), "2200");

    assert!(matches!(
        controller.pbx_proceeding(PbxCallId(1)).as_slice(),
        [DriverEffect::Handset(
            HandsetEffect::PresentOutboundProceeding { info, .. }
        )] if info.called_number == "2200"
    ));
    controller.update_call_info_by_pbx(PbxCallId(1), |info| {
        let mut info = info.clone();
        info.called_name = "Remote Party".into();
        info
    });
    assert!(
        controller
            .pbx_remote_identity_ready(PbxCallId(1))
            .is_empty()
    );
    assert!(matches!(
        controller.pbx_ringing(PbxCallId(1)).as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::PresentOutboundRinging { info, .. }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::RingOut,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::SetCallInfo { .. })
        ] if info.called_name == "Remote Party" && info.called_number == "2200"
    ));
    assert!(controller.pbx_ringing(PbxCallId(1)).is_empty());
    assert!(controller.pbx_proceeding(PbxCallId(1)).is_empty());

    assert!(
        controller
            .pbx_remote_identity_ready(PbxCallId(1))
            .is_empty()
    );

    assert!(controller.pbx_progress(PbxCallId(1), false).is_empty());
    assert!(controller.pbx_ringing(PbxCallId(1)).is_empty());
    assert!(controller.pbx_proceeding(PbxCallId(1)).is_empty());
    assert!(matches!(
        controller.pbx_answer(PbxCallId(1)).as_slice(),
        [DriverEffect::Handset(HandsetEffect::BeginMedia {
            call_id: CallId(7),
            ..
        })]
    ));
    assert!(controller.pbx_answer(PbxCallId(1)).is_empty());
    assert!(controller.pbx_progress(PbxCallId(1), true).is_empty());
    assert!(controller.pbx_ringing(PbxCallId(1)).is_empty());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn outbound_remote_identity_cannot_regress_progress_to_ring_out() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    controller.enbloc(CallId(7), "2200".into());
    controller.pbx_ringing(PbxCallId(1));
    controller.pbx_progress(PbxCallId(1), false);

    assert!(
        controller
            .pbx_remote_identity_ready(PbxCallId(1))
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn connected_digits_are_forwarded_without_collection() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(2));
    assert_eq!(
        controller.digit(CallId(2), Digit::Number(5), Instant::now()),
        [DriverEffect::Backend(PbxEffect::SendDigit {
            call_id: PbxCallId(8),
            digit: '5'
        })]
    );
    assert_eq!(
        controller.enbloc(CallId(2), "12#d".into()),
        [
            DriverEffect::Backend(PbxEffect::SendDigit {
                call_id: PbxCallId(8),
                digit: '1',
            }),
            DriverEffect::Backend(PbxEffect::SendDigit {
                call_id: PbxCallId(8),
                digit: '2',
            }),
            DriverEffect::Backend(PbxEffect::SendDigit {
                call_id: PbxCallId(8),
                digit: '#',
            }),
            DriverEffect::Backend(PbxEffect::SendDigit {
                call_id: PbxCallId(8),
                digit: 'D',
            }),
        ]
    );
    assert!(controller.enbloc(CallId(2), "12x".into()).is_empty());
    assert_eq!(
        controller.pbx_call(PbxCallId(8)).unwrap().state,
        CallState::Connected,
        "connected en-bloc DTMF must not restart dialplan routing"
    );
}

#[test]
fn party_updates_fan_out_in_appearance_order_and_preserve_local_identity() {
    let mut controller = shared_inbound_controller();
    let effects = controller.update_call_info_by_pbx(PbxCallId(8), |current| {
        let mut info = current.clone();
        info.calling_name = "Updated caller".into();
        info.calling_number = "2100".into();
        info.original_called_number = "2000".into();
        info.last_redirecting_number = "2050".into();
        info.last_redirect_reason = 4;
        info
    });

    assert!(matches!(
        effects.as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::SetCallInfo {
                call_id: CallId(2),
                ..
            }),
            DriverEffect::Handset(HandsetEffect::SetCallInfo {
                call_id: CallId(3),
                ..
            })
        ]
    ));
    for call_id in [CallId(2), CallId(3)] {
        let info = controller.call_info(call_id).unwrap();
        assert_eq!(info.calling_name, "Updated caller");
        assert_eq!(info.calling_number, "2100");
        assert_eq!(info.called_name, "Desk");
        assert_eq!(info.called_number, "1001");
        assert_eq!(info.original_called_number, "2000");
        assert_eq!(info.last_redirecting_number, "2050");
        assert_eq!(info.last_redirect_reason, 4);
    }
    assert!(controller.invariant_error().is_none());
}

#[test]
fn group_pickup_requires_permission_and_serializes_one_attempt() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, Instant::now());

    assert_eq!(
        controller.group_pickup(CallId(7), false, true),
        Err(PickupRejection::Permission)
    );
    assert_eq!(
        controller.group_pickup(CallId(7), true, true).unwrap(),
        [DriverEffect::Backend(PbxEffect::Pickup {
            operation: PickupOperation::Group {
                call_id: PbxCallId(1),
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                handset_call_id: CallId(7),
                codec: Codec::Pcmu,
                answer: true,
            },
        })]
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(
        controller.group_pickup(CallId(7), true, true),
        Err(PickupRejection::Conflict)
    );
    assert!(controller.invariant_error().is_none());

    let mut ringing = Controller::new(Duration::from_secs(1));
    ringing.registered(registration());
    ringing.begin_phone_call(CallId(8), binding(), Codec::Pcmu, Instant::now());
    ringing.group_pickup(CallId(8), true, false).unwrap();
    assert_eq!(ringing.call(CallId(8)).unwrap().state, CallState::Ringing);
    assert!(
        ringing
            .pbx_call(PbxCallId(1))
            .unwrap()
            .active_appearance()
            .is_none()
    );
    assert!(!ringing.phone_answer(CallId(8)).is_empty());
    assert!(ringing.invariant_error().is_none());
}

#[test]
fn parking_requires_the_active_connected_owner_and_rolls_back_cleanly() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(2));

    assert_eq!(
        controller.park(CallId(2), false, Some("executive".into())),
        Err(ParkingRejection::Disabled)
    );
    assert_eq!(
        controller
            .park(CallId(2), true, Some("executive".into()))
            .unwrap(),
        [DriverEffect::Backend(PbxEffect::Parking {
            operation: ParkingOperation::Park {
                call_id: PbxCallId(8),
                lot: Some("executive".into()),
            },
        })]
    );
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Parking
    );
    assert_eq!(
        controller.park(CallId(2), true, None),
        Err(ParkingRejection::Conflict)
    );
    assert_eq!(
        controller.parking_failed(CallId(2)),
        [DriverEffect::Handset(HandsetEffect::SetCallState {
            device_id: DeviceId::new("SEP001122334455").unwrap(),
            call_id: CallId(2),
            state: HandsetCallState::Connected,
            stop_media: false,
        })]
    );
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn assigned_parking_slot_is_published_before_owner_channel_cleanup() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(2));
    controller.park(CallId(2), true, None).unwrap();

    let effects = controller.parking_confirmed(CallId(2), 701);
    assert!(matches!(
        &effects[0],
        DriverEffect::Handset(HandsetEffect::SetCallInfo { info, .. })
            if info.called_number == "701" && info.called_name == "Parked"
    ));
    assert!(matches!(
        effects[1],
        DriverEffect::Handset(HandsetEffect::SetCallState {
            state: HandsetCallState::Park,
            ..
        })
    ));
    assert_eq!(
        effects[2],
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(8)
        })
    );
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn retrieval_has_one_call_identity_and_failure_cleans_every_index() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    let info = CallInfo {
        direction: CallDirection::Inbound,
        calling_name: "Caller".into(),
        calling_number: "2100".into(),
        called_name: "Park 701".into(),
        called_number: "701".into(),
        ..CallInfo::default()
    };

    let effects = controller
        .begin_parking_retrieval(
            CallId(22),
            binding(),
            Codec::Pcmu,
            Some("main".into()),
            701,
            info.clone(),
        )
        .unwrap();
    assert!(matches!(
        effects.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::CreateChannel { .. }),
            DriverEffect::Handset(HandsetEffect::SetCallInfo { info: actual, .. }),
            DriverEffect::Backend(PbxEffect::Parking {
                operation: ParkingOperation::Retrieve { slot, .. }
            })
        ] if actual == &info && slot == "701"
    ));
    assert_eq!(
        controller.call(CallId(22)).unwrap().state,
        CallState::Retrieving
    );
    assert_eq!(
        controller.begin_parking_retrieval(
            CallId(22),
            binding(),
            Codec::Pcmu,
            Some("main".into()),
            701,
            info,
        ),
        Err(ParkingRejection::Conflict)
    );
    let cleanup = controller.parking_retrieval_failed(CallId(22));
    assert_eq!(
        cleanup,
        [
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(1)
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: binding().device_id,
                call_id: CallId(22),
                state: HandsetCallState::OnHook,
                stop_media: true,
            })
        ]
    );
    assert!(controller.call(CallId(22)).is_none());
    assert!(controller.primary_call_by_pbx(PbxCallId(1)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn retrieval_confirmation_enters_connected_media_once() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller
        .begin_parking_retrieval(
            CallId(22),
            binding(),
            Codec::Pcma,
            None,
            701,
            CallInfo {
                direction: CallDirection::Inbound,
                calling_name: "Caller".into(),
                calling_number: "2100".into(),
                called_name: "Park 701".into(),
                called_number: "701".into(),
                ..CallInfo::default()
            },
        )
        .unwrap();

    let effects = controller.parking_retrieved(CallId(22));
    assert!(matches!(
        effects.as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::Connected,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::BeginMedia {
                codec: Codec::Pcma,
                ..
            })
        ]
    ));
    assert_eq!(
        controller.call(CallId(22)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.parking_retrieved(CallId(22)).is_empty());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn directed_pickup_collects_extension_and_preserves_context_and_answer_policy() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcma, Instant::now());

    assert_eq!(
        controller.begin_directed_pickup(CallId(7), true, false, "pickup-context".into(), false,),
        Err(PickupRejection::Disabled)
    );
    assert_eq!(
        controller.begin_directed_pickup(CallId(7), false, true, "pickup-context".into(), false,),
        Err(PickupRejection::Permission)
    );
    controller
        .begin_directed_pickup(CallId(7), true, true, "pickup-context".into(), false)
        .unwrap();
    assert_eq!(
        controller.call(CallId(7)).unwrap().state,
        CallState::PickupCollecting
    );
    assert_eq!(
        controller.begin_directed_pickup(CallId(7), true, true, "pickup-context".into(), false,),
        Err(PickupRejection::Conflict)
    );
    controller.digit(CallId(7), Digit::Number(2), Instant::now());
    controller.digit(CallId(7), Digit::Number(1), Instant::now());
    controller.digit(CallId(7), Digit::Number(0), Instant::now());
    assert_eq!(
        controller.digit(CallId(7), Digit::Pound, Instant::now()),
        [DriverEffect::Backend(PbxEffect::Pickup {
            operation: PickupOperation::Directed {
                call_id: PbxCallId(1),
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                handset_call_id: CallId(7),
                codec: Codec::Pcma,
                extension: "210".into(),
                context: "pickup-context".into(),
                answer: false,
            },
        })]
    );
    assert_eq!(
        controller.call(CallId(7)).unwrap().state,
        CallState::Ringing
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn dial_softkey_finishes_previously_collected_digits() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_phone_call(CallId(7), binding(), Codec::Pcmu, now);
    controller.digit(CallId(7), Digit::Number(1), now);
    controller.digit(CallId(7), Digit::Number(2), now);

    assert_outbound_route(&controller.enbloc(CallId(7), String::new()), "12");
}

#[test]
fn disconnect_hangs_up_device_calls() {
    let mut controller = Controller::new(Duration::from_secs(1));
    let device = binding().device_id;
    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    assert_eq!(
        controller.disconnected(&device),
        [DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(8)
        })]
    );
    assert!(controller.call(CallId(2)).is_none());
}

#[test]
fn inbound_offer_fans_out_in_order_to_registered_ringable_appearances() {
    let mut controller = Controller::new(Duration::from_secs(1));
    for device in ["SEP001122334455", "SEP112233445566", "SEP223344556677"] {
        controller.registered(registration_for(device));
    }
    let offers = controller.offer_inbound_call(
        PbxCallId(8),
        [
            InboundAppearance {
                call_id: CallId(30),
                binding: binding_with_ring("SEP112233445566", 2, AppearanceRingMode::Silent),
                codec: Codec::Pcmu,
            },
            InboundAppearance {
                call_id: CallId(20),
                binding: binding_for("SEP001122334455", 1),
                codec: Codec::Pcma,
            },
            InboundAppearance {
                call_id: CallId(40),
                binding: binding_with_ring("SEP223344556677", 3, AppearanceRingMode::Disabled),
                codec: Codec::G72264k,
            },
            InboundAppearance {
                call_id: CallId(50),
                binding: binding_for("SEP334455667788", 4),
                codec: Codec::Pcma,
            },
        ],
    );

    assert_eq!(
        offers,
        [
            InboundOffer {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                line_instance: 2,
                call_id: CallId(30),
                ring_mode: AppearanceRingMode::Silent,
                state: HandsetCallState::RingIn,
            },
            InboundOffer {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                line_instance: 1,
                call_id: CallId(20),
                ring_mode: AppearanceRingMode::Normal,
                state: HandsetCallState::RingIn,
            },
        ]
    );
    assert_eq!(controller.inbound_offers_for_pbx(PbxCallId(8)), offers);
    assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 2);
    assert!(
        controller
            .appearances_for_pbx(PbxCallId(8))
            .all(|appearance| appearance.state == CallState::Ringing)
    );
    assert_eq!(
        controller
            .pbx_call(PbxCallId(8))
            .unwrap()
            .active_appearance(),
        None
    );
    assert!(controller.invariant_error().is_none());
    assert!(controller.cancel_inbound_offer(CallId(30)));
    assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 1);
    assert!(controller.cancel_inbound_offer(CallId(20)));
    assert!(controller.pbx_call(PbxCallId(8)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn per_appearance_ring_policy_is_independent_across_concurrent_offer_snapshots() {
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP112233445566").unwrap();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration_for(first.as_str()));
    controller.registered(registration_for(second.as_str()));

    let first_offers = controller.offer_inbound_call(
        PbxCallId(8),
        [
            InboundAppearance {
                call_id: CallId(20),
                binding: binding_with_ring(first.as_str(), 1, AppearanceRingMode::Normal),
                codec: Codec::Pcma,
            },
            InboundAppearance {
                call_id: CallId(30),
                binding: binding_with_ring(second.as_str(), 2, AppearanceRingMode::Silent),
                codec: Codec::Pcmu,
            },
        ],
    );
    assert_eq!(
        first_offers
            .iter()
            .map(|offer| (offer.call_id, offer.ring_mode))
            .collect::<Vec<_>>(),
        [
            (CallId(20), AppearanceRingMode::Normal),
            (CallId(30), AppearanceRingMode::Silent),
        ]
    );

    let second_offers = controller.offer_inbound_call(
        PbxCallId(9),
        [
            InboundAppearance {
                call_id: CallId(21),
                binding: binding_with_ring(first.as_str(), 1, AppearanceRingMode::Silent),
                codec: Codec::Pcma,
            },
            InboundAppearance {
                call_id: CallId(31),
                binding: binding_with_ring(second.as_str(), 2, AppearanceRingMode::Normal),
                codec: Codec::Pcmu,
            },
        ],
    );
    assert_eq!(
        second_offers
            .iter()
            .map(|offer| (offer.call_id, offer.ring_mode))
            .collect::<Vec<_>>(),
        [
            (CallId(21), AppearanceRingMode::Silent),
            (CallId(31), AppearanceRingMode::Normal),
        ]
    );
    assert_eq!(
        controller
            .appearance_for_call(CallId(20))
            .unwrap()
            .ring_mode,
        AppearanceRingMode::Normal
    );
    assert_eq!(
        controller
            .appearance_for_call(CallId(30))
            .unwrap()
            .ring_mode,
        AppearanceRingMode::Silent
    );

    assert!(controller.cancel_inbound_offer(CallId(21)));
    assert!(controller.appearance_for_call(CallId(31)).is_some());
    assert!(controller.appearance_for_call(CallId(20)).is_some());
    assert!(controller.appearance_for_call(CallId(30)).is_some());
    assert!(!controller.phone_answer(CallId(30)).is_empty());
    assert_eq!(
        controller.appearance_for_call(CallId(20)).unwrap().state,
        CallState::RemoteInUse
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn delayed_auto_answer_has_one_generation_per_idle_shared_appearance() {
    let now = Instant::now();
    let mut controller = shared_inbound_controller();
    assert!(controller.set_auto_answer_request(
        PbxCallId(8),
        AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
            unavailable_cause: None,
        },
    ));
    assert!(controller.has_auto_answer_request(PbxCallId(8)));
    assert_eq!(
        controller.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::from_secs(2),
                tone: Tone::Zip,
            },
            now,
        ),
        Ok(2)
    );
    assert!(!controller.has_auto_answer_request(PbxCallId(8)));
    assert_eq!(controller.pending_auto_answers.len(), 2);
    assert!(controller.pending_auto_answers.values().all(|pending| {
        pending.request
            == AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
                unavailable_cause: None,
            }
    }));
    assert!(
        controller
            .expire_auto_answers(now + Duration::from_millis(1999))
            .is_empty()
    );

    let transitions = controller.expire_auto_answers(now + Duration::from_secs(2));
    assert_eq!(transitions.len(), 1);
    let transition = &transitions[0];
    assert_eq!(transition.target_call_id, CallId(2));
    assert!(matches!(
        transition.effects.last(),
        Some(DriverEffect::Handset(HandsetEffect::StartTone {
            call_id: CallId(2),
            tone: Tone::Zip,
            ..
        }))
    ));
    assert!(controller.pending_auto_answers.is_empty());
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(
        controller.call(CallId(3)).unwrap().state,
        CallState::RemoteInUse
    );
}

#[test]
fn auto_answer_replacement_captures_new_policy_and_disconnect_cancels_it() {
    let now = Instant::now();
    let request = AutoAnswerRequest {
        mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
        unavailable_cause: None,
    };
    let mut controller = shared_inbound_controller();
    assert!(controller.set_auto_answer_request(PbxCallId(8), request));
    assert_eq!(
        controller.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::from_secs(10),
                tone: Tone::Zip,
            },
            now,
        ),
        Ok(2)
    );
    let old_generations = controller
        .pending_auto_answers
        .values()
        .map(|pending| pending.generation)
        .collect::<HashSet<_>>();

    assert!(controller.set_auto_answer_request(PbxCallId(8), request));
    assert_eq!(
        controller.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::from_secs(2),
                tone: Tone::ZipZip,
            },
            now + Duration::from_secs(1),
        ),
        Ok(2)
    );
    assert!(controller.pending_auto_answers.values().all(|pending| {
        !old_generations.contains(&pending.generation)
            && pending.deadline == now + Duration::from_secs(3)
            && pending.tone == Tone::ZipZip
    }));
    assert!(
        controller
            .expire_auto_answers(now + Duration::from_secs(2))
            .is_empty()
    );
    controller.disconnected(&DeviceId::new("SEP001122334455").unwrap());
    assert_eq!(controller.pending_auto_answers.len(), 1);
    controller.disconnected(&DeviceId::new("SEP112233445566").unwrap());
    assert!(controller.pending_auto_answers.is_empty());
    assert!(
        controller
            .expire_auto_answers(now + Duration::from_secs(30))
            .is_empty()
    );
}

#[test]
fn manual_answer_remote_hangup_and_active_call_cancel_auto_answer() {
    let now = Instant::now();
    let request = AutoAnswerRequest {
        mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
        unavailable_cause: None,
    };
    let policy = AutoAnswerPolicy {
        delay: Duration::from_secs(1),
        tone: Tone::Zip,
    };

    let mut manual = shared_inbound_controller();
    assert!(manual.set_auto_answer_request(PbxCallId(8), request));
    assert_eq!(
        manual.schedule_auto_answers(PbxCallId(8), policy, now),
        Ok(2)
    );
    assert!(!manual.phone_answer(CallId(3)).is_empty());
    assert!(manual.pending_auto_answers.is_empty());
    assert!(
        manual
            .expire_auto_answers(now + Duration::from_secs(2))
            .is_empty()
    );

    let mut remote = shared_inbound_controller();
    assert!(remote.set_auto_answer_request(PbxCallId(8), request));
    assert_eq!(
        remote.schedule_auto_answers(PbxCallId(8), policy, now),
        Ok(2)
    );
    assert!(remote.pbx_hangup_with_effects(PbxCallId(8)).is_some());
    assert!(remote.pending_auto_answers.is_empty());
    assert!(
        remote
            .expire_auto_answers(now + Duration::from_secs(2))
            .is_empty()
    );

    let mut busy = connected_outbound_controller();
    busy.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcmu,
        }],
    );
    assert!(busy.set_auto_answer_request(PbxCallId(8), request));
    assert_eq!(busy.schedule_auto_answers(PbxCallId(8), policy, now), Ok(0));
    assert!(busy.pending_auto_answers.is_empty());
    assert_eq!(busy.call(CallId(1)).unwrap().state, CallState::Connected);
    assert_eq!(busy.call(CallId(2)).unwrap().state, CallState::Ringing);
}

#[test]
fn auto_answer_rollback_never_resurrects_cancelled_peer_generations() {
    let now = Instant::now();
    let mut controller = shared_inbound_controller();
    assert!(controller.set_auto_answer_request(
        PbxCallId(8),
        AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
            unavailable_cause: Some(crate::call::auto_answer::AutoAnswerCause::Unavailable),
        },
    ));
    assert_eq!(
        controller.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::ZERO,
                tone: Tone::ZipZip,
            },
            now,
        ),
        Ok(2)
    );
    let transition = controller.expire_auto_answers(now).pop().unwrap();
    assert!(controller.pending_auto_answers.is_empty());
    let cleanup =
        controller.abort_call_transition(transition.id, &CallTransitionProgress::default());
    assert!(cleanup.is_empty());
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Ringing
    );
    assert_eq!(
        controller.call(CallId(3)).unwrap().state,
        CallState::Ringing
    );
    assert!(
        controller
            .expire_auto_answers(now + Duration::from_secs(30))
            .is_empty()
    );

    assert!(controller.set_auto_answer_request(
        PbxCallId(8),
        AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
            unavailable_cause: None,
        },
    ));
    assert_eq!(
        controller.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::ZERO,
                tone: Tone::ZipZip,
            },
            now,
        ),
        Ok(2)
    );
    let transition = controller.expire_auto_answers(now).pop().unwrap();
    let cleanup = controller.abort_call_transition(
        transition.id,
        &CallTransitionProgress::with_completed([
            CallTransitionMilestone::TargetBackendStarted,
            CallTransitionMilestone::TargetHandsetChanged,
        ]),
    );
    assert!(
        cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )),
        "{cleanup:?}"
    );
    assert!(controller.pending_auto_answers.is_empty());
    assert!(controller.expire_auto_answers(now).is_empty());

    let mut exhausted = shared_inbound_controller();
    exhausted.next_auto_answer_generation = u64::MAX;
    assert!(exhausted.set_auto_answer_request(
        PbxCallId(8),
        AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
            unavailable_cause: None,
        },
    ));
    assert_eq!(
        exhausted.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::ZERO,
                tone: Tone::Zip,
            },
            now,
        ),
        Err(AutoAnswerScheduleRejection::GenerationExhausted)
    );
    assert!(exhausted.pending_auto_answers.is_empty());
}

#[test]
fn auto_answer_mode_controls_intercom_media_microphone_and_terminal_restore() {
    let now = Instant::now();
    for (mode, one_way) in [
        (crate::call::auto_answer::AutoAnswerMode::OneWay, true),
        (crate::call::auto_answer::AutoAnswerMode::TwoWay, false),
    ] {
        let mut controller = shared_inbound_controller();
        assert!(controller.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode,
                unavailable_cause: None,
            },
        ));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::ZERO,
                    tone: Tone::ZipZip,
                },
                now,
            ),
            Ok(2)
        );
        let transition = controller.expire_auto_answers(now).pop().unwrap();
        assert_eq!(transition.auto_answer_mode, Some(mode));
        assert_eq!(
            transition.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::BeginOneWayMedia {
                    call_id: CallId(2),
                    ..
                })
            )),
            one_way,
            "{transition:?}"
        );
        assert_eq!(
            transition.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::BeginMedia {
                    call_id: CallId(2),
                    ..
                })
            )),
            !one_way,
            "{transition:?}"
        );
        assert!(matches!(
            transition.effects.iter().rev().find(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::StartTone { .. })
            )),
            Some(DriverEffect::Handset(HandsetEffect::StartTone {
                call_id: CallId(2),
                tone: Tone::ZipZip,
                ..
            }))
        ));
        assert_eq!(
            transition.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                    call_id: CallId(2),
                    enabled: false,
                    ..
                })
            )),
            one_way,
            "{transition:?}"
        );

        for effect in &transition.effects {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }
        assert!(controller.commit_call_transition(transition.id));
        assert_eq!(
            controller
                .appearance_for_call(CallId(2))
                .unwrap()
                .auto_answer_mode,
            Some(mode)
        );
        let hangup = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
        assert_eq!(
            hangup.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
                    call_id: CallId(2),
                    enabled: true,
                    ..
                })
            )),
            one_way,
            "{hangup:?}"
        );
    }
}

#[test]
fn one_way_microphone_is_compensated_on_abort_and_late_completion() {
    let now = Instant::now();
    let prepare = || {
        let mut controller = shared_inbound_controller();
        assert!(controller.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
                unavailable_cause: None,
            },
        ));
        assert_eq!(
            controller.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::ZERO,
                    tone: Tone::Zip,
                },
                now,
            ),
            Ok(2)
        );
        let transition = controller.expire_auto_answers(now).pop().unwrap();
        (controller, transition)
    };

    let (mut aborted, transition) = prepare();
    let cleanup = aborted.abort_call_transition(
        transition.id,
        &CallTransitionProgress::with_completed([
            CallTransitionMilestone::TargetBackendStarted,
            CallTransitionMilestone::TargetHandsetChanged,
            CallTransitionMilestone::TargetMicrophoneDisabled,
        ]),
    );
    assert!(cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
            call_id: CallId(2),
            enabled: true,
            ..
        })
    )));

    let (mut late, transition) = prepare();
    assert!(
        late.abort_call_transition(transition.id, &CallTransitionProgress::default())
            .is_empty()
    );
    let completed = transition
        .effects
        .iter()
        .find(|effect| {
            matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetMicrophoneMode { enabled: false, .. })
            )
        })
        .unwrap();
    let compensation = late.compensate_unrecorded_call_transition_effect(&transition, completed);
    assert!(matches!(
        compensation.effects.as_slice(),
        [DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
            call_id: CallId(2),
            enabled: true,
            ..
        })]
    ));

    let (mut shutdown, transition) = prepare();
    for effect in &transition.effects {
        assert!(shutdown.record_call_transition_success(transition.id, effect));
    }
    assert!(shutdown.commit_call_transition(transition.id));
    assert!(matches!(
        shutdown.drain_one_way_microphones().as_slice(),
        [DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
            call_id: CallId(2),
            enabled: true,
            ..
        })]
    ));
    assert!(shutdown.drain_one_way_microphones().is_empty());
}

#[test]
fn delayed_two_way_auto_answer_cancels_when_the_device_becomes_active() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    assert!(controller.set_auto_answer_request(
        PbxCallId(8),
        AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
            unavailable_cause: None,
        },
    ));
    assert_eq!(
        controller.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::from_secs(2),
                tone: Tone::Zip,
            },
            now,
        ),
        Ok(1)
    );
    assert!(
        !controller
            .begin_phone_call(CallId(9), binding(), Codec::Pcmu, now)
            .is_empty()
    );
    assert!(
        controller
            .expire_auto_answers(now + Duration::from_secs(2))
            .is_empty()
    );
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Ringing
    );
    assert_eq!(
        controller.call(CallId(9)).unwrap().state,
        CallState::Collecting
    );
    assert_eq!(
        controller
            .registered_device(&binding().device_id)
            .unwrap()
            .active_call(),
        Some(CallId(9))
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn local_one_way_hangup_restores_microphone_before_removing_call() {
    let now = Instant::now();
    let mut controller = shared_inbound_controller();
    assert!(controller.set_auto_answer_request(
        PbxCallId(8),
        AutoAnswerRequest {
            mode: crate::call::auto_answer::AutoAnswerMode::OneWay,
            unavailable_cause: None,
        },
    ));
    assert_eq!(
        controller.schedule_auto_answers(
            PbxCallId(8),
            AutoAnswerPolicy {
                delay: Duration::ZERO,
                tone: Tone::Zip,
            },
            now,
        ),
        Ok(2)
    );
    let transition = controller.expire_auto_answers(now).pop().unwrap();
    for effect in &transition.effects {
        assert!(controller.record_call_transition_success(transition.id, effect));
    }
    assert!(controller.commit_call_transition(transition.id));
    let effects = controller.hangup(CallId(2));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetMicrophoneMode {
            call_id: CallId(2),
            enabled: true,
            ..
        })
    )));
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.call(CallId(3)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn answer_hangup_transfer_and_timeout_threads_have_one_serialized_winner() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    for _ in 0..16 {
        let controller = Arc::new(Mutex::new(shared_inbound_controller()));
        let gate = Arc::new(Barrier::new(2));
        let answer = {
            let controller = Arc::clone(&controller);
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| controller.phone_answer(CallId(2)))
            })
        };
        let hangup = {
            let controller = Arc::clone(&controller);
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| {
                    controller.pbx_hangup_with_effects(PbxCallId(8))
                })
            })
        };
        let answer_effects = answer.join().unwrap();
        let hangup_outcome = hangup.join().unwrap();
        assert!(hangup_outcome.is_some());
        assert!(
            answer_effects.is_empty()
                || answer_effects.iter().any(|effect| matches!(
                    effect,
                    DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
                        call_id: CallId(2),
                        ..
                    })
                ))
        );
        let controller = controller.lock().unwrap();
        assert!(controller.pbx_call(PbxCallId(8)).is_none());
        assert!(controller.invariant_error().is_none());
    }

    let now = Instant::now();
    for _ in 0..16 {
        let mut prepared = shared_inbound_controller();
        assert!(prepared.set_auto_answer_request(
            PbxCallId(8),
            AutoAnswerRequest {
                mode: crate::call::auto_answer::AutoAnswerMode::TwoWay,
                unavailable_cause: None,
            },
        ));
        assert_eq!(
            prepared.schedule_auto_answers(
                PbxCallId(8),
                AutoAnswerPolicy {
                    delay: Duration::ZERO,
                    tone: Tone::Zip,
                },
                now,
            ),
            Ok(2)
        );
        let controller = Arc::new(Mutex::new(prepared));
        let gate = Arc::new(Barrier::new(2));
        let timer = {
            let controller = Arc::clone(&controller);
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| {
                    controller.expire_auto_answers(now)
                })
            })
        };
        let manual = {
            let controller = Arc::clone(&controller);
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| controller.phone_answer(CallId(3)))
            })
        };
        let transitions = timer.join().unwrap();
        let manual_effects = manual.join().unwrap();
        let mut controller = controller.lock().unwrap();
        if let Some(transition) = transitions.first() {
            assert!(manual_effects.is_empty());
            assert!(controller.commit_call_transition(transition.id));
        } else {
            assert!(!manual_effects.is_empty());
        }
        assert!(controller.pending_auto_answers.is_empty());
        assert_eq!(
            controller
                .appearances_for_pbx(PbxCallId(8))
                .filter(|appearance| appearance.state == CallState::Connected)
                .count(),
            1
        );
        assert!(controller.invariant_error().is_none());
    }

    for _ in 0..16 {
        let mut prepared = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut prepared, false);
        prepared.enbloc(CallId(2), "2200".into());
        prepared.pbx_progress(PbxCallId(2), false);
        let controller = Arc::new(Mutex::new(prepared));
        let gate = Arc::new(Barrier::new(2));
        let completion = {
            let controller = Arc::clone(&controller);
            let gate = Arc::clone(&gate);
            let device_id = device_id.clone();
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| {
                    controller.complete_transfer(
                        &device_id,
                        CallId(2),
                        TransferTrigger::TransferKey,
                    )
                })
            })
        };
        let hangup = {
            let controller = Arc::clone(&controller);
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| {
                    controller.pbx_hangup_with_effects(PbxCallId(2))
                })
            })
        };
        let completion = completion.join().unwrap();
        assert!(hangup.join().unwrap().is_some());
        let mut controller = controller.lock().unwrap();
        if completion.is_ok() {
            controller
                .abort_transfer(
                    &device_id,
                    transaction_id,
                    TransferCancellationReason::BackendFailure,
                )
                .unwrap();
        }
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.transfer_transaction(CallId(1)).is_none());
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn call_waiting_tone_targets_active_call_repeats_and_cancels_on_answer() {
    let now = Instant::now();
    let mut controller = connected_outbound_controller();
    let offers = controller.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
        }],
    );
    assert_eq!(offers[0].state, HandsetCallState::CallWaiting);
    assert_eq!(
        controller.start_call_waiting_tone(
            CallId(2),
            Some(Tone::PriorityCallWaiting),
            Duration::from_secs(5),
            now,
        ),
        [DriverEffect::Handset(HandsetEffect::StartTone {
            device_id: binding().device_id,
            call_id: CallId(1),
            tone: Tone::PriorityCallWaiting,
        })]
    );
    assert!(
        controller
            .expire_call_waiting_tones(now + Duration::from_secs(4))
            .is_empty()
    );
    assert_eq!(
        controller.expire_call_waiting_tones(now + Duration::from_secs(5)),
        [DriverEffect::Handset(HandsetEffect::StartTone {
            device_id: binding().device_id,
            call_id: CallId(1),
            tone: Tone::PriorityCallWaiting,
        })]
    );

    let answer = controller.phone_answer(CallId(2));
    assert!(matches!(
        answer.first(),
        Some(DriverEffect::Backend(PbxEffect::Hold {
            call_id: PbxCallId(1)
        }))
    ));
    assert!(
        controller
            .expire_call_waiting_tones(now + Duration::from_secs(10))
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn disabled_or_nonwaiting_tone_policy_never_schedules_an_effect() {
    let now = Instant::now();
    let mut idle = Controller::new(Duration::from_secs(1));
    idle.registered(registration());
    idle.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
        }],
    );
    assert!(
        idle.start_call_waiting_tone(
            CallId(2),
            Some(Tone::CallWaiting),
            Duration::from_secs(5),
            now,
        )
        .is_empty()
    );

    let mut waiting = connected_outbound_controller();
    waiting.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
        }],
    );
    assert!(
        waiting
            .start_call_waiting_tone(CallId(2), None, Duration::from_secs(5), now)
            .is_empty()
    );
    assert!(
        waiting
            .expire_call_waiting_tones(now + Duration::from_secs(5))
            .is_empty()
    );

    let mut silent = connected_outbound_controller();
    silent.set_dnd(&binding().device_id, DndMode::Silent);
    silent.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
        }],
    );
    assert!(
        silent
            .start_call_waiting_tone(
                CallId(2),
                Some(Tone::CallWaiting),
                Duration::from_secs(5),
                now,
            )
            .is_empty()
    );
    assert!(silent.cancel_inbound_offer(CallId(2)));
    silent.set_dnd(&binding().device_id, DndMode::Off);
    silent.offer_inbound_call(
        PbxCallId(9),
        [InboundAppearance {
            call_id: CallId(3),
            binding: binding(),
            codec: Codec::Pcma,
        }],
    );
    assert_eq!(
        silent
            .start_call_waiting_tone(
                CallId(3),
                Some(Tone::CallWaiting),
                Duration::from_secs(5),
                now,
            )
            .len(),
        1
    );
    assert!(silent.invariant_error().is_none());
}

#[test]
fn call_waiting_timer_cleans_up_cancel_hangup_and_active_leg_changes() {
    let now = Instant::now();
    for cleanup in 0..3 {
        let mut controller = connected_outbound_controller();
        controller.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert_eq!(
            controller
                .start_call_waiting_tone(
                    CallId(2),
                    Some(Tone::CallWaiting),
                    Duration::from_secs(3),
                    now,
                )
                .len(),
            1
        );
        match cleanup {
            0 => assert!(controller.cancel_inbound_offer(CallId(2))),
            1 => assert!(controller.pbx_hangup_with_effects(PbxCallId(8)).is_some()),
            2 => assert!(!controller.hold(CallId(1)).is_empty()),
            _ => unreachable!(),
        }
        assert!(
            controller
                .expire_call_waiting_tones(now + Duration::from_secs(3))
                .is_empty()
        );
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn call_waiting_policy_reload_is_captured_per_waiting_call() {
    let now = Instant::now();
    let mut controller = connected_outbound_controller();
    for (pbx_id, call_id) in [(PbxCallId(8), CallId(2)), (PbxCallId(9), CallId(3))] {
        controller.offer_inbound_call(
            pbx_id,
            [InboundAppearance {
                call_id,
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
    }
    controller.start_call_waiting_tone(
        CallId(2),
        Some(Tone::CallWaiting),
        Duration::from_secs(5),
        now,
    );
    controller.start_call_waiting_tone(
        CallId(3),
        Some(Tone::PriorityCallWaiting),
        Duration::from_secs(2),
        now,
    );

    assert_eq!(
        controller.expire_call_waiting_tones(now + Duration::from_secs(2)),
        [DriverEffect::Handset(HandsetEffect::StartTone {
            device_id: binding().device_id,
            call_id: CallId(1),
            tone: Tone::PriorityCallWaiting,
        })]
    );
    assert!(controller.cancel_inbound_offer(CallId(3)));
    assert_eq!(
        controller.expire_call_waiting_tones(now + Duration::from_secs(5)),
        [DriverEffect::Handset(HandsetEffect::StartTone {
            device_id: binding().device_id,
            call_id: CallId(1),
            tone: Tone::CallWaiting,
        })]
    );
}

#[test]
fn incoming_limit_counts_logical_calls_and_reopens_after_cleanup() {
    let mut controller = Controller::new(Duration::from_secs(1));
    for device in ["SEP001122334455", "SEP112233445566"] {
        controller.registered(registration_for(device));
    }
    controller.set_line_incoming_limits([("1001".into(), 1)]);
    let shared = [
        InboundAppearance {
            call_id: CallId(2),
            binding: binding_for("SEP001122334455", 1),
            codec: Codec::Pcma,
        },
        InboundAppearance {
            call_id: CallId(3),
            binding: binding_for("SEP112233445566", 2),
            codec: Codec::Pcmu,
        },
    ];
    assert_eq!(controller.offer_inbound_call(PbxCallId(8), shared).len(), 2);
    assert_eq!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(9),
            [InboundAppearance {
                call_id: CallId(4),
                binding: binding_for("SEP001122334455", 1),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit)
    );
    controller.pbx_hangup_with_effects(PbxCallId(8));
    assert!(matches!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(9),
            [InboundAppearance {
                call_id: CallId(4),
                binding: binding_for("SEP001122334455", 1),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Offer(offers) if offers.len() == 1
    ));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn default_incoming_limit_serializes_the_sixth_and_seventh_offer_boundary() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    for offset in 0..6 {
        assert!(matches!(
            controller.offer_inbound_call_with_policy(
                (20 + offset).into(),
                [InboundAppearance {
                    call_id: CallId(20 + offset),
                    binding: binding(),
                    codec: Codec::Pcma,
                }],
            ),
            InboundCallDisposition::Offer(offers) if offers.len() == 1
        ));
    }
    assert_eq!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(26),
            [InboundAppearance {
                call_id: CallId(26),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit)
    );
    controller.pbx_hangup_with_effects(PbxCallId(22));
    assert!(matches!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(26),
            [InboundAppearance {
                call_id: CallId(26),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Offer(_)
    ));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn forwarding_is_resolved_before_zero_incoming_limit_rejects_ringing() {
    let mut controller = Controller::new(Duration::from_secs(1));
    let device = binding().device_id;
    controller.registered(registration());
    controller.set_line_incoming_limits([("1001".into(), 0)]);
    controller.set_forwarding(
        &device,
        ForwardingState {
            all: Some(forwarding("2200")),
            ..ForwardingState::default()
        },
    );

    assert!(matches!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Forward { destination, .. } if destination.as_str() == "2200"
    ));
    assert!(controller.pbx_call(PbxCallId(8)).is_none());

    controller.set_forwarding(&device, ForwardingState::default());
    controller.set_dnd(&device, DndMode::Reject);
    assert_eq!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(9),
            [InboundAppearance {
                call_id: CallId(3),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::IncomingLimit)
    );
    assert!(controller.pbx_call(PbxCallId(9)).is_none());
}

#[test]
fn per_device_dnd_filters_or_silences_only_its_own_shared_appearance() {
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP112233445566").unwrap();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration_for(first.as_str()));
    controller.registered(registration_for(second.as_str()));
    controller.set_dnd(&first, DndMode::Reject);
    controller.set_dnd(&second, DndMode::Silent);

    let disposition = controller.offer_inbound_call_with_policy(
        PbxCallId(8),
        [
            InboundAppearance {
                call_id: CallId(2),
                binding: binding_for(first.as_str(), 1),
                codec: Codec::Pcma,
            },
            InboundAppearance {
                call_id: CallId(3),
                binding: binding_for(second.as_str(), 2),
                codec: Codec::Pcmu,
            },
        ],
    );

    let InboundCallDisposition::Offer(offers) = disposition else {
        panic!("the silent appearance must remain eligible");
    };
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].device_id, second);
    assert_eq!(offers[0].ring_mode, AppearanceRingMode::Silent);
    assert!(controller.appearance_for_call(CallId(2)).is_none());
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().ring_mode,
        AppearanceRingMode::Silent
    );
    assert!(
        controller
            .phone_answer(CallId(3))
            .iter()
            .all(|effect| !matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
    );
    assert!(
        controller
            .media_opened(CallId(3), test_media_endpoint(Codec::Pcmu))
            .iter()
            .any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Answer {
                    call_id: PbxCallId(8)
                })
            ))
    );
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn changing_dnd_preserves_active_and_selected_call_state() {
    let device = binding().device_id;
    let mut controller = connected_outbound_controller();
    controller.set_call_selected(&device, CallId(1), true);

    for mode in [DndMode::Silent, DndMode::Reject, DndMode::Off] {
        controller.set_dnd(&device, mode);
        let registered = controller.registered_device(&device).unwrap();
        assert_eq!(registered.active_call(), Some(CallId(1)));
        assert!(registered.is_call_selected(CallId(1)));
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(controller.feature_state(&device).unwrap().dnd, mode);
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn rejected_offer_leaves_no_state_and_can_be_retried_after_dnd_is_disabled() {
    let device = binding().device_id;
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.set_dnd(&device, DndMode::Reject);
    let candidate = || InboundAppearance {
        call_id: CallId(2),
        binding: binding(),
        codec: Codec::Pcma,
    };

    assert_eq!(
        controller.offer_inbound_call_with_policy(PbxCallId(8), [candidate()]),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::DoNotDisturb)
    );
    assert!(controller.pbx_call(PbxCallId(8)).is_none());
    assert!(controller.appearance_for_call(CallId(2)).is_none());

    controller.set_dnd(&device, DndMode::Off);
    assert!(matches!(
        controller.offer_inbound_call_with_policy(PbxCallId(8), [candidate()]),
        InboundCallDisposition::Offer(offers)
            if offers.len() == 1 && offers[0].ring_mode == AppearanceRingMode::Normal
    ));
    assert_eq!(
        controller.offer_inbound_call_with_policy(PbxCallId(8), [candidate()]),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict)
    );
    assert_eq!(
        controller
            .pbx_call(PbxCallId(8))
            .unwrap()
            .appearance_ids
            .len(),
        1
    );
    assert!(controller.pbx_hangup_with_effects(PbxCallId(8)).is_some());

    controller.set_dnd(&device, DndMode::Silent);
    assert!(matches!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(9),
            [InboundAppearance {
                call_id: CallId(3),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Offer(offers)
            if offers.len() == 1 && offers[0].ring_mode == AppearanceRingMode::Silent
    ));
    assert!(controller.pbx_hangup_with_effects(PbxCallId(9)).is_some());

    controller.set_dnd(&device, DndMode::Off);
    assert!(matches!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(10),
            [InboundAppearance {
                call_id: CallId(4),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        ),
        InboundCallDisposition::Offer(offers)
            if offers.len() == 1 && offers[0].ring_mode == AppearanceRingMode::Normal
    ));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn mixed_structural_exclusions_do_not_report_a_dnd_only_rejection() {
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP112233445566").unwrap();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration_for(first.as_str()));
    controller.set_dnd(&first, DndMode::Reject);

    assert_eq!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(2),
                    binding: binding_for(first.as_str(), 1),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(3),
                    binding: binding_for(second.as_str(), 2),
                    codec: Codec::Pcmu,
                },
            ],
        ),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::NoEligibleAppearance)
    );

    controller.registered(registration_for(second.as_str()));
    let mut disabled = binding_for(second.as_str(), 2);
    disabled.appearance.ring_mode = AppearanceRingMode::Disabled;
    assert_eq!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(4),
                    binding: binding_for(first.as_str(), 1),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(5),
                    binding: disabled,
                    codec: Codec::Pcmu,
                },
            ],
        ),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::NoEligibleAppearance)
    );

    let duplicate = InboundAppearance {
        call_id: CallId(6),
        binding: binding_for(first.as_str(), 1),
        codec: Codec::Pcma,
    };
    assert_eq!(
        controller.offer_inbound_call_with_policy(PbxCallId(8), [duplicate.clone(), duplicate],),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::NoEligibleAppearance)
    );
    assert!(controller.pbx_call(PbxCallId(8)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn simultaneous_reject_and_duplicate_offer_paths_leave_one_consistent_result() {
    use std::sync::{Arc, Barrier};
    use std::thread;

    let device = binding().device_id;
    let mut rejected = Controller::new(Duration::from_secs(1));
    rejected.registered(registration());
    rejected.set_dnd(&device, DndMode::Reject);
    let rejected = Arc::new(Mutex::new(rejected));
    let gate = Arc::new(Barrier::new(2));
    let results = [(PbxCallId(8), CallId(2)), (PbxCallId(9), CallId(3))]
        .into_iter()
        .map(|(pbx_id, call_id)| {
            let controller = Arc::clone(&rejected);
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| {
                    controller.offer_inbound_call_with_policy(
                        pbx_id,
                        [InboundAppearance {
                            call_id,
                            binding: binding(),
                            codec: Codec::Pcma,
                        }],
                    )
                })
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert!(results.iter().all(|result| {
        *result == InboundCallDisposition::Unavailable(InboundUnavailableReason::DoNotDisturb)
    }));
    let rejected = rejected.lock().unwrap();
    assert!(rejected.pbx_call(PbxCallId(8)).is_none());
    assert!(rejected.pbx_call(PbxCallId(9)).is_none());
    assert!(rejected.invariant_error().is_none());
    drop(rejected);

    let mut available = Controller::new(Duration::from_secs(1));
    available.registered(registration());
    let available = Arc::new(Mutex::new(available));
    let gate = Arc::new(Barrier::new(2));
    let results = [CallId(2), CallId(3)]
        .into_iter()
        .map(|call_id| {
            let controller = Arc::clone(&available);
            let gate = Arc::clone(&gate);
            thread::spawn(move || {
                gate.wait();
                controller_step(&controller, |controller| {
                    controller.offer_inbound_call_with_policy(
                        PbxCallId(8),
                        [InboundAppearance {
                            call_id,
                            binding: binding(),
                            codec: Codec::Pcma,
                        }],
                    )
                })
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, InboundCallDisposition::Offer(_)))
            .count(),
        1
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| {
                **result == InboundCallDisposition::Unavailable(InboundUnavailableReason::Conflict)
            })
            .count(),
        1
    );
    let available = available.lock().unwrap();
    assert_eq!(
        available
            .pbx_call(PbxCallId(8))
            .unwrap()
            .appearance_ids
            .len(),
        1
    );
    assert!(available.invariant_error().is_none());
}

#[test]
fn shared_forwarding_rings_remaining_devices_and_requires_one_destination() {
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP112233445566").unwrap();
    let candidates = || {
        [
            InboundAppearance {
                call_id: CallId(2),
                binding: binding_for(first.as_str(), 1),
                codec: Codec::Pcma,
            },
            InboundAppearance {
                call_id: CallId(3),
                binding: binding_for(second.as_str(), 2),
                codec: Codec::Pcmu,
            },
        ]
    };

    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration_for(first.as_str()));
    controller.registered(registration_for(second.as_str()));
    controller.set_forwarding(
        &first,
        ForwardingState {
            all: Some(forwarding("9000")),
            ..ForwardingState::default()
        },
    );
    let InboundCallDisposition::Offer(offers) =
        controller.offer_inbound_call_with_policy(PbxCallId(8), candidates())
    else {
        panic!("one unforwarded shared appearance must still ring");
    };
    assert_eq!(offers.len(), 1);
    assert_eq!(offers[0].device_id, second);

    let mut forwarded = Controller::new(Duration::from_secs(1));
    forwarded.registered(registration_for(first.as_str()));
    forwarded.registered(registration_for(second.as_str()));
    for device in [&first, &second] {
        forwarded.set_forwarding(
            device,
            ForwardingState {
                all: Some(forwarding("9000")),
                ..ForwardingState::default()
            },
        );
    }
    assert!(matches!(
        forwarded.offer_inbound_call_with_policy(PbxCallId(9), candidates()),
        InboundCallDisposition::Forward { destination, .. } if destination.as_str() == "9000"
    ));
    assert!(forwarded.pbx_call(PbxCallId(9)).is_none());

    forwarded.set_forwarding(
        &second,
        ForwardingState {
            all: Some(forwarding("9001")),
            ..ForwardingState::default()
        },
    );
    assert_eq!(
        forwarded.offer_inbound_call_with_policy(PbxCallId(10), candidates()),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::ForwardingConflict)
    );
}

#[test]
fn forward_busy_is_evaluated_per_device_before_shared_fanout() {
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP112233445566").unwrap();
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration_for(first.as_str()));
    controller.registered(registration_for(second.as_str()));
    controller.begin_phone_call(
        CallId(50),
        binding_for(first.as_str(), 1),
        Codec::Pcmu,
        Instant::now(),
    );
    controller.set_forwarding(
        &first,
        ForwardingState {
            busy: Some(forwarding("9000")),
            ..ForwardingState::default()
        },
    );
    controller.set_dnd(&second, DndMode::Reject);

    assert!(matches!(
        controller.offer_inbound_call_with_policy(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(2),
                    binding: binding_for(first.as_str(), 2),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(3),
                    binding: binding_for(second.as_str(), 1),
                    codec: Codec::Pcmu,
                },
            ],
        ),
        InboundCallDisposition::Forward { destination, .. } if destination.as_str() == "9000"
    ));

    let mut free_peer = Controller::new(Duration::from_secs(1));
    free_peer.registered(registration_for(first.as_str()));
    free_peer.registered(registration_for(second.as_str()));
    free_peer.begin_phone_call(
        CallId(50),
        binding_for(first.as_str(), 1),
        Codec::Pcmu,
        Instant::now(),
    );
    free_peer.set_forwarding(
        &first,
        ForwardingState {
            busy: Some(forwarding("9000")),
            ..ForwardingState::default()
        },
    );
    assert!(matches!(
        free_peer.offer_inbound_call_with_policy(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(2),
                    binding: binding_for(first.as_str(), 2),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(3),
                    binding: binding_for(second.as_str(), 1),
                    codec: Codec::Pcmu,
                },
            ],
        ),
        InboundCallDisposition::Offer(offers)
            if offers.len() == 1 && offers[0].device_id == second
    ));

    let mut disagreement = Controller::new(Duration::from_secs(1));
    disagreement.registered(registration_for(first.as_str()));
    disagreement.registered(registration_for(second.as_str()));
    for (call_id, device) in [(CallId(50), &first), (CallId(51), &second)] {
        disagreement.begin_phone_call(
            call_id,
            binding_for(device.as_str(), 1),
            Codec::Pcmu,
            Instant::now(),
        );
    }
    disagreement.set_forwarding(
        &first,
        ForwardingState {
            busy: Some(forwarding("9000")),
            ..ForwardingState::default()
        },
    );
    disagreement.set_forwarding(
        &second,
        ForwardingState {
            busy: Some(forwarding("9001")),
            ..ForwardingState::default()
        },
    );
    assert_eq!(
        disagreement.offer_inbound_call_with_policy(
            PbxCallId(8),
            [
                InboundAppearance {
                    call_id: CallId(2),
                    binding: binding_for(first.as_str(), 2),
                    codec: Codec::Pcma,
                },
                InboundAppearance {
                    call_id: CallId(3),
                    binding: binding_for(second.as_str(), 2),
                    codec: Codec::Pcmu,
                },
            ],
        ),
        InboundCallDisposition::Unavailable(InboundUnavailableReason::ForwardingConflict)
    );
}

#[test]
fn privacy_from_call_appearance_and_device_blocks_remote_shared_control() {
    let first = DeviceId::new("SEP001122334455").unwrap();
    let second = DeviceId::new("SEP112233445566").unwrap();
    let mut first_binding = binding_for(first.as_str(), 1);
    first_binding.appearance.privacy = true;
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration_for(first.as_str()));
    controller.registered(registration_for(second.as_str()));
    controller.offer_inbound_call(
        PbxCallId(8),
        [
            InboundAppearance {
                call_id: CallId(2),
                binding: first_binding,
                codec: Codec::Pcma,
            },
            InboundAppearance {
                call_id: CallId(3),
                binding: binding_for(second.as_str(), 2),
                codec: Codec::Pcmu,
            },
        ],
    );
    controller.phone_answer(CallId(2));
    controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));

    assert_eq!(controller.call_privacy(CallId(2)), Some(true));
    assert!(controller.steal(CallId(3)).is_empty());
    controller.hold(CallId(2));
    assert!(controller.resume(CallId(3)).is_empty());
    assert!(!controller.set_call_privacy(CallId(3), false));
    assert!(controller.set_call_privacy(CallId(2), false));
    assert!(!controller.resume(CallId(3)).is_empty());

    let mut outbound = Controller::new(Duration::from_secs(1));
    outbound.set_privacy(&first, true);
    outbound.begin_phone_call(
        CallId(20),
        binding_for(first.as_str(), 1),
        Codec::Pcmu,
        Instant::now(),
    );
    assert_eq!(outbound.call_privacy(CallId(20)), Some(true));
}

#[test]
fn no_answer_forward_closes_every_ringing_appearance_without_hanging_up_pbx() {
    let mut controller = shared_inbound_controller();
    let effects = controller.forward_ringing_call(PbxCallId(8));

    assert_eq!(effects.len(), 2);
    assert!(effects.iter().all(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            state: HandsetCallState::OnHook,
            stop_media: false,
            ..
        })
    )));
    assert!(
        effects
            .iter()
            .all(|effect| !matches!(effect, DriverEffect::Backend(_)))
    );
    assert!(controller.pbx_call(PbxCallId(8)).is_none());

    let mut answered = shared_inbound_controller();
    answered.phone_answer(CallId(2));
    assert!(answered.forward_ringing_call(PbxCallId(8)).is_empty());
    assert!(answered.pbx_call(PbxCallId(8)).is_some());
}

#[test]
fn no_answer_claim_serializes_shared_answers_rollback_and_pbx_hangup() {
    let mut controller = shared_inbound_controller();
    assert!(controller.claim_ringing_forward(PbxCallId(8)));
    assert!(!controller.claim_ringing_forward(PbxCallId(8)));
    assert!(controller.phone_answer(CallId(2)).is_empty());
    assert!(controller.phone_answer(CallId(3)).is_empty());
    assert!(controller.rollback_ringing_forward(PbxCallId(8)));
    assert!(!controller.phone_answer(CallId(3)).is_empty());

    let mut hung_up = shared_inbound_controller();
    assert!(hung_up.claim_ringing_forward(PbxCallId(8)));
    assert!(hung_up.pbx_hangup_with_effects(PbxCallId(8)).is_some());
    assert!(!hung_up.rollback_ringing_forward(PbxCallId(8)));
    assert!(hung_up.complete_ringing_forward(PbxCallId(8)).is_empty());
    assert!(hung_up.invariant_error().is_none());
}

#[test]
fn first_serialized_answer_wins_and_later_answers_are_noops() {
    let mut controller = shared_inbound_controller();
    let effects = controller.phone_answer(CallId(3));

    assert_eq!(
        effects,
        [
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(2),
                state: HandsetCallState::RemoteMultiline,
                stop_media: false,
            }),
            DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                codec: Codec::Pcmu,
            }),
        ]
    );
    assert!(controller.phone_answer(CallId(2)).is_empty());
    assert_eq!(
        controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcmu)),
        [
            DriverEffect::Backend(PbxEffect::ConfigureMedia {
                call_id: PbxCallId(8),
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                handset_call_id: CallId(3),
                codec: Codec::Pcmu,
                remote: test_media_endpoint(Codec::Pcmu),
            }),
            DriverEffect::Backend(PbxEffect::Answer {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                state: HandsetCallState::Connected,
                stop_media: false,
            }),
        ]
    );
    let winner = controller
        .pbx_call(PbxCallId(8))
        .unwrap()
        .active_appearance()
        .unwrap();
    assert_eq!(
        controller.call_appearance(winner).unwrap().sccp_id,
        CallId(3)
    );
    assert_eq!(
        controller
            .primary_call_by_pbx(PbxCallId(8))
            .unwrap()
            .sccp_id,
        CallId(2)
    );
    assert_eq!(
        controller.active_call_by_pbx(PbxCallId(8)).unwrap().sccp_id,
        CallId(3)
    );
    assert_eq!(
        controller
            .active_or_primary_call_by_pbx(PbxCallId(8))
            .unwrap()
            .sccp_id,
        CallId(3)
    );
    assert_eq!(
        controller.appearance_for_call(CallId(2)).unwrap().state,
        CallState::RemoteInUse
    );
    assert!(
        !effects
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
    );
    assert!(controller.invariant_error().is_none());

    let mut reverse = shared_inbound_controller();
    assert!(!reverse.phone_answer(CallId(2)).is_empty());
    assert!(reverse.phone_answer(CallId(3)).is_empty());
    let winner = reverse
        .pbx_call(PbxCallId(8))
        .unwrap()
        .active_appearance()
        .unwrap();
    assert_eq!(reverse.call_appearance(winner).unwrap().sccp_id, CallId(2));
    assert!(reverse.invariant_error().is_none());
}

#[test]
fn media_timeout_terminates_pending_answer_and_late_ack_cannot_answer() {
    let mut controller = shared_inbound_controller();
    let opening = controller.phone_answer(CallId(2));
    assert!(opening.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::BeginAnswerMedia {
            call_id: CallId(2),
            ..
        })
    )));
    assert!(
        opening
            .iter()
            .all(|effect| !matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
    );

    let cleanup = controller.terminate(CallId(2));
    assert!(cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(8)
        })
    )));
    assert!(cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(2),
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        })
    )));
    assert!(
        controller
            .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn pending_answer_rejects_hold_switch_and_shared_steal_until_media_commits() {
    let mut controller = shared_inbound_controller();
    let device = DeviceId::new("SEP001122334455").unwrap();
    controller.offer_inbound_call(
        PbxCallId(9),
        [InboundAppearance {
            call_id: CallId(4),
            binding: binding_for(device.as_str(), 3),
            codec: Codec::Pcma,
        }],
    );

    assert!(!controller.phone_answer(CallId(2)).is_empty());
    assert!(controller.hold(CallId(2)).is_empty());
    assert!(matches!(
        controller.begin_active_call_switch_transaction(&device, CallId(4)),
        Err(CallSwitchRejection::Conflict)
    ));
    assert!(controller.steal(CallId(3)).is_empty());
    assert_eq!(
        controller.registered_device(&device).unwrap().active_call(),
        Some(CallId(2))
    );
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(
        controller.call(CallId(4)).unwrap().state,
        CallState::Ringing
    );

    let endpoint = test_media_endpoint(Codec::Pcma);
    assert!(
        controller
            .media_opened(CallId(2), endpoint)
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
    );
    assert!(!controller.hold(CallId(2)).is_empty());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn pending_answer_owner_disconnect_terminates_shared_call_and_ignores_late_ack() {
    let mut controller = shared_inbound_controller();
    assert!(!controller.phone_answer(CallId(2)).is_empty());

    let disconnected = DeviceId::new("SEP001122334455").unwrap();
    let effects = controller.disconnected(&disconnected);
    assert!(effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(8)
        })
    )));
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(effect) if effect.device_id() == &disconnected
    )));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(3),
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        })
    )));
    assert!(controller.pbx_call(PbxCallId(8)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.call(CallId(3)).is_none());
    assert!(
        controller
            .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn shared_hold_can_be_resumed_from_another_registered_device() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    let first_endpoint = test_media_endpoint(Codec::Pcma);
    controller.media_opened(CallId(2), first_endpoint);
    controller.media_transmission_started(CallId(2), first_endpoint);
    assert_eq!(
        controller.call(CallId(2)).unwrap().audio,
        MediaStreamState::Open(first_endpoint)
    );
    assert_eq!(
        controller.call(CallId(2)).unwrap().audio_transmit,
        MediaStreamState::Open(first_endpoint)
    );
    let held = controller.hold(CallId(2));
    assert_eq!(
        held,
        [
            DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(2),
                state: HandsetCallState::Hold,
                stop_media: true,
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                state: HandsetCallState::HoldRed,
                stop_media: false,
            }),
        ]
    );
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::SharedHeld
    );
    for call_id in [CallId(2), CallId(3)] {
        let appearance = controller.call(call_id).unwrap();
        assert_eq!(appearance.audio, MediaStreamState::Closed);
        assert_eq!(appearance.audio_transmit, MediaStreamState::Closed);
    }

    let resumed = controller.resume(CallId(3));
    assert_eq!(
        resumed,
        [
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(2),
                state: HandsetCallState::RemoteMultiline,
                stop_media: true,
            }),
            DriverEffect::Handset(HandsetEffect::BeginMedia {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                codec: Codec::Pcmu,
            }),
        ]
    );
    assert_eq!(
        controller.appearance_for_call(CallId(2)).unwrap().state,
        CallState::RemoteInUse
    );
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(
        controller.call(CallId(3)).unwrap().audio,
        MediaStreamState::Opening
    );
    assert_eq!(
        controller.call(CallId(3)).unwrap().audio_transmit,
        MediaStreamState::Closed
    );

    let resumed_endpoint = test_media_endpoint(Codec::Pcmu);
    assert!(matches!(
        controller
            .media_opened(CallId(3), resumed_endpoint)
            .as_slice(),
        [
            DriverEffect::Backend(PbxEffect::ConfigureMedia { .. }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                state: HandsetCallState::Connected,
                ..
            })
        ]
    ));
    assert_eq!(
        controller.call(CallId(3)).unwrap().audio_transmit,
        MediaStreamState::Opening
    );
    controller.media_transmission_started(CallId(3), resumed_endpoint);
    assert_eq!(
        controller.call(CallId(3)).unwrap().audio_transmit,
        MediaStreamState::Open(resumed_endpoint)
    );

    let terminal = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
    assert_eq!(
        terminal
            .effects
            .iter()
            .filter_map(|effect| match effect {
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id,
                    state: HandsetCallState::OnHook,
                    stop_media,
                    ..
                }) => Some((*call_id, *stop_media)),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [(CallId(2), true), (CallId(3), true)]
    );
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.call(CallId(3)).is_none());
    assert!(
        controller
            .media_opened(CallId(3), resumed_endpoint)
            .is_empty()
    );
    controller.media_transmission_started(CallId(3), resumed_endpoint);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn active_call_can_be_stolen_once_by_an_eligible_remote_appearance() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    let effects = controller.steal(CallId(3));

    assert_eq!(
        effects,
        [
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(2),
                state: HandsetCallState::RemoteMultiline,
                stop_media: true,
            }),
            DriverEffect::Handset(HandsetEffect::BeginMedia {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                codec: Codec::Pcmu,
            }),
        ]
    );
    assert!(controller.steal(CallId(3)).is_empty());
    let winner = controller
        .pbx_call(PbxCallId(8))
        .unwrap()
        .active_appearance()
        .unwrap();
    assert_eq!(
        controller.call_appearance(winner).unwrap().sccp_id,
        CallId(3)
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn steal_rejects_disabled_and_unregistered_presentations() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    controller.registered(registration_for("SEP223344556677"));
    controller
        .add_call_appearance(
            PbxCallId(8),
            CallId(4),
            &binding_with_ring("SEP223344556677", 3, AppearanceRingMode::Disabled),
            Codec::Pcma,
        )
        .unwrap();
    controller
        .add_call_appearance(
            PbxCallId(8),
            CallId(5),
            &binding_for("SEP334455667788", 4),
            Codec::Pcma,
        )
        .unwrap();

    assert!(controller.steal(CallId(4)).is_empty());
    assert!(controller.steal(CallId(5)).is_empty());
    assert_eq!(
        controller.appearance_for_call(CallId(2)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn remaining_device_can_claim_a_call_after_the_owner_disconnects() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    assert!(
        controller
            .disconnected(&DeviceId::new("SEP001122334455").unwrap())
            .is_empty()
    );
    assert_eq!(
        controller
            .pbx_call(PbxCallId(8))
            .unwrap()
            .active_appearance(),
        None
    );

    assert_eq!(
        controller.steal(CallId(3)),
        [DriverEffect::Handset(HandsetEffect::BeginMedia {
            device_id: DeviceId::new("SEP112233445566").unwrap(),
            call_id: CallId(3),
            codec: Codec::Pcmu,
        })]
    );
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn directed_barge_checks_privacy_capabilities_and_restores_remote_appearance() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    assert_eq!(
        controller.barge(
            CallId(3),
            binding_for("SEP112233445566", 2),
            Codec::Pcmu,
            BargeMode::Directed,
        ),
        Err(BargeRejection::Capability)
    );
    enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
    enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);

    controller.set_call_privacy(CallId(2), true);
    assert_eq!(
        controller.barge(
            CallId(3),
            binding_for("SEP112233445566", 2),
            Codec::Pcmu,
            BargeMode::Directed,
        ),
        Err(BargeRejection::Private)
    );
    controller.set_call_privacy(CallId(2), false);

    let effects = controller
        .barge(
            CallId(3),
            binding_for("SEP112233445566", 2),
            Codec::Pcmu,
            BargeMode::Directed,
        )
        .unwrap();
    assert_eq!(
        effects,
        [
            DriverEffect::Backend(PbxEffect::CreateChannel {
                handset_call_id: CallId(3),
                call_id: PbxCallId(9),
                binding: Box::new(binding_for("SEP112233445566", 2)),
                codec: Codec::Pcmu,
            }),
            DriverEffect::Backend(PbxEffect::Barge {
                operation: BargeOperation::Join {
                    bridge_id: PbxBridgeId(1),
                    target_call_id: PbxCallId(8),
                    barger_call_id: PbxCallId(9),
                },
            }),
            DriverEffect::Handset(HandsetEffect::BeginMedia {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                codec: Codec::Pcmu,
            }),
        ]
    );
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::Barged
    );
    assert_eq!(
        controller
            .pbx_call(PbxCallId(8))
            .unwrap()
            .active_appearance()
            .and_then(|id| controller.call_appearance(id))
            .map(|appearance| appearance.sccp_id),
        Some(CallId(2))
    );
    let endpoint = MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 20_000,
        rtcp_port: 20_001,
        codec: Codec::Pcmu,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 101,
    };
    assert!(matches!(
        controller.media_opened(CallId(3), endpoint).as_slice(),
        [DriverEffect::Backend(PbxEffect::ConfigureMedia {
            call_id: PbxCallId(9),
            handset_call_id: CallId(3),
            ..
        })]
    ));
    assert!(controller.hold(CallId(2)).is_empty());

    let cleanup = controller.hangup(CallId(3));
    assert_eq!(
        cleanup,
        [
            DriverEffect::Backend(PbxEffect::Barge {
                operation: BargeOperation::Leave {
                    bridge_id: PbxBridgeId(1),
                    barger_call_id: PbxCallId(9),
                    last_participant: true,
                },
            }),
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(9),
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP112233445566").unwrap(),
                call_id: CallId(3),
                state: HandsetCallState::RemoteMultiline,
                stop_media: true,
            }),
        ]
    );
    assert!(controller.pbx_call(PbxCallId(8)).is_some());
    assert!(controller.pbx_call(PbxCallId(9)).is_none());
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::RemoteInUse
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn conference_barge_reuses_one_bridge_and_cleans_every_participant() {
    let mut controller = shared_inbound_controller();
    controller.registered(registration_for("SEP223344556677"));
    controller
        .add_call_appearance(
            PbxCallId(8),
            CallId(4),
            &binding_for("SEP223344556677", 3),
            Codec::Pcmu,
        )
        .unwrap();
    controller.phone_answer(CallId(2));
    enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
    enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
    enable_barge_capabilities(&mut controller, "SEP223344556677", Codec::Pcmu);

    let first = controller
        .barge(
            CallId(3),
            binding_for("SEP112233445566", 2),
            Codec::Pcmu,
            BargeMode::Conference,
        )
        .unwrap();
    let second = controller
        .barge(
            CallId(4),
            binding_for("SEP223344556677", 3),
            Codec::Pcmu,
            BargeMode::Conference,
        )
        .unwrap();
    assert!(matches!(
        first.get(1),
        Some(DriverEffect::Backend(PbxEffect::Barge {
            operation: BargeOperation::Join {
                bridge_id: PbxBridgeId(1),
                barger_call_id: PbxCallId(9),
                ..
            }
        }))
    ));
    assert!(matches!(
        second.get(1),
        Some(DriverEffect::Backend(PbxEffect::Barge {
            operation: BargeOperation::Join {
                bridge_id: PbxBridgeId(1),
                barger_call_id: PbxCallId(10),
                ..
            }
        }))
    ));
    assert!(matches!(
        controller.hangup(CallId(3)).first(),
        Some(DriverEffect::Backend(PbxEffect::Barge {
            operation: BargeOperation::Leave {
                last_participant: false,
                ..
            }
        }))
    ));
    assert!(matches!(
        controller.hangup(CallId(4)).first(),
        Some(DriverEffect::Backend(PbxEffect::Barge {
            operation: BargeOperation::Leave {
                last_participant: true,
                ..
            }
        }))
    ));
    assert!(controller.pbx_call(PbxCallId(8)).is_some());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn first_serialized_steal_or_barge_claim_wins_without_target_hangup() {
    fn prepared() -> Controller {
        let mut controller = shared_inbound_controller();
        controller.registered(registration_for("SEP223344556677"));
        controller
            .add_call_appearance(
                PbxCallId(8),
                CallId(4),
                &binding_for("SEP223344556677", 3),
                Codec::Pcmu,
            )
            .unwrap();
        controller.phone_answer(CallId(2));
        controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
        enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
        enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
        enable_barge_capabilities(&mut controller, "SEP223344556677", Codec::Pcmu);
        controller
    }

    let mut steal_first = prepared();
    assert!(!steal_first.steal(CallId(3)).is_empty());
    assert_eq!(
        steal_first.barge(
            CallId(4),
            binding_for("SEP223344556677", 3),
            Codec::Pcmu,
            BargeMode::Directed,
        ),
        Err(BargeRejection::Conflict)
    );
    assert!(steal_first.pbx_call(PbxCallId(8)).is_some());

    let mut barge_first = prepared();
    let effects = barge_first
        .barge(
            CallId(4),
            binding_for("SEP223344556677", 3),
            Codec::Pcmu,
            BargeMode::Directed,
        )
        .unwrap();
    assert!(barge_first.steal(CallId(3)).is_empty());
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(8)
        })
    )));
    assert!(barge_first.invariant_error().is_none());
}

#[test]
fn fake_handset_races_serialize_answer_hold_steal_and_barge() {
    fn prepared_for_barge() -> Controller {
        let mut controller = shared_inbound_controller();
        controller.registered(registration_for("SEP223344556677"));
        controller
            .add_call_appearance(
                PbxCallId(8),
                CallId(4),
                &binding_for("SEP223344556677", 3),
                Codec::Pcmu,
            )
            .unwrap();
        enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
        enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
        enable_barge_capabilities(&mut controller, "SEP223344556677", Codec::Pcmu);
        controller
    }

    // Two handsets answer the same offer: the first serialized answer is
    // the only one that reaches either the backend or handset media.
    let mut answer = shared_inbound_controller();
    let first = answer.phone_answer(CallId(2));
    let second = answer.phone_answer(CallId(3));
    let mut handsets = FakeHandsets::default();
    handsets.apply(&first);
    handsets.apply(&second);
    assert_eq!(handsets.media_winners(), [CallId(2)]);
    assert_eq!(
        first
            .iter()
            .chain(&second)
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
            .count(),
        0
    );
    assert_eq!(
        answer
            .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Answer { .. })))
            .count(),
        1
    );
    assert!(answer.invariant_error().is_none());

    // Holding first makes a concurrent steal ineligible. Stealing first
    // transfers ownership and makes the former owner's hold a no-op.
    let mut hold_first = shared_inbound_controller();
    hold_first.phone_answer(CallId(2));
    hold_first.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    assert!(!hold_first.hold(CallId(2)).is_empty());
    assert!(hold_first.steal(CallId(3)).is_empty());
    assert!(hold_first.invariant_error().is_none());

    let mut steal_first = shared_inbound_controller();
    steal_first.phone_answer(CallId(2));
    steal_first.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    let steal = steal_first.steal(CallId(3));
    assert!(!steal.is_empty());
    assert!(steal_first.hold(CallId(2)).is_empty());
    assert!(steal_first.invariant_error().is_none());

    // Directed barge has one winner. Reversing request order reverses the
    // winner without ever hanging up the shared target call.
    for (winner, loser, winner_device) in [
        (CallId(3), CallId(4), "SEP112233445566"),
        (CallId(4), CallId(3), "SEP223344556677"),
    ] {
        let mut controller = prepared_for_barge();
        controller.phone_answer(CallId(2));
        let winning_effects = controller
            .barge(
                winner,
                binding_for(winner_device, if winner == CallId(3) { 2 } else { 3 }),
                Codec::Pcmu,
                BargeMode::Directed,
            )
            .unwrap();
        let losing_device = if loser == CallId(3) {
            "SEP112233445566"
        } else {
            "SEP223344556677"
        };
        assert_eq!(
            controller.barge(
                loser,
                binding_for(losing_device, if loser == CallId(3) { 2 } else { 3 }),
                Codec::Pcmu,
                BargeMode::Directed,
            ),
            Err(BargeRejection::AlreadyBarged)
        );
        let mut handsets = FakeHandsets::default();
        handsets.apply(&winning_effects);
        assert_eq!(handsets.media_winners(), [winner]);
        assert!(!winning_effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        assert!(controller.pbx_call(PbxCallId(8)).is_some());
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn barge_abort_and_target_hangup_have_exact_cleanup_without_double_hangup() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    enable_barge_capabilities(&mut controller, "SEP001122334455", Codec::Pcma);
    enable_barge_capabilities(&mut controller, "SEP112233445566", Codec::Pcmu);
    controller
        .barge(
            CallId(3),
            binding_for("SEP112233445566", 2),
            Codec::Pcmu,
            BargeMode::Directed,
        )
        .unwrap();
    let failed_join = controller.abort_barge(CallId(3), false, true);
    assert!(failed_join.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(9)
        })
    )));
    assert!(
        !failed_join
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Barge { .. })))
    );

    controller
        .barge(
            CallId(3),
            binding_for("SEP112233445566", 2),
            Codec::Pcmu,
            BargeMode::Directed,
        )
        .unwrap();
    let outcome = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
    assert_eq!(
        outcome
            .effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Barge {
                    operation: BargeOperation::Leave { .. }
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        outcome
            .effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(10)
                })
            ))
            .count(),
        1
    );
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(8)
        })
    )));
    assert_eq!(controller.calls().count(), 0);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn pbx_hangup_publishes_available_to_every_shared_appearance() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(2));
    let outcome = controller.pbx_hangup_with_effects(PbxCallId(8)).unwrap();

    assert_eq!(outcome.effects.len(), 2);
    assert!(outcome.effects.iter().all(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        })
    )));
    assert!(controller.pbx_call(PbxCallId(8)).is_none());
    assert_eq!(controller.calls().count(), 0);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn immediate_divert_claim_is_exact_and_failure_preserves_the_ringing_call() {
    let mut controller = shared_inbound_controller();
    let device = DeviceId::new("SEP001122334455").unwrap();
    let plan = controller
        .begin_immediate_divert(&device, CallId(2), voicemail_target("61001"))
        .unwrap();

    assert_eq!(plan.transaction.action, VoicemailAction::ImmediateDivert);
    assert_eq!(plan.transaction.phase, VoicemailPhase::Executing);
    assert!(matches!(
        plan.effects.as_slice(),
        [DriverEffect::Backend(PbxEffect::Voicemail { operation })]
            if operation.transaction_id == plan.transaction.id
                && operation.device_id == device
                && operation.handset_call_id == CallId(2)
                && operation.pbx_call_id == PbxCallId(8)
                && operation.action == VoicemailAction::ImmediateDivert
                && operation.target.destination() == "61001"
    ));
    assert!(controller.phone_answer(CallId(2)).is_empty());
    assert_eq!(
        controller.begin_immediate_divert(&device, CallId(2), voicemail_target("61001")),
        Err(VoicemailRejection::Conflict)
    );

    let aborted = controller
        .abort_voicemail(&device, plan.transaction.id)
        .unwrap();
    assert_eq!(aborted.id, plan.transaction.id);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Ringing
    );
    assert_eq!(
        controller.call(CallId(3)).unwrap().state,
        CallState::Ringing
    );
    assert!(!controller.phone_answer(CallId(2)).is_empty());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn immediate_divert_success_ends_every_shared_appearance_once() {
    let mut controller = shared_inbound_controller();
    let device = DeviceId::new("SEP001122334455").unwrap();
    let plan = controller
        .begin_immediate_divert(&device, CallId(2), voicemail_target("61001"))
        .unwrap();
    let outcome = controller
        .voicemail_succeeded(&device, plan.transaction.id)
        .unwrap();

    assert_eq!(outcome.transaction.id, plan.transaction.id);
    assert_eq!(outcome.effects.len(), 2);
    assert!(outcome.effects.iter().all(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        })
    )));
    assert!(controller.pbx_call(PbxCallId(8)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.call(CallId(3)).is_none());
    assert!(!controller.voicemail_generation_is_active(&device, plan.transaction.id));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn voicemail_claim_is_cancelled_by_pbx_hangup_and_last_appearance_disconnect() {
    let device = DeviceId::new("SEP001122334455").unwrap();
    let mut hung_up = shared_inbound_controller();
    let hangup_plan = hung_up
        .begin_immediate_divert(&device, CallId(2), voicemail_target("61001"))
        .unwrap();
    let outcome = hung_up.pbx_hangup_with_effects(PbxCallId(8)).unwrap();
    assert_eq!(outcome.effects.len(), 2);
    assert!(!hung_up.voicemail_generation_is_active(&device, hangup_plan.transaction.id));
    assert_eq!(
        hung_up
            .complete_voicemail_native(
                &device,
                hangup_plan.transaction.id,
                hangup_plan.transaction.pbx_call_id,
            )
            .unwrap(),
        VoicemailNativeOutcome::CallAlreadyEnded
    );

    let mut disconnected = connected_outbound_controller();
    let disconnect_plan = disconnected
        .begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
        .unwrap();
    let _ = disconnected.disconnected(&device);
    assert!(!disconnected.voicemail_generation_is_active(&device, disconnect_plan.transaction.id));
    assert!(
        disconnected
            .pbx_call(disconnect_plan.transaction.pbx_call_id)
            .is_none()
    );
    assert_eq!(
        disconnected
            .complete_voicemail_native(
                &device,
                disconnect_plan.transaction.id,
                disconnect_plan.transaction.pbx_call_id,
            )
            .unwrap(),
        VoicemailNativeOutcome::CallAlreadyEnded
    );
    assert!(disconnected.invariant_error().is_none());
}

#[test]
fn native_voicemail_success_survives_shared_owner_disconnect() {
    let first = DeviceId::new("SEP001122334455").unwrap();

    let mut immediate = shared_inbound_controller();
    let immediate_plan = immediate
        .begin_immediate_divert(&first, CallId(2), voicemail_target("61001"))
        .unwrap();
    let _ = immediate.disconnected(&first);
    assert!(immediate.voicemail_generation_is_active(&first, immediate_plan.transaction.id));
    let immediate_outcome = immediate
        .complete_voicemail_native(
            &first,
            immediate_plan.transaction.id,
            immediate_plan.transaction.pbx_call_id,
        )
        .unwrap();
    assert!(matches!(
        immediate_outcome,
        VoicemailNativeOutcome::Committed(VoicemailTerminalOutcome { ref effects, .. })
            if effects.len() == 1
                && matches!(
                    effects[0],
                    DriverEffect::Handset(HandsetEffect::SetCallState {
                        call_id: CallId(3),
                        state: HandsetCallState::OnHook,
                        ..
                    })
                )
    ));
    assert!(immediate.pbx_call(PbxCallId(8)).is_none());

    let mut selected = shared_inbound_controller();
    selected.phone_answer(CallId(2));
    selected.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    assert!(selected.set_call_selected(&first, CallId(2), true));
    let selected_plan = selected
        .begin_selected_voicemail_transfer(&first, voicemail_target("61001"))
        .unwrap();
    let _ = selected.disconnected(&first);
    assert!(selected.voicemail_generation_is_active(&first, selected_plan.transaction.id));
    assert!(matches!(
        selected
            .complete_voicemail_native(
                &first,
                selected_plan.transaction.id,
                selected_plan.transaction.pbx_call_id,
            )
            .unwrap(),
        VoicemailNativeOutcome::Committed(_)
    ));
    assert!(selected.pbx_call(PbxCallId(8)).is_none());
    assert!(selected.invariant_error().is_none());
}

#[test]
fn selected_voicemail_requires_exactly_one_owned_connected_or_held_call() {
    let device = binding().device_id;
    let mut none = connected_outbound_controller();
    none.set_call_selected(&device, CallId(1), false);
    assert_eq!(
        none.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
        Err(VoicemailRejection::Conflict)
    );

    let mut multiple = connected_outbound_controller();
    multiple
        .begin_additional_phone_call(CallId(2), binding(), Codec::Pcmu, Instant::now())
        .unwrap();
    multiple.set_call_selected(&device, CallId(1), true);
    multiple.set_call_selected(&device, CallId(2), true);
    assert_eq!(
        multiple.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
        Err(VoicemailRejection::Conflict)
    );

    let wrong_device = DeviceId::new("SEP112233445566").unwrap();
    let mut wrong = connected_outbound_controller();
    assert_eq!(
        wrong.begin_selected_voicemail_transfer(&wrong_device, voicemail_target("61001")),
        Err(VoicemailRejection::Conflict)
    );
    let mut ringing = shared_inbound_controller();
    assert!(ringing.set_call_selected(&device, CallId(2), true));
    assert_eq!(
        ringing.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
        Err(VoicemailRejection::InvalidPhase)
    );
    let mut remote = shared_inbound_controller();
    remote.phone_answer(CallId(3));
    assert!(remote.set_call_selected(&device, CallId(2), true));
    assert_eq!(
        remote.begin_selected_voicemail_transfer(&device, voicemail_target("61001")),
        Err(VoicemailRejection::InvalidPhase)
    );
    let mut held = connected_outbound_controller();
    held.hold(CallId(1));
    assert!(held.set_call_selected(&device, CallId(1), true));
    assert!(
        held.begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
            .is_ok()
    );
    assert!(none.invariant_error().is_none());
    assert!(multiple.invariant_error().is_none());
    assert!(wrong.invariant_error().is_none());
    assert!(ringing.invariant_error().is_none());
    assert!(remote.invariant_error().is_none());
    assert!(held.invariant_error().is_none());
}

#[test]
fn selected_voicemail_serializes_against_park_transfer_and_conference() {
    let mut controller = connected_outbound_controller();
    let device = binding().device_id;
    let plan = controller
        .begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
        .unwrap();

    assert_eq!(plan.transaction.action, VoicemailAction::TransferSelected);
    assert_eq!(
        controller.park(CallId(1), true, None),
        Err(ParkingRejection::Conflict)
    );
    assert_eq!(
        controller.begin_transfer(TransferConsultationRequest {
            source_call_id: CallId(1),
            consultation_call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcmu,
            complete_on_hangup: false,
            now: Instant::now(),
        }),
        Err(TransferRejection::Conflict)
    );
    assert_eq!(
        controller.begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        ),
        Err(ConferenceRejection::Conflict)
    );
    assert_eq!(
        controller.call(CallId(1)).unwrap().state,
        CallState::Connected
    );

    controller
        .abort_voicemail(&device, plan.transaction.id)
        .unwrap();
    let retry = controller
        .begin_selected_voicemail_transfer(&device, voicemail_target("61001"))
        .unwrap();
    assert!(retry.transaction.id > plan.transaction.id);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn registered_device_runtime_tracks_capabilities_and_selection() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    let registration = registration();
    let device = registration.id.clone();
    controller.registered(registration);
    controller.capabilities(
        &device,
        vec![MediaCapability {
            codec: Codec::Pcma,
            max_packet_ms: 80,
            codec_parameters: [0; 8],
        }],
    );

    controller.begin_phone_call(CallId(12), binding(), Codec::Pcma, now);
    let state = controller.registered_device(&device).unwrap();
    assert_eq!(state.registration.firmware, "SCCP-test");
    assert_eq!(
        state.capabilities.as_ref().unwrap().audio()[0].codec,
        Codec::Pcma
    );
    assert_eq!(state.selected_line, Some(1));
    assert!(state.is_call_selected(CallId(12)));

    controller.hold(CallId(12));
    assert!(
        !controller
            .registered_device(&device)
            .unwrap()
            .is_call_selected(CallId(12))
    );
    controller.resume(CallId(12));
    assert!(
        controller
            .registered_device(&device)
            .unwrap()
            .is_call_selected(CallId(12))
    );
    controller.hangup(CallId(12));
    assert_eq!(
        controller
            .registered_device(&device)
            .unwrap()
            .selected_calls()
            .count(),
        0
    );
}

#[test]
fn registered_device_distinguishes_pending_empty_and_reported_capabilities() {
    let mut controller = Controller::new(Duration::from_secs(1));
    let registration = registration();
    let device = registration.id.clone();
    controller.registered(registration);

    assert!(
        controller
            .registered_device(&device)
            .unwrap()
            .capabilities
            .is_none()
    );
    controller.capabilities(&device, Vec::new());
    assert!(
        controller
            .registered_device(&device)
            .unwrap()
            .capabilities
            .as_ref()
            .is_some_and(StationMediaCapabilities::is_empty)
    );
    controller.capabilities(
        &device,
        vec![MediaCapability {
            codec: Codec::Pcmu,
            max_packet_ms: 80,
            codec_parameters: [0; 8],
        }],
    );
    assert_eq!(
        controller
            .registered_device(&device)
            .unwrap()
            .capabilities
            .as_ref()
            .unwrap()
            .audio()
            .len(),
        1
    );
}

#[test]
fn newer_session_retires_old_calls_and_rejects_late_session_state() {
    let mut controller = connected_outbound_controller();
    let device = DeviceId::new("SEP001122334455").unwrap();
    let old_generation = controller
        .registered_device(&device)
        .unwrap()
        .session_generation;
    let old_capabilities = StationMediaCapabilities::new(
        vec![MediaCapability {
            codec: Codec::Pcmu,
            max_packet_ms: 80,
            codec_parameters: [0; 8],
        }],
        vec![VideoCapability {
            codec: Codec::H264,
            direction: ReceiveTransmit::RECEIVE | ReceiveTransmit::TRANSMIT,
            level_preferences: Vec::new(),
            codec_parameters: vec![1, 2, 3],
            encryption_capability: None,
            address_type: None,
        }],
    );
    assert!(controller.update_capabilities(&device, old_generation, old_capabilities.clone(),));
    let old_encryption = StationEncryptionCapabilities::Supported(vec![
        crate::media::encryption::AdvertisedEncryptionProfile {
            algorithm: sccp_protocol::EncryptionMethod::Aes128HmacSha1_80,
            master_key_bits: 128,
        },
    ]);
    assert!(controller.update_audio_encryption_capabilities(
        &device,
        old_generation,
        old_encryption.clone(),
    ));

    let new_generation = SessionGeneration::new(old_generation.get() + 1).unwrap();
    let outcome = controller
        .register_session(new_generation, registration())
        .unwrap();

    assert!(outcome.replaced);
    assert_eq!(
        outcome.cleanup,
        vec![DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(1),
        })]
    );
    assert!(controller.call(CallId(1)).is_none());
    assert!(controller.pbx_call(PbxCallId(1)).is_none());
    let state = controller.registered_device(&device).unwrap();
    assert_eq!(state.session_generation, new_generation);
    assert!(state.capabilities.is_none());
    assert_eq!(
        state.audio_encryption,
        StationEncryptionCapabilities::NotReported
    );

    assert!(!controller.session_is_current(&device, old_generation));
    assert!(!controller.update_capabilities(&device, old_generation, old_capabilities.clone(),));
    assert!(!controller.update_audio_encryption_capabilities(
        &device,
        old_generation,
        old_encryption.clone(),
    ));
    assert!(
        controller
            .register_session(old_generation, registration())
            .is_none()
    );
    assert!(
        controller
            .register_session(new_generation, registration())
            .is_none()
    );
    assert!(
        controller
            .registered_device(&device)
            .unwrap()
            .capabilities
            .is_none()
    );

    assert!(controller.session_is_current(&device, new_generation));
    assert!(controller.update_capabilities(&device, new_generation, old_capabilities.clone(),));
    assert!(controller.update_audio_encryption_capabilities(
        &device,
        new_generation,
        old_encryption.clone(),
    ));
    let state = controller.registered_device(&device).unwrap();
    assert_eq!(
        state.capabilities.as_ref().unwrap().audio(),
        old_capabilities.audio()
    );
    assert_eq!(
        state.capabilities.as_ref().unwrap().video(),
        old_capabilities.video()
    );
    assert_eq!(state.audio_encryption, old_encryption);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn session_replacement_retains_cleanup_for_surviving_handsets() {
    let mut controller = shared_inbound_controller();
    let replaced = DeviceId::new("SEP001122334455").unwrap();
    let survivor = DeviceId::new("SEP112233445566").unwrap();
    assert!(!controller.phone_answer(CallId(2)).is_empty());
    let generation = controller
        .registered_device(&replaced)
        .unwrap()
        .session_generation;

    let outcome = controller
        .register_session(
            SessionGeneration::new(generation.get() + 1).unwrap(),
            registration_for(replaced.as_str()),
        )
        .unwrap();

    assert!(outcome.replaced);
    assert!(outcome.cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(8)
        })
    )));
    assert!(outcome.cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            device_id,
            call_id: CallId(3),
            state: HandsetCallState::OnHook,
            stop_media: true,
        }) if device_id == &survivor
    )));
    assert!(!outcome.cleanup.iter().any(
        |effect| matches!(effect, DriverEffect::Handset(effect) if effect.device_id() == &replaced)
    ));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn additional_calls_keep_independent_identity_and_switch_in_exact_order() {
    let now = Instant::now();
    let mut controller = connected_outbound_controller();
    let device = binding().device_id;

    let created = controller
        .begin_additional_phone_call(CallId(2), binding(), Codec::Pcma, now)
        .unwrap();
    assert!(matches!(
        created.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(1)
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(1),
                state: HandsetCallState::Hold,
                stop_media: true,
                ..
            }),
            DriverEffect::Backend(PbxEffect::CreateChannel {
                call_id: PbxCallId(2),
                handset_call_id: CallId(2),
                ..
            }),
            ..
        ]
    ));
    assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Collecting
    );
    assert_eq!(
        controller.registered_device(&device).unwrap().active_call(),
        Some(CallId(2))
    );
    assert_ne!(
        controller.call(CallId(1)).unwrap().pbx_id,
        controller.call(CallId(2)).unwrap().pbx_id
    );

    let switched = controller.switch_active_call(&device, CallId(1)).unwrap();
    assert!(matches!(
        switched.first(),
        Some(DriverEffect::Backend(PbxEffect::Hold {
            call_id: PbxCallId(2)
        }))
    ));
    assert!(switched.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Resume {
            call_id: PbxCallId(1)
        })
    )));
    assert_eq!(
        controller.call(CallId(1)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(controller.call(CallId(2)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.registered_device(&device).unwrap().active_call(),
        Some(CallId(1))
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn answering_waiting_call_holds_active_call_and_stale_switch_is_non_mutating() {
    let mut controller = connected_outbound_controller();
    let device = binding().device_id;
    let offers = controller.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
        }],
    );
    assert_eq!(offers.len(), 1);

    let switched = controller.switch_active_call(&device, CallId(2)).unwrap();
    let backend = switched
        .iter()
        .filter_map(|effect| match effect {
            DriverEffect::Backend(effect) => Some(effect),
            DriverEffect::Handset(_) => None,
        })
        .collect::<Vec<_>>();
    assert!(matches!(
        backend.as_slice(),
        [PbxEffect::Hold {
            call_id: PbxCallId(1)
        }]
    ));
    assert!(
        controller
            .media_opened(CallId(2), test_media_endpoint(Codec::Pcma))
            .iter()
            .any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Answer {
                    call_id: PbxCallId(8)
                })
            ))
    );
    assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Connected
    );
    let snapshot = (
        controller.call(CallId(1)).unwrap().state,
        controller.call(CallId(2)).unwrap().state,
        controller.registered_device(&device).unwrap().active_call(),
    );
    assert_eq!(
        controller.switch_active_call(&device, CallId(999)),
        Err(CallSwitchRejection::Unavailable)
    );
    assert_eq!(
        snapshot,
        (
            controller.call(CallId(1)).unwrap().state,
            controller.call(CallId(2)).unwrap().state,
            controller.registered_device(&device).unwrap().active_call(),
        )
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn selection_is_device_scoped_and_removed_with_each_independent_call() {
    let now = Instant::now();
    let mut controller = Controller::new(Duration::from_secs(1));
    let device = binding().device_id;
    controller.registered(registration());
    controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, now);
    controller.begin_phone_call(CallId(2), binding(), Codec::Pcma, now);

    assert_eq!(
        controller.toggle_call_selected(&device, CallId(1)),
        Some(false)
    );
    assert_eq!(
        controller.toggle_call_selected(&device, CallId(1)),
        Some(true)
    );
    assert_eq!(
        controller.toggle_call_selected(&DeviceId::new("SEP112233445566").unwrap(), CallId(1)),
        None
    );
    controller.hangup(CallId(2));
    assert_eq!(
        controller.registered_device(&device).unwrap().active_call(),
        None
    );
    assert!(controller.call(CallId(1)).is_some());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn three_call_switch_and_cleanup_preserve_unrelated_selection_and_identity() {
    let mut controller = connected_outbound_controller();
    let device = binding().device_id;
    for (pbx_id, call_id) in [(PbxCallId(8), CallId(2)), (PbxCallId(9), CallId(3))] {
        controller.offer_inbound_call(
            pbx_id,
            [InboundAppearance {
                call_id,
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        assert!(controller.set_call_selected(&device, call_id, true));
    }

    let transition = controller
        .begin_active_call_switch_transaction(&device, CallId(2))
        .unwrap();
    for effect in &transition.effects {
        assert!(controller.record_call_transition_success(transition.id, effect));
    }
    assert!(controller.commit_call_transition(transition.id));
    controller
        .pbx_hangup_with_effects(PbxCallId(9))
        .expect("third call is independently addressable");

    assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.call(CallId(3)).is_none());
    let registered = controller.registered_device(&device).unwrap();
    assert_eq!(registered.active_call(), Some(CallId(2)));
    assert!(registered.is_call_selected(CallId(2)));
    assert!(!registered.is_call_selected(CallId(3)));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn hook_flash_resolves_connected_waiting_held_conference_and_duplicate_states() {
    let device = binding().device_id;
    let mut idle = Controller::new(Duration::from_secs(1));
    idle.registered(registration());
    assert_eq!(
        idle.hook_flash_action(&device, CallId(1)),
        HookFlashAction::Ignore
    );
    let mut connected = connected_outbound_controller();
    assert_eq!(
        connected.hook_flash_action(&device, CallId(1)),
        HookFlashAction::Transfer
    );
    assert_eq!(
        connected.hook_flash_action(&device, CallId(99)),
        HookFlashAction::Ignore
    );

    connected.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
        }],
    );
    assert_eq!(
        connected.hook_flash_action(&device, CallId(1)),
        HookFlashAction::AnswerWaiting(CallId(2))
    );
    assert!(!connected.hold(CallId(1)).is_empty());
    assert_eq!(
        connected.hook_flash_action(&device, CallId(1)),
        HookFlashAction::Ignore
    );

    let mut duplicate = connected_outbound_controller();
    duplicate
        .begin_transfer(TransferConsultationRequest {
            source_call_id: CallId(1),
            consultation_call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
            complete_on_hangup: false,
            now: Instant::now(),
        })
        .unwrap();
    assert_eq!(
        duplicate.hook_flash_action(&device, CallId(2)),
        HookFlashAction::Transfer
    );

    let conference = active_three_party_conference();
    assert_eq!(
        conference.hook_flash_action(&device, CallId(4)),
        HookFlashAction::Ignore
    );
    assert!(connected.invariant_error().is_none());
    assert!(duplicate.invariant_error().is_none());
    assert!(conference.invariant_error().is_none());
}

#[test]
fn additional_call_transaction_rolls_back_every_effect_boundary() {
    for fail_at in 0..5 {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        let transition = controller
            .begin_additional_phone_call_transaction(
                CallId(2),
                binding(),
                Codec::Pcma,
                Instant::now(),
            )
            .unwrap();
        let mut progress = CallTransitionProgress::default();
        for effect in transition.effects.iter().take(fail_at) {
            progress.record_success(&transition, effect);
            assert!(controller.record_call_transition_success(transition.id, effect));
        }
        let cleanup = controller.abort_call_transition(transition.id, &progress);
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected,
            "failed at {fail_at}"
        );
        assert_eq!(
            controller.registered_device(&device).unwrap().active_call(),
            Some(CallId(1)),
            "failed at {fail_at}"
        );
        assert!(controller.call(CallId(2)).is_none(), "failed at {fail_at}");
        assert_eq!(
            cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(1)
                })
            )),
            fail_at > 0,
            "failed at {fail_at}"
        );
        assert_eq!(
            cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                })
            )),
            fail_at > 2,
            "failed at {fail_at}"
        );
        assert!(cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(2),
                state: HandsetCallState::OnHook,
                ..
            })
        )));
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn hotline_routes_immediately_without_digit_collection_and_rolls_back_every_boundary() {
    for fail_at in 0..5 {
        let mut controller = connected_outbound_controller();
        let destination = HotlineDestination::new("9911").unwrap();
        let transition = controller
            .begin_hotline_call_transaction(HotlineCallRequest {
                handset_call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
                destination,
                now: Instant::now(),
            })
            .unwrap();
        assert_eq!(transition.effects.len(), 5);
        assert!(!transition.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Handset(HandsetEffect::StartTone {
                call_id: CallId(2),
                tone,
                ..
            })
                if *tone != Tone::Silence
        )));
        assert!(matches!(
            transition.effects.last(),
            Some(DriverEffect::Backend(PbxEffect::StartRouting {
                call_id: PbxCallId(2),
                context,
                destination,
            })) if context == "from-sccp" && destination == "9911"
        ));
        let call = controller.pbx_call(PbxCallId(2)).unwrap();
        assert_eq!(call.state, CallState::Calling);
        assert_eq!(call.digits, "9911");
        assert_eq!(call.digit_deadline, None);

        let mut progress = CallTransitionProgress::default();
        for effect in transition.effects.iter().take(fail_at) {
            progress.record_success(&transition, effect);
            assert!(controller.record_call_transition_success(transition.id, effect));
        }
        let cleanup = controller.abort_call_transition(transition.id, &progress);
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert_eq!(
            cleanup.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                })
            )),
            fail_at > 2,
            "failed at {fail_at}"
        );
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn hotline_routing_completed_after_disconnect_is_compensated_exactly_once() {
    let mut controller = connected_outbound_controller();
    let device = binding().device_id;
    let transition = controller
        .begin_hotline_call_transaction(HotlineCallRequest {
            handset_call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcma,
            destination: HotlineDestination::new("9911").unwrap(),
            now: Instant::now(),
        })
        .unwrap();
    for effect in transition.effects.iter().take(transition.effects.len() - 1) {
        assert!(controller.record_call_transition_success(transition.id, effect));
    }
    controller.disconnected(&device);
    let completed = transition.effects.last().unwrap();
    assert!(!controller.record_call_transition_success(transition.id, completed));
    let compensation =
        controller.compensate_unrecorded_call_transition_effect(&transition, completed);
    assert!(compensation.remove_target_channel);
    assert_eq!(
        compensation
            .effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                })
            ))
            .count(),
        1
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn active_switch_transaction_rolls_back_answer_and_preserves_unrelated_offer() {
    for fail_at in 0..=3 {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        controller.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        let transition = controller
            .begin_active_call_switch_transaction(&device, CallId(2))
            .unwrap();
        let mut progress = CallTransitionProgress::default();
        for effect in transition.effects.iter().take(fail_at) {
            progress.record_success(&transition, effect);
            assert!(controller.record_call_transition_success(transition.id, effect));
        }
        if fail_at == 2 {
            controller.offer_inbound_call(
                PbxCallId(20),
                [InboundAppearance {
                    call_id: CallId(20),
                    binding: binding(),
                    codec: Codec::Pcmu,
                }],
            );
        }
        let cleanup = controller.abort_call_transition(transition.id, &progress);
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert_eq!(
            controller.registered_device(&device).unwrap().active_call(),
            Some(CallId(1))
        );
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Ringing
        );
        assert!(cleanup.iter().all(|effect| !matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        if fail_at == 2 {
            assert!(controller.call(CallId(20)).is_some());
        }
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn successful_call_transition_commit_rejects_late_abort() {
    let mut controller = connected_outbound_controller();
    let transition = controller
        .begin_additional_phone_call_transaction(CallId(2), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    assert!(controller.commit_call_transition(transition.id));
    assert!(
        controller
            .abort_call_transition(transition.id, &CallTransitionProgress::default())
            .is_empty()
    );
    assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Collecting
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn call_transition_pbx_hangup_races_abort_without_resurrection() {
    for hung_up in [PbxCallId(1), PbxCallId(2)] {
        let mut controller = connected_outbound_controller();
        let transition = controller
            .begin_additional_phone_call_transaction(
                CallId(2),
                binding(),
                Codec::Pcma,
                Instant::now(),
            )
            .unwrap();
        let mut progress = CallTransitionProgress::default();
        for effect in transition.effects.iter().take(3) {
            progress.record_success(&transition, effect);
            assert!(controller.record_call_transition_success(transition.id, effect));
        }

        let outcome = controller
            .pbx_hangup_with_effects(hung_up)
            .expect("the racing PBX hangup is claimed");
        assert!(controller.primary_call_by_pbx(hung_up).is_none());
        assert!(
            controller
                .abort_call_transition(transition.id, &progress)
                .is_empty()
        );
        assert!(!outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(
                PbxEffect::Hold { call_id }
                    | PbxEffect::Resume { call_id }
                    | PbxEffect::Answer { call_id }
                    | PbxEffect::Hangup { call_id }
            ) if *call_id == hung_up
        )));
        if hung_up == PbxCallId(2) {
            assert_eq!(
                controller.call(CallId(1)).unwrap().state,
                CallState::Connected
            );
        } else {
            assert!(controller.call(CallId(1)).is_none());
            assert!(controller.call(CallId(2)).is_none());
            assert!(outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                })
            )));
        }
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn additional_call_compensates_each_effect_completed_after_cancellation() {
    for completed_at in 0..3 {
        let mut controller = connected_outbound_controller();
        let transition = controller
            .begin_additional_phone_call_transaction(
                CallId(2),
                binding(),
                Codec::Pcma,
                Instant::now(),
            )
            .unwrap();
        for effect in transition.effects.iter().take(completed_at) {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }

        controller
            .pbx_hangup_with_effects(PbxCallId(1))
            .expect("previous-leg hangup cancels the transition");
        let completed = transition.effects[completed_at].clone();
        assert!(!controller.record_call_transition_success(transition.id, &completed));
        let compensation =
            controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert_eq!(
            compensation.remove_target_channel,
            completed_at == 2,
            "completed effect {completed_at}"
        );
        assert_eq!(
            compensation.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                })
            )),
            completed_at == 2,
            "completed effect {completed_at}"
        );
        assert!(!compensation.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(
                PbxEffect::Hold { call_id }
                    | PbxEffect::Resume { call_id }
                    | PbxEffect::Answer { call_id }
                    | PbxEffect::Hangup { call_id }
            ) if *call_id == PbxCallId(1)
        )));
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn active_switch_compensates_each_effect_completed_after_target_hangup() {
    for completed_at in 0..3 {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        controller.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        let transition = controller
            .begin_active_call_switch_transaction(&device, CallId(2))
            .unwrap();
        for effect in transition.effects.iter().take(completed_at) {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }

        controller
            .pbx_hangup_with_effects(PbxCallId(8))
            .expect("target-leg hangup cancels the transition");
        let completed = transition.effects[completed_at].clone();
        assert!(!controller.record_call_transition_success(transition.id, &completed));
        let compensation =
            controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(!compensation.remove_target_channel);
        assert!(!compensation.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(
                PbxEffect::Hold { call_id }
                    | PbxEffect::Resume { call_id }
                    | PbxEffect::Answer { call_id }
                    | PbxEffect::Hangup { call_id }
            ) if *call_id == PbxCallId(8)
        )));
        assert_eq!(
            compensation.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(1)
                })
            )),
            completed_at == 0,
            "completed effect {completed_at}"
        );
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn active_switch_compensates_each_effect_completed_after_previous_hangup() {
    for completed_at in 0..3 {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        controller.offer_inbound_call(
            PbxCallId(8),
            [InboundAppearance {
                call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcma,
            }],
        );
        let transition = controller
            .begin_active_call_switch_transaction(&device, CallId(2))
            .unwrap();
        for effect in transition.effects.iter().take(completed_at) {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }

        controller
            .pbx_hangup_with_effects(PbxCallId(1))
            .expect("previous-leg hangup cancels the transition");
        let completed = transition.effects[completed_at].clone();
        assert!(!controller.record_call_transition_success(transition.id, &completed));
        let compensation =
            controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

        assert!(controller.call(CallId(1)).is_none());
        assert!(!compensation.remove_target_channel);
        assert!(compensation.effects.iter().all(|effect| !matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(8)
            })
        )));
        assert_eq!(
            controller.call(CallId(2)).unwrap().state,
            CallState::Ringing
        );
        assert!(!compensation.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(
                PbxEffect::Hold { call_id }
                    | PbxEffect::Resume { call_id }
                    | PbxEffect::Answer { call_id }
                    | PbxEffect::Hangup { call_id }
            ) if *call_id == PbxCallId(1)
        )));
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn held_switch_never_discards_the_existing_target_channel_on_abort() {
    let mut controller = connected_outbound_controller();
    let device = binding().device_id;
    controller
        .begin_additional_phone_call(CallId(2), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_answer(PbxCallId(2));
    let transition = controller
        .begin_active_call_switch_transaction(&device, CallId(1))
        .unwrap();
    let mut progress = CallTransitionProgress::default();
    for effect in transition.effects.iter().take(3) {
        progress.record_success(&transition, effect);
        assert!(controller.record_call_transition_success(transition.id, effect));
    }

    assert!(!transition.remove_target_channel_on_abort(&progress));
    let cleanup = controller.abort_call_transition(transition.id, &progress);
    assert!(cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hold {
            call_id: PbxCallId(1)
        })
    )));
    assert!(controller.call(CallId(1)).is_some());
    assert!(controller.call(CallId(2)).is_some());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn disconnect_invalidates_pending_transition_and_late_abort_is_idempotent() {
    let mut controller = connected_outbound_controller();
    let device = binding().device_id;
    let transition = controller
        .begin_additional_phone_call_transaction(CallId(2), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    let cleanup = controller.disconnected(&device);
    assert_eq!(
        cleanup
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
            .count(),
        2
    );
    assert!(
        controller
            .abort_call_transition(transition.id, &CallTransitionProgress::default())
            .is_empty()
    );
    assert!(controller.call(CallId(1)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn disconnect_compensates_each_effect_completed_after_transition_cancellation() {
    for completed_at in 0..3 {
        let mut controller = connected_outbound_controller();
        let device = binding().device_id;
        let transition = controller
            .begin_additional_phone_call_transaction(
                CallId(2),
                binding(),
                Codec::Pcma,
                Instant::now(),
            )
            .unwrap();
        for effect in transition.effects.iter().take(completed_at) {
            assert!(controller.record_call_transition_success(transition.id, effect));
        }

        controller.disconnected(&device);
        let completed = transition.effects[completed_at].clone();
        assert!(!controller.record_call_transition_success(transition.id, &completed));
        let compensation =
            controller.compensate_unrecorded_call_transition_effect(&transition, &completed);

        assert_eq!(
            compensation.remove_target_channel,
            completed_at == 2,
            "completed effect {completed_at}"
        );
        assert!(!compensation.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(
                PbxEffect::Hold { call_id }
                    | PbxEffect::Resume { call_id }
                    | PbxEffect::Answer { call_id }
                    | PbxEffect::Hangup { call_id }
            ) if *call_id == PbxCallId(1)
        )));
        assert!(controller.call(CallId(1)).is_none());
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn device_features_are_independent_of_registration_and_calls() {
    let mut controller = Controller::new(Duration::from_secs(1));
    let device = binding().device_id;
    controller.set_dnd(&device, DndMode::Silent);
    controller.set_privacy(&device, true);
    controller.set_forwarding(
        &device,
        ForwardingState {
            all: Some(forwarding("2000")),
            busy: None,
            no_answer: Some(forwarding("2001")),
        },
    );
    controller.set_feature_button(&device, 4, true);

    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    controller.disconnected(&device);

    assert_eq!(
        controller.feature_state(&device),
        Some(&DeviceFeatureState {
            dnd: DndMode::Silent,
            privacy: true,
            recording_armed: false,
            forwarding: ForwardingState {
                all: Some(forwarding("2000")),
                busy: None,
                no_answer: Some(forwarding("2001")),
            },
            buttons: HashMap::from([(4, true)]),
        })
    );
}

#[test]
fn complete_feature_reload_candidate_removes_stale_device_state() {
    let mut controller = Controller::new(Duration::from_secs(1));
    let removed = DeviceId::new("SEP001122334455").unwrap();
    let retained = DeviceId::new("SEP112233445566").unwrap();
    controller.set_dnd(&removed, DndMode::Reject);
    controller.set_privacy(&retained, true);
    let retained_state = controller.feature_state(&retained).unwrap().clone();

    controller.replace_feature_states(HashMap::from([(retained.clone(), retained_state.clone())]));

    assert_eq!(controller.feature_state(&removed), None);
    assert_eq!(controller.feature_state(&retained), Some(&retained_state));
}

#[test]
fn audio_acknowledgements_cannot_mutate_typed_video_state() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    let audio = MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 20000,
        rtcp_port: 20001,
        codec: Codec::Pcma,
        packet_ms: 20,
        max_frames_per_packet: 1,
        telephone_event_payload: 101,
    };
    let video = MediaEndpoint {
        address: "192.0.2.20".parse().unwrap(),
        rtp_port: 21000,
        rtcp_port: 21001,
        codec: Codec::H264,
        packet_ms: 0,
        max_frames_per_packet: 1,
        telephone_event_payload: 0,
    };

    controller.media_opened(CallId(2), audio);
    controller.media_opened(CallId(2), video);

    let call = controller.call(CallId(2)).unwrap();
    assert_eq!(call.audio, MediaStreamState::Open(audio));
    assert_eq!(
        call.video,
        VideoMediaState::AudioOnly(VideoFallbackReason::NotNegotiated)
    );
}

#[test]
fn video_lifecycle_requires_the_current_session_and_owned_codec() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let plan = test_video_plan(&controller, VideoMode::User);
    let session_generation = plan.session_generation;
    let stale_generation = SessionGeneration::new(session_generation.get() + 1).unwrap();
    assert!(controller.install_video_plan_for_device(
        &device_id,
        CallId(1),
        plan,
        VideoPlanReadiness::Ready,
    ));

    assert_eq!(
        controller.video_mode_for_device(&device_id, CallId(1)),
        [DriverEffect::Handset(HandsetEffect::OpenVideoReceive {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
        })]
    );
    assert!(!controller.video_receive_opened_for_device(
        &device_id,
        stale_generation,
        CallId(1),
        Codec::H264,
        test_video_endpoint(30_002),
    ));
    assert!(!controller.video_receive_opened_for_device(
        &device_id,
        session_generation,
        CallId(1),
        Codec::H263,
        test_video_endpoint(30_002),
    ));
    assert!(controller.video_receive_opened_for_device(
        &device_id,
        session_generation,
        CallId(1),
        Codec::H264,
        test_video_endpoint(30_002),
    ));
    assert_eq!(
        controller.begin_video_transmit_for_device(&device_id, session_generation, CallId(1),),
        [DriverEffect::Handset(HandsetEffect::StartVideoTransmit {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
        })]
    );
    assert!(controller.video_transmit_opened_for_device(
        &device_id,
        session_generation,
        CallId(1),
        Codec::H264,
        test_video_endpoint(30_004),
        PassthroughPartyId::new(41),
    ));
    let pbx_id = controller.call(CallId(1)).unwrap().pbx_id;
    assert_eq!(
        controller.refresh_video_for_pbx(pbx_id),
        [DriverEffect::Handset(HandsetEffect::RefreshVideo {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
            passthrough_party_id: PassthroughPartyId::new(41),
        })]
    );
    assert!(!controller.video_refresh_is_current(
        &device_id,
        session_generation,
        CallId(1),
        PassthroughPartyId::new(42),
    ));
    assert_eq!(
        controller.call(CallId(1)).unwrap().video,
        VideoMediaState::Ready {
            plan: test_video_plan(&controller, VideoMode::User),
            receive: VideoStreamState::Open {
                codec: Codec::H264,
                endpoint: test_video_endpoint(30_002),
            },
            transmit: VideoStreamState::Open {
                codec: Codec::H264,
                endpoint: test_video_endpoint(30_004),
            },
            transmit_token: Some(PassthroughPartyId::new(41)),
        }
    );
    assert_eq!(
        controller.video_mode_for_device(&device_id, CallId(1)),
        [DriverEffect::Handset(HandsetEffect::StopVideo {
            device_id,
            call_id: CallId(1),
            session_generation,
        })]
    );
    let state = &controller.call(CallId(1)).unwrap().video;
    assert_eq!(state.receive(), VideoStreamState::Closed);
    assert_eq!(state.transmit(), VideoStreamState::Closed);
    assert!(controller.refresh_video_for_pbx(pbx_id).is_empty());
}

#[test]
fn automatic_video_starts_only_after_audio_for_the_active_connected_call() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let plan = test_video_plan(&controller, VideoMode::Auto);
    let session_generation = plan.session_generation;
    assert!(controller.install_video_plan_for_device(
        &device_id,
        CallId(1),
        plan,
        VideoPlanReadiness::Ready,
    ));
    assert_eq!(
        controller.call(CallId(1)).unwrap().video.receive(),
        VideoStreamState::Closed
    );

    let effects = controller.media_opened(CallId(1), test_media_endpoint(Codec::Pcmu));
    assert!(
        effects.contains(&DriverEffect::Handset(HandsetEffect::OpenVideoReceive {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
        }))
    );
    let video = &controller.call(CallId(1)).unwrap().video;
    assert_eq!(video.receive(), VideoStreamState::Opening);
    assert_eq!(video.transmit(), VideoStreamState::Closed);
    assert!(
        controller
            .video_mode_for_device(&device_id, CallId(1))
            .is_empty()
    );
}

#[test]
fn user_video_rejects_foreign_calls_and_stale_pending_effects() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let plan = test_video_plan(&controller, VideoMode::User);
    let session_generation = plan.session_generation;
    assert!(controller.install_video_plan_for_device(
        &device_id,
        CallId(1),
        plan,
        VideoPlanReadiness::Ready,
    ));
    assert!(
        controller
            .video_mode_for_device(&device_id, CallId(99))
            .is_empty()
    );

    let open = HandsetEffect::OpenVideoReceive {
        device_id: device_id.clone(),
        call_id: CallId(1),
        session_generation,
    };
    assert_eq!(
        controller.video_mode_for_device(&device_id, CallId(1)),
        [DriverEffect::Handset(open.clone())]
    );
    assert!(
        controller
            .opening_video_receive_plan_for_device(&device_id, session_generation, CallId(1),)
            .is_some()
    );
    assert!(matches!(
        controller
            .video_mode_for_device(&device_id, CallId(1))
            .as_slice(),
        [DriverEffect::Handset(HandsetEffect::StopVideo { .. })]
    ));
    assert!(
        controller
            .opening_video_receive_plan_for_device(&device_id, session_generation, CallId(1),)
            .is_none()
    );
    assert_eq!(
        controller.recover_optional_video_effect_failure(&open),
        Some(Vec::new())
    );
    assert!(matches!(
        controller.call(CallId(1)).unwrap().video,
        VideoMediaState::Ready { .. }
    ));
}

#[test]
fn hold_closes_generation_owned_video_before_changing_presentation() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let plan = test_video_plan(&controller, VideoMode::User);
    let session_generation = plan.session_generation;
    assert!(controller.install_video_plan_for_device(
        &device_id,
        CallId(1),
        plan,
        VideoPlanReadiness::Ready,
    ));
    assert!(
        !controller
            .video_mode_for_device(&device_id, CallId(1))
            .is_empty()
    );

    let effects = controller.hold(CallId(1));
    assert!(
        effects.contains(&DriverEffect::Handset(HandsetEffect::StopVideo {
            device_id,
            call_id: CallId(1),
            session_generation,
        }))
    );
    let call = controller.call(CallId(1)).unwrap();
    assert_eq!(call.state, CallState::Held);
    assert_eq!(call.video.receive(), VideoStreamState::Closed);
    assert_eq!(call.video.transmit(), VideoStreamState::Closed);
}

#[test]
fn optional_video_failure_falls_back_without_terminating_audio() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let audio = test_media_endpoint(Codec::Pcmu);
    controller.media_opened(CallId(1), audio);
    let plan = test_video_plan(&controller, VideoMode::User);
    let session_generation = plan.session_generation;
    assert!(controller.install_video_plan_for_device(
        &device_id,
        CallId(1),
        plan,
        VideoPlanReadiness::Ready,
    ));
    assert!(
        !controller
            .video_mode_for_device(&device_id, CallId(1))
            .is_empty()
    );

    let begin = HandsetEffect::OpenVideoReceive {
        device_id: device_id.clone(),
        call_id: CallId(1),
        session_generation,
    };
    let stop = HandsetEffect::StopVideo {
        device_id: device_id.clone(),
        call_id: CallId(1),
        session_generation,
    };
    assert_eq!(
        controller.recover_optional_video_effect_failure(&begin),
        Some(vec![DriverEffect::Handset(HandsetEffect::StopVideo {
            device_id: device_id.clone(),
            call_id: CallId(1),
            session_generation,
        })])
    );
    let call = controller.call(CallId(1)).unwrap();
    assert_eq!(call.audio, MediaStreamState::Open(audio));
    assert_eq!(call.pbx_id, PbxCallId(1));
    assert_eq!(
        call.video,
        VideoMediaState::AudioOnly(VideoFallbackReason::ReceiveFailed)
    );
    assert!(controller.pbx_call(PbxCallId(1)).is_some());
    assert_eq!(
        controller.recover_optional_video_effect_failure(&stop),
        Some(Vec::new())
    );
    assert_eq!(
        controller.recover_optional_video_effect_failure(&HandsetEffect::StartTone {
            device_id,
            call_id: CallId(1),
            tone: Tone::Silence,
        }),
        None
    );
    assert!(controller.call(CallId(1)).is_some());
}

#[test]
fn blocked_video_mode_is_an_audio_only_noop() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let plan = test_video_plan(&controller, VideoMode::User);
    assert!(controller.install_video_plan_for_device(
        &device_id,
        CallId(1),
        plan.clone(),
        VideoPlanReadiness::Blocked(VideoFallbackReason::DescriptorUnavailable),
    ));

    assert!(
        controller
            .video_mode_for_device(&device_id, CallId(1))
            .is_empty()
    );
    assert_eq!(
        controller.call(CallId(1)).unwrap().video,
        VideoMediaState::Blocked {
            plan,
            reason: VideoFallbackReason::DescriptorUnavailable,
        }
    );
}

#[test]
fn direct_transfer_pairs_exact_selected_held_and_connected_calls() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(2));
    controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    controller.hold(CallId(2));
    controller.begin_asterisk_call(CallId(3), PbxCallId(9), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(3));
    controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcma));
    controller.set_call_selected(&binding().device_id, CallId(2), true);
    controller.set_call_selected(&binding().device_id, CallId(3), true);
    let plan = controller.direct_transfer(&binding().device_id).unwrap();
    assert_eq!(
        plan.effects,
        [DriverEffect::Backend(PbxEffect::Transfer {
            operation: plan.completion.clone(),
        })]
    );
    assert_eq!(plan.completion.source.pbx_call_id, PbxCallId(8));
    assert_eq!(plan.completion.consultation.pbx_call_id, PbxCallId(9));
    assert_eq!(
        plan.completion.kind,
        crate::call::transfer::TransferCompletionKind::Direct
    );
}

fn begin_test_transfer(
    controller: &mut Controller,
    complete_on_hangup: bool,
) -> (TransferId, Vec<DriverEffect>) {
    let effects = controller
        .begin_transfer(TransferConsultationRequest {
            source_call_id: CallId(1),
            consultation_call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcmu,
            complete_on_hangup,
            now: Instant::now(),
        })
        .unwrap();
    let transaction_id = controller.transfer_transaction(CallId(1)).unwrap().id;
    (transaction_id, effects)
}

fn record_test_transfer_progress(
    controller: &mut Controller,
    device_id: &DeviceId,
    transaction_id: TransferId,
    progress: &TransferExecutionProgress,
) {
    for milestone in [
        TransferSetupMilestone::SourceBackendHeld,
        TransferSetupMilestone::SourceHandsetHeld,
        TransferSetupMilestone::ConsultationChannelCreated,
        TransferSetupMilestone::ConsultationHandsetStarted,
    ] {
        if progress.completed(milestone) {
            controller
                .transfer_setup_completed(device_id, transaction_id, milestone)
                .unwrap();
        }
    }
}

fn completed_transfer_progress() -> TransferExecutionProgress {
    TransferExecutionProgress::with_completed([
        TransferSetupMilestone::SourceBackendHeld,
        TransferSetupMilestone::SourceHandsetHeld,
        TransferSetupMilestone::ConsultationChannelCreated,
        TransferSetupMilestone::ConsultationHandsetStarted,
    ])
}

#[test]
fn consultation_transfer_keeps_distinct_identities_and_exact_setup_order() {
    let mut controller = connected_outbound_controller();
    let (_, effects) = begin_test_transfer(&mut controller, true);
    assert!(matches!(
        effects.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(1)
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(1),
                state: HandsetCallState::Hold,
                stop_media: true,
                ..
            }),
            DriverEffect::Backend(PbxEffect::CreateConsultationChannel {
                source_call_id: PbxCallId(1),
                handset_call_id: CallId(2),
                call_id: PbxCallId(2),
                ..
            }),
            DriverEffect::Handset(HandsetEffect::BeginTransfer {
                source_call_id: CallId(1),
                consultation_call_id: CallId(2),
                ..
            }),
        ]
    ));
    let transaction = controller.transfer_transaction(CallId(2)).unwrap();
    assert_eq!(transaction.source.handset_call_id, CallId(1));
    assert_eq!(transaction.source.pbx_call_id, PbxCallId(1));
    assert_eq!(transaction.consultation.unwrap().handset_call_id, CallId(2));
    assert!(transaction.complete_on_hangup);
    assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::TransferCollecting
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn consultation_setup_failure_rolls_back_only_completed_effects_once() {
    for progress in [
        TransferExecutionProgress::default(),
        TransferExecutionProgress::with_completed([TransferSetupMilestone::SourceBackendHeld]),
        completed_transfer_progress(),
    ] {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, false);
        record_test_transfer_progress(&mut controller, &device_id, transaction_id, &progress);
        let outcome = controller
            .abort_transfer(
                &device_id,
                transaction_id,
                TransferCancellationReason::ConsultationFailure,
            )
            .unwrap();
        assert_eq!(
            outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Resume {
                    call_id: PbxCallId(1)
                })
            )),
            progress.completed(TransferSetupMilestone::SourceBackendHeld)
        );
        assert_eq!(
            outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(2)
                })
            )),
            progress.completed(TransferSetupMilestone::ConsultationChannelCreated)
        );
        assert_eq!(
            outcome.effects.iter().any(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id: CallId(2),
                    state: HandsetCallState::OnHook,
                    ..
                })
            )),
            progress.completed(TransferSetupMilestone::ConsultationHandsetStarted)
        );
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.transfer_transaction(CallId(1)).is_none());
        assert_eq!(
            controller.abort_transfer(
                &device_id,
                transaction_id,
                TransferCancellationReason::ConsultationFailure,
            ),
            Err(TransferRejection::Conflict)
        );
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn blind_transfer_commits_only_after_exact_backend_completion_identity() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let (transaction_id, _) = begin_test_transfer(&mut controller, false);
    let consultation = TransferLeg {
        handset_call_id: CallId(2),
        pbx_call_id: PbxCallId(2),
    };
    controller
        .transfers
        .get_mut(&device_id)
        .unwrap()
        .advance_consultation(consultation, TransferPhase::Routing)
        .unwrap();
    controller
        .transfers
        .get_mut(&device_id)
        .unwrap()
        .advance_consultation(consultation, TransferPhase::Ringing)
        .unwrap();
    let plan = controller
        .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
        .unwrap();
    assert_eq!(plan.completion.transaction_id, transaction_id);
    assert_eq!(
        plan.completion.kind,
        crate::call::transfer::TransferCompletionKind::Blind
    );
    assert!(controller.call(CallId(1)).is_some());
    assert!(controller.call(CallId(2)).is_some());
    assert!(
        controller
            .transfer_succeeded(&device_id, TransferId(transaction_id.0 + 1))
            .is_none()
    );

    let outcome = controller
        .transfer_succeeded(&device_id, transaction_id)
        .unwrap();
    assert_eq!(outcome.effects.len(), 2);
    assert!(outcome.effects.iter().all(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        })
    )));
    assert!(controller.call(CallId(1)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert!(
        controller
            .transfer_succeeded(&device_id, transaction_id)
            .is_none()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn device_transfer_completion_accepts_either_leg_or_an_omitted_reference() {
    for reported in [Some(CallId(1)), Some(CallId(2)), Some(CallId(0)), None] {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, false);
        let consultation = TransferLeg {
            handset_call_id: CallId(2),
            pbx_call_id: PbxCallId(2),
        };
        let transaction = controller.transfers.get_mut(&device_id).unwrap();
        transaction
            .advance_consultation(consultation, TransferPhase::Routing)
            .unwrap();
        transaction
            .advance_consultation(consultation, TransferPhase::Ringing)
            .unwrap();

        let plan = controller
            .complete_device_transfer(&device_id, reported, TransferTrigger::TransferKey)
            .unwrap();
        assert_eq!(plan.completion.transaction_id, transaction_id);
        assert_eq!(plan.completion.consultation, consultation);
    }

    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let (_, _) = begin_test_transfer(&mut controller, false);
    assert_eq!(
        controller.complete_device_transfer(
            &device_id,
            Some(CallId(99)),
            TransferTrigger::TransferKey,
        ),
        Err(TransferRejection::WrongCall)
    );
    assert_eq!(
        controller
            .transfer_transaction_for_device(&device_id)
            .unwrap()
            .phase,
        TransferPhase::Collecting
    );
}

#[test]
fn transfer_destination_progress_and_answer_choose_blind_then_attended_kind() {
    let device_id = binding().device_id;

    let mut blind = connected_outbound_controller();
    begin_test_transfer(&mut blind, false);
    assert_outbound_route(&blind.enbloc(CallId(2), "2200".into()), "2200");
    assert_eq!(
        blind.transfer_transaction(CallId(2)).unwrap().phase,
        TransferPhase::Routing
    );
    blind.pbx_progress(PbxCallId(2), false);
    assert_eq!(
        blind
            .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
            .unwrap()
            .completion
            .kind,
        crate::call::transfer::TransferCompletionKind::Blind
    );

    let mut attended = connected_outbound_controller();
    begin_test_transfer(&mut attended, false);
    attended.enbloc(CallId(2), "2200".into());
    attended.pbx_answer(PbxCallId(2));
    assert_eq!(
        attended
            .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
            .unwrap()
            .completion
            .kind,
        crate::call::transfer::TransferCompletionKind::Attended
    );
}

#[test]
fn transfer_consultation_pbx_hangup_restores_source_once() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let (transaction_id, _) = begin_test_transfer(&mut controller, true);
    record_test_transfer_progress(
        &mut controller,
        &device_id,
        transaction_id,
        &completed_transfer_progress(),
    );
    controller.enbloc(CallId(2), "2200".into());
    let outcome = controller.pbx_hangup_with_effects(PbxCallId(2)).unwrap();
    assert_eq!(outcome.primary.unwrap().sccp_id, CallId(2));
    assert!(outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Resume {
            call_id: PbxCallId(1)
        })
    )));
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(2)
        })
    )));
    assert!(outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(2),
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        })
    )));
    assert!(
        !outcome
            .effects
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Transfer { .. })))
    );
    assert_eq!(
        controller.call(CallId(1)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.transfer_transaction(CallId(1)).is_none());
    assert!(controller.pbx_hangup_with_effects(PbxCallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn transfer_source_pbx_hangup_cancels_consultation_without_restoring_source() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let (transaction_id, _) = begin_test_transfer(&mut controller, false);
    record_test_transfer_progress(
        &mut controller,
        &device_id,
        transaction_id,
        &completed_transfer_progress(),
    );
    let outcome = controller.pbx_hangup_with_effects(PbxCallId(1)).unwrap();
    assert_eq!(outcome.primary.unwrap().sccp_id, CallId(1));
    assert!(outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(2)
        })
    )));
    assert!(
        !outcome
            .effects
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
    );
    assert!(controller.call(CallId(1)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn transfer_completion_claim_defers_late_pbx_hangup_until_commit() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let (transaction_id, _) = begin_test_transfer(&mut controller, false);
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_progress(PbxCallId(2), false);
    controller
        .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
        .unwrap();

    let late = controller.pbx_hangup_with_effects(PbxCallId(2)).unwrap();
    assert!(late.effects.is_empty());
    assert!(controller.call(CallId(1)).is_some());
    assert!(controller.call(CallId(2)).is_some());
    assert_eq!(
        controller.transfer_transaction(CallId(2)).unwrap().phase,
        TransferPhase::Completing
    );
    assert!(
        controller
            .transfer_transaction(CallId(2))
            .unwrap()
            .consultation_terminated
    );
    assert!(
        controller
            .transfer_succeeded(&device_id, transaction_id)
            .is_some()
    );
    assert!(controller.call(CallId(1)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn consultation_hangup_during_completion_is_not_hung_up_twice_on_backend_failure() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let (transaction_id, _) = begin_test_transfer(&mut controller, false);
    record_test_transfer_progress(
        &mut controller,
        &device_id,
        transaction_id,
        &completed_transfer_progress(),
    );
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_progress(PbxCallId(2), false);
    controller
        .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
        .unwrap();
    assert!(
        controller
            .pbx_hangup_with_effects(PbxCallId(2))
            .unwrap()
            .effects
            .is_empty()
    );

    let outcome = controller
        .abort_transfer(
            &device_id,
            transaction_id,
            TransferCancellationReason::BackendFailure,
        )
        .unwrap();
    assert!(outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Resume {
            call_id: PbxCallId(1)
        })
    )));
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(2)
        })
    )));
    assert_eq!(
        controller.call(CallId(1)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn source_hangup_during_completion_removes_source_on_backend_failure() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    let (transaction_id, _) = begin_test_transfer(&mut controller, false);
    record_test_transfer_progress(
        &mut controller,
        &device_id,
        transaction_id,
        &completed_transfer_progress(),
    );
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_progress(PbxCallId(2), false);
    controller
        .complete_transfer(&device_id, CallId(2), TransferTrigger::TransferKey)
        .unwrap();
    assert!(
        controller
            .pbx_hangup_with_effects(PbxCallId(1))
            .unwrap()
            .effects
            .is_empty()
    );

    let outcome = controller
        .abort_transfer(
            &device_id,
            transaction_id,
            TransferCancellationReason::BackendFailure,
        )
        .unwrap();
    assert!(
        !outcome
            .effects
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
    );
    assert!(outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(2)
        })
    )));
    assert!(controller.call(CallId(1)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn transfer_on_hangup_policy_is_snapshotted_and_phase_gated() {
    let device_id = binding().device_id;

    let mut disabled = connected_outbound_controller();
    let (disabled_id, _) = begin_test_transfer(&mut disabled, false);
    disabled.enbloc(CallId(2), "2200".into());
    disabled.pbx_progress(PbxCallId(2), false);
    assert_eq!(
        disabled.complete_transfer(&device_id, CallId(2), TransferTrigger::ConsultationHangup,),
        Err(TransferRejection::HangupCompletionDisabled)
    );
    assert_eq!(
        disabled.transfer_transaction(CallId(2)).unwrap().phase,
        TransferPhase::Ringing
    );
    disabled
        .abort_transfer(
            &device_id,
            disabled_id,
            TransferCancellationReason::ConsultationHangup,
        )
        .unwrap();

    let mut ineligible = connected_outbound_controller();
    let (ineligible_id, _) = begin_test_transfer(&mut ineligible, true);
    assert_eq!(
        ineligible.complete_transfer(&device_id, CallId(2), TransferTrigger::ConsultationHangup,),
        Err(TransferRejection::InvalidPhase)
    );
    ineligible
        .abort_transfer(
            &device_id,
            ineligible_id,
            TransferCancellationReason::ConsultationHangup,
        )
        .unwrap();

    for (answered, expected) in [
        (false, crate::call::transfer::TransferCompletionKind::Blind),
        (
            true,
            crate::call::transfer::TransferCompletionKind::Attended,
        ),
    ] {
        let mut enabled = connected_outbound_controller();
        begin_test_transfer(&mut enabled, true);
        enabled.enbloc(CallId(2), "2200".into());
        if answered {
            enabled.pbx_answer(PbxCallId(2));
        } else {
            enabled.pbx_progress(PbxCallId(2), false);
        }
        assert_eq!(
            enabled
                .complete_transfer(&device_id, CallId(2), TransferTrigger::ConsultationHangup,)
                .unwrap()
                .completion
                .kind,
            expected
        );
    }
}

#[test]
fn transfer_end_call_and_source_resume_cancel_and_restore_source() {
    for reason in [
        TransferCancellationReason::EndCall,
        TransferCancellationReason::SourceResume,
    ] {
        let mut controller = connected_outbound_controller();
        let device_id = binding().device_id;
        let (transaction_id, _) = begin_test_transfer(&mut controller, true);
        controller.enbloc(CallId(2), "2200".into());
        record_test_transfer_progress(
            &mut controller,
            &device_id,
            transaction_id,
            &completed_transfer_progress(),
        );
        let outcome = controller
            .abort_transfer(&device_id, transaction_id, reason)
            .unwrap();
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1)
            })
        )));
        assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(2)
            })
        )));
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn direct_transfer_rejects_non_exact_selection_without_mutation() {
    let mut controller = connected_outbound_controller();
    let device_id = binding().device_id;
    controller.set_call_selected(&device_id, CallId(1), false);
    assert_eq!(
        controller.direct_transfer(&device_id),
        Err(TransferRejection::InvalidSelection)
    );
    assert_eq!(
        controller.call(CallId(1)).unwrap().state,
        CallState::Connected
    );
    controller.set_call_selected(&device_id, CallId(1), true);
    assert_eq!(
        controller.direct_transfer(&device_id),
        Err(TransferRejection::InvalidSelection)
    );
    assert!(controller.transfer_transaction(CallId(1)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn direct_transfer_rejects_three_or_cross_device_selections_without_mutation() {
    let mut controller = connected_outbound_controller();
    let first_device = binding().device_id;
    controller.registered(registration_for("SEP112233445566"));
    controller.begin_asterisk_call(
        CallId(2),
        PbxCallId(8),
        &binding_for("SEP001122334455", 1),
        Codec::Pcma,
    );
    controller.phone_answer(CallId(2));
    controller.hold(CallId(2));
    controller.begin_asterisk_call(
        CallId(3),
        PbxCallId(9),
        &binding_for("SEP001122334455", 1),
        Codec::Pcma,
    );
    controller.phone_answer(CallId(3));
    for call_id in [CallId(1), CallId(2), CallId(3)] {
        controller.set_call_selected(&first_device, call_id, true);
    }
    let before = [
        controller.call(CallId(1)).unwrap().state,
        controller.call(CallId(2)).unwrap().state,
        controller.call(CallId(3)).unwrap().state,
    ];
    assert_eq!(
        controller.direct_transfer(&first_device),
        Err(TransferRejection::InvalidSelection)
    );
    assert_eq!(
        [
            controller.call(CallId(1)).unwrap().state,
            controller.call(CallId(2)).unwrap().state,
            controller.call(CallId(3)).unwrap().state,
        ],
        before
    );

    controller.set_call_selected(&first_device, CallId(3), false);
    controller.begin_asterisk_call(
        CallId(4),
        PbxCallId(10),
        &binding_for("SEP112233445566", 2),
        Codec::Pcma,
    );
    controller.phone_answer(CallId(4));
    controller.set_call_selected(&first_device, CallId(2), false);
    assert!(!controller.set_call_selected(&first_device, CallId(4), true));
    assert_eq!(
        controller.direct_transfer(&first_device),
        Err(TransferRejection::InvalidSelection)
    );
    assert!(controller.transfer_transaction(CallId(1)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn direct_transfer_backend_failure_preserves_selection_and_allows_retry() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(2));
    controller.media_opened(CallId(2), test_media_endpoint(Codec::Pcma));
    controller.hold(CallId(2));
    controller.begin_asterisk_call(CallId(3), PbxCallId(9), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(3));
    controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcma));
    let device_id = binding().device_id;
    controller.set_call_selected(&device_id, CallId(2), true);
    controller.set_call_selected(&device_id, CallId(3), true);

    let first = controller.direct_transfer(&device_id).unwrap();
    let outcome = controller
        .abort_transfer(
            &device_id,
            first.completion.transaction_id,
            TransferCancellationReason::BackendFailure,
        )
        .unwrap();
    assert!(outcome.effects.is_empty());
    assert_eq!(controller.call(CallId(2)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(3)).unwrap().state,
        CallState::Connected
    );
    assert!(
        controller
            .registered_device(&device_id)
            .unwrap()
            .is_call_selected(CallId(2))
    );
    assert!(
        controller
            .registered_device(&device_id)
            .unwrap()
            .is_call_selected(CallId(3))
    );
    assert!(controller.direct_transfer(&device_id).is_ok());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn direct_transfer_hangup_race_removes_only_the_terminated_leg_on_failure() {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(2));
    controller.hold(CallId(2));
    controller.begin_asterisk_call(CallId(3), PbxCallId(9), &binding(), Codec::Pcma);
    controller.phone_answer(CallId(3));
    let device_id = binding().device_id;
    controller.set_call_selected(&device_id, CallId(2), true);
    controller.set_call_selected(&device_id, CallId(3), true);

    let plan = controller.direct_transfer(&device_id).unwrap();
    assert!(
        controller
            .pbx_hangup_with_effects(PbxCallId(8))
            .unwrap()
            .effects
            .is_empty()
    );
    let outcome = controller
        .abort_transfer(
            &device_id,
            plan.completion.transaction_id,
            TransferCancellationReason::BackendFailure,
        )
        .unwrap();
    assert!(outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(2),
            state: HandsetCallState::OnHook,
            ..
        })
    )));
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.call(CallId(3)).is_some());
    assert!(
        controller
            .registered_device(&device_id)
            .unwrap()
            .is_call_selected(CallId(3))
    );
    assert!(controller.transfer_transaction(CallId(3)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn pbx_state_and_device_appearance_states_transition_separately() {
    let mut controller = Controller::new(Duration::from_secs(1));
    let first = binding_for("SEP001122334455", 1);
    let second = binding_for("SEP112233445566", 2);
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &first, Codec::Pcma);
    let second_appearance = controller
        .add_call_appearance(PbxCallId(8), CallId(3), &second, Codec::Pcmu)
        .unwrap();

    let pbx_call = controller.pbx_call(PbxCallId(8)).unwrap();
    assert_eq!(pbx_call.appearance_ids().count(), 2);
    assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 2);
    assert_eq!(
        controller.appearances_for_device(&first.device_id).count(),
        1
    );
    assert_eq!(
        controller.appearances_for_device(&second.device_id).count(),
        1
    );
    assert_eq!(
        controller
            .call_appearance(second_appearance)
            .unwrap()
            .sccp_id,
        CallId(3)
    );

    controller.phone_answer(CallId(3));
    controller.media_opened(CallId(3), test_media_endpoint(Codec::Pcmu));
    assert_eq!(
        controller.pbx_call(PbxCallId(8)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(
        controller.appearance_for_call(CallId(2)).unwrap().state,
        CallState::RemoteInUse
    );
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::Connected
    );

    assert!(controller.pbx_answer(PbxCallId(8)).is_empty());
    controller.hold(CallId(3));
    assert_eq!(
        controller.pbx_call(PbxCallId(8)).unwrap().state,
        CallState::Held
    );
    assert_eq!(
        controller.appearance_for_call(CallId(2)).unwrap().state,
        CallState::SharedHeld
    );
    assert_eq!(
        controller.appearance_for_call(CallId(3)).unwrap().state,
        CallState::Held
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn appearance_and_pbx_indexes_are_cleaned_at_their_own_lifetimes() {
    let mut controller = Controller::new(Duration::from_secs(1));
    let first = binding_for("SEP001122334455", 1);
    let second = binding_for("SEP112233445566", 2);
    controller.registered(registration_for(first.device_id.as_str()));
    controller.registered(registration_for(second.device_id.as_str()));
    controller.begin_asterisk_call(CallId(2), PbxCallId(8), &first, Codec::Pcma);
    controller
        .add_call_appearance(PbxCallId(8), CallId(3), &second, Codec::Pcmu)
        .unwrap();
    controller.set_call_selected(&first.device_id, CallId(2), true);
    controller.set_call_selected(&second.device_id, CallId(3), true);

    assert!(controller.disconnected(&second.device_id).is_empty());
    assert!(controller.pbx_call(PbxCallId(8)).is_some());
    assert!(controller.call(CallId(2)).is_some());
    assert!(controller.call(CallId(3)).is_none());
    assert!(controller.appearance_for_call(CallId(3)).is_none());
    assert_eq!(controller.appearances_for_pbx(PbxCallId(8)).count(), 1);
    assert_eq!(
        controller.appearances_for_device(&second.device_id).count(),
        0
    );
    assert!(controller.invariant_error().is_none());

    let removed = controller.pbx_hangup(PbxCallId(8)).unwrap();
    assert_eq!(removed.state, CallState::Ended);
    assert!(controller.pbx_call(PbxCallId(8)).is_none());
    assert!(controller.call(CallId(2)).is_none());
    assert_eq!(controller.calls().count(), 0);
    assert_eq!(
        controller.appearances_for_device(&first.device_id).count(),
        0
    );
    assert_eq!(
        controller
            .registered_device(&first.device_id)
            .unwrap()
            .selected_calls()
            .count(),
        0
    );
    assert!(controller.invariant_error().is_none());
}

fn connected_outbound_controller() -> Controller {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    controller.begin_phone_call(CallId(1), binding(), Codec::Pcmu, Instant::now());
    controller.enbloc(CallId(1), "2100".into());
    controller.pbx_answer(PbxCallId(1));
    assert_eq!(
        controller.call(CallId(1)).unwrap().state,
        CallState::Connected
    );
    controller
}

#[test]
fn conference_identity_uses_the_remote_party_for_each_direction() {
    let mut outbound = connected_outbound_controller();
    let outbound_info = CallInfo {
        direction: CallDirection::Outbound,
        calling_name: "Local desk".into(),
        calling_number: "1001".into(),
        called_name: "Remote destination".into(),
        called_number: "2200".into(),
        ..CallInfo::default()
    };
    outbound.set_call_info(CallId(1), outbound_info);
    let appearance = outbound.appearance_for_call(CallId(1)).unwrap().clone();
    let participant = outbound.conference_participant(&appearance, true);
    assert_eq!(participant.display_name, "Remote destination");
    assert_eq!(participant.number, "2200");

    let mut inbound = shared_inbound_controller();
    let inbound_info = CallInfo {
        direction: CallDirection::Inbound,
        calling_name: "Remote caller".into(),
        calling_number: "3300".into(),
        called_name: "Local desk".into(),
        called_number: "1001".into(),
        ..CallInfo::default()
    };
    inbound.set_call_info(CallId(2), inbound_info);
    let appearance = inbound.appearance_for_call(CallId(2)).unwrap().clone();
    let participant = inbound.conference_participant(&appearance, true);
    assert_eq!(participant.display_name, "Remote caller");
    assert_eq!(participant.number, "3300");
}

#[test]
fn conference_identity_is_empty_for_each_presentation_restriction() {
    let mut controller = connected_outbound_controller();
    controller.set_call_info(
        CallId(1),
        CallInfo {
            direction: CallDirection::Outbound,
            called_name: "Private destination".into(),
            called_number: "4400".into(),
            ..CallInfo::default()
        },
    );
    let call = controller.pbx_call(PbxCallId(1)).unwrap().clone();
    let appearance = controller.appearance_for_call(CallId(1)).unwrap().clone();

    let mut private_call = call.clone();
    private_call.privacy = true;
    assert_eq!(
        conference_participant_identity(&private_call, &appearance),
        ConferenceParticipantIdentity::default()
    );

    let mut private_appearance = appearance.clone();
    private_appearance.privacy = true;
    assert_eq!(
        conference_participant_identity(&call, &private_appearance),
        ConferenceParticipantIdentity::default()
    );

    let mut restricted = appearance;
    restricted.info.party_restrictions = 1;
    assert_eq!(
        conference_participant_identity(&call, &restricted),
        ConferenceParticipantIdentity::default()
    );
}

#[test]
fn connected_line_updates_refresh_identity_without_reopening_the_list() {
    let mut controller = connected_outbound_controller();
    controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    let initial_ids = controller
        .conference_session(CallId(2))
        .unwrap()
        .participants
        .iter()
        .map(|participant| participant.id)
        .collect::<Vec<_>>();

    let effects = controller.set_call_info(
        CallId(2),
        CallInfo {
            direction: CallDirection::Outbound,
            called_name: "Consulted party".into(),
            called_number: "2200".into(),
            ..CallInfo::default()
        },
    );
    assert!(matches!(
        effects.as_slice(),
        [DriverEffect::Handset(HandsetEffect::SetCallInfo {
            call_id: CallId(2),
            ..
        })]
    ));

    let effects = controller.update_call_info_by_pbx(PbxCallId(1), |current| {
        let mut updated = current.clone();
        updated.called_name = "Updated original party".into();
        updated.called_number = "2101".into();
        updated
    });
    assert!(matches!(
        effects.as_slice(),
        [DriverEffect::Handset(HandsetEffect::SetCallInfo {
            call_id: CallId(1),
            ..
        })]
    ));

    let session = controller.conference_session(CallId(2)).unwrap();
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        initial_ids
    );
    let original = session.participants.by_pbx(PbxCallId(1)).unwrap();
    assert_eq!(original.display_name, "Updated original party");
    assert_eq!(original.number, "2101");
    let consultation = session.participants.by_pbx(PbxCallId(2)).unwrap();
    assert_eq!(consultation.display_name, "Consulted party");
    assert_eq!(consultation.number, "2200");

    assert!(controller.set_call_privacy(CallId(1), true));
    let hidden = controller
        .conference_session(CallId(2))
        .unwrap()
        .participants
        .by_pbx(PbxCallId(1))
        .unwrap();
    assert!(hidden.display_name.is_empty());
    assert!(hidden.number.is_empty());
    assert!(controller.set_call_privacy(CallId(1), false));
    let restored = controller
        .conference_session(CallId(2))
        .unwrap()
        .participants
        .by_pbx(PbxCallId(1))
        .unwrap();
    assert_eq!(restored.display_name, "Updated original party");
    assert_eq!(restored.number, "2101");

    controller.update_call_info_by_pbx(PbxCallId(2), |current| {
        let mut restricted = current.clone();
        restricted.party_restrictions = 1;
        restricted
    });
    let hidden = controller
        .conference_session(CallId(2))
        .unwrap()
        .participants
        .by_pbx(PbxCallId(2))
        .unwrap();
    assert!(hidden.display_name.is_empty());
    assert!(hidden.number.is_empty());
    assert_eq!(hidden.id, initial_ids[1]);
}

#[test]
fn pending_invite_keeps_its_identity_update_when_merged() {
    let mut controller = connected_outbound_controller();
    controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_answer(PbxCallId(2));
    controller.confirm_conference(CallId(2)).unwrap();
    assert!(controller.conference_merged(CallId(2)));
    controller
        .begin_conference_invite(CallId(1), CallId(3), binding(), Codec::Pcmu, Instant::now())
        .unwrap();
    let participant_id = controller
        .conference_session(CallId(3))
        .unwrap()
        .pending_invite
        .as_ref()
        .unwrap()
        .participant
        .id;

    controller.set_call_info(
        CallId(3),
        CallInfo {
            direction: CallDirection::Outbound,
            called_name: "Invited party".into(),
            called_number: "3300".into(),
            ..CallInfo::default()
        },
    );
    controller.pbx_answer(PbxCallId(3));
    controller.confirm_conference_invite(CallId(3)).unwrap();
    assert!(controller.conference_invite_merged(CallId(3)));

    let participant = controller
        .conference_session(CallId(3))
        .unwrap()
        .participants
        .by_pbx(PbxCallId(3))
        .unwrap();
    assert_eq!(participant.id, participant_id);
    assert_eq!(participant.display_name, "Invited party");
    assert_eq!(participant.number, "3300");
}

#[test]
fn passive_remote_hangup_detaches_pbx_before_bounded_tone_cleanup() {
    let mut controller = connected_outbound_controller();
    let now = Instant::now();
    let plan = controller
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();

    assert!(plan.pending.is_some());
    assert!(controller.pbx_call(PbxCallId(1)).is_none());
    assert!(controller.call(CallId(1)).is_none());
    assert_eq!(
        plan.outcome.effects,
        vec![
            HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(1),
                state: HandsetCallState::Connected,
                stop_media: true,
            }
            .into(),
            HandsetEffect::StartTone {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(1),
                tone: Tone::Zip,
            }
            .into(),
        ]
    );
    assert!(
        controller
            .expire_remote_hangups(now + Duration::from_secs(14))
            .is_empty()
    );
    assert_eq!(
        controller.expire_remote_hangups(now + Duration::from_secs(15)),
        vec![
            HandsetEffect::SetCallState {
                device_id: DeviceId::new("SEP001122334455").unwrap(),
                call_id: CallId(1),
                state: HandsetCallState::OnHook,
                stop_media: true,
            }
            .into()
        ]
    );
    assert!(
        controller
            .expire_remote_hangups(now + Duration::from_secs(16))
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn passive_remote_hangup_delays_only_the_exact_shared_active_owner() {
    let mut controller = shared_inbound_controller();
    controller.phone_answer(CallId(3));
    let plan = controller
        .begin_remote_hangup(
            PbxCallId(8),
            Some(Tone::Zip),
            Duration::from_secs(15),
            Instant::now(),
        )
        .unwrap();

    assert!(plan.pending.is_some());
    assert!(plan.outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(2),
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        })
    )));
    assert!(!plan.outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(3),
            state: HandsetCallState::OnHook,
            ..
        })
    )));
    assert!(plan.outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::StartTone {
            call_id: CallId(3),
            tone: Tone::Zip,
            ..
        })
    )));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn passive_remote_hangup_is_immediate_when_disabled_held_or_generation_exhausted() {
    let now = Instant::now();
    let mut disabled = connected_outbound_controller();
    let disabled = disabled
        .begin_remote_hangup(PbxCallId(1), None, Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(disabled.pending, None);
    assert!(disabled.outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(1),
            state: HandsetCallState::OnHook,
            ..
        })
    )));

    let mut held = connected_outbound_controller();
    held.hold(CallId(1));
    let held = held
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(held.pending, None);

    let mut ringing = shared_inbound_controller();
    let ringing = ringing
        .begin_remote_hangup(PbxCallId(8), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(ringing.pending, None);

    let mut waiting = connected_outbound_controller();
    waiting.offer_inbound_call(
        PbxCallId(8),
        [InboundAppearance {
            call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcmu,
        }],
    );
    let waiting_plan = waiting
        .begin_remote_hangup(PbxCallId(8), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(waiting_plan.pending, None);
    assert_eq!(
        waiting_plan.outcome.primary.unwrap().state,
        CallState::Ended
    );
    assert_eq!(waiting.call(CallId(1)).unwrap().state, CallState::Connected);

    let mut transfer = connected_outbound_controller();
    transfer
        .begin_transfer(TransferConsultationRequest {
            source_call_id: CallId(1),
            consultation_call_id: CallId(2),
            binding: binding(),
            codec: Codec::Pcmu,
            complete_on_hangup: false,
            now,
        })
        .unwrap();
    let transfer = transfer
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(transfer.pending, None);

    let mut conference = connected_outbound_controller();
    conference
        .begin_conference(CallId(1), CallId(2), binding(), Codec::Pcmu, now, true)
        .unwrap();
    conference.enbloc(CallId(2), "2200".into());
    conference.pbx_answer(PbxCallId(2));
    conference.confirm_conference(CallId(2)).unwrap();
    let conference = conference
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(conference.pending, None);

    let mut exhausted = connected_outbound_controller();
    exhausted.next_remote_hangup_generation = u64::MAX;
    let exhausted_plan = exhausted
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(exhausted_plan.pending, None);
    assert_eq!(exhausted.next_remote_hangup_generation, u64::MAX);
}

#[test]
fn passive_remote_hangup_cancel_disconnect_and_unload_are_exactly_once() {
    let now = Instant::now();
    let mut physical = connected_outbound_controller();
    let token = physical
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap()
        .pending
        .unwrap();
    assert_eq!(physical.hangup(CallId(1)).len(), 1);
    assert!(physical.complete_remote_hangup_token(token).is_none());
    assert!(physical.hangup(CallId(1)).is_empty());

    let mut disconnected = connected_outbound_controller();
    let token = disconnected
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap()
        .pending
        .unwrap();
    assert!(
        disconnected
            .disconnected(&DeviceId::new("SEP001122334455").unwrap())
            .is_empty()
    );
    assert!(disconnected.complete_remote_hangup_token(token).is_none());

    let mut shutdown = connected_outbound_controller();
    shutdown
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap();
    assert_eq!(shutdown.drain_remote_hangups().len(), 1);
    assert!(shutdown.drain_remote_hangups().is_empty());

    let mut presentation_failure = connected_outbound_controller();
    let token = presentation_failure
        .begin_remote_hangup(PbxCallId(1), Some(Tone::Zip), Duration::from_secs(15), now)
        .unwrap()
        .pending
        .unwrap();
    assert!(matches!(
        presentation_failure.complete_remote_hangup_token(token),
        Some(DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(1),
            state: HandsetCallState::OnHook,
            stop_media: true,
            ..
        }))
    ));
    assert!(
        presentation_failure
            .complete_remote_hangup_token(token)
            .is_none()
    );
    assert!(presentation_failure.drain_remote_hangups().is_empty());
}

#[test]
fn consultation_conference_holds_original_and_creates_one_typed_call() {
    let mut controller = connected_outbound_controller();
    let effects = controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();

    assert!(matches!(
        effects.first(),
        Some(DriverEffect::Backend(PbxEffect::Hold {
            call_id: PbxCallId(1)
        }))
    ));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::CreateChannel {
            handset_call_id: CallId(2),
            call_id: PbxCallId(2),
            ..
        })
    )));
    let session = controller.conference_session(CallId(2)).unwrap();
    assert_eq!(session.id, ConferenceId::new(1));
    assert_eq!(session.bridge_id, PbxBridgeId(1));
    assert_eq!(session.phase, ConferencePhase::Consultation);
    assert_eq!(controller.call(CallId(1)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Collecting
    );
    assert_eq!(
        controller.begin_conference(
            CallId(1),
            CallId(3),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        ),
        Err(ConferenceRejection::NotConnected)
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn conference_confirm_merges_both_call_bridges_and_destroys_once_on_end() {
    let mut controller = connected_outbound_controller();
    controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_answer(PbxCallId(2));

    let effects = controller.confirm_conference(CallId(2)).unwrap();
    assert_eq!(
        effects,
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Create {
                    bridge_id: PbxBridgeId(1),
                },
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeConsultation {
                    bridge_id: PbxBridgeId(1),
                    original_call_id: PbxCallId(1),
                    consultation_call_id: PbxCallId(2),
                },
            }),
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1),
            }),
        ]
    );
    assert!(controller.conference_merged(CallId(2)));
    assert_eq!(
        controller.conference_session(CallId(2)).unwrap().phase,
        ConferencePhase::Active
    );

    let cleanup = controller.end_conference(CallId(2));
    assert_eq!(
        cleanup
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        cleanup
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
            .count(),
        2
    );
    assert!(controller.calls().next().is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn destination_dialing_does_not_mutate_an_active_adhoc_conference() {
    let mut controller = connected_outbound_controller();
    controller
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    controller.enbloc(CallId(2), "2200".into());
    controller.pbx_answer(PbxCallId(2));
    controller.confirm_conference(CallId(2)).unwrap();
    let before = controller.conference_session(CallId(1)).unwrap().clone();
    controller.begin_phone_call(CallId(3), binding(), Codec::Pcmu, Instant::now());

    assert_eq!(
        controller.begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(3),
            destination: "700".into(),
            application_options: "Mac".into(),
        }),
        Err(ConferenceDestinationRejection::Conflict)
    );
    let after = controller.conference_session(CallId(1)).unwrap();
    assert_eq!(after.id, before.id);
    assert_eq!(after.bridge_id, before.bridge_id);
    assert_eq!(after.participants, before.participants);
    assert_eq!(
        controller.call(CallId(3)).unwrap().state,
        CallState::Collecting
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn cancelling_or_failing_consultation_restores_original_without_hangup() {
    for bridge_created in [false, true] {
        let mut controller = connected_outbound_controller();
        controller
            .begin_conference(
                CallId(1),
                CallId(2),
                binding(),
                Codec::Pcmu,
                Instant::now(),
                true,
            )
            .unwrap();
        let cleanup = if bridge_created {
            controller.abort_conference(CallId(2), true, true, true, true)
        } else {
            controller.hangup(CallId(2))
        };
        assert!(cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(1)
            })
        )));
        assert!(!cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(1)
            })
        )));
        assert_eq!(
            cleanup
                .iter()
                .filter(|effect| matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                ))
                .count(),
            usize::from(bridge_created)
        );
        assert_eq!(
            controller.call(CallId(1)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.call(CallId(2)).is_none());
        assert!(controller.conference_session(CallId(1)).is_none());
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn disabled_conference_does_not_mutate_the_connected_call() {
    let mut controller = connected_outbound_controller();
    assert_eq!(
        controller.begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            false,
        ),
        Err(ConferenceRejection::Disabled)
    );
    assert_eq!(
        controller.call(CallId(1)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.call(CallId(2)).is_none());
    assert!(controller.invariant_error().is_none());
}

fn three_call_conference_controller() -> Controller {
    let mut controller = Controller::new(Duration::from_secs(1));
    controller.registered(registration());
    for (handset, pbx) in [(2, 8), (3, 9), (4, 10)] {
        controller.begin_asterisk_call(CallId(handset), pbx.into(), &binding(), Codec::Pcma);
        controller.phone_answer(CallId(handset));
        controller.media_opened(CallId(handset), test_media_endpoint(Codec::Pcma));
        if handset != 4 {
            controller.hold(CallId(handset));
        }
    }
    controller
}

#[test]
fn join_uses_exact_multi_selection_with_stable_participant_ids() {
    let mut controller = three_call_conference_controller();
    let device = binding().device_id;
    controller.set_call_selected(&device, CallId(2), true);

    let effects = controller.join_calls(&device, CallId(4), true).unwrap();
    assert_eq!(
        effects,
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Create {
                    bridge_id: PbxBridgeId(1),
                },
            }),
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeCalls {
                    bridge_id: PbxBridgeId(1),
                    call_ids: vec![PbxCallId(10), PbxCallId(8)],
                },
            }),
        ]
    );
    let session = controller.conference_session(CallId(4)).unwrap();
    assert_eq!(session.origin, ConferenceOrigin::Selection);
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| {
                (
                    participant.id,
                    participant.pbx_call_id,
                    participant.moderator,
                )
            })
            .collect::<Vec<_>>(),
        [
            (ParticipantId::new(1), PbxCallId(10), true),
            (ParticipantId::new(2), PbxCallId(8), false),
        ]
    );
    assert!(controller.conference_session(CallId(3)).is_none());
    assert!(controller.conference_merged(CallId(4)));
    assert_eq!(
        controller
            .conference_session(CallId(2))
            .unwrap()
            .participants
            .moderator()
            .unwrap()
            .handset_call_id,
        CallId(4)
    );
    let json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(2)).unwrap()).unwrap();
    assert_eq!(json["moderator_id"], 1);
    assert_eq!(json["participants"].as_array().unwrap().len(), 2);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn join_without_multi_selection_uses_all_eligible_calls_and_rolls_back() {
    let mut controller = three_call_conference_controller();
    let device = binding().device_id;

    let effects = controller.join_calls(&device, CallId(4), true).unwrap();
    assert!(matches!(
        effects.last(),
        Some(DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::MergeCalls { call_ids, .. },
        })) if call_ids == &[PbxCallId(10), PbxCallId(8), PbxCallId(9)]
    ));
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
            .count(),
        2
    );
    let rollback = controller.abort_join_conference(CallId(4), true, &[PbxCallId(8), PbxCallId(9)]);
    assert_eq!(
        rollback,
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy {
                    bridge_id: PbxBridgeId(1),
                },
            }),
            DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(9),
            }),
        ]
    );
    assert!(controller.conference_session(CallId(4)).is_none());
    assert_eq!(
        controller.pbx_call(PbxCallId(8)).unwrap().state,
        CallState::Held
    );
    assert_eq!(
        controller.pbx_call(PbxCallId(10)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn selection_toggle_rejects_cross_device_and_is_deterministic() {
    let mut controller = three_call_conference_controller();
    let device = binding().device_id;
    let other = DeviceId::new("SEP112233445566").unwrap();
    controller.registered(registration_for(other.as_str()));

    assert_eq!(
        controller.toggle_call_selected(&device, CallId(4)),
        Some(false)
    );
    assert_eq!(
        controller.toggle_call_selected(&device, CallId(4)),
        Some(true)
    );
    assert_eq!(controller.toggle_call_selected(&other, CallId(4)), None);
}

fn active_three_party_conference() -> Controller {
    let mut controller = three_call_conference_controller();
    let device = binding().device_id;
    controller.join_calls(&device, CallId(4), true).unwrap();
    assert!(controller.conference_merged(CallId(4)));
    controller
}

fn active_three_party_conference_with_media() -> Controller {
    let mut controller = three_call_conference_controller();
    let device = binding().device_id;
    controller.join_calls(&device, CallId(4), true).unwrap();
    assert!(controller.configure_conference_media(
        CallId(4),
        ConferenceMediaPolicy {
            music_on_hold_class: Some("office".into()),
            mute_on_entry: false,
            play_general_announcements: true,
            play_participant_announcements: true,
        },
    ));
    assert!(controller.conference_merged(CallId(4)));
    controller
}

fn mute_on_entry_policy(enabled: bool) -> ConferenceMediaPolicy {
    ConferenceMediaPolicy {
        music_on_hold_class: None,
        mute_on_entry: enabled,
        play_general_announcements: false,
        play_participant_announcements: false,
    }
}

fn participant_muted(controller: &Controller, call_id: CallId, participant_id: u32) -> bool {
    let json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(call_id).unwrap()).unwrap();
    json["participants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|participant| participant["id"] == participant_id)
        .unwrap()["muted"]
        .as_bool()
        .unwrap()
}

#[test]
fn mute_on_entry_consultation_is_ordered_and_commits_only_after_all_effects() {
    for enabled in [false, true] {
        let mut controller = connected_outbound_controller();
        controller
            .begin_conference_with_media(
                ConferenceConsultationRequest {
                    original_call_id: CallId(1),
                    consultation_call_id: CallId(2),
                    binding: binding(),
                    codec: Codec::Pcmu,
                    now: Instant::now(),
                    permitted: true,
                },
                mute_on_entry_policy(enabled),
            )
            .unwrap();
        controller.enbloc(CallId(2), "2200".into());
        controller.pbx_answer(PbxCallId(2));

        assert!(!participant_muted(&controller, CallId(2), 2));
        let effects = controller.confirm_conference(CallId(2)).unwrap();
        assert!(matches!(
            effects.get(1),
            Some(DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeConsultation { .. }
            }))
        ));
        assert_eq!(
            effects.get(3),
            enabled.then_some(&DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    bridge_id: PbxBridgeId(1),
                    participant_id: ParticipantId::new(2),
                    call_id: PbxCallId(2),
                    muted: true,
                },
            }))
        );
        assert!(!participant_muted(&controller, CallId(2), 2));
        assert!(controller.conference_merged(CallId(2)));
        assert_eq!(participant_muted(&controller, CallId(2), 2), enabled);
        assert!(controller.invariant_error().is_none());
    }

    let mut failed = connected_outbound_controller();
    failed
        .begin_conference_with_media(
            ConferenceConsultationRequest {
                original_call_id: CallId(1),
                consultation_call_id: CallId(2),
                binding: binding(),
                codec: Codec::Pcmu,
                now: Instant::now(),
                permitted: true,
            },
            mute_on_entry_policy(true),
        )
        .unwrap();
    failed.enbloc(CallId(2), "2200".into());
    failed.pbx_answer(PbxCallId(2));
    let effects = failed.confirm_conference(CallId(2)).unwrap();
    assert_eq!(effects.len(), 4);
    assert!(!participant_muted(&failed, CallId(2), 2));
    let cleanup = failed.abort_conference(CallId(2), true, true, true, true);
    assert!(cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(failed.conference_session(CallId(1)).is_none());
    assert_eq!(failed.call(CallId(1)).unwrap().state, CallState::Connected);
    assert!(failed.invariant_error().is_none());
}

#[test]
fn mute_on_entry_selection_covers_exact_and_all_members_with_rollback() {
    let device = binding().device_id;
    let mut selected = three_call_conference_controller();
    selected.set_call_selected(&device, CallId(2), true);
    let effects = selected
        .join_calls_with_media(&device, CallId(4), true, mute_on_entry_policy(true))
        .unwrap();
    assert!(matches!(
        effects.get(effects.len() - 2),
        Some(DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::MergeCalls { .. }
        }))
    ));
    assert!(matches!(
        effects.last(),
        Some(DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                participant_id,
                call_id: PbxCallId(8),
                muted: true,
                ..
            }
        })) if *participant_id == ParticipantId::new(2)
    ));
    assert!(!participant_muted(&selected, CallId(4), 2));
    assert!(selected.conference_merged(CallId(4)));
    assert!(participant_muted(&selected, CallId(4), 2));

    let mut all = three_call_conference_controller();
    let effects = all
        .join_calls_with_media(&device, CallId(4), true, mute_on_entry_policy(true))
        .unwrap();
    let merge_index = effects
        .iter()
        .position(|effect| {
            matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::MergeCalls { .. }
                })
            )
        })
        .unwrap();
    assert_eq!(
        &effects[merge_index + 1..],
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    bridge_id: PbxBridgeId(1),
                    participant_id: ParticipantId::new(2),
                    call_id: PbxCallId(8),
                    muted: true,
                },
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                    bridge_id: PbxBridgeId(1),
                    participant_id: ParticipantId::new(3),
                    call_id: PbxCallId(9),
                    muted: true,
                },
            }),
        ]
    );
    assert!(!participant_muted(&all, CallId(4), 2));
    assert!(!participant_muted(&all, CallId(4), 3));
    let rollback = all.abort_join_conference(CallId(4), true, &[PbxCallId(8), PbxCallId(9)]);
    assert!(rollback.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(all.conference_session(CallId(4)).is_none());
    assert_eq!(all.call(CallId(2)).unwrap().state, CallState::Held);
    assert_eq!(all.call(CallId(3)).unwrap().state, CallState::Held);
    assert!(all.invariant_error().is_none());
}

#[test]
fn mute_on_entry_invite_is_deferred_and_abort_preserves_published_state() {
    fn pending_invite() -> Controller {
        let device = binding().device_id;
        let mut controller = three_call_conference_controller();
        controller
            .join_calls_with_media(&device, CallId(4), true, mute_on_entry_policy(true))
            .unwrap();
        assert!(controller.conference_merged(CallId(4)));
        controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        controller.enbloc(CallId(5), "2300".into());
        controller.pbx_answer(PbxCallId(11));
        controller
    }

    let mut failed = pending_invite();
    let before = failed.conference_json(CallId(4)).unwrap();
    let effects = failed.confirm_conference_invite(CallId(5)).unwrap();
    assert!(matches!(
        effects.get(effects.len() - 2),
        Some(DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                call_id: PbxCallId(11),
                ..
            }
        }))
    ));
    assert!(matches!(
        effects.last(),
        Some(DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                participant_id,
                call_id: PbxCallId(11),
                muted: true,
                ..
            }
        })) if *participant_id == ParticipantId::new(4)
    ));
    assert_eq!(failed.conference_json(CallId(4)).unwrap(), before);
    failed.abort_conference_invite(CallId(5), true, true, true);
    assert_eq!(failed.conference_json(CallId(4)).unwrap(), before);
    assert!(
        failed
            .conference_session(CallId(4))
            .unwrap()
            .pending_invite
            .is_none()
    );

    let mut succeeded = pending_invite();
    let before = succeeded.conference_json(CallId(4)).unwrap();
    succeeded.confirm_conference_invite(CallId(5)).unwrap();
    assert_eq!(succeeded.conference_json(CallId(4)).unwrap(), before);
    assert!(succeeded.conference_invite_merged(CallId(5)));
    assert!(participant_muted(&succeeded, CallId(4), 4));
    assert_eq!(
        succeeded
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .iter()
            .len(),
        4
    );
    assert!(failed.invariant_error().is_none());
    assert!(succeeded.invariant_error().is_none());
}

#[test]
fn fake_handset_consultation_confirm_cancel_and_invite_transcripts_are_exact() {
    let mut cancelled = connected_outbound_controller();
    let mut handset = FakeHandsets::default();
    let start = cancelled
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    handset.apply(&start);
    assert_eq!(
        handset.call_states(),
        [(CallId(1), HandsetCallState::Hold, true)]
    );
    assert!(handset.call_info(CallId(2)).is_empty());
    assert!(handset.tones(CallId(2)).is_empty());
    assert_eq!(
        cancelled.confirm_conference(CallId(2)),
        Err(ConferenceRejection::NotConnected)
    );

    handset.clear();
    handset.apply(&cancelled.cancel_conference(CallId(2)));
    assert_eq!(
        handset.call_states(),
        [(CallId(2), HandsetCallState::OnHook, true)]
    );
    assert_eq!(handset.media_winners(), [CallId(1)]);
    assert_eq!(
        cancelled.call(CallId(1)).unwrap().state,
        CallState::Connected
    );
    assert!(cancelled.call(CallId(2)).is_none());
    assert!(cancelled.invariant_error().is_none());

    let mut merged = connected_outbound_controller();
    merged
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    merged.enbloc(CallId(2), "2200".into());
    assert_eq!(
        merged.confirm_conference(CallId(2)),
        Err(ConferenceRejection::NotConnected)
    );
    merged.pbx_answer(PbxCallId(2));
    let effects = merged.confirm_conference(CallId(2)).unwrap();
    handset.clear();
    handset.apply(&effects);
    assert!(handset.effects.is_empty());
    assert!(merged.conference_merged(CallId(2)));
    let stable_ids = merged
        .conference_session(CallId(2))
        .unwrap()
        .participants
        .iter()
        .map(|participant| participant.id)
        .collect::<Vec<_>>();
    handset.apply(&merged.end_conference(CallId(2)));
    assert_eq!(
        handset.call_states(),
        [
            (CallId(1), HandsetCallState::OnHook, true),
            (CallId(2), HandsetCallState::OnHook, true),
        ]
    );
    assert_eq!(stable_ids, [ParticipantId::new(1), ParticipantId::new(2)]);
    assert!(merged.invariant_error().is_none());

    let mut invited = active_three_party_conference_with_media();
    let conference_id = invited.conference_session(CallId(4)).unwrap().id;
    handset.clear();
    let invite = invited
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    handset.apply(&invite);
    assert_eq!(
        handset.call_states(),
        [(CallId(4), HandsetCallState::Hold, true)]
    );
    assert!(handset.call_info(CallId(5)).is_empty());
    assert!(handset.tones(CallId(5)).is_empty());
    assert_eq!(
        invited.confirm_conference_invite(CallId(5)),
        Err(ConferenceRejection::NotConnected)
    );
    handset.clear();
    handset.apply(&invited.abort_conference_invite(CallId(5), true, true, true));
    assert_eq!(
        handset.call_states(),
        [(CallId(5), HandsetCallState::OnHook, true)]
    );
    assert_eq!(handset.media_winners(), [CallId(4)]);
    let restored = invited.conference_session_by_id(conference_id).unwrap();
    assert_eq!(restored.participants.iter().len(), 3);
    assert!(restored.pending_invite.is_none());
    assert!(invited.invariant_error().is_none());

    let mut completed_invite = active_three_party_conference_with_media();
    let completed_id = completed_invite.conference_session(CallId(4)).unwrap().id;
    completed_invite
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    completed_invite.enbloc(CallId(5), "2300".into());
    completed_invite.pbx_answer(PbxCallId(11));
    handset.clear();
    handset.apply(
        &completed_invite
            .confirm_conference_invite(CallId(5))
            .unwrap(),
    );
    assert!(handset.effects.is_empty());
    assert!(completed_invite.conference_invite_merged(CallId(5)));
    handset.apply(&completed_invite.conference_announcement_effects(
        completed_id,
        ConferenceAnnouncement::ParticipantJoined(ParticipantId::new(4)),
    ));
    assert_eq!(
        handset.announcements(),
        [(
            completed_id,
            vec![
                ParticipantId::new(1),
                ParticipantId::new(2),
                ParticipantId::new(3),
                ParticipantId::new(4),
            ],
            vec![PbxCallId(10), PbxCallId(8), PbxCallId(9), PbxCallId(11)],
            ConferenceAnnouncement::ParticipantJoined(ParticipantId::new(4)),
        )]
    );
    let completed = completed_invite
        .conference_session_by_id(completed_id)
        .unwrap();
    assert_eq!(completed.participants.iter().len(), 4);
    assert_eq!(
        completed
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [
            ParticipantId::new(1),
            ParticipantId::new(2),
            ParticipantId::new(3),
            ParticipantId::new(4),
        ]
    );
    assert!(completed_invite.invariant_error().is_none());
}

#[test]
fn fake_handset_selected_and_all_call_join_paths_preserve_exact_membership() {
    let device = binding().device_id;
    let mut selected = three_call_conference_controller();
    selected.set_call_selected(&device, CallId(2), true);
    let effects = selected.join_calls(&device, CallId(4), true).unwrap();
    let mut handset = FakeHandsets::default();
    handset.apply(&effects);
    assert!(handset.effects.is_empty());
    assert!(selected.conference_merged(CallId(4)));
    let session = selected.conference_session(CallId(4)).unwrap().clone();
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| (participant.id, participant.handset_call_id))
            .collect::<Vec<_>>(),
        [
            (ParticipantId::new(1), CallId(4)),
            (ParticipantId::new(2), CallId(2)),
        ]
    );
    handset.apply(&selected.end_conference(CallId(4)));
    assert_eq!(
        handset.call_states(),
        [
            (CallId(4), HandsetCallState::OnHook, true),
            (CallId(2), HandsetCallState::OnHook, true),
        ]
    );
    assert!(selected.call(CallId(3)).is_some());
    assert!(selected.invariant_error().is_none());

    let mut all = three_call_conference_controller();
    let effects = all.join_calls(&device, CallId(4), true).unwrap();
    handset.clear();
    handset.apply(&effects);
    assert!(handset.effects.is_empty());
    let session = all.conference_session(CallId(4)).unwrap();
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| (participant.id, participant.handset_call_id))
            .collect::<Vec<_>>(),
        [
            (ParticipantId::new(1), CallId(4)),
            (ParticipantId::new(2), CallId(2)),
            (ParticipantId::new(3), CallId(3)),
        ]
    );
    handset.apply(&all.abort_join_conference(CallId(4), true, &[PbxCallId(8), PbxCallId(9)]));
    assert!(handset.effects.is_empty());
    assert!(all.conference_session(CallId(4)).is_none());
    assert_eq!(all.call(CallId(2)).unwrap().state, CallState::Held);
    assert_eq!(all.call(CallId(3)).unwrap().state, CallState::Held);
    assert_eq!(all.call(CallId(4)).unwrap().state, CallState::Connected);
    assert!(all.invariant_error().is_none());
}

#[test]
fn fake_handset_participant_controls_commit_typed_ui_only_after_success() {
    let mut controller = active_three_party_conference_with_media();
    let device = binding().device_id;
    let other_device = DeviceId::new("SEP112233445566").unwrap();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    let original_ids = controller
        .conference_session(CallId(4))
        .unwrap()
        .participants
        .iter()
        .map(|participant| participant.id)
        .collect::<Vec<_>>();
    let initial_json = controller.conference_json(CallId(4)).unwrap();
    let mut handset = FakeHandsets::default();

    for rejection in [
        controller.begin_conference_participant_mute(
            &other_device,
            conference_id,
            ParticipantId::new(2),
            true,
        ),
        controller.begin_conference_participant_mute(
            &device,
            conference_id,
            ParticipantId::new(99),
            true,
        ),
        controller.begin_conference_participant_mute(
            &device,
            conference_id,
            ParticipantId::new(1),
            true,
        ),
    ] {
        assert!(rejection.is_err());
    }
    assert_eq!(controller.conference_json(CallId(4)).unwrap(), initial_json);

    let mute = controller
        .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
        .unwrap();
    handset.apply(&mute);
    assert!(handset.effects.is_empty());
    assert_eq!(controller.conference_json(CallId(4)).unwrap(), initial_json);
    assert!(controller.conference_participant_muted(conference_id, ParticipantId::new(2), true,));
    handset.apply(&controller.conference_announcement_effects(
        conference_id,
        ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
    ));
    assert_eq!(
        handset.announcements(),
        [(
            conference_id,
            vec![ParticipantId::new(2)],
            vec![PbxCallId(8)],
            ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
        )]
    );

    handset.clear();
    controller
        .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), false)
        .unwrap();
    assert!(controller.conference_participant_muted(conference_id, ParticipantId::new(2), false,));
    handset.apply(&controller.conference_announcement_effects(
        conference_id,
        ConferenceAnnouncement::ParticipantUnmuted(ParticipantId::new(2)),
    ));
    assert_eq!(
        handset.announcements(),
        [(
            conference_id,
            vec![ParticipantId::new(2)],
            vec![PbxCallId(8)],
            ConferenceAnnouncement::ParticipantUnmuted(ParticipantId::new(2)),
        )]
    );

    assert!(
        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(2),
                true,
            )
            .unwrap()
            .is_empty()
    );
    assert!(controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(2),
        true,
    ));
    assert!(
        controller
            .begin_conference_participant_role_change(
                &device,
                conference_id,
                ParticipantId::new(1),
                false,
            )
            .unwrap()
            .is_empty()
    );
    assert!(controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(1),
        false,
    ));
    let role_json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(role_json["moderator_id"], 2);
    assert_eq!(role_json["participants"][0]["moderator"], false);
    assert_eq!(role_json["participants"][1]["moderator"], true);
    assert_eq!(
        controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        original_ids
    );

    controller
        .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(3))
        .unwrap();
    handset.clear();
    handset.apply(
        &controller
            .conference_participant_removed(conference_id, ParticipantId::new(3))
            .unwrap(),
    );
    assert_eq!(
        handset.call_states(),
        [(CallId(3), HandsetCallState::OnHook, true)]
    );
    let removed_json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(removed_json["participants"].as_array().unwrap().len(), 2);
    assert_eq!(removed_json["participants"][0]["id"], 1);
    assert_eq!(removed_json["participants"][1]["id"], 2);

    handset.clear();
    handset.apply(
        &controller
            .end_conference_by_moderator(&device, conference_id)
            .unwrap(),
    );
    assert_eq!(
        handset.call_states(),
        [
            (CallId(4), HandsetCallState::OnHook, true),
            (CallId(2), HandsetCallState::OnHook, true),
        ]
    );
    assert!(controller.conference_json(CallId(4)).is_none());
    assert_eq!(
        controller.end_conference_by_moderator(&device, conference_id),
        Err(ConferenceEndRejection::Unavailable)
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn fake_handset_hold_departure_failure_destination_and_shutdown_are_idempotent() {
    let mut handset = FakeHandsets::default();
    let mut held = active_three_party_conference_with_media();
    let conference_id = held.conference_session(CallId(4)).unwrap().id;
    let stable = held
        .conference_session(CallId(4))
        .map(|session| (session.bridge_id, session.participants.clone()))
        .unwrap();
    handset.apply(
        &held
            .begin_conference_moderator_leg_transition(CallId(4), true)
            .unwrap(),
    );
    assert_eq!(
        handset.call_states(),
        [(CallId(4), HandsetCallState::Hold, true)]
    );
    assert!(
        held.conference_moderator_leg_transitioned(conference_id, ParticipantId::new(1), true,)
    );
    handset.clear();
    handset.apply(
        &held
            .begin_conference_moderator_leg_transition(CallId(4), false)
            .unwrap(),
    );
    assert_eq!(handset.media_winners(), [CallId(4)]);
    assert!(held.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        false,
    ));
    let resumed = held.conference_session_by_id(conference_id).unwrap();
    assert_eq!(resumed.bridge_id, stable.0);
    assert_eq!(resumed.participants, stable.1);

    let device = binding().device_id;
    held.begin_conference_participant_role_change(
        &device,
        conference_id,
        ParticipantId::new(2),
        true,
    )
    .unwrap();
    assert!(held.conference_participant_role_changed(conference_id, ParticipantId::new(2), true,));
    handset.clear();
    let departure = held.pbx_hangup_with_effects(PbxCallId(10)).unwrap();
    handset.apply(&departure.effects);
    assert_eq!(
        handset.call_states(),
        [(CallId(4), HandsetCallState::OnHook, true)]
    );
    assert_eq!(
        handset.announcements(),
        [(
            conference_id,
            vec![ParticipantId::new(2), ParticipantId::new(3)],
            vec![PbxCallId(8), PbxCallId(9)],
            ConferenceAnnouncement::ModeratorDeparted(ParticipantId::new(1)),
        )]
    );
    let survivors = held.conference_session_by_id(conference_id).unwrap();
    assert_eq!(survivors.bridge_id, stable.0);
    assert_eq!(
        survivors
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [ParticipantId::new(2), ParticipantId::new(3)]
    );
    assert!(held.pbx_hangup_with_effects(PbxCallId(10)).is_none());

    let mut destination = connected_outbound_controller();
    destination.begin_phone_call(CallId(2), binding(), Codec::Pcmu, Instant::now());
    handset.clear();
    let effects = destination
        .begin_conference_destination(ConferenceDestinationRequest {
            device_id: binding().device_id,
            handset_call_id: CallId(2),
            destination: "700".into(),
            application_options: "Mac".into(),
        })
        .unwrap();
    let mutation = destination
        .conference_destination_mutation(CallId(2))
        .unwrap();
    handset.apply(&effects);
    assert_eq!(
        handset.call_states(),
        [
            (CallId(1), HandsetCallState::Hold, true),
            (CallId(2), HandsetCallState::Proceed, false),
        ]
    );
    assert_eq!(handset.tones(CallId(2)), [Tone::Silence]);
    let info = handset.call_info(CallId(2));
    assert_eq!(info.last().unwrap().called_name, "Conference");
    assert_eq!(info.last().unwrap().called_number, "700");
    handset.clear();
    handset.apply(&destination.conference_destination_failed(
        mutation,
        CallId(2),
        &[PbxCallId(1)],
        &[PbxCallId(1)],
    ));
    assert_eq!(
        handset.call_states(),
        [(CallId(2), HandsetCallState::OnHook, true)]
    );
    assert_eq!(handset.media_winners(), [CallId(1)]);
    assert!(destination.call(CallId(2)).is_none());
    assert_eq!(
        destination.call(CallId(1)).unwrap().state,
        CallState::Connected
    );

    let mut shutdown = active_three_party_conference_with_media();
    shutdown
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    handset.clear();
    let plans = shutdown.drain_conferences_for_shutdown();
    assert_eq!(plans.len(), 1);
    handset.apply(&plans[0].effects);
    assert_eq!(
        handset.call_states(),
        [
            (CallId(4), HandsetCallState::OnHook, true),
            (CallId(2), HandsetCallState::OnHook, true),
            (CallId(3), HandsetCallState::OnHook, true),
            (CallId(5), HandsetCallState::OnHook, true),
        ]
    );
    assert!(shutdown.drain_conferences_for_shutdown().is_empty());
    assert!(shutdown.calls().next().is_none());
    assert!(held.invariant_error().is_none());
    assert!(destination.invariant_error().is_none());
    assert!(shutdown.invariant_error().is_none());
}

#[test]
fn fake_handset_partial_failures_and_disconnect_do_not_target_departed_presentations() {
    let mut handset = FakeHandsets::default();

    let mut consultation = connected_outbound_controller();
    consultation
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    handset.apply(&consultation.abort_conference(CallId(2), true, true, true, true));
    assert_eq!(
        handset.call_states(),
        [(CallId(2), HandsetCallState::OnHook, true)]
    );
    assert_eq!(handset.media_winners(), [CallId(1)]);
    assert!(consultation.conference_session(CallId(1)).is_none());

    let mut mutation = active_three_party_conference_with_media();
    let device = binding().device_id;
    let conference_id = mutation.conference_session(CallId(4)).unwrap().id;
    let initial_json = mutation.conference_json(CallId(4)).unwrap();
    let effects = mutation
        .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
        .unwrap();
    handset.clear();
    handset.apply(&effects);
    assert!(handset.effects.is_empty());
    assert!(
        mutation.abort_conference_participant_mute(conference_id, ParticipantId::new(2), true,)
    );
    assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

    mutation
        .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(2))
        .unwrap();
    assert!(mutation.abort_conference_participant_removal(conference_id, ParticipantId::new(2)));
    assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

    mutation
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert!(mutation.abort_conference_participant_role_change(
        conference_id,
        ParticipantId::new(2),
        true,
    ));
    assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

    let hold = mutation
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    handset.apply(&hold);
    let rollback = mutation.abort_conference_moderator_leg_transition(
        conference_id,
        ParticipantId::new(1),
        true,
        &[ParticipantId::new(2)],
        true,
    );
    handset.apply(&rollback);
    assert_eq!(
        handset.call_states(),
        [(CallId(4), HandsetCallState::Hold, true)]
    );
    assert_eq!(handset.media_winners(), [CallId(4)]);
    assert_eq!(mutation.conference_json(CallId(4)).unwrap(), initial_json);

    let mut failed = active_three_party_conference_with_media();
    let failed_id = failed.conference_session(CallId(4)).unwrap().id;
    handset.clear();
    let outcome = failed.conference_participant_failed(CallId(2)).unwrap();
    handset.apply(&outcome.effects);
    assert_eq!(
        handset.call_states(),
        [(CallId(2), HandsetCallState::OnHook, true)]
    );
    assert_eq!(outcome.call_ids, [PbxCallId(8)]);
    assert_eq!(
        outcome
            .surviving_session
            .unwrap()
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [ParticipantId::new(1), ParticipantId::new(3)]
    );
    assert!(failed.conference_participant_failed(CallId(2)).is_none());
    assert!(failed.conference_session_by_id(failed_id).is_some());

    let mut disconnected = active_three_party_conference_with_media();
    handset.clear();
    handset.apply(&disconnected.disconnected(&device));
    assert!(handset.effects.is_empty());
    assert!(handset.announcements().is_empty());
    assert!(disconnected.calls().next().is_none());
    assert!(consultation.invariant_error().is_none());
    assert!(mutation.invariant_error().is_none());
    assert!(failed.invariant_error().is_none());
    assert!(disconnected.invariant_error().is_none());

    let mut raced = active_three_party_conference_with_media();
    let raced_id = raced.conference_session(CallId(4)).unwrap().id;
    raced
        .begin_conference_participant_mute(&device, raced_id, ParticipantId::new(3), true)
        .unwrap();
    assert_eq!(
        raced.begin_conference_participant_removal(&device, raced_id, ParticipantId::new(2),),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert_eq!(
        raced
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now(),),
        Err(ConferenceRejection::Conflict)
    );
    assert_eq!(
        raced.end_conference_by_moderator(&device, raced_id),
        Err(ConferenceEndRejection::Conflict)
    );
    handset.clear();
    let outcome = raced.conference_participant_failed(CallId(2)).unwrap();
    handset.apply(&outcome.effects);
    assert!(outcome.surviving_session.is_none());
    assert_eq!(
        handset.call_states(),
        [
            (CallId(4), HandsetCallState::OnHook, true),
            (CallId(2), HandsetCallState::OnHook, true),
            (CallId(3), HandsetCallState::OnHook, true),
        ]
    );
    assert!(!raced.conference_participant_muted(raced_id, ParticipantId::new(3), true,));
    assert!(raced.calls().next().is_none());
    assert!(raced.invariant_error().is_none());
}

#[test]
fn conference_media_policy_is_captured_and_announcements_follow_category_flags() {
    let mut controller = active_three_party_conference_with_media();
    let session = controller.conference_session(CallId(4)).unwrap().clone();
    assert_eq!(
        controller.conference_announcement_effects(session.id, ConferenceAnnouncement::Connected,),
        [DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
            operation: ConferenceAnnouncementOperation {
                conference_id: session.id,
                targets: vec![
                    ConferenceAnnouncementTarget {
                        participant_id: ParticipantId::new(1),
                        call_id: PbxCallId(10)
                    },
                    ConferenceAnnouncementTarget {
                        participant_id: ParticipantId::new(2),
                        call_id: PbxCallId(8)
                    },
                    ConferenceAnnouncementTarget {
                        participant_id: ParticipantId::new(3),
                        call_id: PbxCallId(9)
                    },
                ],
                announcement: ConferenceAnnouncement::Connected,
            },
        })]
    );
    assert_eq!(
        controller.conference_announcement_effects(
            session.id,
            ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
        ),
        [DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
            operation: ConferenceAnnouncementOperation {
                conference_id: session.id,
                targets: vec![ConferenceAnnouncementTarget {
                    participant_id: ParticipantId::new(2),
                    call_id: PbxCallId(8)
                }],
                announcement: ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
            },
        })]
    );
    assert!(!controller.configure_conference_media(CallId(4), ConferenceMediaPolicy::default(),));

    let disabled = active_three_party_conference();
    let disabled_id = disabled.conference_session(CallId(4)).unwrap().id;
    assert!(
        disabled
            .conference_announcement_effects(disabled_id, ConferenceAnnouncement::Connected)
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
    assert!(disabled.invariant_error().is_none());
}

#[test]
fn moderator_leg_hold_and_resume_preserve_bridge_calls_ids_and_json() {
    let mut controller = active_three_party_conference_with_media();
    let session = controller.conference_session(CallId(4)).unwrap().clone();
    let bridge_id = session.bridge_id;
    let participant_ids = session
        .participants
        .iter()
        .map(|participant| participant.id)
        .collect::<Vec<_>>();
    assert_eq!(
        controller.begin_conference_moderator_leg_transition(CallId(2), true),
        Err(ConferenceParticipantRejection::NotModerator)
    );
    assert!(controller.resume(CallId(4)).is_empty());

    let hold = controller
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert!(matches!(
        hold.first(),
        Some(DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(4),
            state: HandsetCallState::Hold,
            stop_media: true,
            ..
        }))
    ));
    assert_eq!(
        hold.iter()
            .filter_map(|effect| match effect {
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id,
                            enabled: true,
                            ..
                        },
                }) => Some(*participant_id),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [ParticipantId::new(2), ParticipantId::new(3)]
    );
    assert!(
        !hold
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hold { .. })))
    );
    assert_eq!(
        controller.pbx_call(PbxCallId(10)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.conference_moderator_leg_transitioned(
        session.id,
        ParticipantId::new(1),
        true,
    ));
    assert_eq!(controller.call(CallId(4)).unwrap().state, CallState::Held);
    assert_eq!(
        controller.pbx_call(PbxCallId(10)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(
        controller
            .conference_session_by_id(session.id)
            .unwrap()
            .participants
            .active_moderator_count(),
        0
    );
    let held_json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(held_json["participants"][0]["held"], true);

    let resume = controller
        .begin_conference_moderator_leg_transition(CallId(4), false)
        .unwrap();
    assert_eq!(
        resume
            .iter()
            .filter_map(|effect| match effect {
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            participant_id,
                            enabled: false,
                            ..
                        },
                }) => Some(*participant_id),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [ParticipantId::new(2), ParticipantId::new(3)]
    );
    assert!(matches!(
        resume.last(),
        Some(DriverEffect::Handset(HandsetEffect::BeginMedia {
            call_id: CallId(4),
            ..
        }))
    ));
    assert!(
        !resume
            .iter()
            .any(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. })))
    );
    assert!(controller.conference_moderator_leg_transitioned(
        session.id,
        ParticipantId::new(1),
        false,
    ));
    let resumed = controller.conference_session_by_id(session.id).unwrap();
    assert_eq!(resumed.bridge_id, bridge_id);
    assert_eq!(
        resumed
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        participant_ids
    );
    assert_eq!(
        controller.call(CallId(4)).unwrap().state,
        CallState::Connected
    );
    assert_eq!(
        controller.call(CallId(4)).unwrap().audio,
        MediaStreamState::Opening
    );
    let resumed_json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(resumed_json["participants"][0]["held"], false);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_leg_failure_rolls_back_only_completed_handset_and_music_work() {
    let mut controller = active_three_party_conference_with_media();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    controller
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    let rollback = controller.abort_conference_moderator_leg_transition(
        conference_id,
        ParticipantId::new(1),
        true,
        &[ParticipantId::new(2)],
        true,
    );
    assert!(matches!(
        rollback.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id,
                        enabled: false,
                        ..
                    }
            }),
            DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(4),
                ..
            })
        ] if *participant_id == ParticipantId::new(2)
    ));
    assert!(
        !controller
            .conference_session_by_id(conference_id)
            .unwrap()
            .participants
            .get(ParticipantId::new(1))
            .unwrap()
            .held
    );
    assert_eq!(
        controller.call(CallId(4)).unwrap().state,
        CallState::Connected
    );

    controller
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert!(controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        true,
    ));
    controller
        .begin_conference_moderator_leg_transition(CallId(4), false)
        .unwrap();
    let rollback = controller.abort_conference_moderator_leg_transition(
        conference_id,
        ParticipantId::new(1),
        false,
        &[ParticipantId::new(2)],
        true,
    );
    assert!(matches!(
        rollback.as_slice(),
        [
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(4),
                state: HandsetCallState::Hold,
                stop_media: true,
                ..
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id,
                        enabled: true,
                        ..
                    }
            })
        ] if *participant_id == ParticipantId::new(2)
    ));
    assert!(
        controller
            .conference_session_by_id(conference_id)
            .unwrap()
            .participants
            .get(ParticipantId::new(1))
            .unwrap()
            .held
    );
    assert_eq!(controller.call(CallId(4)).unwrap().state, CallState::Held);
    controller
        .begin_conference_moderator_leg_transition(CallId(4), false)
        .unwrap();
    assert!(
        controller
            .abort_conference_moderator_leg_transition(
                conference_id,
                ParticipantId::new(1),
                false,
                &[],
                false,
            )
            .is_empty()
    );
    assert_eq!(
        controller.call(CallId(4)).unwrap().audio,
        MediaStreamState::Closed
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn multiple_moderator_legs_change_music_only_at_the_listening_boundary() {
    let mut controller = active_three_party_conference_with_media();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert!(controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(2),
        true,
    ));

    let first_hold = controller
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert!(!first_hold.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
        })
    )));
    assert!(controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        true,
    ));

    let last_hold = controller
        .begin_conference_moderator_leg_transition(CallId(2), true)
        .unwrap();
    assert_eq!(
        last_hold
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            enabled: true,
                            ..
                        }
                })
            ))
            .count(),
        1
    );
    assert!(controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(2),
        true,
    ));

    let first_resume = controller
        .begin_conference_moderator_leg_transition(CallId(4), false)
        .unwrap();
    assert_eq!(
        first_resume
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                            enabled: false,
                            ..
                        }
                })
            ))
            .count(),
        1
    );
    assert!(controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        false,
    ));
    let session = controller.conference_session_by_id(conference_id).unwrap();
    assert_eq!(session.bridge_id, PbxBridgeId(1));
    assert_eq!(session.participants.moderator_count(), 2);
    assert_eq!(session.participants.active_moderator_count(), 1);
    assert!(
        session
            .participants
            .get(ParticipantId::new(2))
            .unwrap()
            .held
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_leg_with_disabled_music_changes_only_the_handset_leg() {
    let mut controller = active_three_party_conference();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    let hold = controller
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert!(matches!(
        hold.as_slice(),
        [DriverEffect::Handset(HandsetEffect::SetCallState {
            call_id: CallId(4),
            state: HandsetCallState::Hold,
            ..
        })]
    ));
    assert!(controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        true,
    ));
    let resume = controller
        .begin_conference_moderator_leg_transition(CallId(4), false)
        .unwrap();
    assert!(matches!(
        resume.as_slice(),
        [DriverEffect::Handset(HandsetEffect::BeginMedia {
            call_id: CallId(4),
            ..
        })]
    ));
    assert!(controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        false,
    ));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_leg_transition_serializes_mutation_end_and_departure_races() {
    let mut controller = active_three_party_conference_with_media();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    controller
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert_eq!(
        controller.begin_conference_participant_mute(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert_eq!(
        controller.end_conference_by_moderator(&device, conference_id),
        Err(ConferenceEndRejection::Conflict)
    );

    let outcome = controller
        .pbx_hangup_with_effects(PbxCallId(8))
        .expect("departure is consumed while hold is pending");
    assert!(outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(controller.conference_session_by_id(conference_id).is_none());
    assert!(!controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        true,
    ));
    assert!(
        controller
            .abort_conference_moderator_leg_transition(
                conference_id,
                ParticipantId::new(1),
                true,
                &[],
                true,
            )
            .is_empty()
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn general_and_personal_conference_announcements_have_exact_audiences() {
    let device = binding().device_id;
    let mut general = three_call_conference_controller();
    general
        .join_calls_with_media(
            &device,
            CallId(4),
            true,
            ConferenceMediaPolicy {
                music_on_hold_class: None,
                mute_on_entry: false,
                play_general_announcements: true,
                play_participant_announcements: false,
            },
        )
        .unwrap();
    assert!(general.conference_merged(CallId(4)));
    let general_session = general.conference_session(CallId(4)).unwrap().clone();
    general
        .begin_conference_participant_removal(&device, general_session.id, ParticipantId::new(2))
        .unwrap();
    assert!(
        general
            .conference_participant_removed(general_session.id, ParticipantId::new(2))
            .is_some()
    );
    assert_eq!(
        general.conference_announcement_effects(
            general_session.id,
            ConferenceAnnouncement::ParticipantRemoved(ParticipantId::new(2)),
        ),
        [DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
            operation: ConferenceAnnouncementOperation {
                conference_id: general_session.id,
                targets: vec![
                    ConferenceAnnouncementTarget {
                        participant_id: ParticipantId::new(1),
                        call_id: PbxCallId(10)
                    },
                    ConferenceAnnouncementTarget {
                        participant_id: ParticipantId::new(3),
                        call_id: PbxCallId(9)
                    },
                ],
                announcement: ConferenceAnnouncement::ParticipantRemoved(ParticipantId::new(2),),
            },
        })]
    );
    assert!(
        general
            .conference_announcement_effects(
                general_session.id,
                ConferenceAnnouncement::ParticipantMuted(ParticipantId::new(2)),
            )
            .is_empty()
    );

    let mut personal = three_call_conference_controller();
    personal
        .join_calls_with_media(
            &device,
            CallId(4),
            true,
            ConferenceMediaPolicy {
                music_on_hold_class: None,
                mute_on_entry: false,
                play_general_announcements: false,
                play_participant_announcements: true,
            },
        )
        .unwrap();
    assert!(personal.conference_merged(CallId(4)));
    let personal_id = personal.conference_session(CallId(4)).unwrap().id;
    assert!(
        personal
            .conference_announcement_effects(
                personal_id,
                ConferenceAnnouncement::ParticipantJoined(ParticipantId::new(2)),
            )
            .is_empty()
    );
    assert_eq!(
        personal
            .conference_announcement_effects(
                personal_id,
                ConferenceAnnouncement::ParticipantUnmuted(ParticipantId::new(3)),
            )
            .len(),
        1
    );
    assert!(general.invariant_error().is_none());
    assert!(personal.invariant_error().is_none());
}

#[test]
fn moderator_invite_starts_and_every_exit_stops_configured_music_exactly() {
    let mut controller = active_three_party_conference_with_media();
    let effects = controller
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    let starts: Vec<_> = effects
        .iter()
        .filter_map(|effect| match effect {
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id,
                        call_id,
                        class,
                        enabled: true,
                        ..
                    },
            }) => Some((*participant_id, *call_id, class.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(
        starts,
        [
            (ParticipantId::new(2), PbxCallId(8), "office"),
            (ParticipantId::new(3), PbxCallId(9), "office"),
        ]
    );

    let cleanup = controller.abort_conference_invite(CallId(5), false, true, false);
    let stops: Vec<_> = cleanup
        .iter()
        .filter_map(|effect| match effect {
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id,
                        enabled: false,
                        ..
                    },
            }) => Some(*participant_id),
            _ => None,
        })
        .collect();
    assert_eq!(stops, [ParticipantId::new(2), ParticipantId::new(3)]);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn successful_invite_stops_music_before_moderator_resume_and_bridge_merge() {
    let mut controller = active_three_party_conference_with_media();
    controller
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    controller.enbloc(CallId(5), "2200".into());
    let invite_pbx = controller.call(CallId(5)).unwrap().pbx_id;
    controller.pbx_answer(invite_pbx);

    let effects = controller.confirm_conference_invite(CallId(5)).unwrap();
    assert!(matches!(
        effects.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id: first,
                        enabled: false,
                        ..
                    },
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id: second,
                        enabled: false,
                        ..
                    },
            }),
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(10),
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeParticipant { .. },
            }),
        ] if *first == ParticipantId::new(2) && *second == ParticipantId::new(3)
    ));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn explicitly_disabled_conference_music_emits_no_music_operations() {
    let mut controller = active_three_party_conference();
    let effects = controller
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
        })
    )));
    let cleanup = controller.abort_conference_invite(CallId(5), false, true, false);
    assert!(!cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
        })
    )));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_invite_adds_one_stable_participant_to_the_live_bridge() {
    let mut controller = active_three_party_conference();
    assert_eq!(
        controller.begin_conference_invite(
            CallId(2),
            CallId(5),
            binding(),
            Codec::Pcma,
            Instant::now(),
        ),
        Err(ConferenceRejection::Disabled)
    );

    let effects = controller
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    assert!(matches!(
        effects.first(),
        Some(DriverEffect::Backend(PbxEffect::Hold {
            call_id: PbxCallId(10)
        }))
    ));
    let pending = controller
        .conference_session(CallId(5))
        .unwrap()
        .pending_invite
        .as_ref()
        .unwrap()
        .participant
        .clone();
    assert_eq!(pending.id, ParticipantId::new(4));
    assert_eq!(pending.handset_call_id, CallId(5));

    controller.enbloc(CallId(5), "2200".into());
    controller.pbx_answer(pending.pbx_call_id);
    assert_eq!(
        controller.confirm_conference_invite(CallId(5)).unwrap(),
        [
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(10),
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                    bridge_id: PbxBridgeId(1),
                    call_id: pending.pbx_call_id,
                },
            }),
        ]
    );
    assert!(controller.conference_invite_merged(CallId(5)));
    let session = controller.conference_session(CallId(5)).unwrap();
    assert!(session.pending_invite.is_none());
    assert_eq!(session.participants.iter().len(), 4);
    assert_eq!(
        session
            .participants
            .by_pbx(pending.pbx_call_id)
            .map(|entry| entry.id),
        Some(ParticipantId::new(4))
    );
    assert_eq!(
        controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .moderator()
            .unwrap()
            .id,
        ParticipantId::new(1)
    );
    let cleanup = controller.end_conference(CallId(5));
    assert_eq!(
        cleanup
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
            .count(),
        4
    );
    assert_eq!(
        cleanup
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        1
    );
    assert!(controller.calls().next().is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn secondary_moderator_invite_targets_the_exact_initiating_leg() {
    let mut controller = active_three_party_conference_with_media();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert!(controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(2),
        true,
    ));
    let stable = controller
        .conference_session(CallId(4))
        .unwrap()
        .participants
        .iter()
        .map(|participant| {
            (
                participant.id,
                participant.pbx_call_id,
                participant.handset_call_id,
                participant.moderator,
            )
        })
        .collect::<Vec<_>>();

    let effects = controller
        .begin_conference_invite(CallId(2), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    assert!(matches!(
        effects.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Hold {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(2),
                state: HandsetCallState::Hold,
                stop_media: true,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::BeginCall {
                call_id: CallId(5),
                line_instance: 1,
                codec: Codec::Pcma,
                ..
            }),
            DriverEffect::Backend(PbxEffect::CreateChannel { .. }),
        ]
    ));
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
        })
    )));
    let pending = controller
        .conference_session(CallId(5))
        .unwrap()
        .pending_invite
        .as_ref()
        .unwrap();
    assert_eq!(pending.moderator_id, ParticipantId::new(2));
    assert_eq!(pending.moderator_call_id, PbxCallId(8));
    assert!(!pending.music_started);

    controller.enbloc(CallId(5), "2400".into());
    controller.pbx_answer(PbxCallId(11));
    assert_eq!(
        controller.confirm_conference_invite(CallId(5)).unwrap(),
        [
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::MergeParticipant {
                    bridge_id: PbxBridgeId(1),
                    call_id: PbxCallId(11),
                },
            }),
        ]
    );
    assert!(controller.conference_invite_merged(CallId(5)));
    let session = controller.conference_session(CallId(5)).unwrap();
    assert_eq!(
        session
            .participants
            .iter()
            .take(3)
            .map(|participant| {
                (
                    participant.id,
                    participant.pbx_call_id,
                    participant.handset_call_id,
                    participant.moderator,
                )
            })
            .collect::<Vec<_>>(),
        stable
    );
    assert_eq!(session.participants.iter().len(), 4);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn secondary_moderator_invite_abort_restores_only_its_leg() {
    let mut controller = active_three_party_conference_with_media();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert!(controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(2),
        true,
    ));
    controller
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert!(controller.conference_moderator_leg_transitioned(
        conference_id,
        ParticipantId::new(1),
        true,
    ));
    let start = controller
        .begin_conference_invite(CallId(2), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    assert!(start.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                participant_id,
                call_id: PbxCallId(9),
                class,
                enabled: true,
                ..
            },
        }) if *participant_id == ParticipantId::new(3) && class == "office"
    )));
    assert_eq!(
        start
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation:
                        crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. },
                })
            ))
            .count(),
        1
    );

    let cleanup = controller.abort_conference_invite(CallId(5), true, true, true);
    assert!(matches!(
        cleanup.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id,
                        call_id: PbxCallId(9),
                        class,
                        enabled: false,
                        ..
                    },
            }),
            DriverEffect::Backend(PbxEffect::Hangup {
                call_id: PbxCallId(11),
            }),
            DriverEffect::Backend(PbxEffect::Resume {
                call_id: PbxCallId(8),
            }),
            DriverEffect::Handset(HandsetEffect::SetCallState {
                call_id: CallId(5),
                state: HandsetCallState::OnHook,
                stop_media: true,
                ..
            }),
            DriverEffect::Handset(HandsetEffect::BeginMedia {
                call_id: CallId(2),
                ..
            }),
        ] if *participant_id == ParticipantId::new(3) && class == "office"
    ));
    assert!(!cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Handset(HandsetEffect::BeginMedia {
            call_id: CallId(4),
            ..
        })
    )));
    let session = controller.conference_session(CallId(4)).unwrap();
    assert!(session.pending_invite.is_none());
    assert_eq!(session.participants.iter().len(), 3);
    assert_eq!(
        controller.call(CallId(2)).unwrap().state,
        CallState::Connected
    );
    assert!(controller.call(CallId(5)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn failed_or_cancelled_invite_preserves_the_existing_conference() {
    for channel_created in [false, true] {
        let mut controller = active_three_party_conference();
        controller
            .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
            .unwrap();
        let invite_pbx = controller
            .conference_session(CallId(5))
            .unwrap()
            .pending_invite
            .as_ref()
            .unwrap()
            .participant
            .pbx_call_id;
        let cleanup = controller.abort_conference_invite(CallId(5), channel_created, true, true);
        assert_eq!(
            cleanup
                .iter()
                .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
                .count(),
            usize::from(channel_created)
        );
        assert!(!cleanup.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            })
        )));
        assert!(controller.pbx_call(invite_pbx).is_none());
        let session = controller.conference_session(CallId(4)).unwrap();
        assert_eq!(session.phase, ConferencePhase::Active);
        assert_eq!(session.participants.iter().len(), 3);
        assert!(session.pending_invite.is_none());
        assert_eq!(
            controller.call(CallId(4)).unwrap().state,
            CallState::Connected
        );
        assert!(controller.invariant_error().is_none());
    }
}

#[test]
fn moderator_mute_commits_only_after_backend_success_and_updates_json() {
    let mut controller = active_three_party_conference();
    let moderator_device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    let participant_id = ParticipantId::new(2);

    let effects = controller
        .begin_conference_participant_mute(&moderator_device, conference_id, participant_id, true)
        .unwrap();
    assert_eq!(
        effects,
        [DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMuted {
                bridge_id: PbxBridgeId(1),
                participant_id,
                call_id: PbxCallId(8),
                muted: true,
            },
        })]
    );
    let before: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(before["participants"][1]["muted"], false);
    assert_eq!(
        controller.begin_conference_participant_mute(
            &moderator_device,
            conference_id,
            participant_id,
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );

    assert!(controller.abort_conference_participant_mute(conference_id, participant_id, true));
    assert!(
        !controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .get(participant_id)
            .unwrap()
            .muted
    );

    controller
        .begin_conference_participant_mute(&moderator_device, conference_id, participant_id, true)
        .unwrap();
    assert!(controller.conference_participant_muted(conference_id, participant_id, true));
    let muted: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(muted["participants"][1]["muted"], true);

    controller
        .begin_conference_participant_mute(&moderator_device, conference_id, participant_id, false)
        .unwrap();
    assert!(controller.conference_participant_muted(conference_id, participant_id, false));
    assert!(
        !controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .get(participant_id)
            .unwrap()
            .muted
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn participant_mute_authorization_identity_and_lifecycle_are_deterministic() {
    let mut controller = active_three_party_conference();
    let moderator_device = binding().device_id;
    let other_device = DeviceId::new("SEP112233445566").unwrap();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;

    assert_eq!(
        controller.begin_conference_participant_mute(
            &other_device,
            conference_id,
            ParticipantId::new(2),
            true,
        ),
        Err(ConferenceParticipantRejection::NotModerator)
    );
    assert_eq!(
        controller.begin_conference_participant_mute(
            &moderator_device,
            conference_id,
            ParticipantId::new(1),
            true,
        ),
        Err(ConferenceParticipantRejection::Moderator)
    );
    assert_eq!(
        controller.begin_conference_participant_mute(
            &moderator_device,
            conference_id,
            ParticipantId::new(99),
            true,
        ),
        Err(ConferenceParticipantRejection::InvalidParticipant)
    );

    controller
        .begin_conference_participant_mute(
            &moderator_device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    let cleanup = controller.end_conference(CallId(4));
    assert!(!cleanup.is_empty());
    assert!(!controller.conference_participant_muted(conference_id, ParticipantId::new(2), true,));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_removal_commits_only_after_backend_success_and_rekeys_indexes() {
    let mut controller = active_three_party_conference();
    let moderator_device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    let participant_id = ParticipantId::new(2);

    let effects = controller
        .begin_conference_participant_removal(&moderator_device, conference_id, participant_id)
        .unwrap();
    assert_eq!(
        effects,
        [DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::RemoveConferenceParticipant {
                bridge_id: PbxBridgeId(1),
                participant_id,
                call_id: PbxCallId(8),
            },
        })]
    );
    assert_eq!(
        controller
            .conference_session(CallId(4))
            .unwrap()
            .participants
            .iter()
            .len(),
        3
    );
    let before_abort = controller.conference_json(CallId(4)).unwrap();
    assert_eq!(
        controller.begin_conference_participant_mute(
            &moderator_device,
            conference_id,
            ParticipantId::new(3),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert!(controller.abort_conference_participant_removal(conference_id, participant_id));
    assert_eq!(controller.conference_json(CallId(4)).unwrap(), before_abort);

    controller
        .begin_conference_participant_removal(&moderator_device, conference_id, participant_id)
        .unwrap();
    let cleanup = controller
        .conference_participant_removed(conference_id, participant_id)
        .unwrap();
    assert_eq!(
        cleanup,
        [DriverEffect::Handset(HandsetEffect::SetCallState {
            device_id: moderator_device.clone(),
            call_id: CallId(2),
            state: HandsetCallState::OnHook,
            stop_media: true,
        })]
    );
    let session = controller.conference_session(CallId(4)).unwrap();
    assert_eq!(session.consultation_call_id, PbxCallId(9));
    assert_eq!(session.consultation_handset_call_id, CallId(3));
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [ParticipantId::new(1), ParticipantId::new(3)]
    );
    assert!(controller.call(CallId(2)).is_none());
    let json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(json["participants"].as_array().unwrap().len(), 2);
    assert_eq!(json["participants"][1]["id"], 3);
    assert_eq!(
        controller.begin_conference_participant_removal(
            &moderator_device,
            conference_id,
            ParticipantId::new(3),
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn participant_removal_authorization_failure_and_hangup_race_are_exact() {
    let mut controller = active_three_party_conference();
    let moderator_device = binding().device_id;
    let other_device = DeviceId::new("SEP112233445566").unwrap();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;

    assert_eq!(
        controller.begin_conference_participant_removal(
            &other_device,
            conference_id,
            ParticipantId::new(2),
        ),
        Err(ConferenceParticipantRejection::NotModerator)
    );
    assert_eq!(
        controller.begin_conference_participant_removal(
            &moderator_device,
            conference_id,
            ParticipantId::new(1),
        ),
        Err(ConferenceParticipantRejection::Moderator)
    );
    assert_eq!(
        controller.begin_conference_participant_removal(
            &moderator_device,
            conference_id,
            ParticipantId::new(99),
        ),
        Err(ConferenceParticipantRejection::InvalidParticipant)
    );

    controller
        .begin_conference_participant_removal(
            &moderator_device,
            conference_id,
            ParticipantId::new(2),
        )
        .unwrap();
    let outcome = controller
        .pbx_hangup_with_effects(PbxCallId(8))
        .expect("pending participant hangup is consumed");
    assert_eq!(
        outcome.effects,
        [DriverEffect::Handset(HandsetEffect::SetCallState {
            device_id: moderator_device,
            call_id: CallId(2),
            state: HandsetCallState::OnHook,
            stop_media: true,
        })]
    );
    assert!(controller.conference_session(CallId(4)).is_some());
    assert!(
        controller
            .conference_participant_removed(conference_id, ParticipantId::new(2))
            .is_none()
    );
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
        })
    )));
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_role_changes_commit_transactionally_and_preserve_stable_identity() {
    let mut controller = active_three_party_conference();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    let participant_id = ParticipantId::new(2);
    let original_moderator_id = ParticipantId::new(1);
    let stable_participants = controller
        .conference_session(CallId(4))
        .unwrap()
        .participants
        .iter()
        .map(|participant| {
            (
                participant.id,
                participant.pbx_call_id,
                participant.handset_call_id,
            )
        })
        .collect::<Vec<_>>();
    let before = controller.conference_json(CallId(4)).unwrap();

    assert!(
        controller
            .begin_conference_participant_role_change(&device, conference_id, participant_id, true,)
            .unwrap()
            .is_empty()
    );
    assert_eq!(controller.conference_json(CallId(4)).unwrap(), before);
    assert_eq!(
        controller.begin_conference_participant_mute(
            &device,
            conference_id,
            ParticipantId::new(3),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert_eq!(
        controller.begin_conference_participant_removal(
            &device,
            conference_id,
            ParticipantId::new(3),
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert!(controller.abort_conference_participant_role_change(
        conference_id,
        participant_id,
        true,
    ));
    assert_eq!(controller.conference_json(CallId(4)).unwrap(), before);

    controller
        .begin_conference_participant_role_change(&device, conference_id, participant_id, true)
        .unwrap();
    assert!(controller.conference_participant_role_changed(conference_id, participant_id, true,));
    let promoted = controller.conference_session(CallId(4)).unwrap();
    assert_eq!(promoted.participants.moderator_count(), 2);
    assert!(promoted.participants.get(participant_id).unwrap().moderator);
    assert_eq!(
        promoted
            .participants
            .iter()
            .map(|participant| {
                (
                    participant.id,
                    participant.pbx_call_id,
                    participant.handset_call_id,
                )
            })
            .collect::<Vec<_>>(),
        stable_participants
    );
    let promoted_json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(promoted_json["moderator_id"], 1);
    assert_eq!(promoted_json["participants"][1]["moderator"], true);

    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            original_moderator_id,
            false,
        )
        .unwrap();
    let before_demote: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(before_demote["participants"][0]["moderator"], true);
    assert!(controller.conference_participant_role_changed(
        conference_id,
        original_moderator_id,
        false,
    ));
    let demoted = controller.conference_session(CallId(4)).unwrap();
    assert_eq!(demoted.participants.moderator_count(), 1);
    assert!(
        !demoted
            .participants
            .get(original_moderator_id)
            .unwrap()
            .moderator
    );
    let demoted_json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(demoted_json["moderator_id"], 2);
    assert_eq!(demoted_json["participants"][0]["moderator"], false);
    assert_eq!(demoted_json["participants"][1]["moderator"], true);
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_role_changes_apply_music_at_the_listening_boundary() {
    let device = binding().device_id;

    let mut promotion = active_three_party_conference_with_media();
    let promotion_id = promotion.conference_session(CallId(4)).unwrap().id;
    promotion
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert!(promotion.conference_moderator_leg_transitioned(
        promotion_id,
        ParticipantId::new(1),
        true,
    ));
    let promote = promotion
        .begin_conference_participant_role_change(
            &device,
            promotion_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert!(matches!(
        promote.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id: first,
                        call_id: PbxCallId(8),
                        class,
                        enabled: false,
                        ..
                    },
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id: second,
                        call_id: PbxCallId(9),
                        enabled: false,
                        ..
                    },
            }),
        ] if *first == ParticipantId::new(2)
            && *second == ParticipantId::new(3)
            && class == "office"
    ));
    assert!(promotion.abort_conference_participant_role_change(
        promotion_id,
        ParticipantId::new(2),
        true,
    ));
    assert!(
        !promotion
            .conference_session_by_id(promotion_id)
            .unwrap()
            .participants
            .get(ParticipantId::new(2))
            .unwrap()
            .moderator
    );

    let mut demotion = active_three_party_conference_with_media();
    let demotion_id = demotion.conference_session(CallId(4)).unwrap().id;
    demotion
        .begin_conference_participant_role_change(&device, demotion_id, ParticipantId::new(2), true)
        .unwrap();
    assert!(
        demotion.conference_participant_role_changed(demotion_id, ParticipantId::new(2), true,)
    );
    demotion
        .begin_conference_moderator_leg_transition(CallId(4), true)
        .unwrap();
    assert!(demotion.conference_moderator_leg_transitioned(
        demotion_id,
        ParticipantId::new(1),
        true,
    ));
    let demote = demotion
        .begin_conference_participant_role_change(
            &device,
            demotion_id,
            ParticipantId::new(2),
            false,
        )
        .unwrap();
    assert!(matches!(
        demote.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id: first,
                        call_id: PbxCallId(8),
                        class,
                        enabled: true,
                        ..
                    },
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold {
                        participant_id: second,
                        call_id: PbxCallId(9),
                        enabled: true,
                        ..
                    },
            }),
        ] if *first == ParticipantId::new(2)
            && *second == ParticipantId::new(3)
            && class == "office"
    ));
    assert!(demotion.abort_conference_participant_role_change(
        demotion_id,
        ParticipantId::new(2),
        false,
    ));
    assert!(
        demotion
            .conference_session_by_id(demotion_id)
            .unwrap()
            .participants
            .get(ParticipantId::new(2))
            .unwrap()
            .moderator
    );
    assert!(promotion.invariant_error().is_none());
    assert!(demotion.invariant_error().is_none());
}

#[test]
fn moderator_role_changes_reject_muted_promotion_and_held_demotion() {
    let device = binding().device_id;

    let mut muted = active_three_party_conference_with_media();
    let muted_id = muted.conference_session(CallId(4)).unwrap().id;
    muted
        .begin_conference_participant_mute(&device, muted_id, ParticipantId::new(2), true)
        .unwrap();
    assert!(muted.conference_participant_muted(muted_id, ParticipantId::new(2), true,));
    assert_eq!(
        muted.begin_conference_participant_role_change(
            &device,
            muted_id,
            ParticipantId::new(2),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );

    let mut held = active_three_party_conference_with_media();
    let held_id = held.conference_session(CallId(4)).unwrap().id;
    held.begin_conference_participant_role_change(&device, held_id, ParticipantId::new(2), true)
        .unwrap();
    assert!(held.conference_participant_role_changed(held_id, ParticipantId::new(2), true,));
    held.begin_conference_moderator_leg_transition(CallId(2), true)
        .unwrap();
    assert!(held.conference_moderator_leg_transitioned(held_id, ParticipantId::new(2), true,));
    assert_eq!(
        held.begin_conference_participant_role_change(
            &device,
            held_id,
            ParticipantId::new(2),
            false,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert!(muted.invariant_error().is_none());
    assert!(held.invariant_error().is_none());
}

#[test]
fn moderator_role_authorization_serialization_and_lifecycle_are_deterministic() {
    let mut controller = active_three_party_conference();
    let device = binding().device_id;
    let other_device = DeviceId::new("SEP112233445566").unwrap();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;

    assert_eq!(
        controller.begin_conference_participant_role_change(
            &other_device,
            conference_id,
            ParticipantId::new(2),
            true,
        ),
        Err(ConferenceParticipantRejection::NotModerator)
    );
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(99),
            true,
        ),
        Err(ConferenceParticipantRejection::InvalidParticipant)
    );
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(1),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(1),
            false,
        ),
        Err(ConferenceParticipantRejection::LastModerator)
    );

    controller
        .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
        .unwrap();
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(3),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert!(controller.abort_conference_participant_mute(
        conference_id,
        ParticipantId::new(2),
        true,
    ));
    controller
        .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(2))
        .unwrap();
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(3),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert!(controller.abort_conference_participant_removal(conference_id, ParticipantId::new(2),));

    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(3),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    assert_eq!(
        controller.begin_conference_invite(
            CallId(4),
            CallId(5),
            binding(),
            Codec::Pcma,
            Instant::now(),
        ),
        Err(ConferenceRejection::Conflict)
    );
    assert!(!controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(3),
        true,
    ));
    assert!(controller.abort_conference_participant_role_change(
        conference_id,
        ParticipantId::new(2),
        true,
    ));

    controller
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        ),
        Err(ConferenceParticipantRejection::Conflict)
    );
    controller.abort_conference_invite(CallId(5), false, true, false);

    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    let cleanup = controller
        .pbx_hangup_with_effects(PbxCallId(8))
        .expect("conference participant hangup is consumed");
    assert!(cleanup.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(controller.conference_session(CallId(4)).is_none());
    assert!(!controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(2),
        true,
    ));
    assert_eq!(
        controller.begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        ),
        Err(ConferenceParticipantRejection::Unavailable)
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn explicit_moderator_end_removes_registry_and_restores_every_handset_exactly_once() {
    let mut controller = active_three_party_conference();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;

    let effects = controller
        .end_conference_by_moderator(&device, conference_id)
        .unwrap();
    assert_eq!(
        effects.first(),
        Some(&DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy {
                bridge_id: PbxBridgeId(1),
            },
        }))
    );
    assert_eq!(
        effects
            .iter()
            .filter_map(|effect| match effect {
                DriverEffect::Backend(PbxEffect::Hangup { call_id }) => Some(*call_id),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [PbxCallId(10), PbxCallId(8), PbxCallId(9)]
    );
    assert_eq!(
        effects
            .iter()
            .filter_map(|effect| match effect {
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    call_id,
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                    ..
                }) => Some(*call_id),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [CallId(4), CallId(2), CallId(3)]
    );
    assert!(controller.conference_session_by_id(conference_id).is_none());
    assert!(controller.conference_json(CallId(4)).is_none());
    assert!(controller.calls().next().is_none());
    assert_eq!(
        controller.end_conference_by_moderator(&device, conference_id),
        Err(ConferenceEndRejection::Unavailable)
    );
    assert!(controller.pbx_hangup_with_effects(PbxCallId(10)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn explicit_conference_end_authorization_and_action_races_are_deterministic() {
    let mut controller = active_three_party_conference();
    let device = binding().device_id;
    let other_device = DeviceId::new("SEP112233445566").unwrap();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;

    assert_eq!(
        controller.end_conference_by_moderator(&other_device, conference_id),
        Err(ConferenceEndRejection::NotModerator)
    );
    assert_eq!(
        controller.end_conference_by_moderator(&device, ConferenceId::new(999)),
        Err(ConferenceEndRejection::Unavailable)
    );

    controller
        .begin_conference_participant_mute(&device, conference_id, ParticipantId::new(2), true)
        .unwrap();
    assert_eq!(
        controller.end_conference_by_moderator(&device, conference_id),
        Err(ConferenceEndRejection::Conflict)
    );
    assert!(controller.abort_conference_participant_mute(
        conference_id,
        ParticipantId::new(2),
        true,
    ));

    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert_eq!(
        controller.end_conference_by_moderator(&device, conference_id),
        Err(ConferenceEndRejection::Conflict)
    );
    assert!(controller.abort_conference_participant_role_change(
        conference_id,
        ParticipantId::new(2),
        true,
    ));

    controller
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();
    assert_eq!(
        controller.end_conference_by_moderator(&device, conference_id),
        Err(ConferenceEndRejection::Conflict)
    );
    controller.abort_conference_invite(CallId(5), false, true, false);

    let hangup = controller
        .pbx_hangup_with_effects(PbxCallId(8))
        .expect("PBX hangup wins the serialized cleanup race");
    assert_eq!(
        hangup
            .effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        0
    );
    let end = controller
        .end_conference_by_moderator(&device, conference_id)
        .unwrap();
    assert_eq!(
        end.iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        controller.end_conference_by_moderator(&device, conference_id),
        Err(ConferenceEndRejection::Unavailable)
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn participant_departure_preserves_bridge_ids_roles_json_and_exact_leave_audience() {
    let mut controller = active_three_party_conference_with_media();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    let outcome = controller
        .pbx_hangup_with_effects(PbxCallId(8))
        .expect("departing participant is consumed");

    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: announced,
                    targets,
                    announcement: ConferenceAnnouncement::ParticipantRemoved(participant),
                },
            }) if *announced == conference_id
                && *participant == ParticipantId::new(2)
                && targets == &[
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(1), call_id: PbxCallId(10) },
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(3), call_id: PbxCallId(9) },
                ]
        )));
    let session = controller.conference_session_by_id(conference_id).unwrap();
    assert_eq!(session.bridge_id, PbxBridgeId(1));
    assert_eq!(session.participants.moderator_count(), 1);
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [ParticipantId::new(1), ParticipantId::new(3)]
    );
    let json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(4)).unwrap()).unwrap();
    assert_eq!(json["moderator_id"], 1);
    assert_eq!(json["participants"].as_array().unwrap().len(), 2);
    assert!(controller.pbx_hangup_with_effects(PbxCallId(8)).is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn moderator_departure_preserves_conference_only_when_another_moderator_remains() {
    let mut controller = active_three_party_conference_with_media();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert!(controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(2),
        true,
    ));

    let outcome = controller
        .pbx_hangup_with_effects(PbxCallId(10))
        .expect("departing moderator is consumed");
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(outcome.effects.iter().any(|effect| matches!(
            effect,
            DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    targets,
                    announcement: ConferenceAnnouncement::ModeratorDeparted(participant),
                    ..
                },
            }) if *participant == ParticipantId::new(1)
                && targets == &[
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(2), call_id: PbxCallId(8) },
                    ConferenceAnnouncementTarget { participant_id: ParticipantId::new(3), call_id: PbxCallId(9) },
                ]
        )));
    let session = controller.conference_session_by_id(conference_id).unwrap();
    assert_eq!(session.bridge_id, PbxBridgeId(1));
    assert_eq!(session.original_call_id, PbxCallId(8));
    assert_eq!(session.original_handset_call_id, CallId(2));
    assert_eq!(session.consultation_call_id, PbxCallId(9));
    assert_eq!(session.consultation_handset_call_id, CallId(3));
    assert_eq!(session.participants.moderator_count(), 1);
    assert_eq!(
        session.participants.moderator().unwrap().id,
        ParticipantId::new(2)
    );
    let json: serde_json::Value =
        serde_json::from_str(&controller.conference_json(CallId(2)).unwrap()).unwrap();
    assert_eq!(json["moderator_id"], 2);
    assert!(controller.invariant_error().is_none());

    let mut handset = active_three_party_conference_with_media();
    let handset_id = handset.conference_session(CallId(4)).unwrap().id;
    handset
        .begin_conference_participant_role_change(&device, handset_id, ParticipantId::new(2), true)
        .unwrap();
    assert!(handset.conference_participant_role_changed(handset_id, ParticipantId::new(2), true,));
    let effects = handset.hangup(CallId(4));
    assert!(effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(10)
        })
    )));
    assert!(handset.conference_session_by_id(handset_id).is_some());
    assert!(handset.invariant_error().is_none());

    let mut secondary = active_three_party_conference_with_media();
    let secondary_id = secondary.conference_session(CallId(4)).unwrap().id;
    secondary
        .begin_conference_participant_role_change(
            &device,
            secondary_id,
            ParticipantId::new(2),
            true,
        )
        .unwrap();
    assert!(secondary.conference_participant_role_changed(
        secondary_id,
        ParticipantId::new(2),
        true,
    ));
    secondary.pbx_hangup_with_effects(PbxCallId(8));
    let session = secondary.conference_session_by_id(secondary_id).unwrap();
    assert_eq!(session.bridge_id, PbxBridgeId(1));
    assert_eq!(session.participants.moderator_count(), 1);
    assert_eq!(
        session.participants.moderator().unwrap().id,
        ParticipantId::new(1)
    );
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [ParticipantId::new(1), ParticipantId::new(3)]
    );
    assert!(secondary.invariant_error().is_none());
}

#[test]
fn last_moderator_departure_announces_before_terminal_cleanup_and_is_idempotent() {
    let mut controller = active_three_party_conference_with_media();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    let outcome = controller
        .pbx_hangup_with_effects(PbxCallId(10))
        .expect("last moderator departure is consumed");
    assert!(matches!(
        outcome.effects.as_slice(),
        [
            DriverEffect::Backend(PbxEffect::ConferenceAnnouncement {
                operation: ConferenceAnnouncementOperation {
                    conference_id: announced,
                    targets,
                    announcement: ConferenceAnnouncement::ModeratorDeparted(participant),
                },
            }),
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
            }),
            ..
        ] if *announced == conference_id
            && *participant == ParticipantId::new(1)
            && targets == &[
                ConferenceAnnouncementTarget { participant_id: ParticipantId::new(2), call_id: PbxCallId(8) },
                ConferenceAnnouncementTarget { participant_id: ParticipantId::new(3), call_id: PbxCallId(9) },
            ]
    ));
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(10)
        })
    )));
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
        })
    )));
    assert!(controller.conference_session_by_id(conference_id).is_none());
    assert!(controller.pbx_hangup_with_effects(PbxCallId(10)).is_none());
    assert!(controller.invariant_error().is_none());

    let mut announcements_disabled = active_three_party_conference();
    let disabled = announcements_disabled
        .pbx_hangup_with_effects(PbxCallId(10))
        .unwrap();
    assert!(!disabled.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::ConferenceAnnouncement { .. })
    )));
    assert!(!disabled.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::SetParticipantMusicOnHold { .. }
        })
    )));
    assert!(matches!(
        disabled.effects.first(),
        Some(DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        }))
    ));
    assert!(announcements_disabled.invariant_error().is_none());
}

#[test]
fn moderator_promotion_and_departure_have_one_serialized_winner() {
    let device = binding().device_id;

    let mut departure_first = active_three_party_conference();
    let departure_id = departure_first.conference_session(CallId(4)).unwrap().id;
    departure_first.pbx_hangup_with_effects(PbxCallId(10));
    assert_eq!(
        departure_first.begin_conference_participant_role_change(
            &device,
            departure_id,
            ParticipantId::new(2),
            true,
        ),
        Err(ConferenceParticipantRejection::Unavailable)
    );

    let mut promotion_pending = active_three_party_conference();
    let pending_id = promotion_pending.conference_session(CallId(4)).unwrap().id;
    promotion_pending
        .begin_conference_participant_role_change(&device, pending_id, ParticipantId::new(2), true)
        .unwrap();
    let cleanup = promotion_pending
        .pbx_hangup_with_effects(PbxCallId(10))
        .unwrap();
    assert!(cleanup.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert!(!promotion_pending.conference_participant_role_changed(
        pending_id,
        ParticipantId::new(2),
        true,
    ));

    let mut promotion_first = active_three_party_conference();
    let promoted_id = promotion_first.conference_session(CallId(4)).unwrap().id;
    promotion_first
        .begin_conference_participant_role_change(&device, promoted_id, ParticipantId::new(2), true)
        .unwrap();
    assert!(promotion_first.conference_participant_role_changed(
        promoted_id,
        ParticipantId::new(2),
        true,
    ));
    promotion_first.pbx_hangup_with_effects(PbxCallId(10));
    assert!(
        promotion_first
            .conference_session_by_id(promoted_id)
            .is_some()
    );
    assert!(departure_first.invariant_error().is_none());
    assert!(promotion_pending.invariant_error().is_none());
    assert!(promotion_first.invariant_error().is_none());
}

#[test]
fn conference_owner_disconnect_fails_closed_without_leaking_bridge_or_channels() {
    let mut controller = active_three_party_conference_with_media();
    let device = binding().device_id;
    let effects = controller.disconnected(&device);
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        effects
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
            .count(),
        3
    );
    assert!(!effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::ConferenceAnnouncement { .. })
    )));
    assert!(controller.calls().next().is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn removing_a_secondary_participant_after_promotion_keeps_stable_indexes() {
    let mut controller = active_three_party_conference();
    let device = binding().device_id;
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;

    controller
        .begin_conference_participant_role_change(
            &device,
            conference_id,
            ParticipantId::new(3),
            true,
        )
        .unwrap();
    assert!(controller.conference_participant_role_changed(
        conference_id,
        ParticipantId::new(3),
        true,
    ));
    controller
        .begin_conference_participant_removal(&device, conference_id, ParticipantId::new(2))
        .unwrap();
    controller
        .conference_participant_removed(conference_id, ParticipantId::new(2))
        .unwrap();

    let session = controller.conference_session(CallId(4)).unwrap();
    assert_eq!(session.consultation_call_id, PbxCallId(9));
    assert_eq!(session.consultation_handset_call_id, CallId(3));
    assert_eq!(session.participants.moderator_count(), 2);
    assert_eq!(
        session
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [ParticipantId::new(1), ParticipantId::new(3)]
    );
    assert!(controller.invariant_error().is_none());
}

#[test]
fn consultation_and_active_pbx_hangups_have_exact_conference_cleanup() {
    let mut pending = connected_outbound_controller();
    pending
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    let pending_cleanup = pending
        .pbx_hangup_with_effects(PbxCallId(2))
        .unwrap()
        .effects;
    assert!(pending_cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Resume {
            call_id: PbxCallId(1)
        })
    )));
    assert!(pending.call(CallId(1)).is_some());
    assert!(pending.call(CallId(2)).is_none());

    let mut active = connected_outbound_controller();
    active
        .begin_conference(
            CallId(1),
            CallId(2),
            binding(),
            Codec::Pcmu,
            Instant::now(),
            true,
        )
        .unwrap();
    active.enbloc(CallId(2), "2200".into());
    active.pbx_answer(PbxCallId(2));
    active.confirm_conference(CallId(2)).unwrap();
    assert!(active.conference_merged(CallId(2)));
    let active_cleanup = active
        .pbx_hangup_with_effects(PbxCallId(1))
        .unwrap()
        .effects;
    assert_eq!(
        active_cleanup
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        1
    );
    assert!(active_cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(2)
        })
    )));
    assert!(!active_cleanup.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Hangup {
            call_id: PbxCallId(1)
        })
    )));
    assert!(active.calls().next().is_none());
    assert!(active.invariant_error().is_none());
}

#[test]
fn failed_participant_preserves_survivors_or_fails_closed_during_a_mutation() {
    let mut preserving = active_three_party_conference_with_media();
    let conference_id = preserving.conference_session(CallId(4)).unwrap().id;
    let outcome = preserving
        .conference_participant_failed(CallId(2))
        .expect("active participant failure is claimed");
    assert_eq!(outcome.conference_id, conference_id);
    assert_eq!(outcome.failed_call_id, PbxCallId(8));
    assert_eq!(outcome.call_ids, [PbxCallId(8)]);
    let survivor = outcome
        .surviving_session
        .expect("two eligible participants preserve the conference");
    assert_eq!(survivor.id, conference_id);
    assert_eq!(survivor.bridge_id, PbxBridgeId(1));
    assert_eq!(
        survivor
            .participants
            .iter()
            .map(|participant| participant.id)
            .collect::<Vec<_>>(),
        [ParticipantId::new(1), ParticipantId::new(3)]
    );
    assert!(!outcome.effects.iter().any(|effect| matches!(
        effect,
        DriverEffect::Backend(PbxEffect::Bridge {
            operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
        })
    )));
    assert_eq!(
        outcome
            .effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Hangup {
                    call_id: PbxCallId(8)
                })
            ))
            .count(),
        1
    );
    assert!(
        preserving
            .conference_participant_failed(CallId(2))
            .is_none()
    );

    let mut pending = active_three_party_conference();
    let device = binding().device_id;
    let pending_id = pending.conference_session(CallId(4)).unwrap().id;
    pending
        .begin_conference_participant_mute(&device, pending_id, ParticipantId::new(3), true)
        .unwrap();
    let terminal = pending
        .conference_participant_failed(CallId(2))
        .expect("failure wins the pending mutation race");
    assert!(terminal.surviving_session.is_none());
    assert_eq!(
        terminal.call_ids,
        [PbxCallId(10), PbxCallId(8), PbxCallId(9)]
    );
    assert_eq!(
        terminal
            .effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        terminal
            .effects
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
            .count(),
        3
    );
    assert!(!pending.conference_participant_muted(pending_id, ParticipantId::new(3), true,));
    assert!(preserving.invariant_error().is_none());
    assert!(pending.invariant_error().is_none());
}

#[test]
fn shutdown_drain_owns_pending_participants_and_is_idempotent() {
    let mut controller = active_three_party_conference();
    let conference_id = controller.conference_session(CallId(4)).unwrap().id;
    controller
        .begin_conference_invite(CallId(4), CallId(5), binding(), Codec::Pcma, Instant::now())
        .unwrap();

    let plans = controller.drain_conferences_for_shutdown();
    assert_eq!(plans.len(), 1);
    let plan = &plans[0];
    assert_eq!(plan.conference_id, conference_id);
    assert_eq!(
        plan.call_ids,
        [PbxCallId(10), PbxCallId(8), PbxCallId(9), PbxCallId(11),]
    );
    assert_eq!(
        plan.effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Backend(PbxEffect::Bridge {
                    operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        plan.effects
            .iter()
            .filter(|effect| matches!(effect, DriverEffect::Backend(PbxEffect::Hangup { .. })))
            .count(),
        4
    );
    assert_eq!(
        plan.effects
            .iter()
            .filter(|effect| matches!(
                effect,
                DriverEffect::Handset(HandsetEffect::SetCallState {
                    state: HandsetCallState::OnHook,
                    stop_media: true,
                    ..
                })
            ))
            .count(),
        4
    );
    assert!(controller.drain_conferences_for_shutdown().is_empty());
    assert!(controller.conference_session_by_id(conference_id).is_none());
    assert!(controller.calls().next().is_none());
    assert!(controller.invariant_error().is_none());

    let mut mutating = active_three_party_conference();
    let device = binding().device_id;
    let mutating_id = mutating.conference_session(CallId(4)).unwrap().id;
    mutating
        .begin_conference_participant_mute(&device, mutating_id, ParticipantId::new(2), true)
        .unwrap();
    let mutation_plan = mutating.drain_conferences_for_shutdown();
    assert_eq!(mutation_plan.len(), 1);
    assert!(!mutating.conference_participant_muted(mutating_id, ParticipantId::new(2), true,));
    assert!(mutating.drain_conferences_for_shutdown().is_empty());
    assert!(mutating.invariant_error().is_none());
}

#[test]
fn shutdown_drain_orders_multiple_conferences_and_destroys_each_once() {
    let mut controller = Controller::new(Duration::from_secs(1));
    for (device, first_call, first_pbx) in [
        ("SEP001122334455", 2_u64, 8_u64),
        ("SEP112233445566", 12_u64, 18_u64),
    ] {
        let device_id = DeviceId::new(device).unwrap();
        controller.registered(registration_for(device));
        for offset in 0..2 {
            let call_id = CallId(first_call + offset);
            controller.begin_asterisk_call(
                call_id,
                (first_pbx + offset).into(),
                &binding_for(device, 1),
                Codec::Pcma,
            );
            controller.phone_answer(call_id);
            if offset == 0 {
                controller.hold(call_id);
            }
        }
        let moderator_call_id = CallId(first_call + 1);
        controller
            .join_calls(&device_id, moderator_call_id, true)
            .unwrap();
        assert!(controller.conference_merged(moderator_call_id));
    }

    let plans = controller.drain_conferences_for_shutdown();
    assert_eq!(
        plans
            .iter()
            .map(|plan| plan.conference_id)
            .collect::<Vec<_>>(),
        [ConferenceId::new(1), ConferenceId::new(2)]
    );
    assert!(plans.iter().all(|plan| {
        plan.effects
            .iter()
            .filter(|effect| {
                matches!(
                    effect,
                    DriverEffect::Backend(PbxEffect::Bridge {
                        operation: crate::runtime::backend::BridgeOperation::Destroy { .. }
                    })
                )
            })
            .count()
            == 1
    }));
    assert_eq!(
        plans.iter().map(|plan| plan.call_ids.len()).sum::<usize>(),
        4
    );
    assert!(controller.drain_conferences_for_shutdown().is_empty());
    assert!(controller.calls().next().is_none());
    assert!(controller.invariant_error().is_none());
}

#[test]
fn shared_line_operation_sequences_preserve_ownership_invariants() {
    #[derive(Clone, Copy, Debug)]
    enum Operation {
        Answer(CallId),
        Hold(CallId),
        Resume(CallId),
        Steal(CallId),
        DisconnectFirst,
        DisconnectSecond,
        PbxHangup,
    }

    fn apply(controller: &mut Controller, operation: Operation) {
        match operation {
            Operation::Answer(call_id) => {
                controller.phone_answer(call_id);
            }
            Operation::Hold(call_id) => {
                controller.hold(call_id);
            }
            Operation::Resume(call_id) => {
                controller.resume(call_id);
            }
            Operation::Steal(call_id) => {
                controller.steal(call_id);
            }
            Operation::DisconnectFirst => {
                controller.disconnected(&DeviceId::new("SEP001122334455").unwrap());
            }
            Operation::DisconnectSecond => {
                controller.disconnected(&DeviceId::new("SEP112233445566").unwrap());
            }
            Operation::PbxHangup => {
                controller.pbx_hangup_with_effects(PbxCallId(8));
            }
        }
    }

    fn assert_shared_invariants(controller: &Controller, sequence: &[Operation]) {
        assert_eq!(
            controller.invariant_error(),
            None,
            "invariant failed after {sequence:?}"
        );
        let Some(call) = controller.pbx_call(PbxCallId(8)) else {
            assert_eq!(controller.calls().count(), 0, "after {sequence:?}");
            return;
        };
        let appearances: Vec<_> = controller.appearances_for_pbx(call.id).collect();
        assert!(!appearances.is_empty(), "after {sequence:?}");
        assert_eq!(
            appearances.len(),
            call.appearance_ids().count(),
            "after {sequence:?}"
        );
        assert!(
            appearances
                .iter()
                .all(|appearance| appearance.pbx_id == call.id),
            "after {sequence:?}"
        );
        let active: Vec<_> = appearances
            .iter()
            .filter(|appearance| {
                matches!(
                    appearance.state,
                    CallState::Collecting
                        | CallState::PickupCollecting
                        | CallState::Calling
                        | CallState::Connected
                        | CallState::Held
                        | CallState::TransferCollecting
                )
            })
            .collect();
        assert!(active.len() <= 1, "after {sequence:?}");
        assert_eq!(
            active.first().map(|appearance| appearance.id),
            call.active_appearance(),
            "after {sequence:?}"
        );
    }

    let operations = [
        Operation::Answer(CallId(2)),
        Operation::Answer(CallId(3)),
        Operation::Hold(CallId(2)),
        Operation::Hold(CallId(3)),
        Operation::Resume(CallId(2)),
        Operation::Resume(CallId(3)),
        Operation::Steal(CallId(2)),
        Operation::Steal(CallId(3)),
        Operation::DisconnectFirst,
        Operation::DisconnectSecond,
        Operation::PbxHangup,
    ];

    for first in operations {
        for second in operations {
            for third in operations {
                for fourth in operations {
                    let sequence = [first, second, third, fourth];
                    let mut controller = shared_inbound_controller();
                    assert_shared_invariants(&controller, &[]);
                    for (index, operation) in sequence.into_iter().enumerate() {
                        apply(&mut controller, operation);
                        assert_shared_invariants(&controller, &sequence[..=index]);
                    }
                }
            }
        }
    }
}
