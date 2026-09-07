//! Forwarding expiry and shared-line no-answer orchestration.

use super::{
    Access, AsteriskBackend, ForwardingOperation, ForwardingRouteReason, LogLevel, PbxCallId,
    ast_log, execute_effects, publish_line,
};
use crate::runtime::backend::SupplementaryBackend as _;

pub async fn execute_no_answer_route(
    access: &Access,
    timer: crate::call::forwarding::NoAnswerTimer,
    line: Option<String>,
) {
    let operation = ForwardingOperation {
        call_id: timer.call_id,
        context: timer.context,
        destination: timer.destination,
        reason: ForwardingRouteReason::NoAnswer,
    };
    let result = AsteriskBackend::new(access).forward(&operation);
    let effects = access
        .shared
        .controller
        .finish_no_answer_route(timer.call_id, timer.id, result.is_ok())
        .unwrap_or_default();
    if let Err(error) = result {
        ast_log(
            LogLevel::Warning,
            &format!(
                "unable to apply no-answer routing for PBX call {}: {error}",
                timer.call_id.0
            ),
        );
        return;
    }
    if let Some(line) = line {
        publish_line(access, &line);
    }
    execute_effects(access, effects).await;
}

pub fn cancel_no_answer_timer(access: &Access, pbx_id: PbxCallId) -> bool {
    access
        .shared
        .controller
        .cancel_no_answer_timer(pbx_id)
        .unwrap_or(false)
}
