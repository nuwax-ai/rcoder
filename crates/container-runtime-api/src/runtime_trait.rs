//! Container runtime abstraction trait

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared_types::{ContainerBasicInfo, ServiceResourceLimits, ServiceType};
use std::collections::HashMap;
use thiserror::Error;
use utoipa::ToSchema;
use winnow::ascii::float;
use winnow::prelude::*;
use winnow::token::rest;

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
    /// 容器环境变量（可选，用于获取 project_id 等信息）
    pub env_vars: Option<std::collections::HashMap<String, String>>,
}

/// 已被移除的容器信息（用于清理关联资源）
#[derive(Debug, Clone)]
pub struct RemovedContainerInfo {
    /// 容器名称（稳定标识符）
    pub container_name: String,
    /// 容器 IP
    pub container_ip: String,
    /// 容器标识符（project_id 或 user_id）
    pub identifier: String,
    /// 服务类型
    pub service_type: ServiceType,
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

// ============================================================================
// 应用（UserApp）相关类型
// ============================================================================

/// 应用端口暴露类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum ExposeType {
    /// HTTP 服务（K8s 经 Gateway HTTPRoute，Docker 经 Pingora /proxy）
    Http,
    /// TCP 服务（K8s 经 NodePort，Docker 经 port_bindings）
    Tcp,
}

/// 应用端口规格（创建时由调用方提供）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppPortSpec {
    /// 端口名称
    pub name: String,
    /// 容器端口
    pub port: u16,
    /// 暴露类型
    pub expose_type: ExposeType,
}

/// 健康检查类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum HealthCheckType {
    Http,
    Tcp,
    Exec,
    None,
}

/// 应用健康检查配置
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppHealthCheck {
    pub check_type: HealthCheckType,
    pub path: Option<String>,
    pub port: Option<u16>,
    pub initial_delay_seconds: Option<u32>,
    pub period_seconds: Option<u32>,
}

/// 应用资源需求（字符串格式：cpu="1"/"500m"，memory="512Mi"/"1Gi"，storage="10Gi"）
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct AppResourceRequirements {
    pub cpu: Option<String>,
    pub memory: Option<String>,
    pub storage: Option<String>,
    /// 临时存储限制（overlay 可写层，K8s ephemeral-storage）
    /// 未指定时回退到 storage 值
    pub ephemeral_storage: Option<String>,
}

/// 应用端口运行时状态（含实际分配的对外端口）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppPortStatus {
    pub name: String,
    pub port: u16,
    pub expose_type: ExposeType,
    /// K8s: NodePort；Docker: host_port；未暴露则为 None
    pub external_port: Option<u16>,
}

