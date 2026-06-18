//! LoggingDiagnosticsListener — agent_runner 默认的 DiagnosticsListener 实现
//!
//! 把 agent 子进程生命周期事件转为 tracing 日志。
//! 默认由 [`AgentSessionService`](super::agent_session_service) 注入,
//! 因此 P0-2 接线后,所有 agent 启动/ACP 握手/异常退出事件都会自动记录到日志中。

use agent_abstraction::diagnostics::{DiagnosticsListener, ProcessDiagnostics};
use tracing::{error, info, warn};

/// 把 agent 进程诊断事件打到 tracing 日志的 listener。
///
/// 行为:
/// - `on_process_started` — info 级,带 PID 与命令行
/// - `on_acp_initialized` — info 级,带 session_id
/// - `on_process_exited` — info 级,exit_code 为 0;warn 级,非 0 退出但属于用户主动取消
/// - `on_process_error` — error 级,带完整的 ProcessDiagnostics 格式化输出
pub struct LoggingDiagnosticsListener;

impl DiagnosticsListener for LoggingDiagnosticsListener {
    fn on_process_started(&self, pid: u32, command: &str) {
        info!(
            "[DIAG] agent process started: pid={}, command={}",
            pid, command
        );
    }

    fn on_acp_initialized(&self, session_id: &str) {
        info!("[DIAG] ACP session initialized: session_id={}", session_id);
    }

    fn on_process_exited(&self, diagnostics: &ProcessDiagnostics) {
        let code = diagnostics.exit_code.unwrap_or(-1);
        if code == 0 {
            info!(
                "[DIAG] agent process exited cleanly: pid={}, exit_code={}",
                diagnostics.pid, code
            );
        } else {
            warn!(
                "[DIAG] agent process exited with non-zero code (expected for user-cancel): pid={}, exit_code={}, command={}",
                diagnostics.pid, code, diagnostics.command
            );
        }
    }

    fn on_process_error(&self, diagnostics: &ProcessDiagnostics) {
        error!("[DIAG] agent process encountered error:\n{}", diagnostics);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn sample_diag(exit_code: Option<i32>) -> ProcessDiagnostics {
        ProcessDiagnostics {
            command: "claude-code-acp".to_string(),
            args: vec!["--mcp".to_string()],
            working_dir: PathBuf::from("/tmp"),
            pid: 1234,
            exit_code,
            stderr_tail: vec!["err line".to_string()],
            command_exists: true,
            startup_duration_ms: 100,
            acp_init_success: true,
            error_message: None,
        }
    }

    #[test]
    fn all_callbacks_invoke_without_panic() {
        let listener = LoggingDiagnosticsListener;
        listener.on_process_started(42, "fake-agent");
        listener.on_acp_initialized("session-xyz");
        listener.on_process_exited(&sample_diag(Some(0)));
        listener.on_process_exited(&sample_diag(Some(137)));
        listener.on_process_error(&sample_diag(Some(1)));
    }

    #[test]
    fn process_exited_handles_none_exit_code() {
        let listener = LoggingDiagnosticsListener;
        listener.on_process_exited(&sample_diag(None));
    }
}
