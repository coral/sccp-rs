//! Conference, barge, shared-line, and PBX effect orchestration.

use super::{
    Access, AsteriskBackend, AsteriskBackendError, BargeMode, BargeRejection, BridgeOperation,
    CallFeatureError, CallId, CallState, ConferenceAnnouncement, ConferenceConsultationRequest,
    ConferenceDestinationRequest, ConferenceEndRejection, ConferenceListAction,
    ConferenceMediaPolicy, ConferenceMutationToken, ConferenceParticipantRejection,
    ConferencePhase, ConferenceRejection, ConferenceStartProgress, DeviceId, DriverEffect,
    Duration, EffectExecutionError, Instant, LogLevel, PbxAudioFormat, PbxEffect, PhoneCommand,
    PhoneCommandAction, ServiceProviderError, ast_log, cancel_conference_announcement,
    conference_participant_service_error, execute_cleanup_effects, execute_effects,
    execute_effects_confirmed, execute_handset_effect, execute_one_effect, preferred_codec,
    remove_channel,
};

pub(super) fn conference_mutation_is_active(
    access: &Access,
    mutation: ConferenceMutationToken,
) -> bool {
    access
        .shared
        .controller
        .snapshot()
        .conference_mutation_is_active(mutation)
}

pub(super) async fn handle_barge_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
    mode: BargeMode,
) {
    let binding = access.line_binding(&device_id, line_instance);
    let Some(binding) = binding else {
        return;
    };
    let Some(codec) = preferred_codec(access, &device_id, line_instance, &PbxAudioFormat::ALL)
    else {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds: 4,
                    text: "Barge codec unavailable".into(),
                },
            ))
            .await;
        return;
    };
    let result = access
        .shared
        .controller
        .barge(call_id, binding, codec, mode)
        .unwrap_or_else(|_| Err(crate::runtime::controller::BargeRejection::Unavailable));
    match result {
        Ok(effects) => {
            if execute_effects_confirmed(access, effects).await.is_ok()
                && let Some(pbx_id) = access
                    .shared
                    .controller
                    .snapshot()
                    .call(call_id)
                    .map(|call| call.pbx_id)
            {
                access.enqueue_recording_eligibility(pbx_id);
            }
        }
        Err(rejection) => {
            let text = match rejection {
                BargeRejection::Private => "Private call",
                BargeRejection::Capability => "Barge codec unavailable",
                BargeRejection::Conflict => "Another shared action won",
                BargeRejection::AlreadyBarged => "Another barge is active",
                BargeRejection::Unavailable | BargeRejection::NotRemote => "Barge unavailable",
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
        }
    }
}

pub(super) async fn handle_join_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
) {
    let state = access.shared.controller.snapshot().call_state(call_id);
    if state == Some(CallState::RemoteInUse) {
        handle_barge_soft_key(
            access,
            device_id,
            call_id,
            line_instance,
            BargeMode::Conference,
        )
        .await;
        return;
    }

    let conference_policy = access.config().conference_for_device(&device_id).cloned();
    let permitted = conference_policy
        .as_ref()
        .is_some_and(|conference| conference.allowed);
    let media_policy = conference_policy.map(|conference| ConferenceMediaPolicy {
        music_on_hold_class: conference.music_on_hold_class,
        mute_on_entry: conference.mute_on_entry,
        play_general_announcements: conference.play_general_announcements,
        play_participant_announcements: conference.play_participant_announcements,
    });
    let result = access
        .shared
        .controller
        .prepare_join_calls(
            device_id.clone(),
            call_id,
            permitted,
            media_policy.unwrap_or_default(),
        )
        .unwrap_or_else(|_| Err(crate::runtime::controller::ConferenceRejection::Unavailable));
    match result {
        Ok((mutation, effects)) => {
            let session = access
                .shared
                .controller
                .snapshot()
                .conference_session(call_id)
                .cloned();
            if let Some(session) = session {
                execute_selected_conference_merge(access, session, mutation, effects).await;
            }
        }
        Err(rejection) => {
            let text = match rejection {
                ConferenceRejection::Disabled => "Conference disabled",
                ConferenceRejection::NotConnected => "Select two connected calls",
                ConferenceRejection::Conflict => "Conference selection unavailable",
                ConferenceRejection::Unavailable => "Conference unavailable",
            };
            display_conference_prompt(access, device_id, call_id, text).await;
        }
    }
}

