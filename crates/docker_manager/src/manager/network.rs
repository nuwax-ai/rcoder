//! 容器网络信息查询（从 manager.rs 拆出，extension-impl）。
//!
//! `get_container_network_info`：容器网络名称 → IP 映射（缓存优先，miss 经
//! Docker API inspect 回填）。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

use super::super::{DockerError, DockerManager, DockerResult};

impl DockerManager {
    /// - `Err(ConnectionError)`: 容器不存在或无法获取网络信息
    pub async fn get_container_network_info(
        &self,
        container_id: &str,
    ) -> DockerResult<HashMap<String, String>> {
        // 1. 尝试从缓存获取
        if let Some(Some(cached)) = self.api_cache.get_network(container_id).await {
            debug!(
                "[NETWORK] getting network info: container_id={}",
                container_id
            );
            // Arc::clone 只是增加引用计数，解引用后 clone HashMap
            return Ok((*cached).clone());
        }

        // 1.5 检查是否缓存了 None（空网络信息）
        if let Some(None) = self.api_cache.get_network(container_id).await {
            debug!(
                "[NETWORK] Cache hit (empty network): container_id={}",
                container_id
            );
            return Ok(HashMap::new());
        }

        // 2. 缓存未命中，调用 Docker API（带超时）
        let timeout = Duration::from_secs(self.config.api_timeout_quick_seconds);
        let inspect = match self.inspect_with_timeout(container_id, timeout).await {
            Ok(i) => i,
            Err(DockerError::BollardError(bollard::errors::Error::DockerResponseServerError {
                status_code: 404,
                ..
            })) => {
                // 容器不存在 - 缓存空 HashMap
                debug!(
                    "[NETWORK] Container does not exist, caching empty network: container_id={}",
                    container_id
                );
                self.api_cache
                    .insert_network(container_id.to_string(), None)
                    .await;
                return Ok(HashMap::new());
            }
            Err(DockerError::Timeout(_)) => {
                warn!(
                    "[NETWORK] Query timeout, trying to get from cache: container_id={}",
                    container_id
                );
                // 超时时，尝试返回缓存中的旧值（如果有的话）
                if let Some(Some(cached)) = self.api_cache.get_network(container_id).await {
                    return Ok((*cached).clone());
                }
                return Err(DockerError::Timeout(format!(
                    "Container network info query timeout and no available cache: container_id={}",
                    container_id
                )));
            }
            Err(e) => return Err(e),
        };

        // 3. 解析网络信息
        let mut network_ips = HashMap::new();

        if let Some(network_settings) = inspect.network_settings
            && let Some(networks) = network_settings.networks
        {
            for (network_name, network_info) in networks {
                if let Some(ip_address) = network_info.ip_address
                    && !ip_address.is_empty()
                {
                    network_ips.insert(network_name, ip_address);
                }
            }
        }

        // 4. 写入缓存（如果为空也缓存，避免重复查询）
        let result_to_cache = if network_ips.is_empty() {
            None
        } else {
            // 🔧 使用 Arc 包装，减少 clone 开销
            Some(Arc::new(network_ips.clone()))
        };
        self.api_cache
            .insert_network(container_id.to_string(), result_to_cache)
            .await;

        Ok(network_ips)
    }

    /// 检查容器健康状态
    pub(crate) async fn check_container_health(&self, container_id: &str) -> DockerResult<()> {
        use bollard::query_parameters::InspectContainerOptions;

        // 检查容器详细信息
        let inspect = self
            .docker
            .inspect_container(container_id, None::<InspectContainerOptions>)
            .await
            .map_err(|e| {
                DockerError::ConnectionError(format!("failed to check container status: {}", e))
            })?;

        // 检查容器状态
        if let Some(state) = inspect.state {
            let status = state.status;
            let exit_code = state.exit_code.unwrap_or(-1);

            match status {
                Some(bollard::models::ContainerStateStatusEnum::RUNNING) => {
                    info!("Container {} is running", container_id);
                    Ok(())
                }
                Some(bollard::models::ContainerStateStatusEnum::EXITED) => {
                    let error_msg = state.error.as_deref().unwrap_or("unknown error");
                    error!(
                        "Container {} exited (exit code: {}): {}",
                        container_id, exit_code, error_msg
                    );
                    Err(DockerError::ContainerStartError(format!(
                        "Container exited immediately after startup: {} (exit code: {}), error: {}",
                        container_id, exit_code, error_msg
                    )))
                }
                Some(bollard::models::ContainerStateStatusEnum::CREATED) => {
                    warn!("container {} already created but not started", container_id);
                    Err(DockerError::ContainerStartError(format!(
                        "Container created but not started: {}",
                        container_id
                    )))
                }
                Some(status) => {
                    let status_str = format!("{:?}", status);
                    error!(
                        "Container {} has unexpected status: {}",
                        container_id, status_str
                    );
                    Err(DockerError::ContainerStartError(format!(
                        "Container in unknown state: {} - {}",
                        container_id, status_str
                    )))
                }
                None => {
                    error!("Container {} status is empty", container_id);
                    Err(DockerError::ContainerStartError(format!(
                        "Container status is empty: {}",
                        container_id
                    )))
                }
            }
        } else {
            error!("Unable to get container {} status", container_id);
            Err(DockerError::ContainerStartError(format!(
                "unable to get container status info: {}",
                container_id
            )))
        }
    }
}
