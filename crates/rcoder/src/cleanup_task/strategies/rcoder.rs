//! RCoder 清理策略
//!
//! RCoder 模式支持两种容器关系：
//! - 无 pod_id: 1容器 = 1项目，直接销毁
//! - 有 pod_id: N容器 = 1项目（共享容器），需要引用计数检查

use super::{CleanupContext, CleanupStrategy, DestroyReason, ProjectInfo};
use anyhow::Result;
use async_trait::async_trait;
use chrono::Utc;
use duckdb_manager::ProjectRecord;
use tracing::info;

/// RCoder 清理策略
///
/// 在 RCoder 模式中：
/// - 当 pod_id 为空时，每个项目对应一个独立的容器，清理时直接销毁
/// - 当 pod_id 有值时，多个项目共享同一个容器，需要检查其他项目是否仍然活跃
pub struct RCoderStrategy;

#[async_trait]
impl CleanupStrategy for RCoderStrategy {
    async fn should_destroy_container(
        &self,
        project_id: &str,
        context: &CleanupContext,
    ) -> Result<Option<DestroyReason>> {
        let project = context
            .state
            .get_project(project_id)
            .ok_or_else(|| anyhow::anyhow!("Project does not exist: {}", project_id))?;

        let now = Utc::now();
        let idle_duration = (now - project.last_activity()).num_seconds();
        let timeout_secs = context.config.idle_timeout.as_secs();

        // 检查是否有 pod_id（共享容器模式）
        let effective_idle_duration = if let Some(pod_id) = project.pod_id() {
            // 共享容器模式：检查同 pod_id 下是否有其他活跃项目
            let related_projects = context.state.projects.find_projects_by_pod_id(pod_id);

            let has_active_refs = related_projects.iter().any(|p| {
                p.project_id != project_id
                    && super::computer_runner::is_project_active(p, &context.config)
            });

            if has_active_refs {
                info!(
                    "🛡️ [cleanup] RCoder 容器还被其他项目使用，只删除项目记录: project_id={}, pod_id={}",
                    project_id, pod_id
                );
                return Ok(None);
            }

            // 计算所有相关项目的最大闲置时间（与 ComputerRunnerStrategy 一致）
            let max_idle = compute_max_idle_duration(&related_projects);

            info!(
                "🔥 [cleanup] RCoder 共享容器所有项目都已闲置，可以销毁: project_id={}, pod_id={}, max_idle={}s",
                project_id, pod_id, max_idle
            );

            max_idle
        } else {
            idle_duration
        };

        Ok(Some(DestroyReason::IdleTimeout {
            idle_duration_secs: effective_idle_duration,
            timeout_secs,
        }))
    }

    fn get_container_identifier(&self, project_info: &ProjectInfo) -> Result<String> {
        // RCoder: 有 pod_id 时使用 pod_id（共享容器），否则使用 project_id
        Ok(project_info
            .pod_id
            .clone()
            .unwrap_or_else(|| project_info.project_id.clone()))
    }
}

/// 计算一组项目的最大闲置时间（秒）
fn compute_max_idle_duration(projects: &[ProjectRecord]) -> i64 {
    let now = Utc::now();
    projects
        .iter()
        .map(|p| (now - p.last_activity).num_seconds())
        .max()
        .unwrap_or(0)
}
