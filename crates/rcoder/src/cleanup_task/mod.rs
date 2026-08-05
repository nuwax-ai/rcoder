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
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
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

    // 启动 ResourceReaper（消费 cleanup_rx，处理 RAII 容器销毁请求）
    {
        let reaper_rx = match state.cleanup_rx.lock() {
            Ok(mut guard) => guard.take(),
            Err(e) => {
                tracing::error!("[REAPER] cleanup_rx mutex poisoned: {}", e);
                return Err(anyhow::anyhow!("cleanup_rx mutex poisoned: {}", e));
            }
        };

        let reaper_rx = match reaper_rx {
            Some(rx) => rx,
            None => {
                tracing::error!(
                    "[REAPER] cleanup_rx already consumed, ResourceReaper can only be started once"
                );
                return Err(anyhow::anyhow!("ResourceReaper can only be started once"));
            }
        };

        let reaper = crate::storage::ResourceReaper::new(
            reaper_rx,
            state.runtime().clone(),
            state.grpc_pool.clone(),
            state.pingora_service.clone(),
            docker_manager.clone(),
            state.projects.cleanup_tx(),
        );
        tokio::spawn(reaper.run());
        tracing::info!(
            "[REAPER] ResourceReaper started (docker_manager={})",
            docker_manager.is_some()
        );
    }

    // K8s 与 Docker 统一走下面的 AgentCleaner 公共清理逻辑 (引用计数 + 两分支), 不再有专门的
    // K8s 裸循环 —— 旧 K8s 循环无引用计数, 会因单个 project idle 连坐销毁整个 user 容器
    // (prod 实测 490 次)。AgentCleaner 内部 destroyer 经 ContainerRuntime trait 物理销毁,
    // Docker / K8s 各自正确语义 (K8s 删 Pod+Service 并保留 PVC)。
    let pingora_service = state.pingora_service.clone();

    // AgentCleaner 运行时无关 (destroyer 走 ContainerRuntime trait),
    // Docker / K8s 都从这里复用同一套清理逻辑。
    let mut cleaner = AgentCleaner::new(config, state, pingora_service);

    let shutdown_rx = shutdown_tx.subscribe();
    Ok(tokio::task::spawn(async move {
        cleaner.run(shutdown_rx).await;
    }))
}
