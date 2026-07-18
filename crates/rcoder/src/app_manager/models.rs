//! 应用管理服务数据模型

use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use shared_types::error_codes::{
    ERR_APP_ALREADY_EXISTS, ERR_APP_NOT_FOUND, ERR_BACKEND_ERROR, ERR_CONFLICT, ERR_FILE_NOT_FOUND,
    ERR_INVALID_STATE, ERR_OPERATION_NOT_SUPPORTED, ERR_VALIDATION,
};

/// 应用端口运行时状态（来自 container-runtime-api，含实际分配的对外端口）
pub use container_runtime_api::AppPortStatus;

// ============================================================================
// 请求模型
// ============================================================================

/// 创建应用请求
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct CreateAppRequest {
    /// 应用 ID（可选，外部指定；格式 `app-` + DNS-1123，如 `app-order-svc`；None=自动生成）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub app_id: Option<String>,
    /// 应用名称
    pub name: String,
    /// 容器镜像（**完整地址**，含 registry + 命名空间，如 `nuwax-docker-images-registry.cn-hangzhou.cr.aliyuncs.com/nuwax-k8s-test/app-runtime-java`）。
    /// 由调用方提前准备好并 push 到 registry；RCoder 不构建镜像。初期不限定镜像列表。
    /// 命名空间区分环境：`nuwax-k8s-test`（测试）/ `nuwax-k8s-prod`（线上）。
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
    /// 租户 ID（多租户场景）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户场景）
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
    /// - `None`/`true`（默认）：EG strip 前缀，后端收到 `/api`（与 Docker Pingora 一致）
    /// - `false`：保留完整路径 `/apps/{id}/api`，由 app 自行处理
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
    /// 起始时间（RFC3339）
    pub start: String,
    /// 结束时间（RFC3339）
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
    /// 乐观锁：传入 `GET /apps/{id}` 返回的 `resource_version`；不匹配 → 409 ERR_CONFLICT。
    /// 不传 = 不校验（向后兼容）。Docker 模式 resource_version=None，忽略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_resource_version: Option<String>,
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
    /// 应用 ID
    pub app_id: String,
    /// 应用名称
    pub name: String,
    /// 应用状态
    pub status: AppStatus,
    /// 阶段附加信息（phase=Error 时为失败原因，如 CrashLoopBackOff）
    pub message: Option<String>,
    /// 容器镜像
    pub image: String,
    /// 启动命令
    pub command: Vec<String>,
    /// 副本数
    pub replicas: u32,
    /// 访问信息
    pub access: AccessInfo,
    /// 健康信息
    pub health: HealthInfo,
    /// 资源限制
    pub resources: Option<ResourceLimits>,
    /// 环境变量
    pub env: HashMap<String, String>,
    /// 创建时间（RFC3339）
    pub created_at: String,
    /// 更新时间（RFC3339）
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
    /// 乐观锁用（K8s Deployment.resourceVersion；Docker=None）。
    /// update/delete 时作为 `expected_resource_version` 传入校验。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_version: Option<String>,
}

/// 访问信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AccessInfo {
    /// 外部访问（HTTP 地址 / TCP NodePort）
    pub external: ExternalAccess,
    /// 内部访问（集群内 FQDN / 端口）
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
    /// 端口名称
    pub name: String,
    /// NodePort 端口号
    pub node_port: u16,
    /// 访问地址
    pub access_url: String,
}

/// 内部访问
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InternalAccess {
    /// 集群内完整域名（FQDN）
    pub domain: String,
    /// 简写域名
    pub short_domain: String,
    /// 内部端口列表
    pub ports: Vec<InternalPort>,
}

/// 内部端口
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InternalPort {
    /// 端口名称
    pub name: String,
    /// 端口号
    pub port: u16,
}

/// 健康信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct HealthInfo {
    /// 健康状态：Running/Starting/Unhealthy 等
    pub status: String,
    /// 实例信息（Pod 详情）
    pub instance: Option<InstanceInfo>,
    /// Probe 探针结果
    pub probes: Option<ProbeInfo>,
}

/// 实例信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct InstanceInfo {
    /// 实例名称（Pod 名）
    pub name: String,
    /// 运行阶段
    pub phase: String,
    /// 是否就绪
    pub ready: bool,
    /// 重启次数
    pub restart_count: u32,
    /// 所在节点
    pub node: String,
    /// Pod IP
    pub ip: String,
    /// 启动时间（RFC3339）
    pub started_at: Option<String>,
}

/// Probe 信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProbeInfo {
    /// Liveness 探针状态
    pub liveness: ProbeStatus,
    /// Readiness 探针状态
    pub readiness: ProbeStatus,
}

/// Probe 状态
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ProbeStatus {
    /// 探针结果
    pub status: String,
    /// 最近检查时间（RFC3339）
    pub last_checked: Option<String>,
}

/// 日志条目
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct LogEntry {
    /// 时间戳（RFC3339；文件日志无时间戳则为空）
    pub timestamp: String,
    /// 日志流：stdout / stderr / file
    pub stream: String,
    /// 日志内容
    pub message: String,
}

