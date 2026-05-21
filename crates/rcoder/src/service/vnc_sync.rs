//! VNC 后端同步任务
//!
//! 定期从 Runtime 同步容器的 IP 到 Pingora 的 vnc_backends 映射。
//! 使用容器命名规则解析业务标识符：
//! - RCoder: `rcoder-agent-{project_id}`
//! - ComputerAgentRunner: `computer-agent-runner-{user_id}`
//!
//! 解决服务重启后 VNC 映射丢失的问题，并支持容器重启后自动更新 IP。
//!
//! 注意：本模块大部分由 binary (main.rs) 使用，lib 内仅 sync_single_vnc_backend
//! 被 pod_handler 引用。

#![allow(dead_code)]

use crate::handler::utils::container_identity_from_name;
use rcoder_proxy::PingoraProxyService;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};

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
    rcoder_prefix: String,
    computer_prefix: String,
) -> tokio::task::JoinHandle<()> {
    info!(
        "🔄 [VNC_SYNC] Starting VNC backend sync task: interval={}s",
        config.sync_interval.as_secs()
    );

    tokio::task::spawn(async move {
        let mut interval = tokio::time::interval(config.sync_interval);

        // 首次立即执行同步（用于服务启动时恢复映射）
        sync_vnc_backends(&pingora_service, &rcoder_prefix, &computer_prefix).await;

        loop {
            interval.tick().await;
            sync_vnc_backends(&pingora_service, &rcoder_prefix, &computer_prefix).await;
        }
    })
}

/// 同步 VNC 后端映射
async fn sync_vnc_backends(
    pingora_service: &Arc<PingoraProxyService>,
    rcoder_prefix: &str,
    computer_prefix: &str,
) {
    let runtime = match docker_manager::runtime::RuntimeManager::get().await {
        Ok(rt) => rt,
        Err(e) => {
            warn!("[VNC_SYNC] Failed to get runtime: {}", e);
            return;
        }
    };

    let containers = match runtime.list_containers().await {
        Ok(list) => list,
        Err(e) => {
            warn!("[VNC_SYNC] Failed to list containers from runtime: {}", e);
            return;
        }
    };
    if containers.is_empty() {
        debug!("[VNC_SYNC] Syncing containers");
        return;
    }

    // 预先收集运行中容器的 key 集合（用于后续清理旧映射）
    let active_user_ids: std::collections::HashSet<String> = containers
        .iter()
        .filter(|c| c.status == container_runtime_api::ContainerRuntimeStatus::Running)
        .filter_map(|c| {
            container_identity_from_name(&c.container_name, rcoder_prefix, computer_prefix)
                .map(|(identifier, _service_type)| identifier.to_string())
        })
        .filter(|k| !k.is_empty())
        .collect();

    let mut synced_count = 0;
    let mut updated_count = 0;

    for container_info in containers {
        let user_id = container_identity_from_name(
            &container_info.container_name,
            rcoder_prefix,
            computer_prefix,
        )
        .map(|(identifier, _service_type)| identifier.to_string())
        .unwrap_or_default();
        if user_id.is_empty() {
            debug!(
                "⏭️ [VNC_SYNC] Skipping container without business identifier: {}",
                container_info.container_name
            );
            continue;
        }

        // 检查容器是否在运行
        if container_info.status != container_runtime_api::ContainerRuntimeStatus::Running {
            debug!(
                "⏭️ [VNC_SYNC] Skipping non-running container: {} (status={})",
                container_info.container_name,
                String::from(container_info.status.clone())
            );
            continue;
        }

        let container_ip = container_info.container_ip.clone();
        if container_ip.is_empty() {
            warn!(
                "⚠️ [VNC_SYNC] Container {} has no IP address",
                container_info.container_name
            );
            continue;
        }

        // 检查是否需要更新映射
        let needs_update = match pingora_service.get_vnc_backend(&user_id) {
            Some(existing_ip) if existing_ip == container_ip => {
                // IP 没有变化，无需更新
                false
            }
            Some(existing_ip) => {
                // IP 变化了，需要更新
                debug!(
                    "🔄 [VNC_SYNC] Container IP changed: user_id={}, old={}, new={}",
                    user_id, existing_ip, container_ip
                );
                true
            }
            None => {
                // 新容器，需要添加
                debug!(
                    "➕ [VNC_SYNC] New container mapping: user_id={} -> {}",
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
            "🔄 [VNC_SYNC] Sync completed: checked={}, updated={}",
            synced_count, updated_count
        );
    } else if synced_count > 0 {
        debug!("[VNC_SYNC] Sync completed: synced={}", synced_count);
    }

    // === 清理已销毁容器的旧映射 ===
    // 获取当前所有 VNC 后端映射
    let current_backends = pingora_service.list_vnc_backends();

    let mut removed_count = 0;
    for user_id in current_backends.keys() {
        if !active_user_ids.contains(user_id) {
            pingora_service.remove_vnc_backend(user_id);
            removed_count += 1;
            debug!(
                "🗑️ [VNC_SYNC] Cleaning up already destroyed container mapping: user_id={}",
                user_id
            );
        }
    }

    if removed_count > 0 {
        info!(
            "🗑️ [VNC_SYNC] Cleanup completed: removed={} mappings",
            removed_count
        );
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
        "➕ [VNC_SYNC] Single container mapping updated: user_id={} -> {}",
        user_id, container_ip
    );
}
