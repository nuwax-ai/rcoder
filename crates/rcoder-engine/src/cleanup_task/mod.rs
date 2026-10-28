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

/// Start this leadership generation's idle scanner.
///
/// Metadata retirement has no detached physical-deletion consumer. Each
/// generation owns only its scanner and can be joined and started again.
pub async fn start_cleanup_task(
    config: CleanupConfig,
    state: Arc<crate::app_state::AppState>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) -> anyhow::Result<tokio::task::JoinHandle<()>> {
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
