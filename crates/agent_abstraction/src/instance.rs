//! Agent instance representation.

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::Duration;

/// Agent instance
#[derive(Debug, Clone)]
pub struct AgentInstance {
    /// Configuration used to start this instance
    pub config: crate::types::AgentConfig,

    /// Running process (if any)
    pub process: Option<Arc<super::process::AgentProcess>>,

    /// Current status
    pub status: crate::types::AgentStatus,

    /// When this instance was started
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl AgentInstance {
    /// Create a new agent instance
    pub fn new(config: crate::types::AgentConfig) -> Self {
        Self {
            config,
            process: None,
            status: crate::types::AgentStatus::Stopped,
            started_at: None,
        }
    }

    /// Update the status
    pub fn update_status(&mut self, status: crate::types::AgentStatus) {
        self.status = status;
    }

    /// Set the process
    pub fn set_process(&mut self, process: super::process::AgentProcess) {
        self.process = Some(process);
        self.status = crate::types::AgentStatus::Running;
        self.started_at = Some(chrono::Utc::now());
    }

    /// Check if the instance is running
    pub fn is_running(&self) -> bool {
        matches!(self.status, crate::types::AgentStatus::Running)
    }

    /// Get the running duration
    pub fn running_duration(&self) -> Option<Duration> {
        self.started_at.map(|started| {
            chrono::Utc::now()
                .signed_duration_since(started)
                .to_std()
                .unwrap_or_default()
        })
    }
}