pub async fn show_conference_list(access: &Access, device_id: DeviceId, call_id: CallId) {
    let session = access
        .shared
        .controller
        .snapshot()
        .conference_session(call_id)
        .cloned();
    let Some(session) = session.filter(|session| {
        session.phase == ConferencePhase::Active && session.device_id == device_id
    }) else {
        display_conference_prompt(access, device_id, call_id, "No active conference").await;
        return;
    };
    if let Err(error) = execute_handset_effect(access, session.list_effect(call_id)).await {
        ast_log(
            LogLevel::Warning,
            &format!("unable to show conference list: {error}"),
        );
    }
}

pub(super) async fn show_conference_list_if_configured(
    access: &Access,
    session: &crate::runtime::controller::ConferenceSession,
) {
    let enabled = access
        .config()
        .conference_for_device(&session.device_id)
        .is_some_and(|conference| conference.show_conference_list);
    if enabled {
        show_conference_list(
            access,
            session.device_id.clone(),
            session.original_handset_call_id,
        )
        .await;
    }
}

pub(super) async fn handle_conference_list_action(
    access: &Access,
    device_id: DeviceId,
    action: ConferenceListAction,
) {
    let conference_id = match action {
        ConferenceListAction::Participant { conference_id, .. }
        | ConferenceListAction::Mute { conference_id, .. }
        | ConferenceListAction::Unmute { conference_id, .. }
        | ConferenceListAction::Remove { conference_id, .. }
        | ConferenceListAction::Promote { conference_id, .. }
        | ConferenceListAction::Demote { conference_id, .. }
        | ConferenceListAction::End { conference_id } => conference_id,
    };
    let session = access
        .shared
        .controller
        .snapshot()
        .conference_session_by_id(conference_id)
        .cloned();
    let Some(session) = session.filter(|session| {
        session.phase == ConferencePhase::Active && session.device_id == device_id
    }) else {
        return;
    };
    match action {
        ConferenceListAction::Participant { participant_id, .. } => {
            let Some(participant) = session.participants.get(participant_id) else {
                return;
            };
            let Some(effect) = session.participant_actions_effect(participant_id) else {
                let label = if participant.display_name.is_empty() {
                    participant.number.clone()
                } else {
                    participant.display_name.clone()
                };
                display_conference_prompt(
                    access,
                    device_id,
                    session.original_handset_call_id,
                    &label,
                )
                .await;
                return;
            };
            if let Err(error) = execute_handset_effect(access, effect).await {
                ast_log(
                    LogLevel::Warning,
                    &format!("unable to show conference participant actions: {error}"),
                );
            }
        }
        ConferenceListAction::Mute { participant_id, .. } => {
            let _ = set_conference_participant_muted(access, session, participant_id, true).await;
        }
        ConferenceListAction::Unmute { participant_id, .. } => {
            let _ = set_conference_participant_muted(access, session, participant_id, false).await;
        }
        ConferenceListAction::Remove { participant_id, .. } => {
            let _ = remove_conference_participant(access, session, participant_id).await;
        }
        ConferenceListAction::Promote { participant_id, .. } => {
            let _ =
                set_conference_participant_moderator(access, session, participant_id, true).await;
        }
        ConferenceListAction::Demote { participant_id, .. } => {
            let _ =
                set_conference_participant_moderator(access, session, participant_id, false).await;
        }
        ConferenceListAction::End { .. } => {
            let effects = access
                .shared
                .controller
                .end_conference_by_moderator(&device_id, session.id)
                .unwrap_or_else(|_| {
                    Err(crate::runtime::controller::ConferenceEndRejection::Unavailable)
                });
            match effects {
                Ok(effects) => {
                    cancel_conference_announcement(access, session.id);
                    execute_cleanup_effects(access, effects).await;
                }
                Err(rejection) => {
                    let text = match rejection {
                        ConferenceEndRejection::Unavailable => "Conference unavailable",
                        ConferenceEndRejection::NotModerator => "Moderator access required",
                        ConferenceEndRejection::Conflict => "Conference action pending",
                    };
                    display_conference_prompt(
                        access,
                        session.device_id,
                        session.original_handset_call_id,
                        text,
                    )
                    .await;
                }
            }
        }
    }
}

