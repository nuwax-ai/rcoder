//! 容器状态同步任务
//!
//! 定期从 Docker 同步容器状态到内存缓存。
//! 主要用于检测被外部手动停止/删除的容器，并从缓存中清理。

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
        "🔄 [CONTAINER_SYNC] 启动容器状态同步任务: 间隔={}秒",
        config.sync_interval.as_secs()
    );

    tokio::task::spawn(async move {
        let mut interval = tokio::time::interval(config.sync_interval);

        loop {
            interval.tick().await;

            // 获取全局 DockerManager
            let docker_manager = match docker_manager::global::get_global_docker_manager().await {
                Ok(dm) => dm,
                Err(e) => {
                    warn!("[CONTAINER_SYNC] Failed to get DockerManager: {}", e);
                    continue;
                }
            };

            // 执行同步
            match docker_manager.sync_all_container_states().await {
                Ok((checked, removed)) => {
                    if removed > 0 {
                        info!(
                            "🔄 [CONTAINER_SYNC] 同步完成: 检查={}, 移除={}",
                            checked, removed
                        );
                    } else {
 info!("[CONTAINER_SYNC] message completed: check={}, message removed", checked);
                    }
                }
                Err(e) => {
 warn!("[CONTAINER_SYNC] message failed: {}", e);
                }
            }
        }
    })
}
