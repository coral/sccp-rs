//! Runtime snapshots for the typed native CLI inventory renderer.

use thiserror::Error;

use super::{Access, Arc, DeviceFeatureState, DndMode, RuntimeInventoryProvider};
use crate::ami::cli::{
    CliCapability, CliCapabilityStatus, CliChannel, CliDeviceRuntime, CliFeature,
    CliInventoryCommand, CliInventoryError, CliInventorySnapshot, complete_cli_inventory,
    render_cli_inventory,
};
use crate::ami::inventory::{InventoryProvider, InventoryValue};
use crate::ami::runtime::RuntimeStatusProvider;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RuntimeCliInventoryError {
    #[error("CLI inventory is unavailable")]
    Unavailable,
    #[error(transparent)]
    Inventory(#[from] CliInventoryError),
}

pub fn render_runtime_cli_inventory(
    access: &Access,
    command: CliInventoryCommand,
    arguments: &[&str],
) -> Result<String, RuntimeCliInventoryError> {
    let snapshot = runtime_cli_inventory_snapshot(access)?;
    render_cli_inventory(command, arguments, &snapshot).map_err(Into::into)
}

pub fn complete_runtime_cli_inventory(
    access: &Access,
    command: CliInventoryCommand,
    arguments: &[&str],
    prefix: &str,
    ordinal: usize,
) -> Option<String> {
    let snapshot = runtime_cli_inventory_snapshot(access).ok()?;
    complete_cli_inventory(command, arguments, prefix, ordinal, &snapshot)
}

fn runtime_cli_inventory_snapshot(
    access: &Access,
) -> Result<CliInventorySnapshot, RuntimeCliInventoryError> {
    let provider = RuntimeInventoryProvider {
        shared: Arc::downgrade(&access.shared),
        phone: access.phone.clone(),
    };
    let inventory = InventoryProvider::snapshot(&provider)
        .map_err(|_| RuntimeCliInventoryError::Unavailable)?;
    let status = RuntimeStatusProvider::snapshot(&provider)
        .map_err(|_| RuntimeCliInventoryError::Unavailable)?;
    let device_runtime = {
        let controller = access.shared.controller.snapshot();
        inventory
            .devices
            .iter()
            .map(|device| {
                let (capability_status, capabilities) =
                    match controller.registered_device(&device.id) {
                        None => (CliCapabilityStatus::Unavailable, Vec::new()),
                        Some(registered) => match registered.capabilities.as_ref() {
                            None => (CliCapabilityStatus::Pending, Vec::new()),
                            Some(capabilities) if capabilities.is_empty() => {
                                (CliCapabilityStatus::ReportedEmpty, Vec::new())
                            }
                            Some(capabilities) => (
                                CliCapabilityStatus::Ready,
                                capabilities
                                    .audio()
                                    .iter()
                                    .map(|capability| CliCapability {
                                        codec: capability.codec,
                                        max_packet_ms: capability.max_packet_ms,
                                    })
                                    .collect(),
                            ),
                        },
                    };
                let features = controller
                    .feature_state(&device.id)
                    .map(cli_features)
                    .unwrap_or_default();
                CliDeviceRuntime {
                    device_id: device.id.clone(),
                    capability_status,
                    capabilities,
                    features,
                }
            })
            .collect()
    };
    let channels = status
        .calls
        .into_iter()
        .map(|call| CliChannel {
            pbx_id: call.pbx_id,
            call_id: call.active_call_id.map(|call_id| call_id.0),
            line: call.line,
            context: call.context,
            state: call.state,
            direction: call.direction,
            dialed_number: call.dialed_number,
            privacy: call.privacy,
            appearance_count: call.appearance_count,
        })
        .collect();
    Ok(CliInventorySnapshot {
        inventory,
        device_runtime,
        channels,
    })
}

fn cli_features(state: &DeviceFeatureState) -> Vec<CliFeature> {
    let mut features = vec![
        public_feature(
            "dnd",
            match state.dnd {
                DndMode::Off => "off",
                DndMode::Silent => "silent",
                DndMode::Reject => "reject",
            },
        ),
        public_feature("privacy", on_off(state.privacy)),
        public_feature("forward-all", on_off(state.forwarding.all.is_some())),
        public_feature("forward-busy", on_off(state.forwarding.busy.is_some())),
        public_feature(
            "forward-no-answer",
            on_off(state.forwarding.no_answer.is_some()),
        ),
    ];
    features.extend(state.buttons.iter().map(|(instance, enabled)| CliFeature {
        name: format!("button:{instance}"),
        value: InventoryValue::Public(on_off(*enabled).to_owned()),
    }));
    features
}

fn public_feature(name: &str, value: &str) -> CliFeature {
    CliFeature {
        name: name.to_owned(),
        value: InventoryValue::Public(value.to_owned()),
    }
}

const fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}
