//! Agent manager implementation.
//!
//! AgentManager 是高层业务编排层，负责：
//! - 配置管理和解析
//! - 业务逻辑（选择 agent、统计等）
//! - 委托实际的进程操作给 AgentLifecycleManager

use std::collections::HashMap;
use std::sync::Arc;

use crate::{AgentLifecycleManager, AgentProcess, ProcessLaunchInfo};

/// Agent status (business level)
#[derive(Debug, Clone, PartialEq)]
pub enum AgentStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Error,
    Unknown,
}

impl From<shared_types::AgentStatus> for AgentStatus {
    fn from(status: shared_types::AgentStatus) -> Self {
        match status {
            shared_types::AgentStatus::Pending => AgentStatus::Starting,
            shared_types::AgentStatus::Idle => AgentStatus::Running,
            shared_types::AgentStatus::Active => AgentStatus::Running,
            shared_types::AgentStatus::Terminating => AgentStatus::Stopping,
        }
    }
}

/// Agent idle statistics
#[derive(Debug, Clone)]
pub struct AgentIdleStatistics {
    pub total_enabled: usize,
    pub idle_count: usize,
    pub active_count: usize,
    pub unknown_count: usize,
}

/// Agent manager for managing Agent instances
///
/// AgentManager 是高层业务编排层：
/// - 管理配置 (AgentServersConfig)
/// - 提供业务级别的 start/stop/status 接口
/// - 委托实际的进程操作给 AgentLifecycleManager
pub struct AgentManager {
    /// Configuration
    config: AgentServersConfig,

    /// Lifecycle manager - 负责实际的进程管理
    lifecycle_manager: Arc<AgentLifecycleManager>,
}

impl AgentManager {
    /// Create a new AgentManager
    pub fn new(config: AgentServersConfig) -> Self {
        Self {
            config,
            lifecycle_manager: Arc::new(AgentLifecycleManager::new()),
        }
    }

    /// Create with custom lifecycle manager
    pub fn with_lifecycle_manager(
        config: AgentServersConfig,
        lifecycle_manager: Arc<AgentLifecycleManager>,
    ) -> Self {
        Self {
            config,
            lifecycle_manager,
        }
    }

    /// Get lifecycle manager reference
    pub fn lifecycle_manager(&self) -> &Arc<AgentLifecycleManager> {
        &self.lifecycle_manager
    }

    /// Start an agent
    ///
    /// 1. 从配置中获取 agent 配置
    /// 2. 创建 ProcessLaunchInfo
    /// 3. 委托给 lifecycle_manager 启动进程
    pub async fn start_agent(
        &self,
        agent_id: &str,
        working_dir: Option<std::path::PathBuf>,
    ) -> Result<AgentProcess, Box<dyn std::error::Error + Send + Sync>> {
        // Get agent config
        let agent_config = self
            .config
            .get_agent(agent_id)
            .ok_or_else(|| format!("Agent '{}' not found in config", agent_id))?;

        // Check if enabled
        if !agent_config.enabled {
            return Err(format!("Agent '{}' is disabled", agent_id).into());
        }

        // Create ProcessLaunchInfo from config
        let launch_info = ProcessLaunchInfo {
            id: agent_id.to_string(),
            name: agent_config.agent_id.clone(),
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            working_dir: working_dir.unwrap_or_else(|| std::path::PathBuf::from(".")),
            env: agent_config.env.clone(),
            config: agent_config.clone(),
        };

        // Delegate to lifecycle manager
        self.lifecycle_manager
            .spawn_agent(launch_info)
            .await
            .map_err(|e| e.into())
    }

    /// Stop an agent
    pub async fn stop_agent(
        &self,
        agent_id: &str,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.lifecycle_manager
            .stop_agent(agent_id)
            .await
            .map_err(|e| e.into())
    }

    /// Restart an agent
    pub async fn restart_agent(
        &self,
        agent_id: &str,
    ) -> Result<AgentProcess, Box<dyn std::error::Error + Send + Sync>> {
        self.lifecycle_manager
            .restart_agent(agent_id)
            .await
            .map_err(|e| e.into())
    }

    /// Get agent status
    pub fn get_agent_status(&self, agent_id: &str) -> Option<AgentStatus> {
        self.lifecycle_manager
            .get_status_info(agent_id)
            .map(|info| AgentStatus::from(info.status))
    }

