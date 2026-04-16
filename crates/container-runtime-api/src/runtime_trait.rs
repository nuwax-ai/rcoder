//! Container runtime abstraction trait

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use shared_types::{ContainerBasicInfo, ServiceResourceLimits, ServiceType};
use thiserror::Error;

/// Container runtime errors
#[derive(Error, Debug)]
pub enum ContainerRuntimeError {
    #[error("Connection error: {0}")]
    ConnectionError(String),

    #[error("Container creation failed: {0}")]
    ContainerCreationError(String),

    #[error("Container start failed: {0}")]
    ContainerStartError(String),

    #[error("Container stop failed: {0}")]
    ContainerStopError(String),

    #[error("Container not found: {0}")]
    ContainerNotFound(String),

    #[error("Configuration error: {0}")]
    ConfigurationError(String),

    #[error("Timeout: {0}")]
    Timeout(String),

    #[error("Kubernetes error: {0}")]
    K8sError(String),

    #[error("Docker error: {0}")]
    DockerError(String),
}

/// Result type for container operations
pub type ContainerRuntimeResult<T> = Result<T, ContainerRuntimeError>;

/// Container runtime status
#[derive(Debug, Clone, PartialEq)]
pub enum ContainerRuntimeStatus {
    Pending,
    Running,
    Succeeded,
    Failed,
    Unknown(String),
}

/// Basic container info returned by runtime
#[derive(Debug, Clone)]
pub struct RuntimeContainerInfo {
    pub container_id: String,
    pub container_name: String,
    pub container_ip: String,
    pub status: ContainerRuntimeStatus,
    pub created_at: DateTime<Utc>,
}

impl From<ContainerRuntimeStatus> for String {
    fn from(status: ContainerRuntimeStatus) -> Self {
        match status {
            ContainerRuntimeStatus::Pending => "pending".to_string(),
            ContainerRuntimeStatus::Running => "running".to_string(),
            ContainerRuntimeStatus::Succeeded => "succeeded".to_string(),
            ContainerRuntimeStatus::Failed => "failed".to_string(),
            ContainerRuntimeStatus::Unknown(s) => s,
        }
    }
}

/// Abstraction trait for container runtimes (Docker, Kubernetes, etc.)
///
/// This trait follows the Interface Segregation Principle - it provides
/// a lean interface with only the methods that callers actually need.
#[async_trait]
pub trait ContainerRuntime: Send + Sync {
    /// Create and start a container
    async fn create_container(
        &self,
        project_id: Option<&str>,
        user_id: Option<&str>,
        host_workspace_path: &str,
        service_type: ServiceType,
        resource_limits: Option<ServiceResourceLimits>,
    ) -> ContainerRuntimeResult<ContainerBasicInfo>;

    /// Get container information by project_id
    async fn get_container_info(
        &self,
        project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>>;

    /// Get container information by identifier + service type.
    ///
    /// `identifier` means:
    /// - RCoder: `project_id`
    /// - ComputerAgentRunner: `user_id`
    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        if matches!(service_type, ServiceType::RCoder) {
            return self.get_container_info(identifier).await;
        }

        let info = self.find_container(identifier, service_type).await?;
        Ok(info.map(|pod| ContainerBasicInfo {
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

    /// Find container by project_id (returns None if not running)
    async fn find_container(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>>;

    /// Stop and remove container
    async fn stop_container(&self, project_id: &str) -> ContainerRuntimeResult<()>;

    /// Stop and remove container by identifier + service type.
    ///
    /// `identifier` means:
    /// - RCoder: `project_id`
    /// - ComputerAgentRunner: `user_id`
    async fn stop_container_by_identifier(
        &self,
        identifier: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<()> {
        self.stop_container(identifier).await
    }

    /// Get container status
    async fn is_container_running(&self, project_id: &str) -> ContainerRuntimeResult<bool>;

    /// Get container status by identifier + service type.
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

    /// List all containers managed by this runtime
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>>;

    /// Cleanup all containers (used on shutdown)
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()>;

    /// Health check - verify runtime is accessible
    async fn health_check(&self) -> ContainerRuntimeResult<()>;
}
