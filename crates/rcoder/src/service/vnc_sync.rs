//! VNC 后端同步任务
//!
//! 定期从 DockerManager 同步容器的 IP 到 Pingora 的 vnc_backends 映射。
//! 使用 `DockerContainerInfo.container_key()` 获取正确的业务标识符：
//! - RCoder 模式: 使用 `project_id`
//! - ComputerAgentRunner 模式: 使用 `user_id`
//! 解决服务重启后 VNC 映射丢失的问题，并支持容器重启后自动更新 IP。

use rcoder_proxy::PingoraProxyService;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// VNC 后端同步配置
#[derive(Debug, Clone)]
pub struct VncSyncConfig {
    /// 同步间隔
    pub sync_interval: Duration,
}

impl Default for VncSyncConfig {
    fn default() -> Self {
        Self {
            sync_interval: Duration::from_secs(5), // 默认 5 秒
        }
    }
}

/// 启动 VNC 后端同步任务
///
/// 定期扫描所有容器，使用 `container_key()` 获取正确的业务标识符，
/// 将 user_id/project_id -> container_ip 映射同步到 Pingora 的 vnc_backends。
///
/// 这解决了两个问题：
/// 1. 服务重启后 vnc_backends 丢失
/// 2. 容器重启后 IP 变化
pub fn start_vnc_sync_task(
    pingora_service: Arc<PingoraProxyService>,
    config: VncSyncConfig,
) -> tokio::task::JoinHandle<()> {
    info!(
        "🔄 [VNC_SYNC] 启动 VNC 后端同步任务: 间隔={}秒",
        config.sync_interval.as_secs()
    );

    tokio::task::spawn(async move {
        let mut interval = tokio::time::interval(config.sync_interval);

        // 首次立即执行同步（用于服务启动时恢复映射）
        sync_vnc_backends(&pingora_service).await;

        loop {
            interval.tick().await;
            sync_vnc_backends(&pingora_service).await;
        }
    })
}

/// 同步 VNC 后端映射
async fn sync_vnc_backends(pingora_service: &Arc<PingoraProxyService>) {
    let docker_manager = match docker_manager::global::get_global_docker_manager().await {
        Ok(dm) => dm,
        Err(e) => {
            warn!("[VNC_SYNC] Failed to get DockerManager: {}", e);
            return;
        }
    };

    // 获取所有容器（DockerManager 已经管理了所有容器的元数据）
    let containers = docker_manager.list_containers().await;
    if containers.is_empty() {
 debug!("[VNC_SYNC] message container");
        return;
    }

    // 预先收集运行中容器的 user_id 集合（用于后续清理旧映射）
    let active_user_ids: std::collections::HashSet<String> = containers
        .iter()
        .filter(|c| c.status.to_string().to_lowercase() == "running")
        .map(|c| c.container_key().to_string())
        .filter(|k| !k.is_empty())
        .collect();

    let mut synced_count = 0;
    let mut updated_count = 0;

    for container_info in containers {
        // 使用 container_key() 获取正确的业务标识符
        // - RCoder 模式: 返回 project_id
        // - ComputerAgentRunner 模式: 返回 user_id（如果有），否则回退到 project_id
        let user_id = container_info.container_key();
        if user_id.is_empty() {
            debug!(
                "⏭️ [VNC_SYNC] 跳过无业务标识的容器: {}",
                container_info.container_name
            );
            continue;
        }

        // 检查容器是否在运行
        if container_info.status.to_string().to_lowercase() != "running" {
            debug!(
                "⏭️ [VNC_SYNC] 跳过非运行状态容器: {} (status={})",
                container_info.container_name,
                container_info.status.to_string()
            );
            continue;
        }

        // 获取容器 IP
        let container_ip = match docker_manager
            .get_container_connection_info(&container_info)
            .await
        {
            Ok(Some(ip)) => ip,
            Ok(None) => {
                warn!(
                    "⚠️ [VNC_SYNC] 容器 {} 没有 IP 地址",
                    container_info.container_name
                );
                continue;
            }
            Err(e) => {
                error!(
                    "❌ [VNC_SYNC] 获取容器 {} IP 失败: {}",
                    container_info.container_name, e
                );
                continue;
            }
        };

        // 检查是否需要更新映射
        let needs_update = match pingora_service.get_vnc_backend(&user_id) {
            Some(existing_ip) if existing_ip == container_ip => {
                // IP 没有变化，无需更新
                false
            }
            Some(existing_ip) => {
                // IP 变化了，需要更新
                debug!(
                    "🔄 [VNC_SYNC] 容器 IP 变化: user_id={}, old={}, new={}",
                    user_id, existing_ip, container_ip
                );
                true
            }
            None => {
                // 新容器，需要添加
                debug!(
                    "➕ [VNC_SYNC] 新容器映射: user_id={} -> {}",
                    user_id, container_ip
                );
                true
            }
        };

        if needs_update {
            pingora_service.add_vnc_backend(&user_id, &container_ip);
            updated_count += 1;
        }
        synced_count += 1;
    }

    if updated_count > 0 {
        info!(
            "🔄 [VNC_SYNC] 同步完成: 检查={}, 更新={}",
            synced_count, updated_count
        );
    } else if synced_count > 0 {
 debug!("[VNC_SYNC] message completed: check={}, message updated", synced_count);
    }

    // === 清理已销毁容器的旧映射 ===
    // 获取当前所有 VNC 后端映射
    let current_backends = pingora_service.list_vnc_backends();

    let mut removed_count = 0;
    for user_id in current_backends.keys() {
        if !active_user_ids.contains(user_id) {
            pingora_service.remove_vnc_backend(user_id);
            removed_count += 1;
 debug!("🗑️ [VNC_SYNC] cleanupalreadydestroycontainer message mapping: user_id={}", user_id);
        }
    }

    if removed_count > 0 {
 info!("🗑️ [VNC_SYNC] cleanupcompleted: removed={} message mapping", removed_count);
    }
}

/// 同步单个容器的 VNC 后端映射
///
/// 用于在创建容器时立即更新映射，不需要等待定时任务
pub async fn sync_single_vnc_backend(
    pingora_service: &Arc<PingoraProxyService>,
    user_id: &str,
    container_ip: &str,
) {
    pingora_service.add_vnc_backend(user_id, container_ip);
    debug!(
        "➕ [VNC_SYNC] 单容器映射更新: user_id={} -> {}",
        user_id, container_ip
    );
}