    /// Check if agent is running
    pub fn is_agent_running(&self, agent_id: &str) -> bool {
        self.lifecycle_manager
            .get_process_status(agent_id)
            .map(|status| matches!(status, crate::ProcessStatus::Running))
            .unwrap_or(false)
    }

    /// List all configured agents
    pub fn list_agents(&self) -> Vec<&agent_config::AgentConfig> {
        self.config.list_agents()
    }

    /// List enabled agents
    pub fn list_enabled_agents(&self) -> Vec<&agent_config::AgentConfig> {
        self.config.get_enabled_agents()
    }

    /// Check if agent is idle
    pub fn is_agent_idle(&self, agent_id: &str) -> Option<bool> {
        self.lifecycle_manager.is_agent_idle(agent_id)
    }

    /// Get agent idle status
    pub fn get_agent_idle_status(
        &self,
        agent_id: &str,
    ) -> Option<crate::lifecycle::AgentIdleStatus> {
        self.lifecycle_manager.get_agent_idle_status(agent_id)
    }

    /// List idle agents
    pub fn list_idle_agents(&self) -> Vec<String> {
        self.config
            .list_agents()
            .iter()
            .filter_map(|config| {
                if self.is_agent_idle(&config.agent_id).unwrap_or(false) {
                    Some(config.agent_id.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    /// Get idle statistics
    pub fn get_idle_statistics(&self) -> AgentIdleStatistics {
        let enabled_agents = self.config.get_enabled_agents();
        let total = enabled_agents.len();

        let mut idle_count = 0;
        let mut active_count = 0;
        let mut unknown_count = 0;

        for agent in enabled_agents {
            match self.get_agent_status(&agent.agent_id) {
                Some(AgentStatus::Running) => {
                    if self.is_agent_idle(&agent.agent_id).unwrap_or(false) {
                        idle_count += 1;
                    } else {
                        active_count += 1;
                    }
                }
                Some(AgentStatus::Stopped) => {}
                Some(_) | None => unknown_count += 1,
            }
        }

        AgentIdleStatistics {
            total_enabled: total,
            idle_count,
            active_count,
            unknown_count,
        }
    }

    /// Stop all agents
    pub async fn stop_all(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let agent_ids: Vec<String> = self
            .config
            .list_agents()
            .iter()
            .map(|c| c.agent_id.clone())
            .collect();

        for agent_id in agent_ids {
            let _ = self.stop_agent(&agent_id).await;
        }

        Ok(())
    }

    /// Get the number of running agents
    pub fn running_agent_count(&self) -> usize {
        self.config
            .list_agents()
            .iter()
            .filter(|c| self.is_agent_running(&c.agent_id))
            .count()
    }

    /// Get configuration reference
    pub fn config(&self) -> &AgentServersConfig {
        &self.config
    }
}

// ============================================================================
// Configuration types
// ============================================================================

/// Agent servers configuration
#[derive(Debug, Clone)]
pub struct AgentServersConfig {
    agents: HashMap<String, agent_config::AgentConfig>,
}

impl AgentServersConfig {
    /// Create a new empty config
    pub fn new() -> Self {
        Self {
            agents: HashMap::new(),
        }
    }

    /// Create from HashMap
    pub fn from_map(agents: HashMap<String, agent_config::AgentConfig>) -> Self {
        Self { agents }
    }

    /// Add an agent config
    pub fn add_agent(&mut self, agent_id: String, config: agent_config::AgentConfig) {
        self.agents.insert(agent_id, config);
    }

    /// Get agent by ID
    pub fn get_agent(&self, agent_id: &str) -> Option<&agent_config::AgentConfig> {
        self.agents.get(agent_id)
    }

    /// List all agents
    pub fn list_agents(&self) -> Vec<&agent_config::AgentConfig> {
        self.agents.values().collect()
    }

    /// Get enabled agents
    pub fn get_enabled_agents(&self) -> Vec<&agent_config::AgentConfig> {
        self.agents
            .values()
            .filter(|config| config.enabled)
            .collect()
    }

    /// Check if agent exists
    pub fn contains(&self, agent_id: &str) -> bool {
        self.agents.contains_key(agent_id)
    }

    /// Get agent count
    pub fn len(&self) -> usize {
        self.agents.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }
}

impl Default for AgentServersConfig {
    fn default() -> Self {
        Self::new()
    }
}
