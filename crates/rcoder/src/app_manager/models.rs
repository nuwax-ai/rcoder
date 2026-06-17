//! 应用管理服务数据模型

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
    /// 存储: "10Gi"（仅 K8s）
    pub storage: Option<String>,
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
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct UpdateAppRequest {
    /// 应用名称
    pub name: Option<String>,
    /// 容器镜像
    pub image: Option<String>,
    /// 启动命令
    pub command: Option<Vec<String>>,
    /// 环境变量
    pub env: Option<HashMap<String, String>>,
    /// 敏感信息
    pub secrets: Option<HashMap<String, String>>,
    /// 资源限制
    pub resources: Option<ResourceLimits>,
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

/// 应用信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct AppInfo {
    pub app_id: String,
    pub name: String,
    pub status: AppStatus,
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
