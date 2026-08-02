//! Container runtime 类型定义（错误、状态、UserApp 相关 DTO）

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use shared_types::ServiceType;
use std::collections::HashMap;
use thiserror::Error;
use utoipa::ToSchema;

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

    #[error("Container exec failed: {0}")]
    ContainerExecError(String),
}

/// Result type for container operations
pub type ContainerRuntimeResult<T> = Result<T, ContainerRuntimeError>;

/// exec 命令执行结果(UserApp 容器内跑命令的输出)
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// 标准输出
    pub stdout: String,
    /// 标准错误
    pub stderr: String,
    /// 退出码(0 = 成功;-1 表示无法获取)
    pub exit_code: i64,
}

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
    pub env_vars: Option<HashMap<String, String>>,
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

/// 应用端口暴露类型（只描述协议；对外暴露机制由 [`HttpExpose`] 决定）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
pub enum ExposeType {
    /// HTTP 服务
    Http,
    /// TCP 服务（初期仅集群内访问，不对外）
    Tcp,
}

/// 应用 HTTP 服务的对外暴露模式（全局配置 `http_expose`，默认 [`HttpExpose::Pingora`]）。
///
/// 决定 HTTP 端口走 RCoder 内置 Pingora 代理还是外部 Gateway HTTPRoute。
/// TCP 端口初期不对外（不论此值），只给 internal ClusterIP FQDN。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum HttpExpose {
    /// 走 RCoder 内置 Pingora 代理（`/proxy/{port}`）—— K8s/Docker 两后端统一，默认。
    #[default]
    Pingora,
    /// 走外部 Gateway HTTPRoute（`/apps/{app_id}`）—— 仅 K8s 可选；Docker 无此模式（始终 Pingora）。
    Gateway,
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
    /// HTTP 端口：是否 strip 前缀（EG URLRewrite ReplacePrefixMatch /apps/{id} → /）。
    /// None/true=strip 前缀（默认，与 Docker Pingora 一致）；false=保留完整路径。
    /// 仅对 expose_type=Http 生效；TCP 忽略。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub strip_prefix: Option<bool>,
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
    /// readiness 探针路径(Http 类型时)。
    pub path: Option<String>,
    /// liveness 探针路径(Http 类型时);None = 复用 `path`(向后兼容,两探针同路径)。
    /// 设了不同值时,K8s liveness 用本字段、readiness 用 `path`,从而可拆成两个语义不同的探针。
    #[serde(default)]
    pub liveness_path: Option<String>,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct DeploymentStatus {
    /// 应用 ID（app_id）
    pub app_id: String,
    /// 期望副本数
    pub replicas: i32,
    /// 就绪副本数
    pub ready_replicas: i32,
    /// 阶段：Running/Stopped/Starting/Error 等
    pub phase: String,
    /// 阶段附加信息（如失败原因：CrashLoopBackOff / ImagePullBackOff / 容器退出码等）。
    /// phase=Error 时必填，便于调用方定位"服务为啥没起来"。
    pub message: Option<String>,
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
    /// K8s Deployment.resourceVersion（乐观锁用，update/delete 校验 expected_resource_version）。
    /// Docker 无此概念，填 None（开发环境不校验乐观锁）。
    pub resource_version: Option<String>,
    /// 是否参与闲置自动回收（None=注解缺失/旧 app=按免费默认可回收；Some(false)=付费永不回收）。
    /// 由 `rcoder.io/recycle-enabled` 注解读回。
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（per-app 覆盖；None=用全局配置）。由 `rcoder.io/idle-timeout-seconds` 注解读回。
    pub idle_timeout_seconds: Option<u64>,
    /// Deployment 创建时间（RFC3339，来自 metadata.creationTimestamp；回收扫描器做 protection 龄期判断）。
    pub created_at: Option<String>,
}

/// app 容器当前 `command`/`env` 快照（`update` 部分更新回退用）。
///
/// rcoder 无状态、不存业务元数据（name/image/command/env 由调用方持久化）。
/// `update` 请求若漏 `command`/`env`，从 live 容器读当前值回退——与 `ports` 从运行时状态
/// 回退一致，避免部分更新静默清空（`command` 丢 → 镜像无 ENTRYPOINT 时 CrashLoop；
/// `env` 丢 → K8s `cleanup_orphan_port_resources` 删 ConfigMap → 容器丢环境变量）。
///
/// 注：env 仅回退**字面值**（K8s ConfigMap.data / Docker Config.env）；
/// `valueFrom`（secret/configmap 引用）无字面值，读不回，需调用方 update 时重发 `secrets`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerSpecSnapshot {
    /// 启动命令（K8s UserApp 存于 `container.args`，Docker 存于 `Config.cmd`）
    pub command: Option<Vec<String>>,
    /// 字面值环境变量
    pub env: Option<HashMap<String, String>>,
}

/// 应用资源用量（运行时层；K8s 来自 metrics.k8s.io PodMetrics，Docker 可来自 bollard stats）。
/// 仅含运行时可观测的用量 + 限额；百分比由 app_manager 层算（usage/limit）。
/// network（rx/tx）metrics.k8s.io 不提供，故不在此（app_manager 层留 0）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    /// CPU 已用核数（各容器求和）
    pub cpu_usage_cores: f64,
    /// 内存已用字节（各容器求和）
    pub mem_usage_bytes: u64,
    /// CPU 限额核数（pod resources.limits.cpu 求和；无 limit 则 0 → 百分比为 0）
    pub cpu_limit_cores: f64,
    /// 内存限额字节（pod resources.limits.memory 求和；无 limit 则 0）
    pub mem_limit_bytes: u64,
}

/// 容器日志条目（运行时层；app_manager 层另映射为带 ToSchema 的 LogEntry 暴露给 API）
#[derive(Debug, Clone)]
pub struct ContainerLogEntry {
    /// 时间戳（RFC3339；关闭 timestamps 时为 None）
    pub timestamp: Option<String>,
    /// 流：stdout / stderr
    pub stream: String,
    /// 日志内容（不含末尾换行）
    pub message: String,
}

/// K8s Event 信息（来自 events API，供 app 诊断）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, utoipa::ToSchema)]
pub struct AppEventInfo {
    /// 事件类型：Normal / Warning
    #[serde(rename = "type")]
    pub event_type: String,
    /// 原因简码：Pulled / Created / Started / Failed / BackOff / FailedScheduling ...
    pub reason: String,
    /// 人读描述
    pub message: String,
    /// 最近发生时间（RFC3339）
    pub timestamp: String,
    /// 关联对象名（如 pod 名）
    pub object: String,
    /// 发生次数
    pub count: i32,
}
