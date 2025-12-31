//! Agent process management module.

use std::sync::{Arc, Mutex};
use tokio::process::Child;

/// Agent process wrapper
#[derive(Debug)]
pub struct AgentProcess {
    /// Process ID
    pub id: String,

    /// Child process (wrapped in Arc<Mutex> for thread safety)
    pub child: Arc<Mutex<Child>>,

    /// Agent configuration
    pub config: agent_config::AgentConfig,

    /// Start time
    pub start_time: std::time::Instant,
}

impl AgentProcess {
    /// Create a new agent process
    pub fn new(id: String, child: Child, config: agent_config::AgentConfig) -> Self {
        Self {
            id,
            child: Arc::new(Mutex::new(child)),
            config,
            start_time: std::time::Instant::now(),
        }
    }

    /// Get the process ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the agent configuration
    pub fn config(&self) -> &agent_config::AgentConfig {
        &self.config
    }

    /// Wait for the process to complete
    pub async fn wait(&self) -> tokio::io::Result<std::process::ExitStatus> {
        let mut child = self.child.lock().expect("child process mutex poisoned");
        child.wait().await
    }

    /// Kill the process
    pub async fn kill(&self) -> tokio::io::Result<()> {
        let mut child = self.child.lock().expect("child process mutex poisoned");
        child.kill().await
    }

    /// Try to wait for the process without blocking
    pub fn try_wait(&self) -> tokio::io::Result<Option<std::process::ExitStatus>> {
        let mut child = self.child.lock().expect("child process mutex poisoned");
        child.try_wait()
    }

    /// Get stdin for the process
    pub fn stdin(&mut self) -> Option<tokio::process::ChildStdin> {
        let mut child = self.child.lock().expect("child process mutex poisoned");
        child.stdin.take()
    }

    /// Get stdout for the process
    pub fn stdout(&mut self) -> Option<tokio::process::ChildStdout> {
        let mut child = self.child.lock().expect("child process mutex poisoned");
        child.stdout.take()
    }

    /// Get stderr for the process
    pub fn stderr(&mut self) -> Option<tokio::process::ChildStderr> {
        let mut child = self.child.lock().expect("child process mutex poisoned");
        child.stderr.take()
    }

    /// Get elapsed time since start
    pub fn elapsed(&self) -> std::time::Duration {
        self.start_time.elapsed()
    }
}

impl Drop for AgentProcess {
    fn drop(&mut self) {
        // Try to kill the process if it's still running
        if let Ok(mut child) = self.child.try_lock() {
            if let Err(e) = child.start_kill() {
                tracing::warn!(
                    "⚠️ [PROCESS] start_kill 失败（进程可能已结束）: id={}, error={}",
                    self.id,
                    e
                );
            }
        }
    }
}
