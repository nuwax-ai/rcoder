//! ComputerAgentRunner 清理策略
//!
//! ComputerAgentRunner 模式: 1容器 = N项目，需要引用计数检查
//!
//! 核心修复：只有当容器的所有项目都闲置时才销毁容器

use super::{CleanupContext, CleanupStrategy, DestroyReason, ProjectInfo};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use duckdb_manager::ProjectRecord;
use tracing::info;

/// ComputerAgentRunner 清理策略
///
/// 在 ComputerAgentRunner 模式中，一个用户对应一个容器
/// 该容器可能被多个 project_id 共享
///
/// 清理时需要检查：
/// 1. 获取 user_id
/// 2. 查询该用户的所有项目
/// 3. 检查是否还有其他活跃项目
/// 4. 只有所有项目都闲置时才销毁容器
pub struct ComputerRunnerStrategy;

#[async_trait]
impl CleanupStrategy for ComputerRunnerStrategy {
    async fn should_destroy_container(
        &self,
        project_id: &str,
        context: &CleanupContext,
    ) -> Result<Option<DestroyReason>> {
        // 获取 user_id
        let user_id = context
            .state
            .get_project(project_id)
            .and_then(|p| p.user_id().map(|s| s.to_string()))
            .ok_or_else(|| anyhow::anyhow!("Failed to get user_id: {}", project_id))?;

        // 查询该用户的所有项目
        let related_projects = context.state.projects.find_projects_by_user_id(&user_id);

        // 检查是否还有其他活跃项目（排除当前项目）
        let has_active_refs = related_projects
            .iter()
            .any(|p| p.project_id != project_id && is_project_active(p, &context.config));

        if has_active_refs {
            // 还有其他活跃项目，不销毁容器
            info!(
                "🛡️ [cleanup] 容器还被其他项目使用，只删除项目记录: project_id={}, user_id={}",
                project_id, user_id
            );
            Ok(None)
        } else {
            // 所有项目都闲置，计算最大闲置时间
            let now = Utc::now();
            let max_idle_duration = related_projects
                .iter()
                .map(|p| (now - p.last_activity).num_seconds())
                .max()
                .unwrap_or(0);

            let timeout_secs = context.config.idle_timeout.as_secs();

            info!(
                "🔥 [cleanup] 容器所有项目都已闲置，可以销毁: project_id={}, user_id={}",
                project_id, user_id
            );

            Ok(Some(DestroyReason::IdleTimeout {
                idle_duration_secs: max_idle_duration,
                timeout_secs,
            }))
        }
    }

    fn get_container_identifier(&self, project_info: &ProjectInfo) -> Result<String> {
        // ComputerAgentRunner: 容器标识符是 user_id
        project_info
            .user_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("user_id is missing"))
    }
}

/// 判断项目是否活跃
///
/// 使用 idle_timeout 作为判断标准：如果项目的闲置时间小于 idle_timeout，
/// 则认为项目仍然活跃，不应销毁其关联的容器。
/// 这与 scanner 的 idle 判断标准一致，避免出现 scanner 认为项目未超时
/// 但策略却认为项目不活跃的矛盾情况。
pub fn is_project_active(
    project: &ProjectRecord,
    config: &crate::cleanup_task::config::CleanupConfig,
) -> bool {
    let now = Utc::now();
    let idle_duration = now - project.last_activity;
    idle_duration.num_seconds() < config.idle_timeout.as_secs() as i64
}
