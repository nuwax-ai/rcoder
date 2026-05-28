//! DiagnosticsListener — callback trait for agent process lifecycle events.

use super::ProcessDiagnostics;

/// Callback interface for receiving agent process lifecycle events.
///
/// Implement this trait to get notified when an agent subprocess starts,
/// finishes ACP initialization, exits, or encounters an error.
///
/// # Usage
///
/// Inject via `AcpClientBuilder::diagnostics_listener()`. The `agent_runner`
/// service does not inject a listener, so the overhead is zero in production.
///
/// # Example
///
/// ```ignore
/// struct TerminalDiagnosticsListener;
///
/// impl DiagnosticsListener for TerminalDiagnosticsListener {
///     fn on_process_started(&self, pid: u32, command: &str) {
///         eprintln!("[ACP] Agent started: pid={}, command={}", pid, command);
///     }
///     fn on_acp_initialized(&self, session_id: &str) {
///         eprintln!("[ACP] Session ready: {}", session_id);
///     }
///     fn on_process_exited(&self, diag: &ProcessDiagnostics) {
///         eprintln!("[ACP] Agent exited: code={:?}", diag.exit_code);
///     }
///     fn on_process_error(&self, diag: &ProcessDiagnostics) {
///         eprintln!("[ACP] Agent error:\n{}", diag);
///     }
/// }
/// ```
pub trait DiagnosticsListener: Send + Sync + 'static {
    /// Called when the agent subprocess has been spawned successfully.
    ///
    /// # Arguments
    /// * `pid` — process ID of the spawned subprocess
    /// * `command` — the command used to start the process
    fn on_process_started(&self, pid: u32, command: &str);

    /// Called when the ACP protocol handshake completes and a session is established.
    ///
    /// # Arguments
    /// * `session_id` — the ACP session ID assigned by the agent
    fn on_acp_initialized(&self, session_id: &str);

    /// Called when the agent subprocess exits normally.
    ///
    /// # Arguments
    /// * `diagnostics` — structured information about the exited process
    fn on_process_exited(&self, diagnostics: &ProcessDiagnostics);

    /// Called when the agent process encounters an error (spawn failure,
    /// ACP handshake timeout, unexpected exit, etc.).
    ///
    /// # Arguments
    /// * `diagnostics` — structured information about the failed process
    fn on_process_error(&self, diagnostics: &ProcessDiagnostics);
}

/// No-op diagnostics listener — discards all events.
///
/// Used as the default when no listener is configured.
pub struct NoopDiagnosticsListener;

impl DiagnosticsListener for NoopDiagnosticsListener {
    fn on_process_started(&self, _pid: u32, _command: &str) {}
    fn on_acp_initialized(&self, _session_id: &str) {}
    fn on_process_exited(&self, _diagnostics: &ProcessDiagnostics) {}
    fn on_process_error(&self, _diagnostics: &ProcessDiagnostics) {}
}