pub async fn remove_conference_participant(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    participant_id: sccp_protocol::ParticipantId,
) -> Result<(), ServiceProviderError> {
    let effects = access
        .shared
        .controller
        .prepare_conference_removal(session.device_id.clone(), session.id, participant_id)
        .unwrap_or_else(|_| {
            Err(crate::runtime::controller::ConferenceParticipantRejection::Unavailable)
        });
    let (mutation, effects) = match effects {
        Ok(operation) => operation,
        Err(rejection) => {
            let provider_error = conference_participant_service_error(rejection);
            let text = match rejection {
                ConferenceParticipantRejection::Unavailable => "Conference unavailable",
                ConferenceParticipantRejection::NotModerator => "Moderator access required",
                ConferenceParticipantRejection::InvalidParticipant => "Participant unavailable",
                ConferenceParticipantRejection::Moderator => "Moderator cannot be removed",
                ConferenceParticipantRejection::LastModerator => {
                    "At least one moderator is required"
                }
                ConferenceParticipantRejection::Conflict => "Participant cannot be removed",
            };
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                text,
            )
            .await;
            return Err(provider_error);
        }
    };

    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            let aborted = access
                .shared
                .controller
                .abort_reserved_conference_removal(mutation, session.id, participant_id)
                .unwrap_or_else(|_| false);
            if !aborted {
                let removed = access
                    .shared
                    .controller
                    .snapshot()
                    .conference_session_by_id(session.id)
                    .is_some_and(|conference| {
                        conference.participants.get(participant_id).is_none()
                    });
                if removed {
                    return Ok(());
                }
            }
            ast_log(
                LogLevel::Warning,
                &format!("conference participant removal failed: {error}"),
            );
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to remove participant",
            )
            .await;
            return Err(ServiceProviderError::Delivery);
        }
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
    }

    let cleanup = access
        .shared
        .controller
        .commit_reserved_conference_removal(mutation, session.id, participant_id)
        .unwrap_or_else(|_| None);
    let committed_here = cleanup.is_some();
    if let Some(cleanup) = cleanup {
        execute_effects(access, cleanup).await;
    }
    let removed = access
        .shared
        .controller
        .snapshot()
        .conference_session_by_id(session.id)
        .is_some_and(|conference| conference.participants.get(participant_id).is_none());
    if removed {
        if !committed_here {
            return Ok(());
        }
        let announcement = access
            .shared
            .controller
            .snapshot()
            .conference_announcement_effects(
                session.id,
                ConferenceAnnouncement::ParticipantRemoved(participant_id),
            );
        execute_effects(access, announcement).await;
        show_conference_list_if_configured(access, &session).await;
        Ok(())
    } else {
        Err(ServiceProviderError::ConferenceConflict)
    }
}

pub async fn set_conference_participant_muted(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    participant_id: sccp_protocol::ParticipantId,
    muted: bool,
) -> Result<(), ServiceProviderError> {
    let effects = access
        .shared
        .controller
        .prepare_conference_mute(session.device_id.clone(), session.id, participant_id, muted)
        .unwrap_or_else(|_| {
            Err(crate::runtime::controller::ConferenceParticipantRejection::Unavailable)
        });
    let (mutation, effects) = match effects {
        Ok(operation) => operation,
        Err(rejection) => {
            let provider_error = conference_participant_service_error(rejection);
            let text = match rejection {
                ConferenceParticipantRejection::Unavailable => "Conference unavailable",
                ConferenceParticipantRejection::NotModerator => "Moderator access required",
                ConferenceParticipantRejection::InvalidParticipant => "Participant unavailable",
                ConferenceParticipantRejection::Moderator => "Moderator cannot be muted",
                ConferenceParticipantRejection::LastModerator => {
                    "At least one moderator is required"
                }
                ConferenceParticipantRejection::Conflict => "Participant state changed",
            };
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                text,
            )
            .await;
            return Err(provider_error);
        }
    };

    let backend = AsteriskBackend::new(access);
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            access
                .shared
                .controller
                .abort_reserved_conference_mute(mutation, session.id, participant_id, muted)
                .unwrap_or_else(|_| ());
            ast_log(
                LogLevel::Warning,
                &format!("conference participant mute failed: {error}"),
            );
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to update participant",
            )
            .await;
            return Err(ServiceProviderError::Delivery);
        }
        if !conference_mutation_is_active(access, mutation) {
            return Err(ServiceProviderError::ConferenceConflict);
        }
    }

    let committed = access
        .shared
        .controller
        .commit_reserved_conference_mute(mutation, session.id, participant_id, muted)
        .unwrap_or_else(|_| false);
    if committed {
        let announcement = access
            .shared
            .controller
            .snapshot()
            .conference_announcement_effects(
                session.id,
                if muted {
                    ConferenceAnnouncement::ParticipantMuted(participant_id)
                } else {
                    ConferenceAnnouncement::ParticipantUnmuted(participant_id)
                },
            );
        execute_effects(access, announcement).await;
        show_conference_list_if_configured(access, &session).await;
        Ok(())
    } else {
        Err(ServiceProviderError::ConferenceConflict)
    }
}

