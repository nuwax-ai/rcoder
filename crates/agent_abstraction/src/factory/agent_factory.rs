//! Agent factory implementation.
//!
//! AgentFactory 专注于配置验证。
//! 实际的进程启动由 AgentLifecycleManager 负责。

use std::sync::Arc;

use super::super::{AgentRegistry, AgentSpec};

/// Agent instance status
#[derive(Debug, Clone, PartialEq)]
pub enum AgentInstanceStatus {
    /// Agent is stopped
    Stopped,
    /// Agent is starting
    Starting,
    /// Agent is running
    Running,
    /// Agent is stopping
    Stopping,
    /// Agent has an error
    Error,
    /// Agent status is unknown
    Unknown,
}

/// Agent instance metadata
#[derive(Debug, Clone)]
pub struct AgentInstance {
    /// Agent specification
    pub spec: AgentSpec,

    /// Status
    pub status: AgentInstanceStatus,

    /// Start timestamp
    pub started_at: Option<std::time::Instant>,
}

impl AgentInstance {
    /// Create a new AgentInstance
    pub fn new(spec: AgentSpec) -> Self {
        Self {
            spec,
            status: AgentInstanceStatus::Stopped,
            started_at: None,
        }
    }

    /// Update status to starting
    pub fn mark_starting(&mut self) {
        self.status = AgentInstanceStatus::Starting;
    }

    /// Update status to running
    pub fn mark_running(&mut self) {
        self.status = AgentInstanceStatus::Running;
        self.started_at = Some(std::time::Instant::now());
    }

    /// Update status to stopped
    pub fn mark_stopped(&mut self) {
        self.status = AgentInstanceStatus::Stopped;
        self.started_at = None;
    }

    /// Update status to error
    pub fn mark_error(&mut self) {
        self.status = AgentInstanceStatus::Error;
    }

    /// Check if running
    pub fn is_running(&self) -> bool {
        matches!(self.status, AgentInstanceStatus::Running)
    }

    /// Get uptime
    pub fn uptime(&self) -> Option<std::time::Duration> {
        self.started_at.map(|t| t.elapsed())
    }

    /// Get spec reference
    pub fn spec(&self) -> &AgentSpec {
        &self.spec
    }
}

/// Configuration validation error
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: String,
    pub message: String,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Validation error in '{}': {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}

/// Validation result
pub type ValidationResult = Result<(), Vec<ValidationError>>;

/// Agent factory for configuration validation
///
/// AgentFactory 专注于配置验证：
/// - 验证 AgentSpec 的完整性和正确性
/// - 检查 agent_id 是否已注册
/// - 验证命令和参数格式
///
/// 实际的进程启动由 AgentLifecycleManager 负责。
pub struct AgentFactory {
    /// Agent registry for type validation
    registry: Arc<AgentRegistry>,
}

impl AgentFactory {
    /// Create a new AgentFactory
    pub fn new(registry: Arc<AgentRegistry>) -> Self {
        Self { registry }
    }

    /// Validate an AgentSpec
    pub fn validate_spec(&self, spec: &AgentSpec) -> ValidationResult {
        let mut errors = Vec::new();

        // Validate agent_id is not empty
        if spec.agent_id.trim().is_empty() {
            errors.push(ValidationError {
                field: "agent_id".to_string(),
                message: "Agent ID cannot be empty".to_string(),
            });
        }

        // Validate command is not empty
        if spec.command.trim().is_empty() {
            errors.push(ValidationError {
                field: "command".to_string(),
                message: "Command cannot be empty".to_string(),
            });
        }

        // Validate enabled status
        if !spec.enabled {
            errors.push(ValidationError {
                field: "enabled".to_string(),
                message: "Agent is disabled".to_string(),
            });
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Validate an AgentConfig
    pub fn validate_config(
        &self,
        config: &agent_config::AgentConfig,
    ) -> ValidationResult {
        let mut errors = Vec::new();

        // Validate agent_id is not empty
        if config.agent_id.trim().is_empty() {
            errors.push(ValidationError {
                field: "agent_id".to_string(),
                message: "Agent ID cannot be empty".to_string(),
            });
        }

        // Validate command is not empty
        if config.command.trim().is_empty() {
            errors.push(ValidationError {
                field: "command".to_string(),
                message: "Command cannot be empty".to_string(),
            });
        }

        // Validate enabled status
        if !config.enabled {
            errors.push(ValidationError {
                field: "enabled".to_string(),
                message: "Agent is disabled".to_string(),
            });
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// Quick check if agent_id is registered
    pub fn is_agent_registered(&self, agent_id: &str) -> bool {
        self.registry.contains(agent_id)
    }

    /// Get reference to the registry
    pub fn registry(&self) -> &Arc<AgentRegistry> {
        &self.registry
    }
}
