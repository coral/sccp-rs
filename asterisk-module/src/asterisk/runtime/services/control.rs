//! Runtime control delivery and call-control services.

use tokio::task::JoinSet;

use super::{
    Access, ActiveSystemMessage, CallId, CallState, ControlOperation, ControlOutcome,
    ControlProviderError, DeviceId, Instant, LineInstance, LogLevel,
    MANAGER_CONTROL_DELIVERY_TIMEOUT, MessageTarget, PbxAudioFormat, PhoneCommand,
    PhoneCommandAction, ResetMode, ResetTarget, ResetType, ast_log, cancel_no_answer_timer,
    execute_call_transition_result, execute_control_cleanup, execute_control_effects,
    native_uniqueid_in_use, preferred_codec, registered_device_ids,
};

const MAX_RESET_DELIVERY_CONCURRENCY: usize = 32;

pub async fn handle_control_operation(
    access: &Access,
    operation: ControlOperation,
) -> Result<ControlOutcome, ControlProviderError> {
    match operation {
        ControlOperation::Message {
            target,
            text,
            beep,
            timeout_seconds,
        } => {
            let devices = match &target {
                MessageTarget::Device(device_id) => {
                    if !access.config().devices.contains_key(device_id) {
                        return Err(ControlProviderError::DeviceNotFound);
                    }
                    let registered = access.shared.controller.snapshot().is_registered(device_id);
                    if !registered {
                        return Err(ControlProviderError::DeviceNotRegistered);
                    }
                    vec![device_id.clone()]
                }
                MessageTarget::RegisteredDevices | MessageTarget::System => {
                    let mut devices = registered_device_ids(&access.shared);
                    devices.sort();
                    devices
                }
            };
            let persistent = target == MessageTarget::System;
            let attempted = devices.len();
            let mut delivered = 0;
            let deliveries = async {
                for device_id in devices {
                    if deliver_status_message(
                        access,
                        device_id,
                        text.clone(),
                        beep,
                        timeout_seconds,
                    )
                    .await
                    .is_ok()
                    {
                        delivered += 1;
                    }
                }
            };
            let _ = tokio::time::timeout(MANAGER_CONTROL_DELIVERY_TIMEOUT, deliveries).await;
            if matches!(target, MessageTarget::Device(_)) && delivered == 0 {
                return Err(ControlProviderError::HandsetDelivery);
            }
            Ok(ControlOutcome::Message {
                target,
                attempted,
                delivered,
                persistent,
            })
        }
        ControlOperation::Reset { target, mode } => {
            let reset_type = match mode {
                ResetMode::Reset => ResetType::Reset,
                ResetMode::Restart => ResetType::Restart,
                ResetMode::ApplyConfiguration => ResetType::ApplyConfiguration,
            };
            let (attempted, delivered) = match &target {
                ResetTarget::Device(device_id) => {
                    if !access.config().devices.contains_key(device_id) {
                        return Err(ControlProviderError::DeviceNotFound);
                    }
                    let registered = access.shared.controller.snapshot().is_registered(device_id);
                    if !registered {
                        return Err(ControlProviderError::DeviceNotRegistered);
                    }
                    deliver_reset(access, device_id.clone(), reset_type).await?;
                    (1, 1)
                }
                ResetTarget::RegisteredDevices => {
                    let mut devices = registered_device_ids(&access.shared);
                    devices.sort();
                    let attempted = devices.len();
                    let delivered = deliver_registered_resets(access, devices, reset_type).await;
                    (attempted, delivered)
                }
            };
            Ok(ControlOutcome::Reset {
                target,
                mode,
                attempted,
                delivered,
            })
        }
        ControlOperation::Answer { call_id, device_id } => {
            answer_control_call(access, call_id, device_id).await
        }
        ControlOperation::End { call_id } => end_control_call(access, call_id).await,
        ControlOperation::Originate {
            device_id,
            line,
            destination,
            assigned_channel_id,
        } => {
            originate_control_call(access, device_id, line, destination, assigned_channel_id).await
        }
    }
}

async fn deliver_reset(
    access: &Access,
    device_id: DeviceId,
    reset_type: ResetType,
) -> Result<(), ControlProviderError> {
    send_confirmed_control(
        access,
        PhoneCommand::new(device_id, PhoneCommandAction::ResetDevice { reset_type }),
    )
    .await
}

