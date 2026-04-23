//! Docker runtime implementation
//!
//! This module provides `DockerRuntime` that wraps the existing `DockerManager`
//! and implements the `ContainerRuntime` trait.

use async_trait::async_trait;
use container_runtime_api::{
    ContainerCreateParams, ContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, RuntimeContainerInfo,
};
use shared_types::{ContainerBasicInfo, ServiceType};
use std::sync::Arc;

use crate::DockerManager;

/// Docker runtime implementation wrapping DockerManager
pub struct DockerRuntime {
    inner: Arc<DockerManager>,
}

impl DockerRuntime {
    /// Create a new DockerRuntime wrapping the given DockerManager
    pub fn new(inner: Arc<DockerManager>) -> Self {
        Self { inner }
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
            ServiceType::ComputerAgentRunner => self
                .inner
                .get_user_container_info(identifier)
                .await
                .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string())),
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
                crate::types::ContainerStatus::Unknown(s) => {
                    ContainerRuntimeStatus::Unknown(s)
                }
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
                    crate::types::ContainerStatus::Unknown(s) => {
                        ContainerRuntimeStatus::Unknown(s)
                    }
                },
                created_at: c.created_at,
            });
        }
        Ok(result)
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.inner
            .cleanup_all_containers()
            .await
            .map_err(|e| ContainerRuntimeError::ConnectionError(e.to_string()))
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        self.inner
            .get_docker_client()
            .ping()
            .await
            .map_err(|e| {
                ContainerRuntimeError::ConnectionError(format!("Docker ping failed: {}", e))
            })?;
        Ok(())
    }
}
