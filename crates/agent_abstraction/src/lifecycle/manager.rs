//! Agent lifecycle manager implementation.

use std::sync::Arc;
use std::time::Duration;

use dashmap::DashMap;

use super::super::{AgentProcess, ConnectedAgent, ProcessLaunchInfo, ProcessStatus};
use super::AgentLifecycleError;
use agent_client_protocol::SessionId;

/// Agent status information
#[derive(Debug, Clone)]
pub struct AgentStatusInfo {
    pub status: shared_types::AgentStatus,
    pub session_id: Option<String>,
    pub request_id: Option<String>,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl AgentStatusInfo {
    /// Create a new agent status info
    pub fn new(
        status: shared_types::AgentStatus,
        session_id: Option<String>,
        request_id: Option<String>,
    ) -> Self {
        let now = chrono::Utc::now();
        Self {
            status,
            session_id,
            request_id,
            last_activity: now,
            created_at: now,
        }
    }

    /// Update activity timestamp
    pub fn update_activity(&mut self) {
        self.last_activity = chrono::Utc::now();
    }

    /// Update status
    pub fn update_status(&mut self, status: shared_types::AgentStatus) {
        self.status = status;
        self.update_activity();
    }

    /// Set session ID
    pub fn set_session_id(&mut self, session_id: Option<String>) {
        self.session_id = session_id;
        self.update_activity();
    }

    /// Set request ID
    pub fn set_request_id(&mut self, request_id: Option<String>) {
        self.request_id = request_id;
        self.update_activity();
    }
}

/// Agent idle status information
#[derive(Debug, Clone)]
pub struct AgentIdleStatus {
    pub is_idle: bool,
    pub current_status: shared_types::AgentStatus,
    pub last_activity: chrono::DateTime<chrono::Utc>,
    pub session_id: Option<String>,
    pub current_request_id: Option<String>,
    pub idle_duration: Duration,
}

impl AgentIdleStatus {
    /// Check if agent is idle
    pub fn is_idle(&self) -> bool {
        self.is_idle && self.current_status == shared_types::AgentStatus::Idle
    }

    /// Get idle duration
    pub fn idle_duration(&self) -> Duration {
        self.idle_duration
    }

    /// Update from status info
    pub fn from_status_info(status_info: &AgentStatusInfo) -> Self {
        let now = chrono::Utc::now();
        let idle_duration = if status_info.status == shared_types::AgentStatus::Idle {
            now.signed_duration_since(status_info.last_activity)
                .to_std()
                .unwrap_or_default()
        } else {
            Duration::from_secs(0)
        };

        Self {
            is_idle: status_info.status == shared_types::AgentStatus::Idle,
            current_status: status_info.status,
            last_activity: status_info.last_activity,
            session_id: status_info.session_id.clone(),
            current_request_id: status_info.request_id.clone(),
            idle_duration,
        }
    }
}

/// Agent lifecycle manager
#[derive(Debug)]
pub struct AgentLifecycleManager {
    /// Active agent processes
    processes: DashMap<String, Arc<ConnectedAgent>>,

    /// Agent status information
    agent_status_map: DashMap<String, AgentStatusInfo>,

    /// Saved launch configurations for restart
    launch_configs: DashMap<String, ProcessLaunchInfo>,
}

impl AgentLifecycleManager {
    /// Create a new lifecycle manager
    pub fn new() -> Self {
        Self {
            processes: DashMap::new(),
            agent_status_map: DashMap::new(),
            launch_configs: DashMap::new(),
        }
    }

