//! Handset-facing translation of exhaustive backend effects.

use super::{
    Access, CallId, ConferenceId, DeviceId, HandsetEffect, IpAddr, IpAddressType, Ipv4Addr,
    LineInstance, MANAGER_CONTROL_DELIVERY_TIMEOUT, MediaEndpointAddress,
    MultimediaReceiveDescriptor, MultimediaTransmitControl, MultimediaTransmitDescriptor,
    PhoneCallState, PhoneCommand, PhoneCommandAction, ProtocolVersion, ReceiveChannelPurpose,
    SessionGeneration, VideoPlan, audio_framing, begin_answer_media, begin_handset_media,
    begin_outbound_media, call_event, configured_audio_processing, configured_audio_traffic_class,
    configured_dtmf_mode, configured_video_traffic_class, publish_ami_event, receive_media_source,
};

pub async fn send_handset_call_state(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    state: PhoneCallState,
) -> Result<(), String> {
    let privacy = access
        .shared
        .controller
        .snapshot()
        .call_privacy(call_id)
        .unwrap_or(true);
    access
        .phone
        .send_confirmed(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::SetCallState { call_id, state },
        ))
        .await
        .map_err(|error| error.to_string())?;
    publish_ami_event(access, &call_event(&device_id, call_id, state, privacy));
    Ok(())
}

#[derive(Clone, Copy)]
enum VideoEffectDirection {
    Receive,
    Transmit,
}

fn video_plan_for_effect(
    access: &Access,
    device_id: &DeviceId,
    session_generation: SessionGeneration,
    call_id: CallId,
    direction: VideoEffectDirection,
) -> Result<VideoPlan, String> {
    let controller = access.shared.controller.snapshot();
    match direction {
        VideoEffectDirection::Receive => controller
            .opening_video_receive_plan_for_device(device_id, session_generation, call_id)
            .cloned(),
        VideoEffectDirection::Transmit => controller
            .opening_video_transmit_plan_for_device(device_id, session_generation, call_id)
            .cloned(),
    }
    .ok_or_else(|| format!("call {call_id:?} has no current video plan for {device_id}"))
}

fn video_conference_id(call_id: CallId) -> Result<ConferenceId, String> {
    u32::try_from(call_id.get())
        .map(ConferenceId::new)
        .map_err(|_| format!("call {call_id:?} exceeds the video conference identity space"))
}

fn endpoint_address_type(endpoint: MediaEndpointAddress) -> IpAddressType {
    if endpoint.address.is_ipv4() {
        IpAddressType::Ipv4
    } else {
        IpAddressType::Ipv6
    }
}

fn video_receive_descriptor(
    call_id: CallId,
    plan: &VideoPlan,
) -> Result<MultimediaReceiveDescriptor, String> {
    let (source, requested_address_type) = if plan.protocol < ProtocolVersion::V12 {
        (
            MediaEndpointAddress {
                address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                port: 0,
            },
            IpAddressType::Ipv4,
        )
    } else {
        (
            plan.local_endpoint,
            endpoint_address_type(plan.local_endpoint),
        )
    };
    MultimediaReceiveDescriptor {
        conference_id: video_conference_id(call_id)?,
        payload: plan.payload.clone(),
        conference_creator: false,
        encryption: None,
        stream_passthrough_id: 0,
        associated_stream_id: 0,
        source,
        requested_address_type,
    }
    .validate()
    .map_err(|error| error.to_string())
}

fn video_transmit_descriptor(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
    plan: &VideoPlan,
) -> Result<MultimediaTransmitDescriptor, String> {
    let traffic_class = configured_video_traffic_class(access, device_id)
        .ok_or_else(|| format!("invalid video traffic class for {device_id}"))?;
    MultimediaTransmitDescriptor {
        conference_id: video_conference_id(call_id)?,
        endpoint: plan.local_endpoint,
        payload: plan.payload.clone(),
        traffic_class,
        encryption: None,
        stream_passthrough_id: 0,
        associated_stream_id: 0,
    }
    .validate()
    .map_err(|error| error.to_string())
}