async fn deliver_registered_resets(
    access: &Access,
    devices: Vec<DeviceId>,
    reset_type: ResetType,
) -> usize {
    let mut devices = devices.into_iter();
    let mut pending = JoinSet::new();
    for device_id in devices.by_ref().take(MAX_RESET_DELIVERY_CONCURRENCY) {
        spawn_reset_delivery(&mut pending, access.clone(), device_id, reset_type);
    }
    let mut delivered = 0;
    let deliveries = async {
        while let Some(result) = pending.join_next().await {
            if matches!(result, Ok(Ok(()))) {
                delivered += 1;
            }
            if let Some(device_id) = devices.next() {
                spawn_reset_delivery(&mut pending, access.clone(), device_id, reset_type);
            }
        }
    };
    let _ = tokio::time::timeout(MANAGER_CONTROL_DELIVERY_TIMEOUT, deliveries).await;
    delivered
}

fn spawn_reset_delivery(
    pending: &mut JoinSet<Result<(), ControlProviderError>>,
    access: Access,
    device_id: DeviceId,
    reset_type: ResetType,
) {
    pending.spawn(async move { deliver_reset(&access, device_id, reset_type).await });
}

pub async fn deliver_status_message(
    access: &Access,
    device_id: DeviceId,
    text: String,
    beep: bool,
    timeout_seconds: u8,
) -> Result<(), ControlProviderError> {
    send_confirmed_control(
        access,
        PhoneCommand::new(
            device_id,
            PhoneCommandAction::SetStatusMessage {
                message: sccp_protocol::HandsetStatusMessage::Display {
                    text,
                    timeout_seconds,
                    priority: None,
                },
                beep,
            },
        ),
    )
    .await
}

pub async fn send_confirmed_control(
    access: &Access,
    command: PhoneCommand,
) -> Result<(), ControlProviderError> {
    tokio::time::timeout(
        MANAGER_CONTROL_DELIVERY_TIMEOUT,
        access.phone.send_confirmed(command),
    )
    .await
    .map_err(|_| ControlProviderError::HandsetDelivery)?
    .map_err(|_| ControlProviderError::HandsetDelivery)
}

pub async fn restore_system_message(
    access: &Access,
    active: &mut Option<ActiveSystemMessage>,
    device_id: &DeviceId,
) {
    let now = Instant::now();
    let message = {
        let remaining = active.as_ref().and_then(|message| {
            message
                .expires_at
                .map(|expiry| expiry.saturating_duration_since(now))
        });
        if remaining.is_some_and(|remaining| remaining.is_zero()) {
            *active = None;
            None
        } else {
            active.clone().map(|message| {
                let timeout_seconds = remaining.map_or(0, |remaining| {
                    remaining.as_secs().clamp(1, u64::from(u8::MAX)) as u8
                });
                (message.text, message.beep, timeout_seconds)
            })
        }
    };
    let Some((text, beep, timeout_seconds)) = message else {
        return;
    };
    if deliver_status_message(access, device_id.clone(), text, beep, timeout_seconds)
        .await
        .is_err()
    {
        ast_log(
            LogLevel::Warning,
            "unable to restore the active system message on a registered device",
        );
    }
}

pub async fn answer_control_call(
    access: &Access,
    call_id: CallId,
    requested_device: Option<DeviceId>,
) -> Result<ControlOutcome, ControlProviderError> {
    let call = access
        .shared
        .controller
        .snapshot()
        .call(call_id)
        .ok_or(ControlProviderError::CallNotFound)?;
    if requested_device
        .as_ref()
        .is_some_and(|device| device != &call.device_id)
    {
        return Err(ControlProviderError::CallOwnership);
    }
    if call.state != CallState::Ringing {
        return Err(ControlProviderError::CallNotRinging);
    }
    let transition = access
        .shared
        .controller
        .begin_active_call_switch_transaction(&call.device_id, call_id)
        .unwrap_or_else(|_| Err(crate::runtime::controller::CallSwitchRejection::Unavailable))
        .map_err(|_| ControlProviderError::CallNotRinging)?;
    if !execute_call_transition_result(access, transition).await? {
        return Err(ControlProviderError::CallNotRinging);
    }
    cancel_no_answer_timer(access, call.pbx_id);
    Ok(ControlOutcome::Answer {
        device_id: call.device_id,
        call_id,
    })
}