    /// Spawn an agent process
    pub async fn spawn_agent(
        &self,
        info: ProcessLaunchInfo,
    ) -> Result<AgentProcess, AgentLifecycleError> {
        let agent_id = info.id.clone();

        // Check if agent already exists
        if self.processes.contains_key(&agent_id) {
            return Err(AgentLifecycleError::AlreadyExists(agent_id));
        }

        // Save launch config for potential restart
        self.launch_configs.insert(agent_id.clone(), info.clone());

        // Create spawn logic using tokio::process::Command
        let mut cmd = tokio::process::Command::new(&info.command);
        cmd.args(&info.args)
            .envs(&info.env)
            .current_dir(&info.working_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .map_err(|e| AgentLifecycleError::Process(format!("Failed to spawn: {}", e)))?;

        let process = AgentProcess::new(agent_id.clone(), child, info.config);

        Ok(process)
    }

    /// Register an agent
    pub async fn register_agent(
        &self,
        agent_id: String,
        connected_agent: Arc<ConnectedAgent>,
    ) -> Result<(), AgentLifecycleError> {
        // Insert into processes map
        self.processes
            .insert(agent_id.clone(), Arc::clone(&connected_agent));

        // Update status info
        let session_id_string = connected_agent
            .session_id
            .as_ref()
            .map(|sid| sid.0.to_string());
        let status_info =
            AgentStatusInfo::new(shared_types::AgentStatus::Active, session_id_string, None);
        self.agent_status_map.insert(agent_id, status_info);

        Ok(())
    }

    /// Start a new agent (legacy method)
    #[deprecated = "Start a new agent (legacy method)"]
    pub async fn start_agent(
        &self,
        _config: &(),
    ) -> Result<Arc<ConnectedAgent>, Box<dyn std::error::Error + Send + Sync>> {
        Err("Use spawn_agent instead".into())
    }

    /// Stop an agent by ID
    pub async fn stop_agent(&self, agent_id: &str) -> Result<(), AgentLifecycleError> {
        // Remove from processes map
        if let Some((_, agent)) = self.processes.remove(agent_id) {
            // Cancel and kill the process using ConnectedAgent methods
            let _ = agent.kill().await;
        }

        // Update status to Terminating/Stopped
        if let Some(mut status_info) = self.agent_status_map.get_mut(agent_id) {
            status_info.update_status(shared_types::AgentStatus::Terminating);
        }

        Ok(())
    }

    /// Stop an agent by session ID
    pub async fn stop_agent_by_session(
        &self,
        agent_id: &str,
        session_id: &SessionId,
    ) -> Result<(), AgentLifecycleError> {
        // Check if the agent exists and matches the session ID
        if let Some(process_entry) = self.processes.get(agent_id) {
            if let Some(ref entry_session_id) = process_entry.session_id {
                if entry_session_id == session_id {
                    return self.stop_agent(agent_id).await;
                }
            }
        }

        Err(AgentLifecycleError::NotFound(format!(
            "Agent {} with session {} not found",
            agent_id, session_id.0
        )))
    }

    /// Get agent status by session ID
    pub async fn get_agent_status(
        &self,
        agent_id: &str,
        session_id: &SessionId,
    ) -> Result<shared_types::AgentStatus, AgentLifecycleError> {
        // Check if the agent exists and matches the session ID
        if let Some(process_entry) = self.processes.get(agent_id) {
            if let Some(ref entry_session_id) = process_entry.session_id {
                if entry_session_id == session_id {
                    return Ok(shared_types::AgentStatus::Active);
                }
            }
        }

        Err(AgentLifecycleError::NotFound(format!(
            "Agent {} with session {} not found",
            agent_id, session_id.0
        )))
    }

    /// Restart an agent
    ///
    /// 停止现有 agent 并使用保存的配置重新启动进程。
    /// 注意：此方法只返回 `AgentProcess`，调用方需要自行建立 ACP 连接。
    pub async fn restart_agent(
        &self,
        agent_id: &str,
    ) -> Result<AgentProcess, AgentLifecycleError> {
        // Get saved launch config
        let launch_info = self
            .launch_configs
            .get(agent_id)
            .map(|r| r.value().clone())
            .ok_or_else(|| {
                AgentLifecycleError::NotFound(format!(
                    "No saved launch config for agent {}",
                    agent_id
                ))
            })?;

        // Stop the existing agent
        let _ = self.stop_agent(agent_id).await;

        // Wait a bit for cleanup
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Spawn new agent with saved config
        self.spawn_agent(launch_info).await
    }

    /// Get saved launch config for an agent
    pub fn get_launch_config(&self, agent_id: &str) -> Option<ProcessLaunchInfo> {
        self.launch_configs.get(agent_id).map(|r| r.value().clone())
    }

    /// Save launch config for an agent (used when registering externally launched agents)
    pub fn save_launch_config(&self, agent_id: String, config: ProcessLaunchInfo) {
        self.launch_configs.insert(agent_id, config);
    }

    /// Get process status
    pub fn get_process_status(&self, agent_id: &str) -> Option<ProcessStatus> {
        self.processes.get(agent_id).and_then(|agent| {
            // Try to check process status using ConnectedAgent's try_wait
            match agent.try_wait() {
                Ok(Some(exit_status)) => {
                    if exit_status.success() {
                        Some(ProcessStatus::Exited(0))
                    } else {
                        Some(ProcessStatus::Exited(exit_status.code().unwrap_or(-1)))
                    }
                }
                Ok(None) => Some(ProcessStatus::Running),
                Err(_) => Some(ProcessStatus::Unknown),
            }
        })
    }

    /// Get agent status info
    pub fn get_status_info(&self, agent_id: &str) -> Option<AgentStatusInfo> {
        self.agent_status_map
            .get(agent_id)
            .map(|status_info| status_info.clone())
    }

    /// Check if agent is idle
    pub fn is_agent_idle(&self, agent_id: &str) -> Option<bool> {
        self.agent_status_map
            .get(agent_id)
            .map(|status_info| status_info.status == shared_types::AgentStatus::Idle)
    }

    /// Get agent idle status
    pub fn get_agent_idle_status(&self, agent_id: &str) -> Option<AgentIdleStatus> {
        self.agent_status_map
            .get(agent_id)
            .map(|status_info| AgentIdleStatus::from_status_info(&status_info))
    }

    /// Set agent as active
    pub fn set_agent_active(
        &self,
        agent_id: &str,
        session_id: Option<String>,
        request_id: Option<String>,
    ) {
        if let Some(mut status_info) = self.agent_status_map.get_mut(agent_id) {
            status_info.update_status(shared_types::AgentStatus::Active);
            status_info.set_session_id(session_id);
            status_info.set_request_id(request_id);
        }
    }

    /// Set agent as idle
    pub fn set_agent_idle(&self, agent_id: &str) {
        if let Some(mut status_info) = self.agent_status_map.get_mut(agent_id) {
            status_info.update_status(shared_types::AgentStatus::Idle);
            status_info.set_request_id(None);
        }
    }

    /// Get all running agent IDs
    pub fn get_running_agents(&self) -> Vec<String> {
        self.processes
            .iter()
            .map(|entry| entry.key().clone())
            .collect()
    }

    /// Get agent count
    pub fn agent_count(&self) -> usize {
        self.processes.len()
    }
}

impl Default for AgentLifecycleManager {
    fn default() -> Self {
        Self::new()
    }
}
