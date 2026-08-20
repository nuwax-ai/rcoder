//! 应用管理响应模型

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use container_runtime_api::AppPortStatus;

use super::commons::{AccessInfo, AppStatus, ResourceLimits};

/// 条件（K8s conditions 风格，read 时由 DeploymentStatus 派生，用于诊断）
///
/// 与 headline 的 [`AppStatus`] 同源派生、不矛盾：`status` 给 Java 做状态机判断，
/// `conditions[]` 给人/前端做细粒度诊断（如区分 CrashLoopBackOff vs ImagePullBackOff）。
/// `last_transition_time` 在无状态下不持久追踪，通常为 `None`。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
/// 由 [`crate::service::AppService`] 调用 `ContainerRuntime::get_deployment_status` /
/// `list_deployments` 实时组装，rcoder 重启后仍可查询（真正无状态）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
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
    /// 健康信息（由 build_runtime_info 经 health_from_status 统一派生；消除 handler 重复派生）
    pub health: HealthInfo,
    /// 是否参与闲置自动回收（None=旧 app/未知；Some(false)=付费永不回收）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recycle_enabled: Option<bool>,
    /// 闲置回收阈值秒数（per-app 覆盖；None=用全局配置）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_timeout_seconds: Option<u64>,
    /// scale0 时是否允许流量自动唤醒。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wake_on_traffic: Option<bool>,
    /// Deployment 创建时间（RFC3339）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
}

/// 健康信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct ProbeInfo {
    /// Liveness 探针状态
    pub liveness: ProbeStatus,
    /// Readiness 探针状态
    pub readiness: ProbeStatus,
}

/// Probe 状态
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProbeStatus {
    /// 探针结果
    pub status: String,
    /// 最近检查时间（RFC3339）
    pub last_checked: Option<String>,
}

/// 资源使用
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
pub struct NetworkStats {
    /// 网络接收字节数
    pub rx_bytes: u64,
    /// 网络发送字节数
    pub tx_bytes: u64,
}

/// 分页响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PaginatedResponse<T> {
    /// 数据条目
    pub items: Vec<T>,
    /// 分页信息
    pub pagination: Pagination,
}

/// 分页信息
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
#[serde(rename_all = "camelCase")]
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
