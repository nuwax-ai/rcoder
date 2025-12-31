//! ComputerAgentRunner 清理策略
//!
//! ComputerAgentRunner 模式: 1容器 = N项目，需要引用计数检查
//!
//! 核心修复：只有当容器的所有项目都闲置时才销毁容器

use super::{CleanupContext, CleanupStrategy, ProjectInfo};
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
    ) -> Result<bool> {
        // 获取 user_id
        let user_id = context
            .state
            .get_project(project_id)
            .and_then(|p| p.user_id().map(|s| s.to_string()))
            .ok_or_else(|| anyhow::anyhow!("无法获取 user_id: {}", project_id))?;

        // 查询该用户的所有项目
        let related_projects = context.state.projects.find_projects_by_user_id(&user_id);

        // 检查是否还有其他活跃项目（排除当前项目）
        let has_active_refs = related_projects
            .iter()
            .any(|p| p.project_id != project_id && is_project_active(p, &context.config));

        if has_active_refs {
            info!(
                "🛡️ [cleanup] 容器还被其他项目使用，只删除项目记录: project_id={}, user_id={}",
                project_id, user_id
            );
            Ok(false) // 不销毁容器
        } else {
            info!(
                "🔥 [cleanup] 容器所有项目都已闲置，可以销毁: project_id={}, user_id={}",
                project_id, user_id
            );
            Ok(true) // 销毁容器
        }
    }

    fn get_container_identifier(&self, project_info: &ProjectInfo) -> Result<String> {
        // ComputerAgentRunner: 容器标识符是 user_id
        project_info
            .user_id
            .clone()
            .ok_or_else(|| anyhow::anyhow!("user_id 缺失"))
    }

    fn name(&self) -> &str {
        "ComputerAgentRunner"
    }
}

/// 判断项目是否活跃
///
/// 根据 active_window 判断项目是否在活跃时间窗口内
pub fn is_project_active(
    project: &ProjectRecord,
    config: &crate::cleanup_task::config::CleanupConfig,
) -> bool {
    let now = Utc::now();
    let idle_duration = now - project.last_activity;
    idle_duration.num_seconds() < config.active_window.as_secs() as i64
}
