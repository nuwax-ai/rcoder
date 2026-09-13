//! ContainerCreateParams + Builder（创建容器/Deployment 的参数载体）

use shared_types::{ServiceResourceLimits, ServiceType};
use std::collections::HashMap;

use super::types::{AppHealthCheck, AppPortSpec, AppResourceRequirements};

/// Parameters for creating a container
///
/// Bundles all parameters needed for container creation to avoid
/// long parameter lists that hurt code readability and maintainability.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ContainerCreateParams {
    /// Durable userApp operation identity; absent for Agent operations.
    pub execution_context: Option<shared_types::UserAppExecutionContext>,
    /// Physical update target captured before any application mutation.
    pub mutation_target: Option<shared_types::UserAppMutationTarget>,
    /// Explicit durable proof for an adopted legacy builder resource.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_binding: Option<shared_types::UserAppResourceBinding>,
    /// Project identifier (used as container name base for RCoder service)
    pub project_id: Option<String>,
    /// User identifier (used as container name base for ComputerAgentRunner)
    pub user_id: Option<String>,
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

    // ===== Userapp 专用字段（agent 路径不传，全 Option 向后兼容）=====
    /// 镜像覆盖（Userapp 必填，优先于 ServiceType 驱动的 select_image）
    pub image_override: Option<String>,
    /// 启动命令（Userapp 用，agent 路径由 ServiceType 决定）
    pub command: Option<Vec<String>>,
    /// 启动参数
    pub args: Option<Vec<String>>,
    /// 用户环境变量（额外注入；K8s 模式进 ConfigMap）
    pub env: Option<HashMap<String, String>>,
    /// 敏感环境变量（K8s 模式进 Secret，Docker 模式合并进 env）
    pub secrets: Option<HashMap<String, String>>,
    /// 端口配置（Userapp 用）
    pub ports: Option<Vec<AppPortSpec>>,
    /// 健康检查配置（Userapp 用）
    pub health_check: Option<AppHealthCheck>,
    /// 应用资源需求（字符串格式；与 resource_limits 二选一，Userapp 专用）
    pub app_resources: Option<AppResourceRequirements>,
    /// 是否参与闲置自动回收（Userapp 用；None/Some(true)=可回收=免费用户默认，Some(false)=永不回收=付费/常驻）
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（Userapp 用；None=用全局默认，Some=per-app 覆盖）
    pub idle_timeout_seconds: Option<u64>,
}

impl ContainerCreateParams {
    pub fn validate_execution_context(&self) -> super::types::ContainerRuntimeResult<()> {
        let Some(context) = &self.execution_context else {
            if self.mutation_target.is_some() || self.resource_binding.is_some() {
                return Err(super::types::ContainerRuntimeError::ConfigurationError(
                    "Captured mutation target or binding requires execution context".into(),
                ));
            }
            return Ok(());
        };
        if let Some(binding) = &self.resource_binding {
            if self.service_type != ServiceType::UserappBuilder {
                return Err(super::types::ContainerRuntimeError::ConfigurationError(
                    "Physical builder binding requires builder service family".into(),
                ));
            }
            binding
                .validate(context, &binding.physical_uid)
                .map_err(super::types::ContainerRuntimeError::ConfigurationError)?;
        }
        if let Some(target) = &self.mutation_target
            && (&target.context != context
                || target.resource.uid.is_empty()
                || target.resource.name.is_empty())
        {
            return Err(super::types::ContainerRuntimeError::ConfigurationError(
                "Captured mutation target does not belong to this operation".into(),
            ));
        }
        if !matches!(
            self.service_type,
            ServiceType::Userapp | ServiceType::UserappBuilder
        ) {
            return Err(super::types::ContainerRuntimeError::ConfigurationError(
                "Application context requires UserApp service family".into(),
            ));
        }
        let identifier = self
            .service_type
            .container_identifier(
                self.pod_id.as_deref(),
                self.user_id.as_deref(),
                self.project_id.as_deref(),
            )
            .map_err(|error| {
                super::types::ContainerRuntimeError::ConfigurationError(error.to_string())
            })?;
        context
            .validate_identity(identifier, self.user_id.as_deref())
            .map_err(super::types::ContainerRuntimeError::ConfigurationError)
    }

