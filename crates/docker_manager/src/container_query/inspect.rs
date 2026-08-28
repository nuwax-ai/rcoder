//! 容器查询原语：缓存查询 + 实时 Docker inspect + 模式列表。
//!
//! 从 container_query.rs 目录化拆出（extension-impl，方法体原样搬迁）。

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tracing::{debug, error, info, warn};

use crate::{
    ContainerQueryResult, ContainerStatus, DockerContainerInfo, DockerError, DockerManager,
    DockerResult,
};

impl DockerManager {
    // ========================================================================
    // Cache-based queries
    // ========================================================================

    /// 获取容器信息（从缓存）
    ///
    /// 从内存缓存中获取容器信息，速度快但可能不是最新的。
    /// 如果需要最新状态，请使用 `find_container_realtime`。
    ///
    /// # Arguments
    /// * `project_id` - 项目 ID
    ///
    /// # Returns
    /// 容器信息（如果缓存中存在）
    pub async fn get_container_info(&self, project_id: &str) -> Option<DockerContainerInfo> {
        self.containers.get(project_id).await
    }

    /// 清理容器缓存
    ///
    /// 从 DockerManager 的内存缓存中移除容器信息。
    /// 通常在容器被销毁后调用，以保持缓存与实际状态同步。
    pub async fn remove_container_cache(&self, project_id: &str) -> Option<DockerContainerInfo> {
        self.containers.remove(project_id).await
    }

    /// 获取所有容器信息（从缓存）
    pub async fn list_containers(&self) -> Vec<DockerContainerInfo> {
        self.containers.list().await
    }

    // ========================================================================
    // Realtime Docker API queries
    // ========================================================================