pub async fn set_conference_participant_moderator(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    participant_id: sccp_protocol::ParticipantId,
    moderator: bool,
) -> Result<(), ServiceProviderError> {
    let effects = access
        .shared
        .controller
        .prepare_conference_role_change(
            session.device_id.clone(),
            session.id,
            participant_id,
            moderator,
        )
        .unwrap_or_else(|_| {
            Err(crate::runtime::controller::ConferenceParticipantRejection::Unavailable)
        });
    let (mutation, effects) = match effects {
        Ok(operation) => operation,
        Err(rejection) => {
            let provider_error = conference_participant_service_error(rejection);
            let text = match rejection {
                ConferenceParticipantRejection::Unavailable => "Conference unavailable",
                ConferenceParticipantRejection::NotModerator => "Moderator access required",
                ConferenceParticipantRejection::InvalidParticipant => "Participant unavailable",
                ConferenceParticipantRejection::Moderator
                | ConferenceParticipantRejection::Conflict => "Participant state changed",
                ConferenceParticipantRejection::LastModerator => {
                    "At least one moderator is required"
                }
            };
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                text,
            )
            .await;
            return Err(provider_error);
        }
    };

    let backend = AsteriskBackend::new(access);
    let mut compensation = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            compensation.reverse();
            execute_cleanup_effects(access, compensation).await;
            return Err(ServiceProviderError::ConferenceConflict);
        }
        let compensate = match &effect {
            DriverEffect::Backend(PbxEffect::Bridge {
                operation:
                    BridgeOperation::SetParticipantMusicOnHold {
                        bridge_id,
                        participant_id,
                        call_id,
                        class,
                        enabled,
                    },
            }) => Some(
                PbxEffect::Bridge {
                    operation: BridgeOperation::SetParticipantMusicOnHold {
                        bridge_id: *bridge_id,
                        participant_id: *participant_id,
                        call_id: *call_id,
                        class: class.clone(),
                        enabled: !enabled,
                    },
                }
                .into(),
            ),
            _ => None,
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            access
                .shared
                .controller
                .abort_reserved_conference_role_change(
                    mutation,
                    session.id,
                    participant_id,
                    moderator,
                )
                .unwrap_or_else(|_| ());
            compensation.reverse();
            execute_cleanup_effects(access, compensation).await;
            ast_log(
                LogLevel::Warning,
                &format!("conference participant role change failed: {error}"),
            );
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to update participant",
            )
            .await;
            return Err(ServiceProviderError::Delivery);
        }
        if let Some(compensate) = compensate {
            compensation.push(compensate);
        }
        if !conference_mutation_is_active(access, mutation) {
            compensation.reverse();
            execute_cleanup_effects(access, compensation).await;
            return Err(ServiceProviderError::ConferenceConflict);
        }
    }

    let committed = access
        .shared
        .controller
        .commit_reserved_conference_role_change(mutation, session.id, participant_id, moderator)
        .unwrap_or_else(|_| false);
    if committed {
        show_conference_list_if_configured(access, &session).await;
        Ok(())
    } else {
        compensation.reverse();
        execute_cleanup_effects(access, compensation).await;
        Err(ServiceProviderError::ConferenceConflict)
    }
}

pub(super) async fn start_conference_invite(
    access: &Access,
    device_id: DeviceId,
    moderator_call_id: CallId,
    line_instance: u32,
) {
    let current = access.shared.controller.snapshot().call(moderator_call_id);
    let Some(current) = current.filter(|call| call.device_id == device_id) else {
        return;
    };
    let selected_line = if line_instance == 0 {
        current.line_instance
    } else {
        line_instance
    };
    let config = access.config();
    let permitted = config
        .conference_for_device(&device_id)
        .is_some_and(|conference| conference.allowed);
    let binding = access.line_binding(&device_id, selected_line);
    drop(config);
    let Some(binding) = binding.filter(|_| permitted) else {
        display_conference_prompt(access, device_id, moderator_call_id, "Conference disabled")
            .await;
        return;
    };
    let Some(codec) = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    ) else {
        display_conference_prompt(
            access,
            device_id,
            moderator_call_id,
            "Conference codec unavailable",
        )
        .await;
        return;
    };
    let invite_call_id = access.phone.reserve_call_id();
    let result = access
        .shared
        .controller
        .prepare_conference_invite(
            moderator_call_id,
            invite_call_id,
            binding,
            codec,
            Instant::now(),
        )
        .unwrap_or_else(|_| Err(crate::runtime::controller::ConferenceRejection::Unavailable));
    match result {
        Ok((mutation, effects)) => {
            execute_conference_invite_start(access, invite_call_id, mutation, effects).await;
        }
        Err(rejection) => {
            let text = match rejection {
                ConferenceRejection::Disabled => "Moderator access required",
                ConferenceRejection::NotConnected => "Connect the conference first",
                ConferenceRejection::Conflict => "Conference invite unavailable",
                ConferenceRejection::Unavailable => "Conference unavailable",
            };
            display_conference_prompt(access, device_id, moderator_call_id, text).await;
        }
    }
}

