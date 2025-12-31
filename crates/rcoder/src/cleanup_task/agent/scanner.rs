//! Agent 扫描器
//!
//! 扫描并识别需要清理的闲置 agent

use crate::AgentStatus;
use anyhow::Result;
use chrono::Utc;
use std::sync::Arc;
use tracing::{debug, info};

/// Agent 扫描器
pub struct AgentScanner {
    pub state: Arc<crate::router::AppState>,
    pub config: crate::cleanup_task::config::CleanupConfig,
}

impl AgentScanner {
    pub fn new(
        state: Arc<crate::router::AppState>,
        config: crate::cleanup_task::config::CleanupConfig,
    ) -> Self {
        Self { state, config }
    }

    /// 扫描需要清理的 agent
    pub async fn scan_idle_agents(&self) -> Result<Vec<String>> {
        let mut idle_agents = Vec::new();
        let current_time = Utc::now();

        info!("🔍 [scanner] 开始扫描闲置 agent");

        // 收集所有项目 ID
        let project_ids: Vec<String> = self.state.projects.iter().map(|(id, _)| id).collect();

        for project_id in project_ids {
            if let Some(agent) = self.state.get_project(&project_id) {
                if self.should_cleanup_agent(&agent, current_time).await {
                    idle_agents.push(project_id);
                }
            }
        }

        info!(
            "🎯 [scanner] 扫描完成: 发现 {} 个闲置 agent",
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
                debug!("✅ [scanner] 状态=Idle: {}", agent.project_id());
            }
            Some(AgentStatus::Pending) | Some(AgentStatus::Active) => {
                debug!("⏸️ [scanner] 跳过活跃状态: {:?}", status);
                return false;
            }
            None => {
                // 状态为 None，检查保护期
                let age = current_time - agent.created_at();
                if age.num_seconds() < self.config.container_protection_duration.as_secs() as i64 {
                    debug!("⏸️ [scanner] 状态=None 且在保护期内");
                    return false;
                }
            }
            Some(AgentStatus::Terminating) => {
                return false;
            }
        }

        // 超时检查
        let idle_duration = current_time - agent.last_activity();
        let is_timeout = idle_duration
            > chrono::Duration::from_std(self.config.idle_timeout).unwrap_or_default();

        if !is_timeout {
            return false;
        }

        // 保护期检查
        if self.should_skip_cleanup_due_to_protection(agent.created_at(), &agent.project_id()) {
            return false;
        }

        // TODO: gRPC 二次确认（调用 status_checker）
        // 如果有容器信息，可以进行 gRPC 查询

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
                "🛡️ [scanner] 容器在保护期内，跳过清理: project_id={}, 创建时长={}秒",
                project_id,
                age.num_seconds()
            );
            return true;
        }

        false
    }
}
