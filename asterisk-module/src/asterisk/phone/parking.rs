//! Handset pickup, parking, and retrieval interactions.

use super::{
    Access, CallDirection, CallId, CallInfo, CallState, DeviceId, Instant, LineInstance,
    PARKING_CONFIRM_TIMEOUT, PARKING_MENU_MAX_ITEMS, PARKING_NOTIFICATION_TIME, ParkedCall,
    ParkingEvent, ParkingMenuEntry, ParkingRejection, ParkingRetrievalBehavior, PbxAudioFormat,
    PhoneCommand, PhoneCommandAction, PickupRejection, ServiceProviderError, TransactionId,
    execute_cleanup_effects, execute_effects, execute_service_effects,
    handset_call_id_from_channel, parking_service_error, preferred_codec, send_confirmed_service,
};

pub(super) async fn handle_pickup_soft_key(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
    directed: bool,
) {
    let config = access.config();
    let binding = access.line_binding(&device_id, line_instance);
    let pickup = binding
        .as_ref()
        .and_then(|binding| config.features_for_line(&binding.line.number))
        .map(|features| features.pickup.clone());
    drop(config);
    let (Some(binding), Some(pickup)) = (binding, pickup) else {
        reject_pickup(access, device_id, call_id, PickupRejection::Unavailable).await;
        return;
    };
    let permitted = !pickup.pickup_groups.is_empty() || !pickup.named_pickup_groups.is_empty();
    if directed {
        let context = pickup
            .directed_context
            .unwrap_or_else(|| binding.line.context.clone());
        let result = access
            .shared
            .controller
            .begin_directed_pickup(
                call_id,
                permitted,
                pickup.directed,
                context,
                pickup.answer_directed,
            )
            .unwrap_or_else(|_| Err(crate::runtime::controller::PickupRejection::Unavailable));
        match result {
            Ok(()) => {
                let _ = access
                    .phone
                    .send(PhoneCommand::new(
                        device_id,
                        PhoneCommandAction::DisplayPrompt {
                            call_id,
                            timeout_seconds: 0,
                            text: "Enter pickup extension".into(),
                        },
                    ))
                    .await;
            }
            Err(rejection) => reject_pickup(access, device_id, call_id, rejection).await,
        }
    } else {
        let result = access
            .shared
            .controller
            .group_pickup(call_id, permitted, pickup.answer_directed)
            .unwrap_or_else(|_| Err(crate::runtime::controller::PickupRejection::Unavailable));
        match result {
            Ok(effects) => execute_effects(access, effects).await,
            Err(rejection) => reject_pickup(access, device_id, call_id, rejection).await,
        }
    }
}

pub(super) async fn reject_pickup(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    rejection: PickupRejection,
) {
    let text = match rejection {
        PickupRejection::Permission => "Pickup not permitted",
        PickupRejection::Disabled => "Directed pickup disabled",
        PickupRejection::Conflict => "Another pickup attempt won",
        PickupRejection::Unavailable => "Pickup unavailable",
    };
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: 4,
                text: text.into(),
            },
        ))
        .await;
    let collecting = access
        .shared
        .controller
        .snapshot()
        .call(call_id)
        .is_some_and(|call| call.state == CallState::Collecting);
    if collecting {
        let cleanup = access
            .shared
            .controller
            .hangup(call_id)
            .unwrap_or_else(|_| Vec::new());
        execute_effects(access, cleanup).await;
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
    }
}

