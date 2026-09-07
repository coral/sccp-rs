//! Audio/video acknowledgement, recovery, and media telemetry events.

use super::super::{
    Access, AmiMediaDirection, AmiMediaKind, AmiMediaState, CallId, DeviceId, DriverEffect,
    LogLevel, MediaEndpoint, MediaFailureDisposition, MediaStatus, NonNull, PhoneDeviceEvent,
    PhoneDeviceEventKind, TransmitOpenOutcome, ast_log, execute_effects, media_event,
    native_channel, normalize_phone_media_endpoint, normalize_phone_video_endpoint,
    publish_ami_event, recover_failed_media_transmission, set_remote_video_endpoint, with_channel,
};
use super::{handle_handset_hangup, owned_pbx_call};
use crate::runtime::controller::VideoFallbackReason;

async fn commit_transmit_open(
    access: &Access,
    device_id: &DeviceId,
    call_id: CallId,
    endpoint: MediaEndpoint,
) {
    let codec_id = endpoint.codec.wire_value();
    let packet_ms = endpoint.packet_ms;
    let (actions, accepted) = access
        .shared
        .controller
        .accept_media_transmission(device_id, call_id, endpoint)
        .unwrap_or_else(|_| (Vec::new(), false));
    execute_effects(access, actions).await;
    if accepted {
        publish_ami_event(
            access,
            &media_event(
                device_id,
                call_id,
                AmiMediaKind::Audio,
                AmiMediaDirection::Transmit,
                AmiMediaState::Open,
                MediaStatus::Ok,
                Some(codec_id),
                Some(packet_ms),
            ),
        );
    }
}

