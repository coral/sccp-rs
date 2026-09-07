//! Runtime snapshot composition for native media and session diagnostics.

use thiserror::Error;

use super::{Access, RuntimeInventoryProvider, RwLockExt as _};
use crate::ami::diagnostics::{
    CliDiagnosticCommand, CliDiagnosticError, CliDiagnosticSnapshot, CliSessionCall,
    complete_cli_diagnostics, render_cli_diagnostics,
};
use crate::ami::inventory::InventoryProvider;
use crate::ami::runtime::RuntimeStatusProvider;

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RuntimeCliDiagnosticError {
    #[error("CLI diagnostics are unavailable")]
    Unavailable,
    #[error(transparent)]
    Diagnostic(#[from] CliDiagnosticError),
}

pub fn render_runtime_cli_diagnostics(
    access: &Access,
    command: CliDiagnosticCommand,
    arguments: &[&str],
) -> Result<String, RuntimeCliDiagnosticError> {
    let snapshot = runtime_cli_diagnostic_snapshot(access)?;
    let mut rendered = render_cli_diagnostics(command, arguments, &snapshot)?;
    if command == CliDiagnosticCommand::Sessions && arguments.is_empty() {
        use std::fmt::Write as _;
        rendered.push_str("\nRuntime queues (admission retained through completion):\n");
        let events = access.shared.event_diagnostics.read_unpoisoned().clone();
        for (name, queue) in [
            ("controls", access.shared.control_requests.snapshot()),
            ("services", access.shared.service_requests.snapshot()),
            ("AMI publication", access.shared.ami_events.snapshot()),
            ("Presence", access.shared.presence.snapshot()),
            ("backgrounds", access.shared.background_runtime.snapshot()),
            ("parking callbacks", access.shared.parking_events.snapshot()),
            ("DND schedules", access.shared.dnd_schedules.snapshot()),
            ("Controller", access.shared.controller.diagnostics()),
            (
                "controller completions",
                access.shared.controller.completion_diagnostics(),
            ),
            ("media", access.shared.media_runtime.snapshot()),
            ("bridges", access.shared.bridge_runtime.snapshot()),
            ("call delivery", access.shared.call_signals.snapshot()),
            (
                "configuration transactions",
                access.shared.configuration_transactions.snapshot(),
            ),
            ("effect workers", access.shared.workers.snapshot()),
        ]
        .into_iter()
        .chain(
            events
                .queues
                .iter()
                .map(|(name, monitor)| (*name, monitor.snapshot())),
        ) {
            let _ = writeln!(
                rendered,
                "  {name}: queued={} outstanding={} capacity={} high-water={} admission-failures={} expired={}",
                queue.queued,
                queue.outstanding,
                queue.capacity,
                queue.high_water,
                queue.admission_failures,
                queue.expired,
            );
        }
    }
    Ok(rendered)
}

pub fn complete_runtime_cli_diagnostics(
    access: &Access,
    command: CliDiagnosticCommand,
    arguments: &[&str],
    prefix: &str,
    ordinal: usize,
) -> Option<String> {
    let snapshot = runtime_cli_diagnostic_snapshot(access).ok()?;
    complete_cli_diagnostics(command, arguments, prefix, ordinal, &snapshot)
}

fn runtime_cli_diagnostic_snapshot(
    access: &Access,
) -> Result<CliDiagnosticSnapshot, RuntimeCliDiagnosticError> {
    let provider = RuntimeInventoryProvider {
        shared: std::sync::Arc::downgrade(&access.shared),
        phone: access.phone.clone(),
    };
    let inventory = InventoryProvider::snapshot(&provider)
        .map_err(|_| RuntimeCliDiagnosticError::Unavailable)?;
    let runtime = RuntimeStatusProvider::snapshot(&provider)
        .map_err(|_| RuntimeCliDiagnosticError::Unavailable)?;
    let session_calls = access
        .shared
        .controller
        .snapshot()
        .calls()
        .map(|call| CliSessionCall {
            device_id: call.device_id,
            pbx_id: call.pbx_id,
            call_id: call.sccp_id,
        })
        .collect();
    Ok(CliDiagnosticSnapshot {
        inventory,
        runtime,
        session_calls,
    })
}
