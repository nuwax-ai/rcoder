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
    let docker_manager = docker_manager::global::get_global_docker_manager()
        .await
        .map_err(|e| {
            tracing::error!(
                "🚨 [CLEANUP_TASK] Failed to get DockerManager: {}，清理任务无法启动",
                e
            );
            anyhow::anyhow!("Failed to get DockerManager: {}", e)
        })?;

    let pingora_service = state.pingora_service.clone();

    let mut cleaner = AgentCleaner::new(config, state, docker_manager, pingora_service);

    Ok(tokio::task::spawn(async move {
        cleaner.run().await;
    }))
}