    /// Create a new builder for container create params
    pub fn builder() -> ContainerCreateParamsBuilder {
        ContainerCreateParamsBuilder::default()
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContainerCreateParamsBuilder {
    execution_context: Option<shared_types::UserAppExecutionContext>,
    resource_binding: Option<shared_types::UserAppResourceBinding>,
    project_id: Option<String>,
    user_id: Option<String>,
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
    pub fn resource_binding(mut self, binding: shared_types::UserAppResourceBinding) -> Self {
        self.resource_binding = Some(binding);
        self
    }
    pub fn execution_context(mut self, context: shared_types::UserAppExecutionContext) -> Self {
        self.execution_context = Some(context);
        self
    }
    pub fn project_id(mut self, project_id: impl Into<String>) -> Self {
        self.project_id = Some(project_id.into());
        self
    }

    pub fn user_id(mut self, user_id: impl Into<String>) -> Self {
        self.user_id = Some(user_id.into());
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
            execution_context: self.execution_context,
            mutation_target: None,
            resource_binding: self.resource_binding,
            project_id: self.project_id,
            user_id: self.user_id,
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

#[cfg(test)]
mod mutation_target_tests {
    use super::*;
    #[test]
    fn captured_target_requires_the_exact_admitted_operation() {
        let context = shared_types::UserAppExecutionContext {
            app_id: "app-one".into(),
            user_id: "owner-one".into(),
            lifecycle_id: "life-one".into(),
            operation_id: "update-one".into(),
            executor_id: "executor-one".into(),
            request_fingerprint: "a".repeat(64),
        };
        let mut params = ContainerCreateParams::builder()
            .project_id("app-one")
            .user_id("owner-one")
            .service_type(ServiceType::Userapp)
            .execution_context(context.clone())
            .build();
        params.mutation_target = Some(shared_types::UserAppMutationTarget {
            context,
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Deployment,
                name: "rcoder-app-one".into(),
                uid: "original-uid".into(),
                resource_version: Some("12".into()),
            },
        });
        params.validate_execution_context().expect("same operation");
        let mut foreign = params.clone();
        foreign
            .mutation_target
            .as_mut()
            .expect("target")
            .context
            .operation_id = "other-operation".into();
        assert!(foreign.validate_execution_context().is_err());
        let mut missing = params.clone();
        missing.execution_context = None;
        assert!(missing.validate_execution_context().is_err());
        params
            .mutation_target
            .as_mut()
            .expect("target")
            .resource
            .uid
            .clear();
        assert!(params.validate_execution_context().is_err());
    }
    #[test]
    fn adopted_binding_requires_builder_context_and_current_lifecycle() {
        let context = shared_types::UserAppExecutionContext {
            app_id: "app".into(),
            user_id: "owner".into(),
            lifecycle_id: "life".into(),
            operation_id: "wake".into(),
            executor_id: "worker".into(),
            request_fingerprint: "a".repeat(64),
        };
        let binding = shared_types::UserAppResourceBinding {
            app_id: "app".into(),
            user_id: "owner".into(),
            lifecycle_id: "life".into(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "physical-original".into(),
            adopted_by_operation: "adopt".into(),
        };
        let mut params = ContainerCreateParams::builder()
            .project_id("app")
            .user_id("owner")
            .service_type(ServiceType::UserappBuilder)
            .resource_binding(binding)
            .build();
        assert!(params.validate_execution_context().is_err());
        params.execution_context = Some(context);
        params.validate_execution_context().expect("bound builder");
        params.service_type = ServiceType::Userapp;
        assert!(params.validate_execution_context().is_err());
        params.service_type = ServiceType::UserappBuilder;
        params
            .resource_binding
            .as_mut()
            .expect("binding")
            .lifecycle_id = "obsolete-life".into();
        assert!(params.validate_execution_context().is_err());
    }
}
