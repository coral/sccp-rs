//! Registration lifecycle and session replacement handling.

use std::sync::Arc;

use super::super::{
    Access, DriverEffect, LogLevel, PhoneDeviceEvent, PhoneDeviceEventKind, RegistrationStatus,
    RuntimeRecordings, ast_log, cancel_conference_announcement, configured_feature_state,
    enqueue_registered_background, execute_cleanup_effects, install_blf, log_feature_store_error,
    prune_recording_sessions, publish_ami_event, publish_device_features, publish_device_lines,
    publish_recording_button_state, registered_device_ids, registration_event,
    registration_state_or_fallback, restore_mobility_appearances, restore_system_message,
    show_conference_list,
};

use crate::asterisk::runtime::{
    publish_registration_contexts, retire_registration_contexts, uninstall_device_blf_for_session,
};

pub struct PreparedSessionEvent {
    pub device_id: sccp_protocol::DeviceId,
    pub session_generation: sccp_protocol::SessionGeneration,
    preparation: SessionPreparation,
}

enum SessionPreparation {
    RejectedRegistration,
    Registered {
        registration: sccp_protocol::DeviceRegistration,
        session: crate::runtime::controller::RegisterSessionOutcome,
        affected_conferences: Vec<sccp_protocol::ConferenceId>,
        surviving_conferences: Vec<crate::runtime::controller::ConferenceSession>,
    },
    Disconnected {
        actions: Vec<DriverEffect>,
        surviving_conferences: Vec<crate::runtime::controller::ConferenceSession>,
        affected_conferences: Vec<sccp_protocol::ConferenceId>,
    },
}

impl PreparedSessionEvent {
    pub fn is_disconnected(&self) -> bool {
        matches!(self.preparation, SessionPreparation::Disconnected { .. })
    }
}

pub async fn prepare_session_event(
    access: &Access,
    event: &PhoneDeviceEvent,
) -> Option<PreparedSessionEvent> {
    let preparation = match &event.event {
        PhoneDeviceEventKind::Registered(registration) => {
            match access
                .shared
                .controller
                .prepare_register_session_async(event.session_generation, registration.clone())
                .await
            {
                Ok(Some((session, affected_conferences, surviving_conferences))) => {
                    SessionPreparation::Registered {
                        registration: registration.clone(),
                        session,
                        affected_conferences,
                        surviving_conferences,
                    }
                }
                Ok(None) => return None,
                Err(_) => SessionPreparation::RejectedRegistration,
            }
        }
        PhoneDeviceEventKind::Disconnected {} => {
            let (actions, surviving_conferences, affected_conferences) = access
                .shared
                .controller
                .prepare_disconnect_async(event.device_id.clone(), event.session_generation)
                .await
                .ok()
                .flatten()?;
            SessionPreparation::Disconnected {
                actions,
                surviving_conferences,
                affected_conferences,
            }
        }
        _ => return None,
    };
    Some(PreparedSessionEvent {
        device_id: event.device_id.clone(),
        session_generation: event.session_generation,
        preparation,
    })
}

pub async fn handle_prepared_session_event(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    system_message: &mut Option<crate::asterisk::runtime::ActiveSystemMessage>,
    event: PreparedSessionEvent,
) {
    let actions = execute_prepared_session_event(access, recordings, system_message, event).await;
    super::execute_effects(access, actions).await;
}

pub(super) async fn handle_session_event(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    system_message: &mut Option<crate::asterisk::runtime::ActiveSystemMessage>,
    event: PhoneDeviceEvent,
) -> Vec<DriverEffect> {
    if let PhoneDeviceEventKind::Capabilities { capabilities } = event.event {
        let _ = access.shared.controller.update_capabilities(
            &event.device_id,
            event.session_generation,
            capabilities,
        );
        return Vec::new();
    }
    let Some(prepared) = prepare_session_event(access, &event).await else {
        return Vec::new();
    };
    execute_prepared_session_event(access, recordings, system_message, prepared).await
}

