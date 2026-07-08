//! 应用管理服务数据模型

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use shared_types::error_codes::{
    ERR_APP_ALREADY_EXISTS, ERR_APP_NOT_FOUND, ERR_BACKEND_ERROR, ERR_FILE_NOT_FOUND,
    ERR_INVALID_STATE, ERR_OPERATION_NOT_SUPPORTED,
};

/// 应用端口运行时状态（来自 container-runtime-api，含实际分配的对外端口）
pub use container_runtime_api::AppPortStatus;

// ============================================================================
// 请求模型
// ============================================================================

/// 创建应用请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateAppRequest {
    /// 应用名称
    pub name: String,
    /// 容器镜像
    pub image: String,
    /// 启动命令
    pub command: Option<Vec<String>>,
    /// 环境变量（存储到 ConfigMap）
    pub env: Option<HashMap<String, String>>,
    /// 敏感信息（存储到 Secret）
    pub secrets: Option<HashMap<String, String>>,
    /// 资源限制
    pub resources: Option<ResourceLimits>,
    /// 端口配置
    pub ports: Option<Vec<PortConfig>>,
    /// 健康检查配置
    pub health_check: Option<HealthCheckConfig>,
    /// 多租户字段
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
}

/// 资源限制
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ResourceLimits {
    /// CPU: "1", "500m", "0.5"
    pub cpu: Option<String>,
    /// 内存: "512Mi", "1Gi"
    pub memory: Option<String>,
    /// 存储: "10Gi"（仅 K8s，UserApp 用于 ephemeral-storage 回退）
    pub storage: Option<String>,
    /// 临时存储限制（overlay 可写层，K8s ephemeral-storage）
    /// 未指定时回退到 storage 值
    pub ephemeral_storage: Option<String>,
}

/// 端口配置
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PortConfig {
    /// 端口名称: "http", "postgres"
    pub name: String,
    /// 容器端口
    pub port: u16,
    /// 暴露类型
    pub expose_type: ExposeType,
    /// [HTTP 端口] 是否 strip `/apps/{app_id}` 前缀（EG URLRewrite）。
    /// - `None`/`false`（默认）：后端收到完整路径 `/apps/{id}/api`，由 app 自行处理
    /// - `true`：EG 把前缀替换为 `/`，后端收到 `/api`（适合静态服务/不感知前缀的 app）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strip_prefix: Option<bool>,
}

/// 暴露类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum ExposeType {
    /// HTTP 服务（通过 Gateway）
    Http,
    /// TCP 服务（通过 NodePort）
    Tcp,
}

/// 健康检查配置
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthCheckConfig {
    /// 检查类型
    pub check_type: HealthCheckType,
    /// HTTP 检查路径
    pub path: Option<String>,
    /// 检查端口
    pub port: Option<u16>,
}

/// 健康检查类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum HealthCheckType {
    Http,
    Tcp,
    Exec,
    None,
}

/// 查询应用请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueryAppsRequest {
    /// 页码
    pub page: Option<u32>,
    /// 每页数量
    pub page_size: Option<u32>,
    /// 过滤条件
    pub filters: Option<AppFilters>,
    /// 排序字段
    pub sort_by: Option<String>,
    /// 排序方式
    pub sort_order: Option<SortOrder>,
}

/// 应用过滤条件
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppFilters {
    /// 按状态过滤
    pub status: Option<Vec<AppStatus>>,
    /// 按名称模糊搜索
    pub name: Option<String>,
    /// 按应用 ID 过滤
    pub app_ids: Option<Vec<String>>,
    /// 创建时间范围
    pub created_at: Option<DateRange>,
}

/// 时间范围
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct DateRange {
    pub start: String,
    pub end: String,
}

/// 排序方式
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum SortOrder {
    Asc,
    Desc,
}

/// 更新应用请求
///
/// **rcoder 无状态**：不持有旧 desired state，无法做"部分字段保留"。因此本请求语义为
/// **全量替换**——调用方（Java，desired state 的 source of truth）需发送完整新状态。
/// `image` 必填（无法保留旧 image）；`ports`/`health_check` 为整段替换。
/// `tenant_id`/`space_id` 携带以保持资源 label（rcoder 不主动修改租户归属）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateAppRequest {
    /// 应用名称（仅元数据，不影响 K8s 资源命名；rcoder 忽略）
    pub name: Option<String>,
    /// 容器镜像（**必填**，rcoder 无状态无法保留旧 image；缺失 → ERR_VALIDATION）
    pub image: Option<String>,
    /// 启动命令
    pub command: Option<Vec<String>>,
    /// 环境变量
    pub env: Option<HashMap<String, String>>,
    /// 敏感信息
    pub secrets: Option<HashMap<String, String>>,
    /// 资源限制
    pub resources: Option<ResourceLimits>,
    /// 端口配置（整段替换）
    pub ports: Option<Vec<PortConfig>>,
    /// 健康检查配置
    pub health_check: Option<HealthCheckConfig>,
    /// 租户 ID（携带以保持 label）
    pub tenant_id: Option<String>,
    /// 空间 ID（携带以保持 label）
    pub space_id: Option<String>,
}

