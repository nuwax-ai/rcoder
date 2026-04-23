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

/// Parameters for creating a container
///
/// Bundles all parameters needed for container creation to avoid
/// long parameter lists that hurt code readability and maintainability.
#[derive(Debug, Clone)]
pub struct ContainerCreateParams {
    /// Project identifier (used as container name base for RCoder service)
    pub project_id: Option<String>,
    /// User identifier (used as container name base for ComputerAgentRunner)
    pub user_id: Option<String>,
    /// Workspace path on host
    pub host_workspace_path: String,
    /// Service type determining container purpose
    pub service_type: ServiceType,
    /// Optional resource constraints
    pub resource_limits: Option<ServiceResourceLimits>,
    /// Pod identifier for container reuse (for multi-tenant scenarios)
    pub pod_id: Option<String>,
    /// Isolation type: tenant|space|project (for multi-tenant scenarios)
    pub isolation_type: Option<String>,
    /// Tenant identifier (for multi-tenant scenarios)
    pub tenant_id: Option<String>,
    /// Space identifier (for multi-tenant scenarios)
    pub space_id: Option<String>,
}

impl ContainerCreateParams {
    /// Create a new builder for container create params
    pub fn builder() -> ContainerCreateParamsBuilder {
        ContainerCreateParamsBuilder::default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContainerCreateParamsBuilder {
    project_id: Option<String>,
    user_id: Option<String>,
    host_workspace_path: Option<String>,
    service_type: Option<ServiceType>,
    resource_limits: Option<ServiceResourceLimits>,
    pod_id: Option<String>,
    isolation_type: Option<String>,
    tenant_id: Option<String>,
    space_id: Option<String>,
}

impl ContainerCreateParamsBuilder {
    pub fn project_id(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
        self
    }

    pub fn host_workspace_path(mut self, host_workspace_path: impl Into<String>) -> Self {
        self.host_workspace_path = Some(host_workspace_path.into());
        self
    }

    pub fn service_type(mut self, service_type: ServiceType) -> Self {
        self.service_type = Some(service_type);
        self
    }

    pub fn resource_limits(mut self, resource_limits: ServiceResourceLimits) -> Self {
        self.resource_limits = Some(resource_limits);
        self
    }

    pub fn pod_id(mut self, pod_id: impl Into<String>) -> Self {
        self.pod_id = Some(pod_id.into());
        self
    }

    pub fn isolation_type(mut self, isolation_type: impl Into<String>) -> Self {
        self.isolation_type = Some(isolation_type.into());
        self
    }

    pub fn tenant_id(mut self, tenant_id: impl Into<String>) -> Self {
        self.tenant_id = Some(tenant_id.into());
        self
    }

    pub fn space_id(mut self, space_id: impl Into<String>) -> Self {
        self.space_id = Some(space_id.into());
        self
    }

    pub fn build(self) -> ContainerCreateParams {
        ContainerCreateParams {
            project_id: self.project_id,
            user_id: self.user_id,
            host_workspace_path: self
                .host_workspace_path
                .unwrap_or_else(|| String::new()),
            service_type: self.service_type.unwrap_or(ServiceType::RCoder),
            resource_limits: self.resource_limits,
            pod_id: self.pod_id,
            isolation_type: self.isolation_type,
            tenant_id: self.tenant_id,
            space_id: self.space_id,
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
        params: ContainerCreateParams,
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
