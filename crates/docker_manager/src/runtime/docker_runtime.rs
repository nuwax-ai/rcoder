//! Docker runtime implementation
//!
//! This module provides `DockerRuntime` that wraps the existing `DockerManager`
//! and implements the `ContainerRuntime` trait.

use async_trait::async_trait;
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, RemovedContainerInfo, RuntimeContainerInfo,
};
use moka::future::Cache;
use shared_types::{ContainerBasicInfo, ServiceType};
use std::sync::Arc;
use std::time::Duration;

use crate::DockerManager;

/// Docker runtime implementation wrapping DockerManager
pub struct DockerRuntime {
    inner: Arc<DockerManager>,
    /// TTL cache for list_containers result (15 seconds)
    list_cache: Cache<(), Vec<RuntimeContainerInfo>>,
}

impl DockerRuntime {
    /// Create a new DockerRuntime wrapping the given DockerManager
    pub fn new(inner: Arc<DockerManager>) -> Self {
        Self {
            inner,
            list_cache: Cache::builder()
                .max_capacity(1)
                .time_to_live(Duration::from_secs(15))
                .build(),
        }
    }
}

#[async_trait]
impl ContainerRuntime for DockerRuntime {
    async fn create_container(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.inner
            .start_agent_container(params)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerCreationError(e.to_string()))
    }

    async fn get_container_info(
        &self,
        project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.inner
            .get_agent_info(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        match service_type {
            ServiceType::RCoder => self
                .inner
                .get_agent_info(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string())),
            // 使用 find_container 实时查询 Docker API 获取 IP，
            // 避免 get_user_container_info → get_agent_info → get_container_info 只查缓存
            // 导致服务重启后缓存丢失返回 None
            ServiceType::ComputerAgentRunner => {
                let result = self.find_container(identifier, service_type).await?;
                Ok(result.map(|pod| ContainerBasicInfo {
                    container_id: pod.container_id,
                    container_name: pod.container_name,
                    container_ip: pod.container_ip.clone(),
                    internal_port: shared_types::GRPC_DEFAULT_PORT,
                    external_port: 0,
                    project_id: identifier.to_string(),
                    status: String::from(pod.status),
                    created_at: pod.created_at,
                    service_url: format!(
                        "http://{}:{}",
                        pod.container_ip,
                        shared_types::GRPC_DEFAULT_PORT
                    ),
                }))
            }
        }
    }

    async fn find_container(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        let result = self
            .inner
            .find_project_container(project_id, service_type)
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?;

        Ok(result.map(|r| RuntimeContainerInfo {
            container_id: r.container_id,
            container_name: r.container_name,
            container_ip: r.container_ip,
            status: match r.status {
                crate::types::ContainerStatus::Running => ContainerRuntimeStatus::Running,
                crate::types::ContainerStatus::Stopped => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Creating => ContainerRuntimeStatus::Pending,
                crate::types::ContainerStatus::Restarting => ContainerRuntimeStatus::Pending,
                crate::types::ContainerStatus::Paused => {
                    ContainerRuntimeStatus::Unknown("paused".to_string())
                }
                crate::types::ContainerStatus::Dead => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Removing => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Exited => ContainerRuntimeStatus::Failed,
                crate::types::ContainerStatus::Unknown(s) => ContainerRuntimeStatus::Unknown(s),
            },
            created_at: chrono::Utc::now(),
        }))
    }

    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()> {
        self.inner
            .stop_container(project_id)
            .await
            .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))
    }

    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        match service_type {
            ServiceType::RCoder => self
                .inner
                .stop_container(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string())),
            ServiceType::ComputerAgentRunner => {
                if let Some(container) = self
                    .inner
                    .find_user_container(identifier, service_type)
                    .await
                    .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?
                {
                    self.inner
                        .stop_container_by_id(&container.container_id)
                        .await
                        .map_err(|e| ContainerRuntimeError::ContainerStopError(e.to_string()))?;
                }
                Ok(())
            }
        }
    }

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        if let Some(info) = self.get_container_info(project_id).await? {
            Ok(info.status == "running")
        } else {
            Ok(false)
        }
    }

    async fn is_container_running_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self
            .find_container(identifier, service_type)
            .await?
            .map(|c| c.status == ContainerRuntimeStatus::Running)
            .unwrap_or(false))
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        // 尝试从缓存获取
        if let Some(cached) = self.list_cache.get(&()).await {
            return Ok(cached);
        }

        // 缓存未命中或过期，fetch 并写入缓存
        let result = self.fetch_containers().await?;
        self.list_cache.insert((), result.clone()).await;
        Ok(result)
    }

    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.inner
            .sync_all_container_states()
            .await
            .map_err(|e| ContainerRuntimeError::DockerError(e.to_string()))
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.inner
            .cleanup_all_containers()
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        self.inner.get_docker_client().ping().await.map_err(|e| {
            ContainerRuntimeError::ConnectionError(format!("Docker ping failed: {}", e))
        })?;
        Ok(())
    }
}

impl DockerRuntime {
    /// Fetch containers from Docker API (used as cache loader)
    async fn fetch_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        let containers = self.inner.list_containers().await;
        let mut result = Vec::with_capacity(containers.len());
        for c in containers {
            let container_ip = self
                .inner
                .get_container_connection_info(&c)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))?
                .unwrap_or_default();

            result.push(RuntimeContainerInfo {
                container_id: c.container_id,
                container_name: c.container_name,
                container_ip,
                status: match c.status {
                    crate::types::ContainerStatus::Running => ContainerRuntimeStatus::Running,
                    crate::types::ContainerStatus::Stopped => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Creating => ContainerRuntimeStatus::Pending,
                    crate::types::ContainerStatus::Restarting => ContainerRuntimeStatus::Pending,
                    crate::types::ContainerStatus::Paused => {
                        ContainerRuntimeStatus::Unknown("paused".to_string())
                    }
                    crate::types::ContainerStatus::Dead => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Removing => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Exited => ContainerRuntimeStatus::Failed,
                    crate::types::ContainerStatus::Unknown(s) => ContainerRuntimeStatus::Unknown(s),
                },
                created_at: c.created_at,
            });
        }
        Ok(result)
    }
}
