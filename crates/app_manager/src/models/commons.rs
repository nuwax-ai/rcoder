//! 共享数据类型与枚举（请求/响应两侧均引用，独立成模块避免循环依赖）

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
#[serde(rename_all = "lowercase")]
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
    /// readiness 探针路径(Http 类型时)。
    pub path: Option<String>,
    /// liveness 探针路径(Http 类型时);None = 复用 `path`。
    /// 设了不同值 → K8s liveness 用本字段、readiness 用 `path`(两探针语义拆分)。
    #[serde(default)]
    pub liveness_path: Option<String>,
    /// 检查端口
    pub port: Option<u16>,
}

/// 健康检查类型
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HealthCheckType {
    Http,
    Tcp,
    Exec,
    None,
}

/// 应用状态
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum AppStatus {
    Created,
    Starting,
    Running,
    Stopped,
    Error,
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
