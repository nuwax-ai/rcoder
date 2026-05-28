//! 终端诊断监听器
//!
//! 将 agent 进程生命周期事件输出到终端，用于调试启动失败等问题。

use agent_abstraction::{DiagnosticsListener, ProcessDiagnostics};

use crate::output::OutputFormatter;

/// 终端诊断监听器
///
/// 接收 `DiagnosticsListener` 回调，将 agent 进程启动、退出、错误等事件
/// 格式化输出到终端。agent 启动失败时输出 stderr 和 exit code。
pub struct TerminalDiagnosticsListener {
    formatter: OutputFormatter,
}

impl TerminalDiagnosticsListener {
    pub fn new(formatter: OutputFormatter) -> Self {
        Self { formatter }
    }
}

impl DiagnosticsListener for TerminalDiagnosticsListener {
    fn on_process_started(&self, pid: u32, command: &str) {
        self.formatter
            .info(&format!("Agent 进程已启动: pid={}, command={}", pid, command));
    }

    fn on_acp_initialized(&self, session_id: &str) {
        self.formatter
            .success(&format!("ACP 初始化完成: session_id={}", session_id));
    }

    fn on_process_exited(&self, diagnostics: &ProcessDiagnostics) {
        if diagnostics.exit_code == Some(0) {
            self.formatter.success("Agent 进程正常退出");
        } else {
            self.formatter.error(&format!(
                "Agent 进程异常退出: exit_code={:?}",
                diagnostics.exit_code
            ));
            eprintln!("{}", diagnostics.format_terminal());
        }
    }

    fn on_process_error(&self, diagnostics: &ProcessDiagnostics) {
        self.formatter
            .error(&format!("Agent 进程错误: {}", diagnostics.error_message.as_deref().unwrap_or("unknown")));
        eprintln!("{}", diagnostics.format_terminal());
    }
}