pub(super) async fn handle_park_request(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    line_instance: u32,
    button_lot: Option<String>,
) {
    let config = access.config();
    let enabled = config
        .parking_for_device(&device_id)
        .is_some_and(|parking| parking.enabled);
    let binding = access.line_binding(&device_id, line_instance);
    let line_lot = binding.as_ref().and_then(|binding| {
        config
            .parking_for_line(&binding.line.number)
            .and_then(|parking| parking.lot.clone())
    });
    let lot = button_lot.or(line_lot);
    drop(config);
    if binding.is_none() {
        reject_parking(access, device_id, call_id, ParkingRejection::Unavailable).await;
        return;
    }
    let result = access
        .shared
        .controller
        .prepare_parking(
            call_id,
            enabled,
            lot.clone(),
            Instant::now() + PARKING_CONFIRM_TIMEOUT,
        )
        .unwrap_or_else(|_| {
            (
                None,
                Err(crate::runtime::controller::ParkingRejection::Unavailable),
            )
        });
    let (Some(_pbx_id), Ok(effects)) = result else {
        reject_parking(
            access,
            device_id,
            call_id,
            result.1.err().unwrap_or(ParkingRejection::Unavailable),
        )
        .await;
        return;
    };
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::DisplayPrompt {
                call_id,
                timeout_seconds: PARKING_CONFIRM_TIMEOUT.as_secs() as u32,
                text: "Parking call".into(),
            },
        ))
        .await;
    execute_effects(access, effects).await;
}

