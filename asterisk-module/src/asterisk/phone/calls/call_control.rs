//! Handset call-control, feature-button, and supplementary-service events.

use super::super::transfer::{
    handle_direct_transfer, handle_transfer_hangup, handle_transfer_soft_key,
};
use super::super::{
    Access, AsteriskCallCompletion, AsteriskChannel, BargeMode, CallCompletionError,
    CallCompletionOwnership, CallState, DriverEffect, HookFlashAction, HotlineCallRequest, Instant,
    LogLevel, PbxAudioFormat, PhoneCommand, PhoneCommandAction, PhoneDeviceEvent,
    PhoneDeviceEventKind, RuntimeRecordings, SoftKey, ast_log, begin_parking_retrieval,
    cancel_forwarding_entry_for_call, commit_forwarding_entry, execute_answer_call_transition,
    execute_call_transition, forwarding_entry_exists, handle_barge_soft_key,
    handle_conference_destination, handle_conference_list_action, handle_conference_soft_key,
    handle_dnd_button, handle_feature_button, handle_feature_soft_key, handle_forwarding_backspace,
    handle_forwarding_digit, handle_hold_or_resume, handle_join_soft_key, handle_mobility_button,
    handle_mobility_response, handle_park_request, handle_parking_lot_button,
    handle_pickup_soft_key, handle_recording_button, handle_voicemail_soft_key, preferred_codec,
    replace_and_commit_forwarding_entry, replace_forwarding_entry, show_conference_list,
    toggle_monitor_recording, with_channel,
};
use super::handle_handset_hangup;