/// 资源使用
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct ResourceStats {
    /// CPU 使用
    pub cpu: CpuStats,
    /// 内存使用
    pub memory: MemoryStats,
    /// 网络使用
    pub network: NetworkStats,
    /// 重启次数
    pub restart_count: u32,
}

/// CPU 使用统计
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct CpuStats {
    /// CPU 使用率 (0-100)
    pub usage_percent: f64,
    /// CPU 使用核数
    pub usage_cores: f64,
    /// CPU 限制核数
    pub limit_cores: f64,
}

/// 内存使用统计
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct MemoryStats {
    /// 内存使用（字节）
    pub usage_bytes: u64,
    /// 内存使用率 (0-100)
    pub usage_percent: f64,
    /// 内存限制（字节）
    pub limit_bytes: u64,
}

/// 网络使用统计
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct NetworkStats {
    /// 网络接收字节数
    pub rx_bytes: u64,
    /// 网络发送字节数
    pub tx_bytes: u64,
}

/// 分页响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PaginatedResponse<T> {
    /// 数据条目
    pub items: Vec<T>,
    /// 分页信息
    pub pagination: Pagination,
}

/// 分页信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Pagination {
    /// 当前页码
    pub page: u32,
    /// 每页数量
    pub page_size: u32,
    /// 总条目数
    pub total: u64,
    /// 总页数
    pub total_pages: u32,
}

/// 文件上传结果
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UploadResult {
    /// 文件路径（单文件=target；压缩包=解压目录，app 根相对）
    pub file_path: String,
    /// 文件大小（字节；单文件=文件大小，压缩包=压缩包大小）
    pub file_size: u64,
    /// 上传时间（RFC3339）
    pub uploaded_at: String,
    /// 压缩包解压文件数（仅压缩包上传时返回；单文件为 None）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extracted_count: Option<usize>,
}

/// 文件信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct FileInfo {
    /// 文件路径（app 根相对）
    pub path: String,
    /// 文件大小（字节）
    pub size: u64,
    /// 是否目录
    pub is_dir: bool,
    /// 最后修改时间（RFC3339）
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
    /// 乐观锁：传入 `GET /apps/{id}` 返回的 `resource_version`；不匹配 → 409 ERR_CONFLICT。
    /// 不传 = 不校验（向后兼容）。Docker 模式忽略。
    #[serde(default)]
    pub expected_resource_version: Option<String>,
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
// 错误（v2 §12）—— service 层抛出强类型错误，handler 用 From 直接转 AppError
// ============================================================================

/// app 操作级错误（携带业务错误码，供 handler 精确映射 HTTP）。
///
/// 每个错误场景一个 variant，`code()`/`message()` 用 match 实现，编译器强制穷举
/// （新增 variant 时所有 match 编译报错，OCP）。message 含完整因果链，由 service
/// 层在构造时拼入。handler 通过 `impl From<AppOperationError> for AppError` 直接转换，
/// 无需 downcast / 字符串匹配。
#[derive(Debug)]
pub enum AppOperationError {
    /// 应用不存在（404 ERR_APP_NOT_FOUND）
    NotFound(String),
    /// 应用已存在（409 ERR_APP_ALREADY_EXISTS）
    AlreadyExists(String),
    /// 操作状态非法，如未 delete 就清空存储（409 ERR_INVALID_STATE）
    InvalidState(String),
    /// 操作不支持（400 ERR_OPERATION_NOT_SUPPORTED）
    NotSupported(String),
    /// 文件/目录不存在（404 ERR_FILE_NOT_FOUND）
    FileNotFound(String),
    /// 请求参数校验失败（400 ERR_VALIDATION）
    Validation(String),
    /// 后端运行时错误（500 ERR_BACKEND_ERROR，兜底）
    Backend(String),
    /// 乐观锁冲突（409 ERR_CONFLICT）—— expected_resource_version 不匹配
    Conflict(String),
}

impl AppOperationError {
    /// 业务错误码（ERR_* 常量）
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => ERR_APP_NOT_FOUND,
            Self::AlreadyExists(_) => ERR_APP_ALREADY_EXISTS,
            Self::InvalidState(_) => ERR_INVALID_STATE,
            Self::NotSupported(_) => ERR_OPERATION_NOT_SUPPORTED,
            Self::FileNotFound(_) => ERR_FILE_NOT_FOUND,
            Self::Validation(_) => ERR_VALIDATION,
            Self::Backend(_) => ERR_BACKEND_ERROR,
            Self::Conflict(_) => ERR_CONFLICT,
        }
    }

    /// 人读错误信息（含完整因果链，由 service 构造时拼入）
    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m)
            | Self::AlreadyExists(m)
            | Self::InvalidState(m)
            | Self::NotSupported(m)
            | Self::FileNotFound(m)
            | Self::Validation(m)
            | Self::Backend(m)
            | Self::Conflict(m) => m,
        }
    }
}

impl fmt::Display for AppOperationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code(), self.message())
    }
}

impl std::error::Error for AppOperationError {}

/// app_manager 服务统一返回类型（减少签名噪音）
pub type AppResult<T> = std::result::Result<T, AppOperationError>;
