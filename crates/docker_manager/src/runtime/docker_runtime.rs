//! Docker runtime implementation
//!
//! This module provides `DockerRuntime` that wraps the existing `DockerManager`
//! and implements the `ContainerRuntime` trait.

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use container_runtime_api::{
    ContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
    RuntimeContainerInfo,
};
use shared_types::{ContainerBasicInfo, ServiceResourceLimits, ServiceType};
use std::sync::Arc;
use tracing::warn;

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
        project_id: Option<&str>,
        user_id: Option<&str>,
        host_workspace_path: &str,
        service_type: ServiceType,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.inner
            .start_agent_container(
                project_id,
                user_id,
                host_workspace_path,
                service_type,
                resource_limits,
            )
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

    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool> {
        if let Some(info) = self.get_container_info(project_id).await? {
            Ok(info.status == "running")
        } else {
            Ok(false)
        }
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        // TODO: Implement list_containers for Docker runtime
        warn!("[DockerRuntime] list_containers not yet implemented");
        Ok(Vec::new())
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