/// 日志查询参数
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LogParams {
    /// 返回最后 N 行
    pub tail: Option<u32>,
    /// 是否持续输出
    pub follow: Option<bool>,
    /// 是否显示时间戳
    pub timestamps: Option<bool>,
    /// 起始时间
    pub since: Option<String>,
}

// ============================================================================
// 响应模型
// ============================================================================

/// 应用状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum AppStatus {
    Created,
    Starting,
    Running,
    Stopping,
    Stopped,
    Error,
    Deleting,
}

/// 条件（K8s conditions 风格，read 时由 DeploymentStatus 派生，用于诊断）
///
/// 与 headline 的 [`AppStatus`] 同源派生、不矛盾：`status` 给 Java 做状态机判断，
/// `conditions[]` 给人/前端做细粒度诊断（如区分 CrashLoopBackOff vs ImagePullBackOff）。
/// `last_transition_time` 在无状态下不持久追踪，通常为 `None`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub struct Condition {
    /// 条件类型：Ready / Available / Progressing / Error
    #[serde(rename = "type")]
    pub r#type: String,
    /// True / False / Unknown
    pub status: String,
    /// 简短机器码（原因）：CrashLoopBackOff / ImagePullBackOff / ScaledDown / Starting ...
    pub reason: Option<String>,
    /// 人读描述
    pub message: Option<String>,
    /// 最近一次状态变迁时间（RFC3339）；无状态下通常为 None
    pub last_transition_time: Option<String>,
}

/// 应用信息
///
/// 仅在 `create_app` 时返回完整字段（rcoder 此时持有请求参数）。
/// 后续读路径（get/start/stop/restart）返回 [`AppRuntimeInfo`]——rcoder 是无状态的应用
/// pod 引擎，业务元数据（name/image/command/env 等）由调用方（Java）持久化。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    pub status: AppStatus,
    /// 阶段附加信息（phase=Error 时为失败原因，如 CrashLoopBackOff）
    pub message: Option<String>,
    pub image: String,
    pub command: Vec<String>,
    pub replicas: u32,
    pub access: AccessInfo,
    pub health: HealthInfo,
    pub resources: Option<ResourceLimits>,
    pub env: HashMap<String, String>,
    pub created_at: String,
    pub updated_at: String,
}

/// 应用运行时信息（rcoder 实时从集群查询）
///
/// 只含运行时字段（phase/副本/Pod IP/端口状态/访问地址），**不含业务元数据**。
/// 由 [`crate::app_manager::AppService`] 调用 `ContainerRuntime::get_deployment_status` /
/// `list_deployments` 实时组装，rcoder 重启后仍可查询（真正无状态）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppRuntimeInfo {
    /// 应用 ID
    pub app_id: String,
    /// 应用状态（由运行时 phase 映射）
    pub status: AppStatus,
    /// 运行时阶段原始值：Running/Stopped/Starting/Error 等
    pub phase: String,
    /// 阶段附加信息（phase=Error 时为失败原因，如 CrashLoopBackOff / 容器退出码）
    pub message: Option<String>,
    /// 期望副本数
    pub replicas: i32,
    /// 就绪副本数
    pub ready_replicas: i32,
    /// 重启次数
    pub restart_count: u32,
    /// Pod IP（K8s）/ 容器 IP（Docker）
    pub pod_ip: Option<String>,
    /// 所在节点（仅 K8s）
    pub node: Option<String>,
    /// 启动时间（RFC3339）
    pub started_at: Option<String>,
    /// 端口运行时状态（含实际分配的对外端口：K8s NodePort / Docker host_port）
    pub ports: Vec<AppPortStatus>,
    /// 访问信息
    pub access: AccessInfo,
    /// 诊断条件（read 时由 DeploymentStatus 派生，见 [`Condition`]）
    pub conditions: Vec<Condition>,
}

/// 访问信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AccessInfo {
    pub external: ExternalAccess,
    pub internal: InternalAccess,
}

/// 外部访问
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ExternalAccess {
    /// HTTP 访问地址
    pub http: Option<String>,
    /// TCP 端口列表
    pub tcp: Vec<TcpPortMapping>,
}

/// TCP 端口映射
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct TcpPortMapping {
    pub name: String,
    pub node_port: u16,
    pub access_url: String,
}

/// 内部访问
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InternalAccess {
    pub domain: String,
    pub short_domain: String,
    pub ports: Vec<InternalPort>,
}

/// 内部端口
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InternalPort {
    pub name: String,
    pub port: u16,
}

/// 健康信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthInfo {
    pub status: String,
    pub instance: Option<InstanceInfo>,
    pub probes: Option<ProbeInfo>,
}