pub(super) async fn handle_conference_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
) {
    let session = access
        .shared
        .controller
        .snapshot()
        .conference_session(call_id)
        .cloned();
    if let Some(session) = session {
        if session.phase == ConferencePhase::Active {
            if session
                .pending_invite
                .as_ref()
                .is_some_and(|invite| invite.participant.handset_call_id == call_id)
            {
                let effects = access
                    .shared
                    .controller
                    .prepare_confirm_conference_invite(call_id)
                    .unwrap_or_else(|_| {
                        Err(crate::runtime::controller::ConferenceRejection::Unavailable)
                    });
                match effects {
                    Ok((mutation, effects)) => {
                        execute_conference_invite_merge(access, session, mutation, effects).await
                    }
                    Err(ConferenceRejection::NotConnected) => {
                        display_conference_prompt(
                            access,
                            device_id,
                            call_id,
                            "Invite is not connected",
                        )
                        .await;
                    }
                    Err(_) => {
                        display_conference_prompt(
                            access,
                            device_id,
                            call_id,
                            "Unable to add participant",
                        )
                        .await;
                    }
                }
                return;
            }
            if session.pending_invite.is_none()
                && session.participants.iter().any(|participant| {
                    participant.moderator && participant.handset_call_id == call_id
                })
            {
                start_conference_invite(access, device_id, call_id, line_instance).await;
                return;
            }
            display_conference_prompt(access, device_id, call_id, "Conference invite pending")
                .await;
            return;
        }
        if session.phase != ConferencePhase::Consultation
            || session.consultation_handset_call_id != call_id
        {
            display_conference_prompt(access, device_id, call_id, "Conference already active")
                .await;
            return;
        }
        let effects = access
            .shared
            .controller
            .prepare_confirm_conference(call_id)
            .unwrap_or_else(|_| Err(crate::runtime::controller::ConferenceRejection::Unavailable));
        match effects {
            Ok((mutation, effects)) => {
                execute_conference_merge(access, session, mutation, effects).await
            }
            Err(ConferenceRejection::NotConnected) => {
                display_conference_prompt(
                    access,
                    device_id,
                    call_id,
                    "Consultation is not connected",
                )
                .await;
            }
            Err(_) => {
                display_conference_prompt(access, device_id, call_id, "Conference unavailable")
                    .await;
            }
        }
        return;
    }

    let current = access.shared.controller.snapshot().call(call_id);
    let Some(current) = current.filter(|call| call.device_id == device_id) else {
        return;
    };
    let selected_line = if line_instance == 0 {
        current.line_instance
    } else {
        line_instance
    };
    let config = access.config();
    let conference_policy = config.conference_for_device(&device_id).cloned();
    let permitted = conference_policy
        .as_ref()
        .is_some_and(|conference| conference.allowed);
    let media_policy = conference_policy.map(|conference| ConferenceMediaPolicy {
        music_on_hold_class: conference.music_on_hold_class,
        mute_on_entry: conference.mute_on_entry,
        play_general_announcements: conference.play_general_announcements,
        play_participant_announcements: conference.play_participant_announcements,
    });
    let binding = access.line_binding(&device_id, selected_line);
    drop(config);
    let Some(binding) = binding else {
        return;
    };
    let Some(codec) = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    ) else {
        display_conference_prompt(access, device_id, call_id, "Conference codec unavailable").await;
        return;
    };
    let consultation_call_id = access.phone.reserve_call_id();
    let result = access
        .shared
        .controller
        .prepare_conference(
            ConferenceConsultationRequest {
                original_call_id: call_id,
                consultation_call_id,
                binding,
                codec,
                now: Instant::now(),
                permitted,
            },
            media_policy.unwrap_or_default(),
        )
        .unwrap_or_else(|_| Err(crate::runtime::controller::ConferenceRejection::Unavailable));
    match result {
        Ok((mutation, effects)) => {
            execute_conference_start(access, consultation_call_id, mutation, effects).await;
        }
        Err(rejection) => {
            let text = match rejection {
                ConferenceRejection::Disabled => "Conference disabled",
                ConferenceRejection::NotConnected => "Connect the call first",
                ConferenceRejection::Conflict => "Conference already pending",
                ConferenceRejection::Unavailable => "Conference unavailable",
            };
            display_conference_prompt(access, device_id, call_id, text).await;
        }
    }
}