pub async fn execute_handset_effect(access: &Access, effect: HandsetEffect) -> Result<(), String> {
    match effect {
        HandsetEffect::BeginCall {
            device_id,
            line_instance,
            call_id,
            codec,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::BeginCall {
                    line_instance: LineInstance::new(line_instance),
                    call_id,
                    codec,
                },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::BeginTransfer {
            device_id,
            source_call_id,
            consultation_call_id,
            consultation_line_instance,
            codec,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::BeginTransfer {
                    source_call_id,
                    consultation_line_instance: LineInstance::new(consultation_line_instance),
                    consultation_call_id,
                    codec,
                },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::StartTone {
            device_id,
            call_id,
            tone,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::StartTone { call_id, tone },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::CommitOutboundCall {
            device_id,
            call_id,
            info,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CommitOutboundCall { call_id, info },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::PresentOutboundProceeding {
            device_id,
            call_id,
            info,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::PresentOutboundProceeding { call_id, info },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::PresentOutboundRinging {
            device_id,
            call_id,
            info,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::PresentOutboundRinging { call_id, info },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::SetCallInfo {
            device_id,
            call_id,
            info,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::SetCallInfo { call_id, info },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::BeginMedia {
            device_id,
            call_id,
            codec,
        } => {
            begin_handset_media(access, device_id, call_id, codec, PhoneCallState::Connected).await
        }
        HandsetEffect::BeginAnswerMedia {
            device_id,
            call_id,
            codec,
        } => begin_answer_media(access, device_id, call_id, codec).await,
        HandsetEffect::BeginOutboundMedia {
            device_id,
            call_id,
            codec,
        } => begin_outbound_media(access, device_id, call_id, codec).await,
        HandsetEffect::BeginOneWayMedia {
            device_id,
            call_id,
            codec,
        } => {
            begin_handset_media(
                access,
                device_id,
                call_id,
                codec,
                PhoneCallState::IntercomOneWay,
            )
            .await
        }
        HandsetEffect::BeginEarlyMedia {
            device_id,
            call_id,
            codec,
        } => begin_handset_media(access, device_id, call_id, codec, PhoneCallState::Proceed).await,
        HandsetEffect::OpenVideoReceive {
            device_id,
            call_id,
            session_generation,
        } => {
            let plan = video_plan_for_effect(
                access,
                &device_id,
                session_generation,
                call_id,
                VideoEffectDirection::Receive,
            )?;
            let descriptor = video_receive_descriptor(call_id, &plan)?;
            access
                .phone
                .send_confirmed(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::OpenMultimediaReceiveChannel {
                        call_id,
                        descriptor,
                    },
                ))
                .await
                .map_err(|error| error.to_string())
        }
        HandsetEffect::StartVideoTransmit {
            device_id,
            call_id,
            session_generation,
        } => {
            let plan = video_plan_for_effect(
                access,
                &device_id,
                session_generation,
                call_id,
                VideoEffectDirection::Transmit,
            )?;
            let descriptor = video_transmit_descriptor(access, &device_id, call_id, &plan)?;
            access
                .phone
                .send_confirmed(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::StartMultimediaTransmission {
                        call_id,
                        descriptor,
                    },
                ))
                .await
                .map_err(|error| error.to_string())
        }
        HandsetEffect::RefreshVideo {
            device_id,
            call_id,
            session_generation,
            passthrough_party_id,
        } => {
            if !access
                .shared
                .controller
                .snapshot()
                .video_refresh_is_current(
                    &device_id,
                    session_generation,
                    call_id,
                    passthrough_party_id,
                )
            {
                return Ok(());
            }
            access
                .phone
                .send_confirmed(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::ControlMultimediaTransmission {
                        call_id,
                        passthrough_party_id,
                        control: MultimediaTransmitControl::FastPictureUpdate {
                            first_gob: 0,
                            gob_count: 0,
                        },
                    },
                ))
                .await
                .map_err(|error| error.to_string())
        }
        HandsetEffect::StopVideo {
            device_id,
            call_id,
            session_generation,
        } => {
            if !access
                .shared
                .controller
                .snapshot()
                .session_is_current(&device_id, session_generation)
            {
                return Ok(());
            }
            let close = access
                .phone
                .send_confirmed(PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::CloseMultimediaReceiveChannel { call_id },
                ))
                .await
                .map_err(|error| error.to_string());
            let stop = access
                .phone
                .send_confirmed(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::StopMultimediaTransmission { call_id },
                ))
                .await
                .map_err(|error| error.to_string());
            close.and(stop)
        }
        HandsetEffect::StartMedia {
            device_id,
            call_id,
            mut endpoint,
        } => {
            let framing = audio_framing(access, &device_id, call_id, endpoint.codec)
                .map_err(|error| error.to_string())?;
            let dtmf_mode = configured_dtmf_mode(access, &device_id, call_id);
            let audio_processing = configured_audio_processing(access, &device_id, call_id);
            let traffic_class = configured_audio_traffic_class(access, &device_id)
                .ok_or_else(|| format!("invalid audio traffic class for {device_id}"))?;
            endpoint.packet_ms = framing.packet_ms;
            endpoint.max_frames_per_packet = framing.max_frames_per_packet;
            access
                .phone
                .send_confirmed(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::StartMedia {
                        call_id,
                        endpoint,
                        dtmf_mode,
                        audio_processing,
                        traffic_class,
                    },
                ))
                .await
                .map_err(|error| error.to_string())
        }
        HandsetEffect::PickupCompleted {
            device_id,
            call_id,
            codec,
            answer,
            parties,
        } => {
            let info = access
                .shared
                .controller
                .apply_pickup_identity(call_id, parties)
                .map_err(|error| error.to_string())?;
            access
                .phone
                .send_confirmed(PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::SetCallInfo { call_id, info },
                ))
                .await
                .map_err(|error| error.to_string())?;
            if answer {
                let framing = audio_framing(access, &device_id, call_id, codec)
                    .map_err(|error| error.to_string())?;
                let source = receive_media_source(access, &device_id, call_id, codec)?;
                let dtmf_mode = configured_dtmf_mode(access, &device_id, call_id);
                let audio_processing = configured_audio_processing(access, &device_id, call_id);
                access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id.clone(),
                        PhoneCommandAction::StopRinging { call_id },
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
                send_handset_call_state(
                    access,
                    device_id.clone(),
                    call_id,
                    PhoneCallState::Connected,
                )
                .await?;
                access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::OpenReceiveChannel {
                            call_id,
                            purpose: ReceiveChannelPurpose::Media,
                            source: Some(source),
                            codec,
                            packet_ms: framing.packet_ms,
                            max_frames_per_packet: framing.max_frames_per_packet,
                            dtmf_mode,
                            audio_processing,
                        },
                    ))
                    .await
                    .map_err(|error| error.to_string())
            } else {
                access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id.clone(),
                        PhoneCommandAction::StartRinging { call_id },
                    ))
                    .await
                    .map_err(|error| error.to_string())?;
                send_handset_call_state(access, device_id, call_id, PhoneCallState::RingIn).await
            }
        }
        HandsetEffect::ShowConferenceList {
            device_id,
            call_id,
            conference_id,
            participants,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::ShowConferenceList {
                    call_id,
                    conference_id,
                    participants,
                },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::ShowConferenceParticipantActions {
            device_id,
            call_id,
            conference_id,
            participant,
            removable,
            demotable,
        } => access
            .phone
            .send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::ShowConferenceParticipantActions {
                    call_id,
                    conference_id,
                    participant,
                    removable,
                    demotable,
                },
            ))
            .await
            .map_err(|error| error.to_string()),
        HandsetEffect::SetCallState {
            device_id,
            call_id,
            state,
            stop_media,
        } => {
            let mut first_error = None;
            if stop_media {
                if let Err(error) = access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id.clone(),
                        PhoneCommandAction::CloseReceiveChannel { call_id },
                    ))
                    .await
                {
                    first_error = Some(error.to_string());
                }
                if let Err(error) = access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id.clone(),
                        PhoneCommandAction::StopMedia { call_id },
                    ))
                    .await
                {
                    first_error.get_or_insert_with(|| error.to_string());
                }
            }
            if state != PhoneCallState::OnHook {
                match send_handset_call_state(access, device_id.clone(), call_id, state).await {
                    Ok(()) => {}
                    Err(error) => {
                        first_error.get_or_insert(error);
                    }
                }
            }
            if state == PhoneCallState::Connected
                && let Some(info) = access
                    .shared
                    .controller
                    .snapshot()
                    .call_info(call_id)
                    .cloned()
                && let Err(error) = access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id.clone(),
                        PhoneCommandAction::SetCallInfo { call_id, info },
                    ))
                    .await
            {
                first_error.get_or_insert_with(|| error.to_string());
            }
            if state == PhoneCallState::OnHook
                && let Err(error) = access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::CloseCall { call_id },
                    ))
                    .await
            {
                first_error.get_or_insert_with(|| error.to_string());
            }
            first_error.map_or(Ok(()), Err)
        }
        HandsetEffect::SetMicrophoneMode {
            device_id,
            call_id: _,
            enabled,
        } => tokio::time::timeout(
            MANAGER_CONTROL_DELIVERY_TIMEOUT,
            access.phone.send_confirmed(PhoneCommand::new(
                device_id,
                PhoneCommandAction::SetMicrophoneMode { enabled },
            )),
        )
        .await
        .map_err(|_| "handset microphone command timed out".to_owned())?
        .map_err(|error| error.to_string()),
    }
}