/// 实例信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstanceInfo {
    pub name: String,
    pub phase: String,
    pub ready: bool,
    pub restart_count: u32,
    pub node: String,
    pub ip: String,
    pub started_at: Option<String>,
}

/// Probe 信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProbeInfo {
    pub liveness: ProbeStatus,
    pub readiness: ProbeStatus,
}

/// Probe 状态
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProbeStatus {
    pub status: String,
    pub last_checked: Option<String>,
}

/// 日志条目
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LogEntry {
    pub timestamp: String,
    pub stream: String,
    pub message: String,
}

/// 资源使用
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ResourceStats {
    pub cpu: CpuStats,
    pub memory: MemoryStats,
    pub network: NetworkStats,
    pub restart_count: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct CpuStats {
    pub usage_percent: f64,
    pub usage_cores: f64,
    pub limit_cores: f64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct MemoryStats {
    pub usage_bytes: u64,
    pub usage_percent: f64,
    pub limit_bytes: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct NetworkStats {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

/// 分页响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PaginatedResponse<T> {
    pub items: Vec<T>,
    pub pagination: Pagination,
}

/// 分页信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Pagination {
    pub page: u32,
    pub page_size: u32,
    pub total: u64,
    pub total_pages: u32,
}

/// 文件上传结果
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadResult {
    pub file_path: String,
    pub file_size: u64,
    pub uploaded_at: String,
}

/// 文件信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileInfo {
    pub path: String,
    pub size: u64,
    pub is_dir: bool,
    pub modified_at: String,
}

// ============================================================================
// 存储管理（v2 §5.4）—— 删应用默认保留数据，由这组接口显式管理残留
// ============================================================================

/// 删除应用请求
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct DeleteAppRequest {
    /// 是否同时清空持久存储（默认 `false`：只删计算面，保留数据面）
    #[serde(default)]
    pub purge: Option<bool>,
}

/// 存储查询请求（**强制分页，无全量模式**——扫存储后端代价高）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct QueryStorageRequest {
    /// 页码（必填，从 1 开始）
    pub page: u32,
    /// 每页数量（必填，上限 100）
    pub page_size: u32,
    /// 过滤条件
    pub filters: Option<StorageFilters>,
}

/// 存储过滤条件
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct StorageFilters {
    /// `true` = 只返回"有数据、无对应运行应用"的孤儿存储
    pub orphan_only: Option<bool>,
    /// 按 app_id 精确过滤（最省扫描）
    pub app_ids: Option<Vec<String>>,
    /// 按租户过滤
    pub tenant_id: Option<String>,
    /// 按空间过滤
    pub space_id: Option<String>,
}

/// 存储信息（**不含 `size_bytes`**——CephFS 上不能用 du，见设计文档 §5.4）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct StorageInfo {
    /// 应用 ID
    pub app_id: String,
    /// 目录是否存在
    pub exists: bool,
    /// app 根路径（rcoder 视角）
    pub path: String,
    /// 最近修改时间（RFC3339）
    pub modified_at: Option<String>,
    /// 是否孤儿（无对应运行应用）
    pub is_orphan: bool,
}

// ============================================================================
// 错误（v2 §12）—— service 层抛出，handler 层 downcast 取 code 精确映射 HTTP
// ============================================================================

/// app 操作级错误（携带业务错误码，供 handler 精确映射 HTTP 与 retryable）。
///
/// service 层抛出此错误（`.into()` 转 `anyhow::Error`），handler 的 `map_app_error`
/// 通过 `downcast_ref` 取出 code；未携带此类型的 anyhow 错误兜底为 `ERR_BACKEND_ERROR`。
/// `retryable` 既是错误码的固有属性（见 `shared_types::error_codes::is_retryable_code`），
/// 不在响应体重复（HttpResult 不变）。
#[derive(Debug)]
pub struct AppOperationError {
    /// 业务错误码（ERR_* 常量）
    pub code: &'static str,
    /// 人读错误信息
    pub message: String,
}

impl AppOperationError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self::new(ERR_APP_NOT_FOUND, msg)
    }
    pub fn already_exists(msg: impl Into<String>) -> Self {
        Self::new(ERR_APP_ALREADY_EXISTS, msg)
    }
    pub fn invalid_state(msg: impl Into<String>) -> Self {
        Self::new(ERR_INVALID_STATE, msg)
    }
    pub fn not_supported(msg: impl Into<String>) -> Self {
        Self::new(ERR_OPERATION_NOT_SUPPORTED, msg)
    }
    pub fn file_not_found(msg: impl Into<String>) -> Self {
        Self::new(ERR_FILE_NOT_FOUND, msg)
    }
    pub fn backend(msg: impl Into<String>) -> Self {
        Self::new(ERR_BACKEND_ERROR, msg)
    }
}

impl fmt::Display for AppOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for AppOperationError {}
