//! ProcessDiagnostics — structured information about an agent subprocess.

use std::path::PathBuf;

/// Structured diagnostic information about an agent subprocess.
///
/// Assembled by `AgentLifecycleGuard` when the process exits or encounters
/// an error. Delivered to the consumer via [`DiagnosticsListener`](super::DiagnosticsListener).
#[derive(Debug, Clone)]
pub struct ProcessDiagnostics {
    /// Agent startup command (e.g. `"python"`, `"./my-agent"`)
    pub command: String,

    /// Command arguments
    pub args: Vec<String>,

    /// Working directory the agent was launched in
    pub working_dir: PathBuf,

    /// Process PID (0 if the process was never started)
    pub pid: u32,

    /// Exit code (None if the process is still running or was never started)
    pub exit_code: Option<i32>,

    /// Last N lines of stderr output (typically 20 lines)
    pub stderr_tail: Vec<String>,

    /// Whether `which(command)` found the binary in PATH
    pub command_exists: bool,

    /// Time from `Command::spawn()` to ACP session creation (milliseconds)
    pub startup_duration_ms: u64,

    /// Whether the ACP protocol handshake completed successfully
    pub acp_init_success: bool,

    /// Human-readable error description (if any)
    pub error_message: Option<String>,
}

impl ProcessDiagnostics {
    /// Create a minimal diagnostics instance for a command that couldn't be found.
    pub fn command_not_found(command: &str, working_dir: PathBuf) -> Self {
        Self {
            command: command.to_string(),
            args: Vec::new(),
            working_dir,
            pid: 0,
            exit_code: None,
            stderr_tail: Vec::new(),
            command_exists: false,
            startup_duration_ms: 0,
            acp_init_success: false,
            error_message: Some(format!(
                "Command '{}' not found in PATH",
                command
            )),
        }
    }

    /// Format diagnostics as a multi-line human-readable string (for terminal output).
    pub fn format_terminal(&self) -> String {
        let mut lines = Vec::new();

        lines.push(format!(
            "  command: {} {}",
            self.command,
            self.args.join(" ")
        ));
        lines.push(format!("  working_dir: {}", self.working_dir.display()));
        lines.push(format!("  command_exists: {}", self.command_exists));

        if self.pid > 0 {
            lines.push(format!("  pid: {}", self.pid));
        }
        if let Some(code) = self.exit_code {
            lines.push(format!("  exit_code: {}", code));
        }
        lines.push(format!(
            "  startup_duration_ms: {}",
            self.startup_duration_ms
        ));
        lines.push(format!("  acp_init_success: {}", self.acp_init_success));

        if !self.stderr_tail.is_empty() {
            lines.push("  stderr (tail):".to_string());
            for line in &self.stderr_tail {
                lines.push(format!("    {}", line));
            }
        }

        if let Some(ref msg) = self.error_message {
            lines.push(format!("  error: {}", msg));
        }

        lines.join("\n")
    }
}

impl std::fmt::Display for ProcessDiagnostics {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.format_terminal())
    }
}