/// Deployment 运行时状态（供 app_manager 实时查询，rcoder 无状态化读路径的数据载体）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DeploymentStatus {
    /// 应用 ID（app_id）
    pub app_id: String,
    /// 期望副本数
    pub replicas: i32,
    /// 就绪副本数
    pub ready_replicas: i32,
    /// 阶段：Running/Stopped/Starting/Error 等
    pub phase: String,
    /// Pod IP（K8s）/ 容器 IP（Docker）
    pub pod_ip: Option<String>,
    /// 所在节点（K8s）
    pub node: Option<String>,
    /// 重启次数
    pub restart_count: u32,
    /// 启动时间（RFC3339）
    pub started_at: Option<String>,
    /// 端口状态
    pub ports: Vec<AppPortStatus>,
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
        if matches!(service_type, ServiceType::WebAgentRunner) {
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

    /// 同步缓存状态，清理失效的容器记录
    ///
    /// 对于 Docker：遍历 ContainerStateActor 缓存，通过 Docker API 验证容器是否仍存在
    /// 对于 K8s：遍历 pod_cache，通过 K8s API 验证 Pod 是否仍存在
    ///
    /// # Returns
    /// 返回元组 (已检查数量, 已移除容器信息列表)
    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        // 默认实现：不做任何事（向后兼容）
        Ok((0, Vec::new()))
    }

    /// Cleanup all containers (used on shutdown)
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()>;

    /// Health check - verify runtime is accessible
    async fn health_check(&self) -> ContainerRuntimeResult<()>;

    // ====================================================================
    // Deployment 生命周期（UserApp 专用，agent 路径不调用）
    //
    // K8s 由 KubernetesRuntime 实现真实 Deployment 操作；
    // Docker 由 DockerRuntime 做等价语义映射（容器 create/stop/start）。
    // 默认实现返回 ConfigurationError，强制具体 runtime 按需实现。
    // ====================================================================

    /// 创建并启动一个 Deployment（K8s）或等价容器（Docker）
    async fn create_deployment(
        &self,
        params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        let _ = params;
        Err(ContainerRuntimeError::ConfigurationError(
            "create_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 伸缩 Deployment 副本数（K8s scale；Docker: 0=stop, >=1=start）
    async fn scale_deployment(&self, app_id: &str, replicas: i32) -> ContainerRuntimeResult<()> {
        let _ = (app_id, replicas);
        Err(ContainerRuntimeError::ConfigurationError(
            "scale_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 触发滚动重启（K8s rollout annotation；Docker: stop+start）
    async fn restart_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let _ = app_id;
        Err(ContainerRuntimeError::ConfigurationError(
            "restart_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 删除 Deployment 及其关联资源（Service/HTTPRoute/ConfigMap/Secret 等）
    async fn delete_deployment(&self, app_id: &str) -> ContainerRuntimeResult<()> {
        let _ = app_id;
        Err(ContainerRuntimeError::ConfigurationError(
            "delete_deployment not supported by this runtime".to_string(),
        ))
    }

    /// 实时查询 Deployment 运行时状态（供 app_manager 无状态化读路径）
    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
        let _ = app_id;
        Err(ContainerRuntimeError::ConfigurationError(
            "get_deployment_status not supported by this runtime".to_string(),
        ))
    }

    /// 列出当前 runtime 托管的所有 UserApp Deployment（供对账接口）
    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        Err(ContainerRuntimeError::ConfigurationError(
            "list_deployments not supported by this runtime".to_string(),
        ))
    }
}

// ============================================================================
// K8s Quantity 解析（winnow 实现）
//
// 当前 K8s 模式把 Quantity 字符串直接塞进 `Quantity(...)`（不解析），Docker 模式忽略
// ephemeral；此函数作为 pub 工具，供未来"Docker 模式支持 ephemeral / 显示实际字节 /
// 统一校验"等场景使用（parse 成功即合法 Quantity）。
// ============================================================================

/// 解析 K8s 内存 Quantity（`"512Mi"`、`"1Gi"`、`"1e9"`、`"1024"` 等）为字节数
///
/// 基于 winnow，完整支持 K8s Quantity 规范（apimachinery/pkg/api/resource）：
/// - **BinarySI**：`Ki`/`Mi`/`Gi`/`Ti`/`Pi`/`Ei`（1024 进制）
/// - **DecimalSI**：`k`/`M`/`G`/`T`/`P`/`E`（1000 进制，**`k` 为小写**）+ `m`（毫）
/// - **DecimalExponent**：`e`/`E`[+-]?digits（科学计数法，如 `1e9`，由 `float` 直接解析）
/// - 纯数字（字节）+ 小数（如 `1.5`）
///
/// 非法格式（大写 `K`、负数、未识别后缀、非有限/溢出）返回 `None`。
pub fn parse_memory_quantity(quantity: &str) -> Option<u64> {
    let trimmed = quantity.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut input = trimmed;
    // float 已涵盖 DecimalExponent（"1e9" → 1e9），故后缀查表无需再处理 e/E 分支
    // turbofish 指定 Error=()：解析结果经 `.ok()` 转 Option，不关心错误细节
    let value: f64 = float::<_, f64, ()>.parse_next(&mut input).ok()?;
    let suffix: &str = rest::<_, ()>.parse_next(&mut input).ok()?;
    let multiplier = suffix_to_multiplier(suffix)?;
    let bytes = value * multiplier;
    if !bytes.is_finite() || bytes < 0.0 {
        return None;
    }
    Some(bytes.round() as u64)
}

/// K8s Quantity 后缀 → 乘数；未识别后缀（含大写 `K`）返回 `None`
fn suffix_to_multiplier(suffix: &str) -> Option<f64> {
    match suffix {
        "" => Some(1.0),
        // BinarySI（1024 进制）
        "Ki" => Some(1024.0),
        "Mi" => Some(1024f64.powi(2)),
        "Gi" => Some(1024f64.powi(3)),
        "Ti" => Some(1024f64.powi(4)),
        "Pi" => Some(1024f64.powi(5)),
        "Ei" => Some(1024f64.powi(6)),
        // DecimalSI（1000 进制）；K8s 用小写 k，大写 K 非法
        "k" => Some(1e3),
        "M" => Some(1e6),
        "G" => Some(1e9),
        "T" => Some(1e12),
        "P" => Some(1e15),
        "E" => Some(1e18),
        // 毫（DecimalSI）；内存少用但 K8s 支持，结果按需 round
        "m" => Some(1e-3),
        _ => None,
    }
}

#[cfg(test)]
mod quantity_tests {
    use super::*;

    #[test]
    fn parses_binary_si() {
        assert_eq!(parse_memory_quantity("1Ki"), Some(1024));
        assert_eq!(parse_memory_quantity("1Mi"), Some(1024 * 1024));
        assert_eq!(parse_memory_quantity("1Gi"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_memory_quantity("2Gi"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_memory_quantity("1Pi"), Some(1024u64.pow(5)));
    }

    #[test]
    fn parses_decimal_si() {
        assert_eq!(parse_memory_quantity("1k"), Some(1_000));
        assert_eq!(parse_memory_quantity("1M"), Some(1_000_000));
        assert_eq!(parse_memory_quantity("1G"), Some(1_000_000_000));
    }

    #[test]
    fn parses_decimal_exponent_and_plain() {
        assert_eq!(parse_memory_quantity("1e9"), Some(1_000_000_000));
        assert_eq!(parse_memory_quantity("1024"), Some(1024));
        assert_eq!(parse_memory_quantity("1.5"), Some(2)); // 1.5 字节 round → 2
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(parse_memory_quantity("5K"), None); // 大写 K 非法（K8s 用 k）
        assert_eq!(parse_memory_quantity("-5Gi"), None); // 负数
        assert_eq!(parse_memory_quantity("1Xi"), None); // 未识别后缀
        assert_eq!(parse_memory_quantity(""), None);
        assert_eq!(parse_memory_quantity("   "), None);
        assert_eq!(parse_memory_quantity("abc"), None);
    }
}
