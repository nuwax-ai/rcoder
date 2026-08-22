//! 容器状态对账（从 manager.rs 拆出，extension-impl）。
//!
//! `sync_all_container_states`：遍历缓存容器逐一校验 Docker 真实状态，
//! 外部删除的清缓存 + 运行中容器做服务健康检查（HTTP + gRPC）。

use std::sync::Arc;

use container_runtime_api::RemovedContainerInfo;
use shared_types::ServiceType;
use tracing::{debug, info, warn};

use super::super::{ContainerStatus, DockerContainerInfo, DockerManager, DockerResult};

impl DockerManager {
    /// 遍历缓存中的所有容器，调用 Docker API 检查其真实状态。
    /// 如果容器已被外部删除（如手动 `docker stop`），则从缓存中移除。
    /// 🆕 对运行中的容器执行服务健康检查（HTTP + gRPC）
    ///
    /// # Returns
    /// 返回元组 (已检查数量, 已移除容器信息列表)
    pub async fn sync_all_container_states(
        &self,
    ) -> DockerResult<(u32, Vec<RemovedContainerInfo>)> {
        // 获取所有 project_id 的快照
        let project_ids: Vec<String> = self.containers.keys().await;

        if project_ids.is_empty() {
            return Ok((0, Vec::new()));
        }

        let total = project_ids.len() as u32;
        let mut removed = Vec::new();
        let mut health_checked_count = 0u32;

        // 创建健康检查器（复用同一个实例）
        let health_checker = Arc::new(crate::health::ServiceHealthChecker::new());

        // 提前获取主网络名称（避免每个并发任务重复获取 RwLock）
        let main_network_name = self.get_main_network_name().await;

        // 🚀 并发批处理：每批最多 10 个容器
        let batch_size = 10;
        for chunk in project_ids.chunks(batch_size) {
            let futures: Vec<_> = chunk
                .iter()
                .map(|project_id| {
                    let health_checker = health_checker.clone();
                    let main_network_name = main_network_name.clone();
                    self.sync_single_container_state(project_id, health_checker, main_network_name)
                })
                .collect();

            // 并发执行当前批次
            let results = futures_util::future::join_all(futures).await;
            for (project_id, removed_info, health_checked) in results.into_iter().flatten() {
                if let Some(info) = removed_info {
                    info!(
                        "[SYNC] Container removed from cache (does not exist in Docker): project_id={}",
                        project_id
                    );
                    removed.push(info);
                }
                if health_checked {
                    health_checked_count += 1;
                }
            }
        }

        if !removed.is_empty() || health_checked_count > 0 {
            info!(
                "[SYNC] Container status sync completed: checked={}, removed={}, health_checked={}",
                total,
                removed.len(),
                health_checked_count
            );
        }

        // 清理被移除容器的 API 缓存（避免残留数据导致后续查询返回旧 IP）
        if !removed.is_empty() {
            self.invalidate_cache_for_removed_containers(&removed).await;
        }

        Ok((total, removed))
    }

    /// 同步单个容器的状态（`sync_all_container_states` 的内部步骤）
    ///
    /// 返回 (project_id, 被移除容器的信息, 是否执行了健康检查)；
    /// 检查状态失败时返回 None。
    async fn sync_single_container_state(
        &self,
        project_id: &str,
        health_checker: Arc<crate::health::ServiceHealthChecker>,
        main_network_name: String,
    ) -> Option<(String, Option<RemovedContainerInfo>, bool)> {
        let project_id = project_id.to_string();
        let container_info_before_update = self.containers.get(&project_id).await;

        match self.update_container_status(&project_id).await {
            Ok(None) => {
                // 容器不存在，需要从缓存中移除
                if let Some(info) = container_info_before_update {
                    let removed_info = self.build_removed_container_info(&project_id, &info).await;
                    Some((project_id, Some(removed_info), false))
                } else {
                    Some((project_id, None, false))
                }
            }
            Ok(Some(status)) => {
                // 🆕 对运行中的容器执行服务健康检查
                if matches!(status, ContainerStatus::Running)
                    && self
                        .health_check_running_container(
                            &project_id,
                            &health_checker,
                            &main_network_name,
                        )
                        .await
                {
                    return Some((project_id, None, true));
                }
                Some((project_id, None, false))
            }
            Err(e) => {
                warn!(
                    "[SYNC] Check container status failed: project_id={}, error={}",
                    project_id, e
                );
                None
            }
        }
    }

    /// 收集被外部移除容器的信息（含获取容器 IP，用于清理 gRPC 连接池）
    async fn build_removed_container_info(
        &self,
        project_id: &str,
        info: &DockerContainerInfo,
    ) -> RemovedContainerInfo {
        let container_ip = match self.get_container_network_info(&info.container_id).await {
            Ok(ips) => ips.values().next().cloned().unwrap_or_default(),
            Err(e) => {
                warn!(
                    "[SYNC] Failed to get container IP for cleanup: container_id={}, error={}",
                    info.container_id, e
                );
                String::new()
            }
        };

        RemovedContainerInfo {
            container_name: info.container_name.clone(),
            container_ip,
            identifier: project_id.to_string(),
            service_type: info
                .service_type
                .clone()
                .unwrap_or(ServiceType::WebAgentRunner),
        }
    }

    /// 对运行中的容器执行服务健康检查并写回缓存
    ///
    /// 返回是否实际执行了健康检查（获取到 IP 并完成检查）
    async fn health_check_running_container(
        &self,
        project_id: &str,
        health_checker: &crate::health::ServiceHealthChecker,
        main_network_name: &str,
    ) -> bool {
        let container_info = self.containers.get(project_id).await;
        let Some(container_info) = container_info else {
            return false;
        };

        // 获取容器 IP
        let Ok(network_ips) = self
            .get_container_network_info(&container_info.container_id)
            .await
        else {
            return false;
        };

        let container_ip = network_ips
            .get(main_network_name)
            .or_else(|| network_ips.values().next());

        let Some(ip) = container_ip else {
            return false;
        };

        let previous_failures = container_info
            .service_health
            .as_ref()
            .map(|h| h.consecutive_failures)
            .unwrap_or(0);

        let health_status = health_checker.check_service(ip, previous_failures).await;

        let mut updated_info = container_info.clone();
        updated_info.service_health = Some(health_status.clone());
        self.containers
            .insert(project_id.to_string(), updated_info)
            .await;

        if health_status.is_fully_healthy() {
            debug!(
                "[SYNC] Service healthy: container_id={}, service_type={:?}",
                project_id, container_info.service_type
            );
        } else {
            warn!(
                "[SYNC] Service unhealthy: container_id={}, service_type={:?}, http={}, grpc={}, failures={}",
                project_id,
                container_info.service_type,
                health_status.http_healthy,
                health_status.grpc_healthy,
                health_status.consecutive_failures
            );
        }

        true
    }

    /// 清理被移除容器的 API 缓存
    async fn invalidate_cache_for_removed_containers(&self, removed: &[RemovedContainerInfo]) {
        let identifiers: Vec<String> = removed.iter().map(|r| r.identifier.clone()).collect();
        self.api_cache.invalidate_all(&identifiers).await;
        debug!(
            "[SYNC] Invalidated API cache for {} removed containers",
            identifiers.len()
        );
    }
}
