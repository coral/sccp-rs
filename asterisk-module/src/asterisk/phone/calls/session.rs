//! Registration lifecycle and session replacement handling.

use super::super::{
    Access, DriverEffect, LogLevel, MutexExt as _, PhoneCommand, PhoneCommandAction,
    PhoneDeviceEvent, PhoneDeviceEventKind, RegistrationStatus, RuntimeRecordings, ast_log,
    cancel_conference_announcement, cancel_forwarding_entry_for_device, configured_feature_state,
    controller_step, enqueue_registered_background, execute_cleanup_effects, install_blf,
    log_feature_store_error, prune_recording_sessions, publish_ami_event, publish_device_features,
    publish_device_lines, publish_recording_button_state, registered_device_ids,
    registration_event, registration_state_or_fallback, restore_mobility_appearances,
    restore_system_message, show_conference_list, uninstall_device_blf,
};

pub(super) async fn handle_session_event(
    access: &Access,
    recordings: &mut RuntimeRecordings,
    event: PhoneDeviceEvent,
) -> Vec<DriverEffect> {
    let PhoneDeviceEvent {
        device_id,
        session_generation,
        event,
    } = event;
    match event {
        PhoneDeviceEventKind::Registered(registration) => {
            let device = registration.id.clone();
            let registered_event =
                registration_event(&device, RegistrationStatus::Registered, Some(&registration));
            let Some((session, affected_conferences, surviving_conferences)) =
                controller_step(&access.shared.controller, |controller| {
                    let mut affected = controller
                        .calls()
                        .filter(|call| call.device_id == device)
                        .filter_map(|call| {
                            controller
                                .conference_session(call.sccp_id)
                                .map(|conference| conference.id)
                        })
                        .collect::<Vec<_>>();
                    affected.sort_unstable();
                    affected.dedup();
                    let session = controller.register_session(session_generation, registration)?;
                    if !session.replaced {
                        affected.clear();
                    }
                    let surviving = affected
                        .iter()
                        .filter_map(|conference_id| {
                            controller.conference_session_by_id(*conference_id).cloned()
                        })
                        .collect::<Vec<_>>();
                    Some((session, affected, surviving))
                })
            else {
                return Vec::new();
            };
            if session.replaced {
                access
                    .shared
                    .pending_mobility_prompts
                    .lock_unpoisoned()
                    .retain(|(pending_device, _), _| pending_device != &device);
                cancel_forwarding_entry_for_device(access, &device);
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
            let feature_guard = access.shared.feature_mutations.lock_unpoisoned();
            let config = access.config();
            let defaults = configured_feature_state(&config, &device).unwrap_or_default();
            let previous = controller_step(&access.shared.controller, |controller| {
                controller.feature_state(&device).cloned()
            });
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
            controller_step(&access.shared.controller, |controller| {
                controller.set_feature_state(&device, features.clone());
            });
            let registered = registered_device_ids(&access.shared);
            let registration_result = {
                let mut contexts = access.shared.registration_contexts.lock_unpoisoned();
                contexts.suppressed_devices.remove(&device);
                contexts.reconcile(&config, &registered)
            };
            if let Err(error) = registration_result {
                access
                    .shared
                    .registration_contexts
                    .lock_unpoisoned()
                    .suppressed_devices
                    .insert(device.clone());
                ast_log(
                    LogLevel::Error,
                    &format!(
                        "unable to publish registration-context extensions for a registered device: {error}"
                    ),
                );
                let actions = controller_step(&access.shared.controller, |controller| {
                    controller.disconnected(&device)
                });
                drop(feature_guard);
                if let Err(error) = access
                    .phone
                    .send(PhoneCommand::new(
                        device,
                        PhoneCommandAction::DisconnectDevice {},
                    ))
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
                install_blf(access, &device);
                publish_device_lines(access, &device);
                publish_device_features(access, &device, &features);
                publish_recording_button_state(access, recordings, &device);
                drop(feature_guard);
                publish_ami_event(access, &registered_event);
                restore_system_message(access, &device).await;
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
        PhoneDeviceEventKind::Disconnected {} => {
            access
                .shared
                .pending_mobility_prompts
                .lock_unpoisoned()
                .retain(|(pending_device, _), _| pending_device != &device_id);
            cancel_forwarding_entry_for_device(access, &device_id);
            let feature_guard = access.shared.feature_mutations.lock_unpoisoned();
            uninstall_device_blf(access, &device_id);
            let (actions, surviving_conferences, affected_conferences) =
                controller_step(&access.shared.controller, |controller| {
                    let mut affected = controller
                        .calls()
                        .filter(|call| call.device_id == device_id)
                        .filter_map(|call| {
                            controller
                                .conference_session(call.sccp_id)
                                .map(|session| session.id)
                        })
                        .collect::<Vec<_>>();
                    affected.sort_unstable();
                    affected.dedup();
                    let actions = controller.disconnected(&device_id);
                    let surviving = affected
                        .iter()
                        .filter_map(|conference_id| {
                            controller.conference_session_by_id(*conference_id).cloned()
                        })
                        .collect::<Vec<_>>();
                    (actions, surviving, affected)
                });
            let registered = registered_device_ids(&access.shared);
            let registration_result = {
                let mut contexts = access.shared.registration_contexts.lock_unpoisoned();
                contexts.suppressed_devices.insert(device_id.clone());
                contexts.reconcile(&access.config(), &registered)
            };
            if let Err(error) = registration_result {
                ast_log(
                    LogLevel::Error,
                    &format!(
                        "unable to remove registration-context extensions for a disconnected device: {error}"
                    ),
                );
            }
            publish_device_lines(access, &device_id);
            drop(feature_guard);
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
            publish_ami_event(
                access,
                &registration_event(&device_id, RegistrationStatus::Disconnected, None),
            );
            Vec::new()
        }
        PhoneDeviceEventKind::Capabilities { capabilities } => {
            controller_step(&access.shared.controller, |controller| {
                controller.update_capabilities(&device_id, session_generation, capabilities)
            });
            Vec::new()
        }
        _ => unreachable!("session event was classified before dispatch"),
    }
}
