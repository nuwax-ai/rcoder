//! RCoder 清理策略
//!
//! RCoder 模式: 1容器 = 1项目，直接销毁

use super::{CleanupContext, CleanupStrategy, DestroyReason, ProjectInfo};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;

/// RCoder 清理策略
///
/// 在 RCoder 模式中，每个项目对应一个独立的容器
/// 清理时可以直接销毁容器，无需检查其他项目
pub struct RCoderStrategy;

#[async_trait]
impl CleanupStrategy for RCoderStrategy {
    async fn should_destroy_container(
        &self,
        project_id: &str,
        context: &CleanupContext,
    ) -> Result<Option<DestroyReason>> {
        // RCoder: 1容器=1项目，始终销毁容器
        let project = context
            .state
            .get_project(project_id)
            .ok_or_else(|| anyhow::anyhow!("Project does not exist: {}", project_id))?;

        let now = Utc::now();
        let idle_duration = (now - project.last_activity()).num_seconds();
        let timeout_secs = context.config.idle_timeout.as_secs();

        Ok(Some(DestroyReason::IdleTimeout {
            idle_duration_secs: idle_duration,
            timeout_secs,
        }))
    }

    fn get_container_identifier(&self, project_info: &ProjectInfo) -> Result<String> {
        // RCoder: 容器标识符就是 project_id
        Ok(project_info.project_id.clone())
    }
}
