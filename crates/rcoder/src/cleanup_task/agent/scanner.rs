//! Agent 扫描器
//!
//! 扫描并识别需要清理的闲置 agent

use crate::AgentStatus;
use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;
use tracing::{debug, info, warn};

/// Agent 扫描器
pub struct AgentScanner {
    pub state: Arc<crate::router::AppState>,
    pub config: crate::cleanup_task::config::CleanupConfig,
    pub status_checker: super::AgentStatusChecker,
}

impl AgentScanner {
    pub fn new(
        state: Arc<crate::router::AppState>,
        config: crate::cleanup_task::config::CleanupConfig,
    ) -> Self {
        use crate::cleanup_task::agent::AgentStatusChecker;
        let status_checker = AgentStatusChecker::new(state.grpc_pool.clone());
        Self {
            state,
            config,
            status_checker,
        }
    }

    /// 扫描需要清理的 agent
    pub async fn scan_idle_agents(&self) -> Result<Vec<String>> {
        let mut idle_agents = Vec::new();
        let current_time = Utc::now();

        info!("[scanner] Starting agent scan");

        // 收集所有项目 ID
        let project_ids: Vec<String> = self.state.projects.iter().map(|(id, _)| id).collect();

        for project_id in project_ids {
            if let Some(agent) = self.state.get_project(&project_id)
                && self.should_cleanup_agent(&agent, current_time).await {
                    idle_agents.push(project_id);
                }
        }

        info!(
            "🎯 [scanner] Scan completed: found {} idle agents",
            idle_agents.len()
        );
        Ok(idle_agents)
    }

    async fn should_cleanup_agent(
        &self,
        agent: &shared_types::ProjectAndContainerInfo,
        current_time: chrono::DateTime<Utc>,
    ) -> bool {
        // 状态检查
        let status = agent.status();
        match status {
            Some(AgentStatus::Idle) => {
                debug!("[scanner] status=Idle: {}", agent.project_id());
            }
            Some(AgentStatus::Pending) | Some(AgentStatus::Active) => {
                // 🔧 修复：即使是 Active/Pending 状态，也要检查是否真的活跃
                // 如果状态卡住（比如 gRPC 服务异常），仍需要清理
                debug!("⏸️ [scanner] status={:?}, checking", status);
                // 继续检查，不要直接返回 false
            }
            None => {
                // 状态为 None，检查保护期
                let age = current_time - agent.created_at();
                if age.num_seconds() < self.config.container_protection_duration.as_secs() as i64 {
                    debug!("⏸️ [scanner] status=None, in protection period");
                    return false;
                }
            }
            Some(AgentStatus::Terminating) => {
                // 🔧 修复：Terminating 状态不应该持续很久（最多 30 秒）
                // 如果长时间停留在 Terminating，说明操作卡住了，应该清理
                let terminating_duration = current_time - agent.last_activity();
                let terminating_stuck_secs = terminating_duration.num_seconds();
                let max_terminating_secs = 30; // docker_stop_timeout 默认 30 秒

                if terminating_stuck_secs > max_terminating_secs {
                    warn!(
                        "⚠️ [scanner] Terminating status stuck for more than {} seconds, forcing cleanup: project_id={}, stuck_duration={}s",
                        max_terminating_secs,
                        agent.project_id(),
                        terminating_stuck_secs
                    );
                    // 继续检查，不要返回 false
                } else {
                    debug!("⏸️ [scanner] status=Terminating, waiting...");
                    return false;
                }
            }
        }

        // 超时检查
        let idle_duration = current_time - agent.last_activity();
        let is_timeout = idle_duration
            > chrono::Duration::from_std(self.config.idle_timeout).unwrap_or_default();

        if !is_timeout {
            // 未超时，但如果状态是 Active/Pending，仍需要通过 gRPC 确认
            if matches!(
                status,
                Some(AgentStatus::Active) | Some(AgentStatus::Pending)
            ) {
                debug!(
                    "⏸️ [scanner] Not timeout, status active, skip: {:?}",
                    status
                );
                return false;
            }
            return false;
        }

        // 保护期检查
        if self.should_skip_cleanup_due_to_protection(agent.created_at(), agent.project_id()) {
            return false;
        }

        // 🆕 gRPC 二次确认：查询容器内 agent 的真实状态
        if let Some(container) = agent.container() {
            // 从 service_url 提取 gRPC 地址
            let grpc_addr = match crate::handler::utils::extract_grpc_addr_with_port(
                &container.service_url,
                shared_types::GRPC_DEFAULT_PORT,
            ) {
                Ok(addr) => addr,
                Err(e) => {
                    debug!(
                        "⚠️ [scanner] Failed to parse gRPC address: project_id={}, error={}",
                        agent.project_id(),
                        e
                    );
                    return true; // 解析失败，允许清理
                }
            };

            let project_id = agent.project_id();
            // 根据 service_type 获取正确的容器标识符
            // - RCoder: 使用 project_id
            // - ComputerAgentRunner: 使用 user_id（如果存在），否则使用 project_id
            let user_id = agent.user_id().unwrap_or(project_id);

            match self
                .status_checker
                .is_container_active(&grpc_addr, user_id, project_id)
                .await
            {
                Ok(true) => {
                    info!(
                        "🔄 [scanner] gRPC secondary confirmation: agent in container is still active, skipping cleanup: project_id={}, user_id={}",
                        project_id, user_id
                    );
                    return false;
                }
                Ok(false) => {
                    debug!(
                        "💤 [scanner] gRPC secondary confirmation: agent in container is idle, can cleanup: project_id={}, user_id={}",
                        project_id, user_id
                    );
                }
                Err(e) => {
                    debug!(
                        "⚠️ [scanner] gRPC secondary confirmation failed, allowing cleanup: project_id={}, user_id={}, error={}",
                        project_id, user_id, e
                    );
                }
            }
        }

        true
    }

    fn should_skip_cleanup_due_to_protection(
        &self,
        created_at: chrono::DateTime<Utc>,
        project_id: &str,
    ) -> bool {
        let current_time = Utc::now();
        let age = current_time.signed_duration_since(created_at);

        if age.num_seconds() < self.config.container_protection_duration.as_secs() as i64 {
            info!(
                "🛡️ [scanner] Container in protection period, skipping cleanup: project_id={}, age={}s",
                project_id,
                age.num_seconds()
            );
            return true;
        }

        false
    }
}
