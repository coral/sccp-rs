//! Parking service operations.

use super::{
    Access, AmiParkingCommand, CallId, DeviceId, Instant, PARKING_CONFIRM_TIMEOUT,
    ParkingRejection, ServiceOutcome, ServiceProviderError, begin_parking_retrieval,
    execute_service_effects,
};

pub fn parking_service_error(error: ParkingRejection) -> ServiceProviderError {
    match error {
        ParkingRejection::Disabled => ServiceProviderError::ParkingDisabled,
        ParkingRejection::Conflict => ServiceProviderError::ParkingConflict,
        ParkingRejection::Unavailable => ServiceProviderError::CallNotFound,
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn parking_service_operation(
    access: &Access,
    command: AmiParkingCommand,
    device_id: DeviceId,
    call_id: Option<CallId>,
    line_instance: Option<u32>,
    requested_lot: Option<String>,
    slot: Option<u32>,
) -> Result<ServiceOutcome, ServiceProviderError> {
    if !access.config().devices.contains_key(&device_id) {
        return Err(ServiceProviderError::DeviceNotFound);
    }
    if !access
        .shared
        .controller
        .snapshot()
        .is_registered(&device_id)
    {
        return Err(ServiceProviderError::DeviceNotRegistered);
    }
    match command {
        AmiParkingCommand::Park => {
            let call_id = call_id.ok_or(ServiceProviderError::CallNotFound)?;
            let call = access
                .shared
                .controller
                .snapshot()
                .call(call_id)
                .ok_or(ServiceProviderError::CallNotFound)?;
            if call.device_id != device_id {
                return Err(ServiceProviderError::CallOwnership);
            }
            let config = access.config();
            let enabled = config
                .parking_for_device(&device_id)
                .is_some_and(|parking| parking.enabled);
            let line_lot = access
                .line_binding(&device_id, call.line_instance)
                .and_then(|binding| {
                    config
                        .parking_for_line(&binding.line.number)
                        .and_then(|parking| parking.lot.clone())
                });
            let lot = requested_lot.or(line_lot);
            drop(config);
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
            let _pbx_id = result.0.ok_or(ServiceProviderError::CallNotFound)?;
            let effects = result.1.map_err(parking_service_error)?;
            execute_service_effects(access, effects).await?;
            Ok(ServiceOutcome::Parking {
                command,
                device_id,
                call_id,
                lot,
                slot: None,
            })
        }
        AmiParkingCommand::Retrieve => {
            let slot = slot.ok_or(ServiceProviderError::ParkingNotFound)?;
            let config = access.config();
            let selected_line = line_instance.or_else(|| {
                access
                    .shared
                    .controller
                    .snapshot()
                    .registered_device(&device_id)
                    .and_then(|device| device.selected_line)
            });
            let binding = selected_line
                .and_then(|line| access.line_binding(&device_id, line))
                .or_else(|| config.appearances_for_device(&device_id).next().cloned())
                .ok_or(ServiceProviderError::CallState)?;
            let lot = requested_lot
                .or_else(|| {
                    config
                        .parking_for_line(&binding.line.number)
                        .and_then(|parking| parking.lot.clone())
                })
                .unwrap_or_else(|| "default".to_owned());
            drop(config);
            let call_id = begin_parking_retrieval(
                access,
                device_id.clone(),
                binding.line_instance,
                lot.clone(),
                slot,
            )
            .await?;
            Ok(ServiceOutcome::Parking {
                command,
                device_id,
                call_id,
                lot: Some(lot),
                slot: Some(slot),
            })
        }
    }
}
