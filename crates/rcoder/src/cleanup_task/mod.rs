//! 清理任务模块
//!
//! 重构后的清理任务，修复 ComputerAgentRunner 引用计数问题并模块化拆分

use std::sync::Arc;

pub mod agent;
pub mod cleaner;
pub mod config;
pub mod container;
pub mod logs;
pub mod storage;
pub mod strategies;

// 集成测试
#[cfg(test)]
mod integration_tests;

pub use cleaner::AgentCleaner;
pub use config::CleanupConfig;
#[allow(unused_imports)] // CleanupStats 用于类型导出
pub use config::CleanupStats;

/// 启动清理任务
///
/// # Errors
/// 如果Failed to get DockerManager，返回错误而不是静默失败
pub async fn start_cleanup_task(
    config: CleanupConfig,
    state: Arc<crate::router::AppState>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
    let docker_manager = match docker_manager::global::get_global_docker_manager().await {
        Ok(dm) => Some(dm),
        Err(e) => {
            if matches!(
                docker_manager::runtime::RuntimeManager::runtime_type(),
                docker_manager::runtime_selection::RuntimeType::Kubernetes
            ) {
                tracing::warn!(
                    "⚠️ [CLEANUP_TASK] DockerManager unavailable in Kubernetes mode, starting lightweight cleanup task: {}",
                    e
                );
                None
            } else {
                tracing::error!(
                    "🚨 [CLEANUP_TASK] Failed to get DockerManager: {}, cleanup task cannot start",
                    e
                );
                return Err(anyhow::anyhow!("Failed to get DockerManager: {}", e));
            }
        }
    };

    if docker_manager.is_none() {
        let state_for_k8s = state.clone();
        return Ok(tokio::task::spawn(async move {
            let mut interval = tokio::time::interval(config.cleanup_interval);
            loop {
                interval.tick().await;
                let runtime = match docker_manager::runtime::RuntimeManager::get().await {
                    Ok(rt) => rt,
                    Err(e) => {
                        tracing::warn!("[CLEANUP_TASK] failed to get runtime: {}", e);
                        continue;
                    }
                };
                let idle_threshold = match chrono::Duration::from_std(config.idle_timeout) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!("[CLEANUP_TASK] invalid idle_timeout config: {}", e);
                        continue;
                    }
                };
                let now = chrono::Utc::now();
                let projects: Vec<(String, Arc<shared_types::ProjectAndContainerInfo>)> =
                    state_for_k8s.projects.iter().collect();

                for (project_id, project_info) in projects {
                    let idle = now.signed_duration_since(project_info.last_activity());
                    if idle < idle_threshold {
                        continue;
                    }

                    let service_type = project_info
                        .service_type()
                        .unwrap_or(shared_types::ServiceType::RCoder);
                    let identifier = match service_type {
                        shared_types::ServiceType::ComputerAgentRunner => project_info
                            .user_id()
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| project_id.clone()),
                        shared_types::ServiceType::RCoder => project_id.clone(),
                    };

                    if let Err(e) = runtime
                        .stop_container_by_identifier(&identifier, &service_type)
                        .await
                    {
                        tracing::warn!(
                            "[CLEANUP_TASK] failed to stop runtime container: identifier={}, service_type={:?}, error={}",
                            identifier,
                            service_type,
                            e
                        );
                        continue;
                    }

                    state_for_k8s.remove_project(&project_id);
                    tracing::info!(
                        "[CLEANUP_TASK] cleaned idle runtime container: project_id={}, identifier={}, service_type={:?}",
                        project_id,
                        identifier,
                        service_type
                    );
                }
            }
        }));
    }

    let pingora_service = state.pingora_service.clone();

    let mut cleaner = AgentCleaner::new(
        config,
        state,
        docker_manager.expect("docker_manager checked above"),
        pingora_service,
    );

    Ok(tokio::task::spawn(async move {
        cleaner.run().await;
    }))
}