pub(super) async fn reject_parking(
    access: &Access,
    device_id: DeviceId,
    call_id: CallId,
    rejection: ParkingRejection,
) {
    let text = match rejection {
        ParkingRejection::Disabled => "Parking disabled",
        ParkingRejection::Conflict => "Call cannot be parked",
        ParkingRejection::Unavailable => "Parking unavailable",
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

pub(super) async fn handle_parking_lot_button(
    access: &Access,
    device_id: DeviceId,
    instance: u32,
    call_id: Option<CallId>,
    line_instance: u32,
) {
    let button = access
        .config()
        .parking_lot_for_button(&device_id, instance)
        .cloned();
    let Some(button) = button else {
        return;
    };
    let connected_call = call_id.filter(|call_id| {
        access
            .shared
            .controller
            .snapshot()
            .call(*call_id)
            .is_some_and(|call| call.state == CallState::Connected)
    });
    if let Some(call_id) = connected_call {
        handle_park_request(access, device_id, call_id, line_instance, Some(button.lot)).await;
        return;
    }

    let parked = access
        .shared
        .controller
        .snapshot()
        .parking()
        .calls_in_lot(&button.lot);
    if parked.len() == 1 && button.retrieval == ParkingRetrievalBehavior::RetrieveSingle {
        let _ =
            begin_parking_retrieval(access, device_id, line_instance, button.lot, parked[0].slot)
                .await;
    } else {
        show_parking_menu(access, device_id, instance, &button.lot, &parked).await;
    }
}

pub(super) async fn show_parking_menu(
    access: &Access,
    device_id: DeviceId,
    instance: u32,
    lot: &str,
    calls: &[ParkedCall],
) {
    let calls = parking_menu_entries(calls);
    let _ = access
        .phone
        .send(PhoneCommand::new(
            device_id,
            PhoneCommandAction::ShowParkingMenu {
                instance: LineInstance::new(instance),
                transaction_id: TransactionId::new(instance),
                lot: lot.to_owned(),
                calls,
            },
        ))
        .await;
}

fn parking_menu_entries(calls: &[ParkedCall]) -> Vec<ParkingMenuEntry> {
    calls
        .iter()
        .take(PARKING_MENU_MAX_ITEMS)
        .map(|call| ParkingMenuEntry {
            slot: call.slot,
            caller_name: call.caller_name.clone(),
            caller_number: call.caller_number.clone(),
            connected_name: call.connected_name.clone(),
            connected_number: call.connected_number.clone(),
        })
        .collect()
}

fn parking_retrieval_call_info(call: ParkedCall) -> CallInfo {
    CallInfo {
        direction: CallDirection::Inbound,
        calling_name: call.caller_name,
        calling_number: call.caller_number,
        called_name: if call.connected_name.is_empty() {
            format!("Parked call {}", call.slot)
        } else {
            call.connected_name
        },
        called_number: call.slot.to_string(),
        ..CallInfo::default()
    }
}

pub async fn begin_parking_retrieval(
    access: &Access,
    device_id: DeviceId,
    requested_line_instance: u32,
    lot: String,
    slot: u32,
) -> Result<CallId, ServiceProviderError> {
    let config = access.config();
    let binding = if requested_line_instance == 0 {
        config.appearances_for_device(&device_id).next().cloned()
    } else {
        access.line_binding(&device_id, requested_line_instance)
    };
    let Some(binding) = binding else {
        return Err(ServiceProviderError::CallState);
    };
    let Some(codec) = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    ) else {
        return Err(ServiceProviderError::CallState);
    };
    let call = access
        .shared
        .controller
        .snapshot()
        .parking()
        .call(&lot, slot)
        .cloned();
    let Some(call) = call else {
        publish_parking_lot(access, &lot);
        return Err(ServiceProviderError::ParkingNotFound);
    };
    let call_id = access.phone.reserve_call_id();
    let claimed = access
        .shared
        .controller
        .claim_parking(
            lot.clone(),
            slot,
            device_id.clone(),
            call_id,
            Instant::now() + PARKING_CONFIRM_TIMEOUT,
        )
        .unwrap_or(false);
    if !claimed {
        return Err(ServiceProviderError::ParkingConflict);
    }
    if send_confirmed_service(
        access,
        PhoneCommand::new(
            device_id.clone(),
            PhoneCommandAction::BeginCall {
                line_instance: LineInstance::new(binding.line_instance),
                call_id,
                codec,
            },
        ),
    )
    .await
    .is_err()
    {
        let _ = access
            .shared
            .controller
            .release_parking_claim(lot.clone(), slot, call_id);
        return Err(ServiceProviderError::Delivery);
    }
    let info = parking_retrieval_call_info(call);
    let result = access
        .shared
        .controller
        .prepare_parking_retrieval(
            call_id,
            binding,
            codec,
            lot.clone(),
            slot,
            info,
            Instant::now() + PARKING_CONFIRM_TIMEOUT,
        )
        .unwrap_or_else(|_| {
            (
                None,
                Err(crate::runtime::controller::ParkingRejection::Unavailable),
            )
        });
    let (Some(_pbx_id), effects) = result else {
        let _ = access
            .shared
            .controller
            .release_parking_claim(lot.clone(), slot, call_id);
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
        return Err(ServiceProviderError::ParkingConflict);
    };
    let effects = match effects {
        Ok(effects) => effects,
        Err(error) => {
            let _ = access
                .shared
                .controller
                .release_parking_claim(lot.clone(), slot, call_id);
            let _ = access
                .phone
                .send(PhoneCommand::new(
                    device_id,
                    PhoneCommandAction::CloseCall { call_id },
                ))
                .await;
            return Err(parking_service_error(error));
        }
    };
    execute_service_effects(access, effects).await?;
    Ok(call_id)
}

pub async fn handle_parking_event(access: &Access, event: ParkingEvent) {
    let lot = event.lot.clone();
    let retriever = handset_call_id_from_channel(&event.retriever_channel);
    let update = access
        .shared
        .controller
        .apply_parking_event(event, retriever, Instant::now() + PARKING_NOTIFICATION_TIME)
        .unwrap_or_default();
    execute_parking_update(access, update).await;
    publish_parking_lot(access, &lot);
}

pub async fn execute_parking_update(
    access: &Access,
    update: crate::runtime::controller::parking::ParkingUpdate,
) {
    for peer in update.retired_peers {
        access.shared.parking_events.retire(&peer);
    }
    execute_cleanup_effects(access, update.effects).await;
    for (device_id, call_id, timeout_seconds, text) in update.prompts {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::DisplayPrompt {
                    call_id,
                    timeout_seconds,
                    text: text.into(),
                },
            ))
            .await;
    }
    for (device_id, call_id) in update.close {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
    }
}

pub(super) fn publish_parking_lot(access: &Access, lot: &str) {
    let enabled = access
        .shared
        .controller
        .snapshot()
        .parking()
        .lot_has_calls(lot);
    let config = access.config();
    for (device_id, device) in &config.devices {
        for (&instance, button) in &device.parking.feature_buttons {
            if button.lot == lot {
                access.spawn_phone(PhoneCommand::new(
                    device_id.clone(),
                    PhoneCommandAction::SetFeatureStatus {
                        instance: LineInstance::new(instance),
                        enabled,
                    },
                ));
            }
        }
    }
}