pub(super) async fn handle_media_event(
    access: &Access,
    event: PhoneDeviceEvent,
) -> Vec<DriverEffect> {
    let PhoneDeviceEvent {
        device_id,
        session_generation,
        event,
    } = event;
    match event {
        PhoneDeviceEventKind::ReceiveChannelOpened {
            call_id,
            status: MediaStatus::Ok,
            mut endpoint,
        } => match normalize_phone_media_endpoint(access, &device_id, &mut endpoint) {
            Ok(()) => {
                let codec_id = endpoint.codec.wire_value();
                let packet_ms = endpoint.packet_ms;
                let (actions, accepted) = access
                    .shared
                    .controller
                    .accept_media_receive(device_id.clone(), call_id, endpoint)
                    .unwrap_or_else(|_| (Vec::new(), false));
                execute_effects(access, actions).await;
                if accepted {
                    publish_ami_event(
                        access,
                        &media_event(
                            &device_id,
                            call_id,
                            AmiMediaKind::Audio,
                            AmiMediaDirection::Receive,
                            AmiMediaState::Open,
                            MediaStatus::Ok,
                            Some(codec_id),
                            Some(packet_ms),
                        ),
                    );
                }
                Vec::new()
            }
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone reported an unusable media endpoint for call {call_id:?}: {error}"
                    ),
                );
                handle_handset_hangup(access, call_id, false).await;
                Vec::new()
            }
        },
        PhoneDeviceEventKind::ReceiveChannelOpened {
            call_id, status, ..
        } => {
            ast_log(
                LogLevel::Warning,
                &format!("phone failed to open media for call {call_id:?}: {status:?}"),
            );
            handle_handset_hangup(access, call_id, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::MultimediaReceiveChannelOpened {
            call_id,
            codec,
            mut endpoint,
            passthrough_party_id: _,
        } => {
            let pbx_id = owned_pbx_call(access, &device_id, call_id);
            let normalized = normalize_phone_video_endpoint(access, &device_id, &mut endpoint);
            let accepted = normalized.is_ok()
                && access
                    .shared
                    .controller
                    .video_receive_opened_for_device(
                        &device_id,
                        session_generation,
                        call_id,
                        codec,
                        endpoint,
                    )
                    .unwrap_or_else(|_| false);
            let configured = accepted
                && pbx_id.is_some_and(|pbx_id| {
                    set_remote_video_endpoint(access, pbx_id, endpoint).is_ok()
                });
            if configured {
                if let Some(pbx_id) = pbx_id {
                    let queued = with_channel(access, pbx_id, |channel| unsafe {
                        native_channel::queue_control(
                            NonNull::new_unchecked(channel),
                            native_channel::ChannelControl::VideoUpdate,
                        )
                    });
                    if !matches!(queued, Some(Ok(()))) {
                        ast_log(
                            LogLevel::Warning,
                            &format!("unable to request a fresh video frame for call {call_id:?}"),
                        );
                    }
                }
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Receive,
                        AmiMediaState::Open,
                        MediaStatus::Ok,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            } else {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to configure video receive endpoint for call {call_id:?}"),
                );
            }
            if configured {
                access
                    .shared
                    .controller
                    .begin_video_transmit_for_device(&device_id, session_generation, call_id)
                    .unwrap_or_else(|_| Vec::new())
            } else {
                access
                    .shared
                    .controller
                    .video_fallback_for_device(
                        &device_id,
                        session_generation,
                        call_id,
                        VideoFallbackReason::ReceiveFailed,
                    )
                    .unwrap_or_else(|_| crate::runtime::controller::VideoFallbackOutcome::Ignored)
                    .into_effects()
            }
        }
        PhoneDeviceEventKind::MultimediaReceiveChannelFailed {
            call_id,
            codec,
            status,
            endpoint: _,
            passthrough_party_id: _,
        } => {
            let fallback = access
                .shared
                .controller
                .video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::ReceiveFailed,
                )
                .unwrap_or_else(|_| crate::runtime::controller::VideoFallbackOutcome::Ignored);
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Receive,
                        AmiMediaState::Failed,
                        status,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::MultimediaReceiveChannelTimedOut {
            call_id,
            codec,
            passthrough_party_id: _,
        } => {
            let fallback = access
                .shared
                .controller
                .video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::ReceiveFailed,
                )
                .unwrap_or_else(|_| crate::runtime::controller::VideoFallbackOutcome::Ignored);
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone did not acknowledge video receive setup for call {call_id:?} with codec {codec:?}"
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::MultimediaTransmitStarted {
            call_id,
            codec,
            endpoint,
            passthrough_party_id,
        } => {
            let accepted = access
                .shared
                .controller
                .video_transmit_opened_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    codec,
                    endpoint,
                    passthrough_party_id,
                )
                .unwrap_or_else(|_| false);
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Transmit,
                        AmiMediaState::Open,
                        MediaStatus::Ok,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            } else {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "ignored unexpected video transmit acknowledgement for call {call_id:?}"
                    ),
                );
            }
            Vec::new()
        }
        PhoneDeviceEventKind::MultimediaTransmitFailed {
            call_id,
            codec,
            status,
            endpoint: _,
            passthrough_party_id: _,
        } => {
            let fallback = access
                .shared
                .controller
                .video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::TransmitFailed,
                )
                .unwrap_or_else(|_| crate::runtime::controller::VideoFallbackOutcome::Ignored);
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Video,
                        AmiMediaDirection::Transmit,
                        AmiMediaState::Failed,
                        status,
                        Some(codec.wire_value()),
                        None,
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::MultimediaTransmitTimedOut {
            call_id,
            codec,
            passthrough_party_id: _,
        } => {
            let fallback = access
                .shared
                .controller
                .video_fallback_for_device(
                    &device_id,
                    session_generation,
                    call_id,
                    VideoFallbackReason::TransmitFailed,
                )
                .unwrap_or_else(|_| crate::runtime::controller::VideoFallbackOutcome::Ignored);
            let accepted = fallback.is_applied();
            let actions = fallback.into_effects();
            if accepted && owned_pbx_call(access, &device_id, call_id).is_some() {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone did not acknowledge video transmit setup for call {call_id:?} with codec {codec:?}"
                    ),
                );
            }
            actions
        }
        PhoneDeviceEventKind::TransmitChannelOpen {
            call_id,
            outcome: TransmitOpenOutcome::Acknowledged,
            mut endpoint,
        } => match normalize_phone_media_endpoint(access, &device_id, &mut endpoint) {
            Ok(()) => {
                commit_transmit_open(access, &device_id, call_id, endpoint).await;
                Vec::new()
            }
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!(
                        "phone reported an unusable transmit endpoint for call {call_id:?}: {error}"
                    ),
                );
                handle_handset_hangup(access, call_id, false).await;
                Vec::new()
            }
        },
        PhoneDeviceEventKind::TransmitChannelOpen {
            call_id,
            outcome: TransmitOpenOutcome::Implied | TransmitOpenOutcome::NotReported,
            endpoint,
        } => {
            commit_transmit_open(access, &device_id, call_id, endpoint).await;
            Vec::new()
        }
        PhoneDeviceEventKind::TransmitChannelOpen {
            call_id,
            outcome: TransmitOpenOutcome::Rejected(status),
            ..
        } => {
            ast_log(
                LogLevel::Warning,
                &format!("phone failed to start media for call {call_id:?}: {status:?}"),
            );
            handle_handset_hangup(access, call_id, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::HandsetAcknowledgementTimedOut { call_id, .. } => {
            ast_log(
                LogLevel::Warning,
                &format!(
                    "timed out waiting for open receive channel acknowledgement for call {call_id:?}"
                ),
            );
            handle_handset_hangup(access, call_id, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::MediaTransmissionFailed {
            call_id,
            status,
            endpoint,
        } => {
            let disposition =
                recover_failed_media_transmission(access, &device_id, call_id, endpoint);
            if disposition != MediaFailureDisposition::Ignored {
                publish_ami_event(
                    access,
                    &media_event(
                        &device_id,
                        call_id,
                        AmiMediaKind::Audio,
                        AmiMediaDirection::Transmit,
                        AmiMediaState::Failed,
                        status,
                        Some(endpoint.codec.wire_value()),
                        Some(endpoint.packet_ms),
                    ),
                );
            }
            match disposition {
                MediaFailureDisposition::Retrying => ast_log(
                    LogLevel::Notice,
                    &format!(
                        "retrying failed direct media through the local RTP anchor for call {call_id:?}"
                    ),
                ),
                MediaFailureDisposition::Hangup => {
                    ast_log(
                        LogLevel::Warning,
                        &format!(
                            "media transmission failed without an available recovery path for call {call_id:?}: {status:?}"
                        ),
                    );
                    handle_handset_hangup(access, call_id, false).await;
                }
                MediaFailureDisposition::Ignored => {}
            }
            Vec::new()
        }
        PhoneDeviceEventKind::MulticastReceptionStarted {
            conference_id: _,
            call_id: _,
            route: _,
        }
        | PhoneDeviceEventKind::MulticastReceptionFailed {
            conference_id: _,
            call_id: _,
            status: _,
        }
        | PhoneDeviceEventKind::MulticastReceptionTimedOut {
            conference_id: _,
            call_id: _,
        }
        | PhoneDeviceEventKind::MulticastTransmissionStarted {
            conference_id: _,
            call_id: _,
            route: _,
        }
        | PhoneDeviceEventKind::MulticastTransmissionFailed {
            conference_id: _,
            call_id: _,
            status: _,
            address: _,
            port: _,
        } => Vec::new(),
        PhoneDeviceEventKind::ConnectionStatisticsCollected { .. } => Vec::new(),
        _ => unreachable!("media event was classified before dispatch"),
    }
}
