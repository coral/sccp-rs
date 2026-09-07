//! call service backend-effect translation.

use super::{
    AsteriskBackend, AsteriskBackendError, CallFeatureProvider as _, CallServiceBackend,
    MutexExt as _, ParkingOperation, PickupOperation, PickupOutcome, native_bridging,
    native_pickup_result,
};

impl CallServiceBackend for AsteriskBackend<'_> {
    fn pickup(&self, operation: &PickupOperation) -> Result<PickupOutcome, Self::Error> {
        let (call_id, result) = match operation {
            PickupOperation::Group {
                call_id, answer, ..
            } => (
                *call_id,
                self.with_call_feature_channel("group pickup", *call_id, |channel| {
                    self.call_features
                        .group_pickup(channel, *answer)
                        .map_err(AsteriskBackendError::CallFeature)
                })?,
            ),
            PickupOperation::Directed {
                call_id,
                extension,
                context,
                answer,
                ..
            } => (
                *call_id,
                self.with_call_feature_channel("directed pickup", *call_id, |channel| {
                    self.call_features
                        .directed_pickup(channel, extension, context, *answer)
                        .map_err(AsteriskBackendError::CallFeature)
                })?,
            ),
        };
        let (replacement, parties) =
            native_pickup_result(result).map_err(AsteriskBackendError::CallFeature)?;
        let replaced = {
            let mut channels = self.access.shared.channels.lock_unpoisoned();
            let current = channels
                .get(&call_id)
                .filter(|binding| !binding.is_closed())
                .ok_or(AsteriskBackendError::CallUnavailable {
                    operation: "pickup replacement",
                    call_id,
                })?;
            let replacement = current.replacement(replacement);
            channels.insert(call_id, replacement)
        };
        if let Some(replaced) = replaced {
            drop(replaced.close());
        }
        Ok(parties)
    }

    fn parking(&self, operation: &ParkingOperation) -> Result<(), Self::Error> {
        match operation {
            ParkingOperation::Park { call_id, lot } => {
                self.with_call_feature_channel("park call", *call_id, |channel| {
                    let unique_id = native_bridging::parking_peer_uniqueid(channel)
                        .map_err(AsteriskBackendError::CallFeature)?;
                    let unique_id = unique_id.ok_or(AsteriskBackendError::CallUnavailable {
                        operation: "park peer identity",
                        call_id: *call_id,
                    })?;
                    let admission = self
                        .access
                        .shared
                        .parking_events
                        .reserve(unique_id.clone())
                        .map_err(|_| AsteriskBackendError::CallUnavailable {
                            operation: "park completion admission",
                            call_id: *call_id,
                        })?;
                    self.access
                        .shared
                        .controller
                        .set_park_peer(*call_id, unique_id)
                        .map_err(|_| AsteriskBackendError::CallUnavailable {
                            operation: "park peer identity",
                            call_id: *call_id,
                        })?;
                    self.call_features
                        .park(channel, lot.as_deref())
                        .map_err(AsteriskBackendError::CallFeature)?;
                    admission.commit();
                    Ok(())
                })
            }
            ParkingOperation::Retrieve { call_id, lot, slot } => {
                self.with_call_feature_channel("retrieve parked call", *call_id, |channel| {
                    let snapshot = self.access.shared.controller.snapshot();
                    let unique_id = snapshot
                        .pending_retrievals()
                        .values()
                        .find(|attempt| attempt.pbx_id == *call_id)
                        .and_then(|attempt| snapshot.parking().call(&attempt.lot, attempt.slot))
                        .map(|parked| parked.parkee_unique_id.clone())
                        .ok_or(AsteriskBackendError::CallUnavailable {
                            operation: "parking retrieval identity",
                            call_id: *call_id,
                        })?;
                    let admission = self
                        .access
                        .shared
                        .parking_events
                        .reserve(unique_id)
                        .map_err(|_| AsteriskBackendError::CallUnavailable {
                            operation: "parking retrieval completion admission",
                            call_id: *call_id,
                        })?;
                    self.call_features
                        .retrieve(channel, lot.as_deref(), slot)
                        .map_err(AsteriskBackendError::CallFeature)?;
                    admission.commit();
                    Ok(())
                })
            }
        }
    }
}
