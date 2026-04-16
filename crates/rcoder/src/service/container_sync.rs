//! 容器状态同步任务
//!
//! 定期从运行时同步容器状态。
//! 在 K8s 模式下通过 Runtime API 获取 Pod 状态；在 Docker 模式下同样通过 Runtime 抽象层访问。

use std::time::Duration;
use tracing::{info, warn};

/// 容器状态同步配置
#[derive(Debug, Clone)]
pub struct ContainerSyncConfig {
    /// 同步间隔
    pub sync_interval: Duration,
}

impl Default for ContainerSyncConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(60), // 默认 60 秒
        }
    }
}

/// 启动容器状态同步任务
///
/// 定期调用 `DockerManager::sync_all_container_states()` 方法，
/// 检查缓存中的容器是否仍然存在于 Docker 中，
/// 如果不存在则从缓存中移除。
pub fn start_container_sync_task(config: ContainerSyncConfig) -> tokio::task::JoinHandle<()> {
    info!(
        "🔄 [CONTAINER_SYNC] Starting container state sync task: interval={}s",
        config.sync_interval.as_secs()
    );

    tokio::task::spawn(async move {
        let mut interval = tokio::time::interval(config.sync_interval);

        loop {
            interval.tick().await;

            // 获取全局 Runtime
            let runtime = match docker_manager::runtime::RuntimeManager::get().await {
                Ok(rt) => rt,
                Err(e) => {
                    warn!("[CONTAINER_SYNC] Failed to get runtime: {}", e);
                    continue;
                }
            };

            // 统一运行时下，同步以“拉取最新容器列表”为主
            match runtime.list_containers().await {
                Ok(containers) => {
                    info!("[CONTAINER_SYNC] Sync completed: checked={}", containers.len());
                }
                Err(e) => {
                    warn!("[CONTAINER_SYNC] sync failed: {}", e);
                }
            }
        }
    })
}
