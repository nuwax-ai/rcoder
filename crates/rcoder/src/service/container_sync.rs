//! 容器状态同步任务
//!
//! 定期从运行时同步容器状态。
//! 在 K8s 模式下通过 Runtime API 获取 Pod 状态；在 Docker 模式下同样通过 Runtime 抽象层访问。

use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

use crate::grpc::{ContainerIpCache, GrpcChannelPool};

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
/// 定期调用 `ContainerRuntime::sync_states()` 方法，
/// 检查缓存中的容器是否仍然存在于运行时（Docker/K8s）中，
/// 如果不存在则从缓存中移除。
///
/// 同时清理已移除容器的关联资源：
/// - gRPC 连接池中的旧连接
/// - 容器 IP 缓存
pub fn start_container_sync_task(
    config: ContainerSyncConfig,
    grpc_pool: Arc<GrpcChannelPool>,
    container_ip_cache: Arc<ContainerIpCache>,
) -> tokio::task::JoinHandle<()> {
    info!(
        "🔄 [CONTAINER_SYNC] Starting container state sync task: interval={}s",
        config.sync_interval.as_secs()
    );

    tokio::task::spawn(async move {
        let mut interval = tokio::time::interval(config.sync_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

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

            // 同步缓存状态 - 清理失效的容器记录
            debug!("[CONTAINER_SYNC] Syncing container states...");
            match runtime.sync_states().await {
                Ok((checked, removed)) => {
                    if !removed.is_empty() {
                        info!(
                            "[CONTAINER_SYNC] Sync completed: checked={}, removed_stale={}",
                            checked,
                            removed.len()
                        );

                        // 清理关联资源
                        for container in removed {
                            // 清理 gRPC 连接池
                            if !container.container_ip.is_empty() {
                                let grpc_addr = format!(
                                    "{}:{}",
                                    container.container_ip,
                                    shared_types::GRPC_DEFAULT_PORT
                                );
                                grpc_pool.remove(&grpc_addr);
                            }
                            // 使 IP 缓存失效
                            container_ip_cache.invalidate(&container.container_name);
                        }
                    }
                }
                Err(e) => {
                    warn!("[CONTAINER_SYNC] sync failed: {}", e);
                }
            }
        }
    })
}