    /// 实时查询容器状态（使用缓存 + 超时保护）
    ///
    /// 此方法直接查询 Docker API 获取最新的容器状态（返回的 container_id 保证新鲜）。
    ///
    /// 🔧 优化：使用 Moka 缓存减少 Docker API 调用，使用超时保护防止阻塞
    /// 📝 缓存策略：同时缓存 container_id 和 container_name，支持 404 响应缓存
    ///
    /// # 参数
    /// * `identifier` - 容器名称或容器 ID
    ///
    /// # 返回
    /// * 如果找到容器，返回 `Some(ContainerQueryResult)`
    /// * 如果容器不存在，返回 `None`
    pub async fn find_container_realtime(
        &self,
        identifier: &str,
    ) -> DockerResult<Option<ContainerQueryResult>> {
        debug!(
            "[REALTIME] Getting container status: identifier={}",
            identifier
        );

        // 1. 尝试从缓存获取（只缓存成功结果，不缓存 404）
        if let Some(Some(cached)) = self.api_cache.get_status(identifier).await {
            debug!("[REALTIME] cache hit: identifier={}", identifier);
            // Arc::clone 只是增加引用计数，开销很小
            return Ok(Some((*cached).clone()));
        }

        // 2. 缓存未命中，调用 Docker API（带超时）
        let timeout = Duration::from_secs(self.config.api_timeout_quick_seconds);
        let result = match self.inspect_with_timeout(identifier, timeout).await {
            Ok(details) => {
                // 解析结果
                let container_id = details.id.unwrap_or_default();
                let container_name = details
                    .name
                    .map(|n| n.trim_start_matches('/').to_string())
                    .unwrap_or_else(|| identifier.to_string());

                let (status, is_running) = if let Some(state) = details.state {
                    if let Some(state_status) = state.status {
                        let is_running =
                            state_status == bollard::models::ContainerStateStatusEnum::RUNNING;
                        let status = ContainerStatus::from(state_status.to_string());
                        (status, is_running)
                    } else {
                        (ContainerStatus::Unknown("no status".to_string()), false)
                    }
                } else {
                    (ContainerStatus::Unknown("no state".to_string()), false)
                };

                // 🔧 获取容器 IP（用于 gRPC 连接）
                let container_ip = match self.get_container_network_info(&container_id).await {
                    Ok(network_ips) => {
                        let network_name = self.get_main_network_name().await;
                        network_ips
                            .get(&network_name)
                            .cloned()
                            .or_else(|| network_ips.values().next().cloned())
                            .unwrap_or_default()
                    }
                    Err(e) => {
                        warn!(
                            "[REALTIME] Failed to get container AP, will retry later: container_id={}, error={}",
                            container_id, e
                        );
                        String::new()
                    }
                };

                // 🔧 解析容器创建时间（从 Docker API 的 RFC3339 字符串）
                let created_at = details
                    .created
                    .as_deref()
                    .and_then(|s| Self::parse_rfc3339_timestamp(s, "realtime_created").ok())
                    .unwrap_or_else(|| {
                        warn!(
                            "[REALTIME] Failed to parse container created time, using current time: container_id={}",
                            container_id
                        );
                        Utc::now()
                    });

                // 🔧 使用 Arc 包装，减少 clone 开销
                let query_result = ContainerQueryResult::new(
                    container_id.clone(),
                    container_name.clone(),
                    status,
                    is_running,
                    container_ip,
                    created_at,
                );
                let result_arc = Arc::new(query_result);

                // 同时用 container_id 和 container_name 作为缓存 key
                // Arc::clone 只是增加引用计数，开销很小
                self.api_cache
                    .insert_status(container_id.clone(), Some(result_arc.clone()))
                    .await;
                self.api_cache
                    .insert_status(container_name.clone(), Some(result_arc.clone()))
                    .await;

                info!(
                    "[REALTIME] Container status query succeeded: id={}, name={}, status={:?}, running={}, ip={}",
                    container_id,
                    container_name,
                    result_arc.status,
                    result_arc.is_running,
                    result_arc.container_ip
                );

                // 返回解引用后的值（因为返回类型不是 Arc）
                Some((*result_arc).clone())
            }
            Err(DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })) => {
                // 🔧 修复：不再缓存 404 响应
                // 原因：容器可能刚被创建，缓存 404 会导致 SSE 连接时序问题
                // 容器状态变化快，404 缓存收益小但风险大
                // 同时清理 network_cache 避免残留旧 IP（容器重建后 IP 可能变化）
                self.api_cache.invalidate(identifier).await;
                debug!(
                    "[REALTIME] Container does not exist (not caching 404): identifier={}",
                    identifier
                );
                None
            }
            Err(DockerError::Timeout(_)) => {
                warn!(
                    "[REALTIME] Query timeout, trying to get from cache: identifier={}",
                    identifier
                );
                // 超时时，尝试返回缓存中的旧值（如果有的话）
                if let Some(Some(cached)) = self.api_cache.get_status(identifier).await {
                    return Ok(Some((*cached).clone()));
                }
                return Err(DockerError::Timeout(format!(
                    "Container status query timeout with no available cache: identifier={}",
                    identifier
                )));
            }
            Err(e) => {
                error!(
                    "[REALTIME] Query container status failed: identifier={}, error={}",
                    identifier, e
                );
                return Err(e);
            }
        };

        Ok(result)
    }

    /// 列出匹配指定模式的容器
    ///
    /// 使用 Docker API 列出所有容器（包括停止的），并根据名称模式过滤。
    /// 自动排除当前容器自身（通过 HOSTNAME 环境变量识别）。
    ///
    /// # Arguments
    /// * `pattern` - 容器名称匹配模式
    ///
    /// # Returns
    /// 匹配的容器列表
    pub async fn list_containers_with_pattern(
        &self,
        pattern: &str,
    ) -> DockerResult<Vec<bollard::models::ContainerSummary>> {
        info!("Listing containers: pattern={}", pattern);

        // 🎯 获取当前容器的 ID（用于排除自己）
        let current_container_id = std::env::var("HOSTNAME").ok();

        // 使用 Docker API 列出所有容器（包括停止的）
        let options = Some(bollard::query_parameters::ListContainersOptions {
            all: true,
            ..Default::default()
        });

        let containers = self.docker.list_containers(options).await.map_err(|e| {
            DockerError::ConnectionError(format!("failed to get container list: {}", e))
        })?;

        // 创建过滤器
        let filter = crate::ContainerFilter::name_pattern(pattern);

        let total_count = containers.len();
        // 过滤容器，排除当前容器自己（直接用 into_iter 消费，避免 clone）
        let matched_containers: Vec<bollard::models::ContainerSummary> = containers
            .into_iter()
            .filter(|container| {
                // 排除当前容器自己
                if let Some(ref current_id) = current_container_id
                    && let Some(ref container_id) = container.id
                {
                    // HOSTNAME 是容器 ID 的前 12 位
                    if container_id.starts_with(current_id) {
                        info!("skip removing container: {}", container_id);
                        return false;
                    }
                }
                filter.matches(container)
            })
            .collect();

        info!(
            "Container lookup completed: total={}, matched={} (self excluded), pattern={}",
            total_count,
            matched_containers.len(),
            pattern
        );

        Ok(matched_containers)
    }
}