async fn execute_prepared_session_event(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    system_message: &mut Option<crate::asterisk::runtime::ActiveSystemMessage>,
    event: PreparedSessionEvent,
) -> Vec<DriverEffect> {
    let PreparedSessionEvent {
        device_id,
        session_generation,
        preparation,
    } = event;
    match preparation {
        SessionPreparation::RejectedRegistration => {
            let _ = access
                .phone
                .disconnect_session(device_id, session_generation)
                .await;
            Vec::new()
        }

        SessionPreparation::Registered {
            registration,
            session,
            affected_conferences,
            surviving_conferences,
        } => {
            let device = registration.id.clone();
            let registered_event =
                registration_event(&device, RegistrationStatus::Registered, Some(&registration));
            if session.replaced {
                for conference_id in affected_conferences {
                    cancel_conference_announcement(access, conference_id);
                }
            }
            execute_cleanup_effects(access, session.cleanup).await;
            prune_recording_sessions(access, recordings).await;
            for conference in surviving_conferences {
                if access
                    .config()
                    .conference_for_device(&conference.device_id)
                    .is_some_and(|config| config.show_conference_list)
                {
                    show_conference_list(
                        access,
                        conference.device_id,
                        conference.original_handset_call_id,
                    )
                    .await;
                }
            }
            let Ok(feature_guard) = access
                .shared
                .configuration_transactions
                .begin_async(
                    crate::runtime::configuration_transaction::ConfigurationOperation::Registration,
                    std::time::Instant::now() + std::time::Duration::from_secs(3),
                )
                .await
            else {
                let (cleanup, _, _) = access
                    .shared
                    .controller
                    .prepare_disconnect_async(device.clone(), session_generation)
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or_default();
                retire_registration_contexts(access);
                execute_cleanup_effects(access, cleanup).await;
                let _ = access
                    .phone
                    .disconnect_session(device, session_generation)
                    .await;
                return Vec::new();
            };
            if !access
                .shared
                .controller
                .snapshot()
                .session_is_current(&device, session_generation)
            {
                return Vec::new();
            }
            let config = access.config();
            let defaults = configured_feature_state(&config, &device).unwrap_or_default();
            let previous = access
                .shared
                .controller
                .snapshot()
                .feature_state(&device)
                .cloned();
            let (features, restore_error) = registration_state_or_fallback(
                access
                    .shared
                    .feature_store
                    .load_configured_device(&config, &device),
                previous,
                defaults,
            );
            if let Some(error) = restore_error {
                log_feature_store_error(
                    "restore feature state during registration",
                    Some(&device),
                    &error,
                );
            }
            if !access
                .shared
                .controller
                .commit_registered_features(&device, session_generation, features.clone())
                .unwrap_or(false)
            {
                return Vec::new();
            }
            let registered = registered_device_ids(&access.shared);
            let registration_result = publish_registration_contexts(
                access,
                Arc::clone(&config),
                registered,
                device.clone(),
                &feature_guard,
            );
            if let Err(error) = registration_result {
                ast_log(
                    LogLevel::Error,
                    &format!(
                        "unable to publish registration-context extensions for a registered device: {error}"
                    ),
                );
                let actions = access
                    .shared
                    .controller
                    .prepare_disconnect_async(device.clone(), session_generation)
                    .await
                    .ok()
                    .flatten()
                    .map(|(actions, _, _)| actions)
                    .unwrap_or_default();
                retire_registration_contexts(access);
                drop(feature_guard);
                if let Err(error) = access
                    .phone
                    .disconnect_session(device, session_generation)
                    .await
                {
                    ast_log(
                        LogLevel::Error,
                        &format!(
                            "unable to disconnect a device after registration-context publication failed: {error}"
                        ),
                    );
                }
                execute_cleanup_effects(access, actions).await;
                prune_recording_sessions(access, recordings).await;
                Vec::new()
            } else {
                if !access
                    .shared
                    .controller
                    .snapshot()
                    .session_is_current(&device, session_generation)
                {
                    return Vec::new();
                }
                install_blf(access, &device);
                publish_device_lines(access, &device);
                publish_device_features(access, &device, &features);
                publish_recording_button_state(access, recordings, &device);
                drop(feature_guard);
                publish_ami_event(access, &registered_event);
                restore_system_message(access, system_message, &device).await;
                restore_mobility_appearances(access, &device).await;
                if let Err(error) = enqueue_registered_background(access, &device).await {
                    ast_log(
                        LogLevel::Warning,
                        &format!("unable to apply the registered device background: {error}"),
                    );
                }
                Vec::new()
            }
        }
        SessionPreparation::Disconnected {
            actions,
            surviving_conferences,
            affected_conferences,
        } => {
            uninstall_device_blf_for_session(access, &device_id, session_generation);
            retire_registration_contexts(access);
            publish_device_lines(access, &device_id);
            for conference_id in affected_conferences {
                cancel_conference_announcement(access, conference_id);
            }
            execute_cleanup_effects(access, actions).await;
            for session in surviving_conferences {
                let show_list = access
                    .config()
                    .conference_for_device(&session.device_id)
                    .is_some_and(|conference| conference.show_conference_list);
                if show_list {
                    show_conference_list(
                        access,
                        session.device_id,
                        session.original_handset_call_id,
                    )
                    .await;
                }
            }
            if !access
                .shared
                .controller
                .snapshot()
                .is_registered(&device_id)
            {
                publish_ami_event(
                    access,
                    &registration_event(&device_id, RegistrationStatus::Disconnected, None),
                );
            }
            Vec::new()
        }
    }
}