pub(super) async fn handle_conference_destination(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
) {
    let config = access.config();
    let policy = access
        .line_binding(&device_id, line_instance)
        .as_ref()
        .and_then(|binding| config.conference_dialing_for_binding(binding));
    let target_matches = access
        .shared
        .controller
        .snapshot()
        .call(call_id)
        .is_some_and(|call| call.device_id == device_id && call.line_instance == line_instance);
    if !target_matches {
        return;
    }
    let Some(policy) = policy.filter(|policy| policy.enabled && policy.destination.is_some())
    else {
        display_conference_prompt(
            access,
            device_id.clone(),
            call_id,
            "Conference dialing unavailable",
        )
        .await;
        let effects = access
            .shared
            .controller
            .hangup(call_id)
            .unwrap_or_else(|_| Vec::new());
        execute_cleanup_effects(access, effects).await;
        return;
    };
    let Some(destination) = policy.destination else {
        debug_assert!(false, "enabled conference policy lost its destination");
        return;
    };
    let result = access
        .shared
        .controller
        .begin_conference_destination(ConferenceDestinationRequest {
            device_id: device_id.clone(),
            handset_call_id: call_id,
            destination,
            application_options: policy.application_options,
        })
        .unwrap_or_else(|_| {
            Err(crate::runtime::controller::ConferenceDestinationRejection::Unavailable)
        });
    match result {
        Ok(effects) => {
            execute_conference_destination_start(access, device_id, call_id, effects).await
        }
        Err(_) => {
            display_conference_prompt(access, device_id, call_id, "Conference dialing unavailable")
                .await;
            let effects = access
                .shared
                .controller
                .hangup(call_id)
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, effects).await;
        }
    }
}

pub(super) async fn execute_conference_destination_start(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    effects: Vec<DriverEffect>,
) {
    let mutation = effects.iter().find_map(|effect| match effect {
        DriverEffect::Backend(PbxEffect::StartConferenceDestination { operation }) => {
            Some(operation.mutation)
        }
        _ => None,
    });
    let Some(mutation) = mutation else {
        return;
    };
    let held_calls = effects
        .iter()
        .filter_map(|effect| match effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id }) => Some(*call_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    let backend = AsteriskBackend::new(access);
    let mut completed_holds = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        if !access
            .shared
            .controller
            .snapshot()
            .conference_mutation_is_active(mutation)
        {
            return;
        }
        let held_call = match &effect {
            DriverEffect::Backend(PbxEffect::Hold { call_id }) => Some(*call_id),
            _ => None,
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference destination launch failed: {error}"),
            );
            display_conference_prompt(access, device_id, call_id, "Conference dialing failed")
                .await;
            let cleanup = access
                .shared
                .controller
                .conference_destination_failed(mutation, call_id, &held_calls, &completed_holds)
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, cleanup).await;
            return;
        }
        if let Some(held_call) = held_call {
            completed_holds.push(held_call);
        }
        if !access
            .shared
            .controller
            .snapshot()
            .conference_mutation_is_active(mutation)
        {
            return;
        }
    }
}

pub(super) async fn execute_conference_start(
    access: &Access,
    consultation_call_id: CallId,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut progress = ConferenceStartProgress::default();
    let consultation_pbx = access
        .shared
        .controller
        .snapshot()
        .conference_session(consultation_call_id)
        .map(|session| session.consultation_call_id);
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = consultation_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        let completed = ConferenceStartProgress::from(&effect);
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference consultation setup failed: {error}"),
            );
            let cleanup = access
                .shared
                .controller
                .abort_reserved_conference(
                    mutation,
                    consultation_call_id,
                    false,
                    progress.channel_created(),
                    progress.active_leg_held(),
                    progress.active_handset_held(),
                )
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, cleanup).await;
            if progress.channel_created()
                && let Some(pbx_id) = consultation_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        progress |= completed;
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = consultation_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
    }
    access
        .shared
        .controller
        .complete_conference_mutation(mutation)
        .unwrap_or_else(|_| false);
}