pub(super) async fn handle_call_control_event(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    event: PhoneDeviceEvent,
) -> Vec<DriverEffect> {
    let PhoneDeviceEvent {
        device_id,
        session_generation: _,
        event,
    } = event;
    match event {
        PhoneDeviceEventKind::OffHook {
            call_id,
            line_instance,
        } => {
            let config = access.config();
            let binding = access.line_binding(&device_id, line_instance.get());
            let hotline = binding
                .as_ref()
                .and_then(|binding| config.hotline_destination_for_binding(binding).cloned());
            drop(config);
            let codec = preferred_codec(
                access,
                &device_id,
                line_instance.get(),
                &PbxAudioFormat::ALL,
            );
            let ringing = access
                .shared
                .controller
                .snapshot()
                .call(call_id)
                .is_some_and(|call| call.state == CallState::Ringing);
            if ringing {
                let transition = access
                    .shared
                    .controller
                    .begin_active_call_switch_transaction(&device_id, call_id)
                    .unwrap_or_else(|_| {
                        Err(crate::runtime::controller::CallSwitchRejection::Unavailable)
                    });
                if let Ok(transition) = transition {
                    execute_answer_call_transition(access, transition).await;
                }
                Vec::new()
            } else if let Some((binding, codec)) = binding.zip(codec) {
                let transition = {
                    if let Some(destination) = hotline {
                        access
                            .shared
                            .controller
                            .begin_hotline_call_transaction(HotlineCallRequest {
                                handset_call_id: call_id,
                                binding,
                                codec,
                                destination,
                                now: Instant::now(),
                            })
                            .unwrap_or_else(|_| {
                                Err(crate::runtime::controller::CallSwitchRejection::Unavailable)
                            })
                    } else {
                        access
                            .shared
                            .controller
                            .begin_additional_phone_call_transaction(
                                call_id,
                                binding,
                                codec,
                                Instant::now(),
                            )
                            .unwrap_or_else(|_| {
                                Err(crate::runtime::controller::CallSwitchRejection::Unavailable)
                            })
                    }
                };
                if let Ok(transition) = transition {
                    execute_call_transition(access, transition).await;
                }
                Vec::new()
            } else {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id.clone(),
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "No compatible audio codec".into(),
                        },
                    ))
                    .await;
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::CloseCall { call_id },
                    ))
                    .await;
                Vec::new()
            }
        }
        PhoneDeviceEventKind::OnHook { call_id, .. } => {
            if cancel_forwarding_entry_for_call(access, &device_id, call_id) {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::CloseCall { call_id },
                    ))
                    .await;
            } else if !handle_transfer_hangup(access, device_id, call_id, true).await {
                handle_handset_hangup(access, call_id, true).await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::Digit { call_id, digit } => {
            if handle_forwarding_digit(access, &device_id, call_id, digit).await {
                Vec::new()
            } else {
                access
                    .shared
                    .controller
                    .digit(call_id, digit, Instant::now())
                    .unwrap_or_else(|_| Vec::new())
            }
        }
        PhoneDeviceEventKind::EnblocCall {
            call_id, number, ..
        } => {
            if replace_and_commit_forwarding_entry(access, &device_id, call_id, &number).await {
                Vec::new()
            } else {
                access
                    .shared
                    .controller
                    .enbloc(call_id, number)
                    .unwrap_or_else(|_| Vec::new())
            }
        }
        PhoneDeviceEventKind::SpeedDial {
            call_id,
            number,
            await_further_digits,
            ..
        } => {
            let forwarding_handled = if await_further_digits {
                replace_forwarding_entry(access, &device_id, call_id, &number).await
            } else {
                replace_and_commit_forwarding_entry(access, &device_id, call_id, &number).await
            };
            if forwarding_handled {
                Vec::new()
            } else {
                access
                    .shared
                    .controller
                    .speed_dial(call_id, number, await_further_digits, Instant::now())
                    .unwrap_or_else(|_| Vec::new())
            }
        }
        PhoneDeviceEventKind::FeatureButton { instance } => {
            handle_feature_button(access, device_id, instance.get());
            Vec::new()
        }
        PhoneDeviceEventKind::DoNotDisturbButton { instance } => {
            handle_dnd_button(access, device_id, instance.get());
            Vec::new()
        }
        PhoneDeviceEventKind::RecordingButton { instance } => {
            handle_recording_button(access, recordings, device_id, instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::MobilityButton { instance } => {
            handle_mobility_button(access, device_id, instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::VoicemailButton {
            call_id,
            line_instance,
        } => {
            let destination = {
                let config = access.config();
                access
                    .line_binding(&device_id, line_instance.get())
                    .and_then(|binding| {
                        config
                            .features_for_line(&binding.line.number)
                            .and_then(|features| features.voicemail.number.as_ref())
                            .map(|destination| destination.as_str().to_owned())
                    })
            };
            if let Some(destination) = destination {
                access
                    .shared
                    .controller
                    .enbloc(call_id, destination)
                    .unwrap_or_else(|_| Vec::new())
            } else {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "Voicemail is not configured for this line".into(),
                        },
                    ))
                    .await;
                Vec::new()
            }
        }
        PhoneDeviceEventKind::ParkingLotButton {
            instance,
            call_id,
            line_instance,
        } => {
            handle_parking_lot_button(
                access,
                device_id,
                instance.get(),
                call_id,
                line_instance.get(),
            )
            .await;
            Vec::new()
        }
        PhoneDeviceEventKind::ParkingMenuSelection { lot, slot } => {
            let _ = begin_parking_retrieval(access, device_id, 0, lot, slot).await;
            Vec::new()
        }
        PhoneDeviceEventKind::PhoneServiceResponse { response } => {
            handle_mobility_response(access, device_id, response).await;
            Vec::new()
        }
        PhoneDeviceEventKind::ConferenceListAction { action } => {
            handle_conference_list_action(access, device_id, action).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Answer,
            ..
        } => {
            let transition = access
                .shared
                .controller
                .begin_active_call_switch_transaction(&device_id, call_id)
                .unwrap_or_else(|_| {
                    Err(crate::runtime::controller::CallSwitchRejection::Unavailable)
                });
            if let Ok(transition) = transition {
                execute_answer_call_transition(access, transition).await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id,
            line_instance,
            soft_key,
        } if matches!(
            soft_key,
            SoftKey::ImmediateDivert | SoftKey::TransferToVoicemail
        ) =>
        {
            handle_voicemail_soft_key(access, device_id, call_id, line_instance.get(), soft_key)
                .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id,
            line_instance,
            soft_key,
        } if matches!(
            soft_key,
            SoftKey::DoNotDisturb
                | SoftKey::Private
                | SoftKey::ForwardAll
                | SoftKey::ForwardBusy
                | SoftKey::ForwardNoAnswer
        ) =>
        {
            handle_feature_soft_key(access, device_id, call_id, line_instance.get(), soft_key)
                .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Park,
        } => {
            handle_park_request(access, device_id, call_id, line_instance.get(), None).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: soft_key @ (SoftKey::Pickup | SoftKey::GroupPickup),
        } => {
            handle_pickup_soft_key(
                access,
                device_id,
                call_id,
                line_instance.get(),
                soft_key == SoftKey::Pickup,
            )
            .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Conference,
        } => {
            handle_conference_soft_key(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::MeetMe,
        } => {
            handle_conference_destination(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::ConferenceList,
            ..
        } => {
            show_conference_list(access, device_id, call_id).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Barge,
        } => {
            handle_barge_soft_key(
                access,
                device_id,
                call_id,
                line_instance.get(),
                BargeMode::Directed,
            )
            .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            line_instance,
            soft_key: SoftKey::Join,
        } => {
            handle_join_soft_key(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Select,
            ..
        } => {
            let selected = access
                .shared
                .controller
                .toggle_call_selected(&device_id, call_id)
                .unwrap_or_else(|_| None);
            if let Some(selected) = selected {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::SetCallSelected { call_id, selected },
                    ))
                    .await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Backspace,
            ..
        } if forwarding_entry_exists(access, &device_id, call_id) => {
            handle_forwarding_backspace(access, &device_id, call_id);
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Dial,
            ..
        } if forwarding_entry_exists(access, &device_id, call_id) => {
            commit_forwarding_entry(access, &device_id, call_id).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::EndCall,
            ..
        } => {
            if cancel_forwarding_entry_for_call(access, &device_id, call_id) {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::CloseCall { call_id },
                    ))
                    .await;
            } else if !handle_transfer_hangup(access, device_id, call_id, false).await {
                handle_handset_hangup(access, call_id, false).await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Hold,
            ..
        } => {
            handle_hold_or_resume(access, call_id, true, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Resume,
            ..
        } => {
            handle_hold_or_resume(access, call_id, false, false).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id,
            line_instance,
            soft_key: SoftKey::Transfer,
        } => {
            handle_transfer_soft_key(access, device_id, call_id, line_instance.get()).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(_),
            soft_key: SoftKey::DirectTransfer,
            ..
        } => {
            handle_direct_transfer(access, device_id).await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Callback,
            ..
        } => {
            let owner = access
                .shared
                .controller
                .snapshot()
                .call(call_id)
                .map(|call| (call.device_id.clone(), call.sccp_id, call.pbx_id));
            let result = owner
                .as_ref()
                .and_then(|(owner_device, owner_call_id, pbx_id)| {
                    with_channel(access, *pbx_id, |channel| {
                        let channel = unsafe { AsteriskChannel::from_raw(channel.cast()) }
                            .map_err(|_| CallCompletionError::Unavailable)?;
                        AsteriskCallCompletion::new().request_owned(
                            CallCompletionOwnership {
                                requested_device: device_id.as_str(),
                                requested_call_id: call_id.0,
                                owner_device: Some(owner_device.as_str()),
                                owner_call_id: Some(owner_call_id.0),
                            },
                            Some(&channel),
                        )
                    })
                })
                .unwrap_or(Err(CallCompletionError::Unavailable));
            let text = match result {
                Ok(ticket) => {
                    ast_log(
                        LogLevel::Debug,
                        &format!(
                            "accepted call-completion core {} for handset call {}",
                            ticket.core_id, call_id.0
                        ),
                    );
                    "Callback requested"
                }
                Err(error) => error.handset_prompt(),
            };
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::DisplayPrompt {
                        call_id,
                        timeout_seconds: 4,
                        text: text.into(),
                    },
                ))
                .await;
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::Monitor,
            ..
        } => {
            if let Err(error) =
                toggle_monitor_recording(access, recordings, &device_id, call_id).await
            {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to change SCCP recording state: {error}"),
                );
                let _ = access
                    .phone
                    .send_confirmed(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 4,
                            text: "Recording unavailable".into(),
                        },
                    ))
                    .await;
            }
            Vec::new()
        }
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key: SoftKey::VideoMode,
            ..
        } => access
            .shared
            .controller
            .video_mode_for_device(&device_id, call_id)
            .unwrap_or_else(|_| Vec::new()),
        PhoneDeviceEventKind::SoftKey {
            call_id: Some(call_id),
            soft_key,
            ..
        } => match soft_key {
            SoftKey::Answer => access
                .shared
                .controller
                .phone_answer(call_id)
                .unwrap_or_default(),
            SoftKey::Intercept => access.shared.controller.steal(call_id).unwrap_or_default(),
            SoftKey::Dial => access
                .shared
                .controller
                .enbloc(call_id, String::new())
                .unwrap_or_default(),
            _ => Vec::new(),
        },
        PhoneDeviceEventKind::LineButton {
            call_id: Some(call_id),
            ..
        } => {
            let state = access.shared.controller.snapshot().call_state(call_id);
            if matches!(state, Some(CallState::Held | CallState::SharedHeld)) {
                let transition = access
                    .shared
                    .controller
                    .begin_active_call_switch_transaction(&device_id, call_id)
                    .unwrap_or_else(|_| {
                        Err(crate::runtime::controller::CallSwitchRejection::Unavailable)
                    });
                if let Ok(transition) = transition {
                    execute_call_transition(access, transition).await;
                }
                Vec::new()
            } else {
                match state {
                    Some(CallState::Ringing | CallState::Connected) => {
                        let transition = access
                            .shared
                            .controller
                            .begin_active_call_switch_transaction(&device_id, call_id)
                            .unwrap_or_else(|_| {
                                Err(crate::runtime::controller::CallSwitchRejection::Unavailable)
                            });
                        if let Ok(transition) = transition {
                            if state == Some(CallState::Ringing) {
                                execute_answer_call_transition(access, transition).await;
                            } else {
                                execute_call_transition(access, transition).await;
                            }
                        }
                        Vec::new()
                    }
                    Some(CallState::RemoteInUse) => access
                        .shared
                        .controller
                        .steal(call_id)
                        .unwrap_or_else(|_| Vec::new()),
                    _ => Vec::new(),
                }
            }
        }
        PhoneDeviceEventKind::HookFlash {
            call_id: Some(call_id),
            line_instance,
        } => {
            let action = access
                .shared
                .controller
                .snapshot()
                .hook_flash_action(&device_id, call_id);
            match action {
                HookFlashAction::AnswerWaiting(waiting_call_id) => {
                    let transition = access
                        .shared
                        .controller
                        .begin_active_call_switch_transaction(&device_id, waiting_call_id)
                        .unwrap_or_else(|_| {
                            Err(crate::runtime::controller::CallSwitchRejection::Unavailable)
                        });
                    if let Ok(transition) = transition {
                        execute_answer_call_transition(access, transition).await;
                    }
                    Vec::new()
                }
                HookFlashAction::Transfer => {
                    handle_transfer_soft_key(access, device_id, Some(call_id), line_instance.get())
                        .await;
                    Vec::new()
                }
                HookFlashAction::Ignore => Vec::new(),
            }
        }
        PhoneDeviceEventKind::HookFlash { .. } => Vec::new(),
        _ => unreachable!("call-control event was classified before dispatch"),
    }
}