pub async fn end_control_call(
    access: &Access,
    call_id: CallId,
) -> Result<ControlOutcome, ControlProviderError> {
    let call = access
        .shared
        .controller
        .snapshot()
        .call(call_id)
        .ok_or(ControlProviderError::CallNotFound)?;
    let effects = access
        .shared
        .controller
        .hangup(call_id)
        .unwrap_or_else(|_| Vec::new());
    if effects.is_empty() {
        return Err(ControlProviderError::CallNotFound);
    }
    execute_control_cleanup(access, effects).await?;
    Ok(ControlOutcome::End {
        device_id: call.device_id,
        call_id,
    })
}

pub async fn originate_control_call(
    access: &Access,
    device_id: DeviceId,
    requested_line: Option<String>,
    destination: String,
    assigned_channel_id: Option<String>,
) -> Result<ControlOutcome, ControlProviderError> {
    let config = access.config();
    if !config.devices.contains_key(&device_id) {
        return Err(ControlProviderError::DeviceNotFound);
    }
    let selected_line = access
        .shared
        .controller
        .snapshot()
        .registered_device(&device_id)
        .map(|registered| registered.selected_line)
        .ok_or(ControlProviderError::DeviceNotRegistered)?;
    let mut bindings = config
        .appearances_for_device(&device_id)
        .cloned()
        .collect::<Vec<_>>();
    bindings.sort_by_key(|binding| binding.line_instance);
    let binding = if let Some(line) = requested_line.as_deref() {
        bindings
            .into_iter()
            .find(|binding| binding.line.number == line)
    } else if let Some(instance) = selected_line {
        bindings
            .iter()
            .find(|binding| binding.line_instance == instance)
            .cloned()
            .or_else(|| bindings.into_iter().next())
    } else {
        bindings.into_iter().next()
    }
    .ok_or(ControlProviderError::LineNotFound)?;
    drop(config);
    let codec = preferred_codec(
        access,
        &device_id,
        binding.line_instance,
        &PbxAudioFormat::ALL,
    )
    .ok_or(ControlProviderError::NoCompatibleCodec)?;
    if let Some(uniqueid) = assigned_channel_id.as_ref() {
        let in_use = native_uniqueid_in_use(uniqueid).map_err(|error| {
            ast_log(
                LogLevel::Warning,
                &format!("assigned channel identity contains invalid native text: {error}"),
            );
            ControlProviderError::Backend
        })?;
        if in_use {
            return Err(ControlProviderError::AssignedChannelIdConflict);
        }
    }
    let call_id = access.phone.reserve_call_id();
    send_confirmed_control(
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
    .await?;
    let (pbx_id, mut effects) = access
        .shared
        .controller
        .prepare_phone_call(call_id, binding.clone(), codec, Instant::now())
        .unwrap_or_else(|_| (None, Vec::new()));
    let Some(pbx_id) = pbx_id else {
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
        return Err(ControlProviderError::Backend);
    };
    if let Some(uniqueid) = &assigned_channel_id {
        access
            .shared
            .controller
            .set_assigned_channel_id(pbx_id, Some(uniqueid.clone()))
            .map_err(|_| ControlProviderError::Unavailable)?;
    }
    effects.extend(
        access
            .shared
            .controller
            .enbloc(call_id, destination)
            .unwrap_or_else(|_| Vec::new()),
    );
    let result = execute_control_effects(access, effects).await;
    access
        .shared
        .controller
        .set_assigned_channel_id(pbx_id, None)
        .map_err(|_| ControlProviderError::Unavailable)?;
    if let Err(error) = result {
        let conflict = assigned_channel_id
            .as_ref()
            .and_then(|uniqueid| native_uniqueid_in_use(uniqueid).ok())
            .unwrap_or(false);
        let cleanup = access
            .shared
            .controller
            .hangup(call_id)
            .unwrap_or_else(|_| Vec::new());
        let _ = execute_control_cleanup(access, cleanup).await;
        let _ = access
            .phone
            .send(PhoneCommand::new(
                device_id,
                PhoneCommandAction::CloseCall { call_id },
            ))
            .await;
        return Err(if conflict {
            ControlProviderError::AssignedChannelIdConflict
        } else {
            error
        });
    }
    Ok(ControlOutcome::Originate {
        device_id,
        line: binding.line.number,
        call_id,
    })
}