pub(super) async fn execute_conference_invite_start(
    access: &Access,
    invite_call_id: CallId,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut progress = ConferenceStartProgress::default();
    let invite_pbx = access
        .shared
        .controller
        .snapshot()
        .conference_session(invite_call_id)
        .and_then(|session| session.pending_invite.as_ref())
        .map(|invite| invite.participant.pbx_call_id);
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = invite_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        let completed = ConferenceStartProgress::from(&effect);
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference invite setup failed: {error}"),
            );
            let cleanup = access
                .shared
                .controller
                .abort_reserved_conference_invite(
                    mutation,
                    invite_call_id,
                    progress.channel_created(),
                    progress.active_leg_held(),
                    progress.active_handset_held(),
                )
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, cleanup).await;
            if progress.channel_created()
                && let Some(pbx_id) = invite_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
        progress |= completed;
        if !conference_mutation_is_active(access, mutation) {
            if progress.channel_created()
                && let Some(pbx_id) = invite_pbx
            {
                remove_channel(access, pbx_id);
            }
            return;
        }
    }
    access
        .shared
        .controller
        .complete_conference_mutation(mutation)
        .unwrap_or_else(|_| false);
}

const CONFERENCE_BRIDGE_READY_RETRY_INTERVAL: Duration = Duration::from_millis(20);
const CONFERENCE_BRIDGE_READY_TIMEOUT: Duration = Duration::from_secs(1);

fn is_transient_conference_bridge_readiness_error(
    error: &EffectExecutionError<AsteriskBackendError, String>,
) -> bool {
    matches!(
        error,
        EffectExecutionError::Backend {
            effect,
            error: AsteriskBackendError::CallFeature(CallFeatureError::NotFound {
                operation: "merge conference consultation",
            }),
            ..
        } if matches!(
            effect.as_ref(),
            PbxEffect::Bridge {
                operation: BridgeOperation::MergeConsultation { .. },
            }
        )
    )
}

/// Asterisk documents that a two-party bridge can temporarily have no bridge
/// (or fewer than two members) while its members finish joining. SCCP answer
/// and soft-key events are asynchronous to that transition, so wait for the
/// exact consultation-bridge lookup to become ready without retrying topology
/// conflicts or native merge failures.
async fn execute_conference_merge_effect(
    access: &Access,
    backend: &AsteriskBackend<'_>,
    index: usize,
    effect: DriverEffect,
    mutation: ConferenceMutationToken,
) -> Result<bool, EffectExecutionError<AsteriskBackendError, String>> {
    let started = Instant::now();
    let mut retries = 0_u32;
    loop {
        match execute_one_effect(access, backend, index, effect.clone()).await {
            Ok(()) => {
                if retries != 0 {
                    ast_log(
                        LogLevel::Debug,
                        &format!(
                            "conference bridge became ready after {retries} retr{}",
                            if retries == 1 { "y" } else { "ies" }
                        ),
                    );
                }
                return Ok(true);
            }
            Err(error)
                if is_transient_conference_bridge_readiness_error(&error)
                    && started.elapsed() < CONFERENCE_BRIDGE_READY_TIMEOUT =>
            {
                if !conference_mutation_is_active(access, mutation) {
                    return Ok(false);
                }
                retries += 1;
                tokio::time::sleep(CONFERENCE_BRIDGE_READY_RETRY_INTERVAL).await;
                if !conference_mutation_is_active(access, mutation) {
                    return Ok(false);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

pub(super) async fn execute_conference_merge(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut bridge_created = false;
    let mut original_resumed = false;
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, session.consultation_call_id);
            return;
        }
        let completed_create = matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: BridgeOperation::Create { .. }
            })
        );
        let completed_resume = matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. }));
        let completed =
            execute_conference_merge_effect(access, &backend, index, effect, mutation).await;
        let completed = match completed {
            Ok(completed) => completed,
            Err(error) => {
                ast_log(
                    LogLevel::Warning,
                    &format!("conference merge failed: {error}"),
                );
                let cleanup = access
                    .shared
                    .controller
                    .abort_reserved_conference(
                        mutation,
                        session.consultation_handset_call_id,
                        bridge_created,
                        true,
                        !original_resumed,
                        true,
                    )
                    .unwrap_or_else(|_| Vec::new());
                execute_cleanup_effects(access, cleanup).await;
                remove_channel(access, session.consultation_call_id);
                display_conference_prompt(
                    access,
                    session.device_id,
                    session.original_handset_call_id,
                    "Unable to create conference",
                )
                .await;
                return;
            }
        };
        if !completed {
            remove_channel(access, session.consultation_call_id);
            return;
        }
        bridge_created |= completed_create;
        original_resumed |= completed_resume;
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, session.consultation_call_id);
            return;
        }
    }
    let (committed, announcement) = access
        .shared
        .controller
        .commit_reserved_conference(mutation, session.consultation_handset_call_id, session.id)
        .unwrap_or_else(|_| (false, None));
    if !committed {
        return;
    }
    if let Some(effects) = announcement {
        execute_effects(access, effects).await;
    }
    display_conference_prompt(
        access,
        session.device_id.clone(),
        session.consultation_handset_call_id,
        "Conference connected",
    )
    .await;
    show_conference_list_if_configured(access, &session).await;
}

