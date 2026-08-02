//! ContainerCreateParams + Builder（创建容器/Deployment 的参数载体）

use shared_types::{ServiceResourceLimits, ServiceType};
use std::collections::HashMap;

use super::types::{AppHealthCheck, AppPortSpec, AppResourceRequirements};

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
    /// PVC storage size (K8s resource format, e.g., "10Gi", "100Mi")
    /// Only effective in K8s mode, Docker mode ignores this parameter
    pub storage_size: Option<String>,

    // ===== UserApp 专用字段（agent 路径不传，全 Option 向后兼容）=====
    /// 镜像覆盖（UserApp 必填，优先于 ServiceType 驱动的 select_image）
    pub image_override: Option<String>,
    /// 启动命令（UserApp 用，agent 路径由 ServiceType 决定）
    pub command: Option<Vec<String>>,
    /// 启动参数
    pub args: Option<Vec<String>>,
    /// 用户环境变量（额外注入；K8s 模式进 ConfigMap）
    pub env: Option<HashMap<String, String>>,
    /// 敏感环境变量（K8s 模式进 Secret，Docker 模式合并进 env）
    pub secrets: Option<HashMap<String, String>>,
    /// 端口配置（UserApp 用）
    pub ports: Option<Vec<AppPortSpec>>,
    /// 健康检查配置（UserApp 用）
    pub health_check: Option<AppHealthCheck>,
    /// 应用资源需求（字符串格式；与 resource_limits 二选一，UserApp 专用）
    pub app_resources: Option<AppResourceRequirements>,
    /// 是否参与闲置自动回收（UserApp 用；None/Some(true)=可回收=免费用户默认，Some(false)=永不回收=付费/常驻）
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（UserApp 用；None=用全局默认，Some=per-app 覆盖）
    pub idle_timeout_seconds: Option<u64>,
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
    storage_size: Option<String>,
    image_override: Option<String>,
    command: Option<Vec<String>>,
    args: Option<Vec<String>>,
    env: Option<HashMap<String, String>>,
    secrets: Option<HashMap<String, String>>,
    ports: Option<Vec<AppPortSpec>>,
    health_check: Option<AppHealthCheck>,
    app_resources: Option<AppResourceRequirements>,
    recycle_enabled: Option<bool>,
    idle_timeout_seconds: Option<u64>,
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

    pub fn storage_size(mut self, storage_size: impl Into<String>) -> Self {
        self.storage_size = Some(storage_size.into());
        self
    }

    pub fn image_override(mut self, image: impl Into<String>) -> Self {
        self.image_override = Some(image.into());
        self
    }

    pub fn command(mut self, command: Vec<String>) -> Self {
        self.command = Some(command);
        self
    }

    pub fn args(mut self, args: Vec<String>) -> Self {
        self.args = Some(args);
        self
    }

    pub fn env(mut self, env: HashMap<String, String>) -> Self {
        self.env = Some(env);
        self
    }

    pub fn secrets(mut self, secrets: HashMap<String, String>) -> Self {
        self.secrets = Some(secrets);
        self
    }

    pub fn ports(mut self, ports: Vec<AppPortSpec>) -> Self {
        self.ports = Some(ports);
        self
    }

    pub fn health_check(mut self, health_check: AppHealthCheck) -> Self {
        self.health_check = Some(health_check);
        self
    }

    pub fn app_resources(mut self, resources: AppResourceRequirements) -> Self {
        self.app_resources = Some(resources);
        self
    }

    pub fn recycle_enabled(mut self, recycle_enabled: bool) -> Self {
        self.recycle_enabled = Some(recycle_enabled);
        self
    }

    pub fn idle_timeout_seconds(mut self, idle_timeout_seconds: u64) -> Self {
        self.idle_timeout_seconds = Some(idle_timeout_seconds);
        self
    }

    pub fn build(self) -> ContainerCreateParams {
        ContainerCreateParams {
            project_id: self.project_id,
            user_id: self.user_id,
            host_workspace_path: self.host_workspace_path.unwrap_or_default(),
            service_type: self.service_type.unwrap_or(ServiceType::WebAgentRunner),
            resource_limits: self.resource_limits,
            pod_id: self.pod_id,
            isolation_type: self.isolation_type,
            tenant_id: self.tenant_id,
            space_id: self.space_id,
            storage_size: self.storage_size,
            image_override: self.image_override,
            command: self.command,
            args: self.args,
            env: self.env,
            secrets: self.secrets,
            ports: self.ports,
            health_check: self.health_check,
            app_resources: self.app_resources,
            recycle_enabled: self.recycle_enabled,
            idle_timeout_seconds: self.idle_timeout_seconds,
        }
    }
}
