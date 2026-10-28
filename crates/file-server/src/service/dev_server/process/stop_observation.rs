//! Signal acknowledgement, PID-slot presence and actual execution are distinct.
use crate::error::{AppError, AppResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TerminationObservation {
    pub signal_sent: bool,
    pub exit_confirmed: bool,
}

/// Owned children actively wait/reap their original handle. Raw Unix legacy
/// candidates use execution state, so a zombie alone cannot veto a successor.
/// Live group members and permission/observation failures remain protected.
#[cfg(unix)]
pub(crate) async fn stop_exit_observed(
    pid: u32,
    deadline: tokio::time::Instant,
) -> AppResult<bool> {
    let live = tokio::time::timeout_at(
        deadline,
        process_utils::execution_tree::has_live_members(pid),
    )
    .await
    .map_err(|_| {
        AppError::process("execution observation exceeded the original stop deadline")
    })??;
    Ok(!live)
}

// Modern Windows previews always retain their original Job wrapper. Numeric
// root absence alone cannot confirm the unknown descendants of an old record.
#[cfg(windows)]
pub(crate) async fn stop_exit_observed(
    _pid: u32,
    _deadline: tokio::time::Instant,
) -> AppResult<bool> {
    Err(AppError::process(
        "Windows process-tree exit requires its original managed child handle",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn signal_acknowledgement_is_not_an_exit_receipt() {
        let held = TerminationObservation {
            signal_sent: true,
            exit_confirmed: false,
        };
        assert!(!held.exit_confirmed);
        assert!(held.signal_sent);
    }
}