pub(super) async fn execute_selected_conference_merge(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let backend = AsteriskBackend::new(access);
    let mut bridge_created = false;
    let mut resumed_call_ids = Vec::new();
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            return;
        }
        let completed_create = matches!(
            effect,
            DriverEffect::Backend(PbxEffect::Bridge {
                operation: BridgeOperation::Create { .. }
            })
        );
        let resumed = match &effect {
            DriverEffect::Backend(PbxEffect::Resume { call_id }) => Some(*call_id),
            _ => None,
        };
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("selected-call conference merge failed: {error}"),
            );
            let cleanup = access
                .shared
                .controller
                .abort_reserved_join(
                    mutation,
                    session.original_handset_call_id,
                    bridge_created,
                    resumed_call_ids.clone(),
                )
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, cleanup).await;
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to join calls",
            )
            .await;
            return;
        }
        bridge_created |= completed_create;
        if let Some(call_id) = resumed {
            resumed_call_ids.push(call_id);
        }
        if !conference_mutation_is_active(access, mutation) {
            return;
        }
    }
    let (committed, announcement) = access
        .shared
        .controller
        .commit_reserved_conference(mutation, session.original_handset_call_id, session.id)
        .unwrap_or_else(|_| (false, None));
    if !committed {
        return;
    }
    if let Some(effects) = announcement {
        execute_effects(access, effects).await;
    }
    display_conference_prompt(
        access,
        session.device_id.clone(),
        session.original_handset_call_id,
        "Conference connected",
    )
    .await;
    show_conference_list_if_configured(access, &session).await;
}

pub(super) async fn execute_conference_invite_merge(
    access: &Access,
    session: crate::runtime::controller::ConferenceSession,
    mutation: ConferenceMutationToken,
    effects: Vec<DriverEffect>,
) {
    let Some(invite) = session.pending_invite.as_ref() else {
        return;
    };
    let invite_call_id = invite.participant.handset_call_id;
    let invite_pbx_id = invite.participant.pbx_call_id;
    let backend = AsteriskBackend::new(access);
    let mut moderator_resumed = false;
    for (index, effect) in effects.into_iter().enumerate() {
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, invite_pbx_id);
            return;
        }
        let completed_resume = matches!(effect, DriverEffect::Backend(PbxEffect::Resume { .. }));
        if let Err(error) = execute_one_effect(access, &backend, index, effect).await {
            ast_log(
                LogLevel::Warning,
                &format!("conference participant merge failed: {error}"),
            );
            let cleanup = access
                .shared
                .controller
                .abort_reserved_conference_invite(
                    mutation,
                    invite_call_id,
                    true,
                    !moderator_resumed,
                    true,
                )
                .unwrap_or_else(|_| Vec::new());
            execute_cleanup_effects(access, cleanup).await;
            remove_channel(access, invite_pbx_id);
            display_conference_prompt(
                access,
                session.device_id,
                session.original_handset_call_id,
                "Unable to add participant",
            )
            .await;
            return;
        }
        moderator_resumed |= completed_resume;
        if !conference_mutation_is_active(access, mutation) {
            remove_channel(access, invite_pbx_id);
            return;
        }
    }
    let (committed, announcement) = access
        .shared
        .controller
        .commit_reserved_conference_invite(
            mutation,
            invite_call_id,
            session.id,
            invite.participant.id,
        )
        .unwrap_or_else(|_| (false, None));
    if !committed {
        return;
    }
    if let Some(effects) = announcement {
        execute_effects(access, effects).await;
    }
    display_conference_prompt(
        access,
        session.device_id.clone(),
        invite_call_id,
        "Participant added",
    )
    .await;
    show_conference_list_if_configured(access, &session).await;
}

pub(super) async fn display_conference_prompt(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    text: &str,
) {
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
}
