use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};

use super::*;

/// 获取容器列表的查询参数
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct PodListQuery {
    /// 分页大小（默认100，不传则返回所有）
    #[param(example = 100)]
    #[schema(example = 100)]
    #[serde(default)]
    pub limit: Option<u32>,
}

/// 容器详细信息
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodDetailInfo {
    /// 容器 ID
    #[schema(example = "abc123def456")]
    pub container_id: String,

    /// 容器名称
    #[schema(example = "computer-agent-runner-user_123")]
    pub container_name: String,

    /// 容器 IP 地址 (内部网络)
    #[schema(example = "172.17.0.5")]
    pub container_ip: String,

    /// 服务 URL
    #[schema(example = "http://172.17.0.5:8086")]
    pub service_url: String,

    /// 容器状态
    #[schema(example = "running")]
    pub status: String,

    /// 服务类型
    #[schema(example = "ComputerAgentRunner")]
    pub service_type: String,

    /// 项目 ID（如果有）
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    /// 用户 ID（如果有）
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 创建时间 (Unix 毫秒时间戳)
    #[schema(example = 1702700000000_u64)]
    pub created_at: u64,

    /// 最后活动时间 (Unix 毫秒时间戳)
    #[schema(example = 1702700600000_u64)]
    pub last_activity: Option<u64>,

    /// 镜像名称
    #[schema(example = "rcoder-agent-runner:latest")]
    pub image: Option<String>,

    /// 内部端口
    #[schema(example = 8086)]
    pub internal_port: Option<u16>,

    /// 外部端口
    #[schema(example = 30001)]
    pub external_port: Option<u16>,
}

/// 获取容器列表响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodListResponse {
    /// 容器列表
    pub containers: Vec<PodDetailInfo>,

    /// 总数量
    #[schema(example = 5)]
    pub total: u32,

    /// 返回数量
    #[schema(example = 5)]
    pub returned: u32,

    /// 是否已分页
    #[schema(example = false)]
    pub paginated: bool,

    /// 查询时间戳 (Unix 毫秒)
    #[schema(example = 1702700000000_u64)]
    pub timestamp: u64,
}

// ============================================================================
// 接口二：启动容器
// ============================================================================

/// 启动容器请求
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct EnsurePodRequest {
    /// 用户唯一标识符 (必填)
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目唯一标识符 (必填)
    #[schema(example = "proj_456")]
    pub project_id: String,

    /// 可选的资源限制配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<ServiceResourceLimits>,

    /// 容器唯一标识，若传值则使用此 ID 标识容器，实现容器复用
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    ///
    /// 注意：与 app_id 互斥（userApp 场景的容器类型由 app_stage 推导）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,

    /// UserApp 容器标识（可选；存在即进入 userApp 分派——启动/保活/重启
    /// userApp 容器，启用其虚拟终端(ttyd)与文件服务等）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "app_789")]
    pub app_id: Option<String>,

    /// UserApp 容器阶段（可选，缺省 "dev"）
    /// - "dev": 开发容器（UserAppBuilder，含虚拟终端/文件服务/PG 全套开发栈）
    /// - "prod": 生产 Deployment（AppService 托管）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
}

/// 启动容器响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct EnsurePodResponse {
    /// 容器是否为新创建 (false 表示已存在)
    pub created: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 提示消息
    #[schema(example = "容器已就绪，可通过 VNC 访问")]
    pub message: String,
}

/// 容器基本信息（对外接口）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct PodContainerInfo {
    /// 容器 ID
    #[schema(example = "abc123def456")]
    pub container_id: String,

    /// 容器状态
    #[schema(example = "running")]
    pub status: String,
}

// ============================================================================
// 接口三：容器保活
// ============================================================================

/// 容器保活请求
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct KeepalivePodRequest {
    /// 用户唯一标识符
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目唯一标识符
    #[schema(example = "proj_456")]
    pub project_id: String,

    // === 新增字段 (多租户隔离支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,

    /// UserApp 容器标识（可选；存在即进入 userApp 分派——保活/重启 userApp 容器）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "app_789")]
    pub app_id: Option<String>,

    /// UserApp 容器阶段（可选，缺省 "dev"；dev=开发容器 UserAppBuilder / prod=生产 Deployment）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
}

/// 容器保活响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct KeepalivePodResponse {
    /// 容器是否已存在
    pub existed: bool,

    /// 容器是否为新创建 (当 existed=false 时为 true)
    pub created: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 上次活动时间 (Unix 毫秒时间戳, 更新前)
    #[schema(example = 1702700000000_u64)]
    pub previous_activity_time: u64,

    /// 当前活动时间 (Unix 毫秒时间戳, 更新后)
    #[schema(example = 1702700600000_u64)]
    pub current_activity_time: u64,

    /// 上次活动时间 (东八区时间字符串)
    #[schema(example = "2023-12-16 10:00:00")]
    pub previous_activity_time_str: String,

    /// 当前活动时间 (东八区时间字符串)
    #[schema(example = "2023-12-16 10:10:00")]
    pub current_activity_time_str: String,

    /// 距离下次清理的剩余时间 (秒)
    #[schema(example = 1800)]
    pub time_until_cleanup: u64,

    /// 提示消息
    #[schema(example = "容器活动时间已刷新")]
    pub message: String,
}

// ============================================================================
// 接口四：重启容器
// ============================================================================

/// 重启容器请求
#[derive(Debug, Clone, Deserialize, Serialize, ToSchema)]
pub struct RestartPodRequest {
    /// 用户唯一标识符 (必填)
    #[schema(example = "user_123")]
    pub user_id: String,

    /// 项目唯一标识符 (必填)
    #[schema(example = "proj_456")]
    pub project_id: String,

    /// 可选的资源限制配置
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_limits: Option<ServiceResourceLimits>,

    /// 容器唯一标识，若传值则使用此 ID 标识容器，实现容器复用
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,

    /// UserApp 容器标识（可选；存在即进入 userApp 分派——保活/重启 userApp 容器）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "app_789")]
    pub app_id: Option<String>,

    /// UserApp 容器阶段（可选，缺省 "dev"；dev=开发容器 UserAppBuilder / prod=生产 Deployment）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "dev")]
    pub app_stage: Option<String>,
}

/// 重启容器响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RestartPodResponse {
    /// 容器是否为新创建 (之前不存在时为 true)
    pub was_existing: bool,

    /// 容器是否已重启
    pub restarted: bool,

    /// 容器基本信息
    pub container_info: PodContainerInfo,

    /// 提示消息
    #[schema(example = "容器已重启，可通过 VNC 访问虚拟桌面")]
    pub message: String,
}

/// 查询容器状态请求
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct PodStatusQuery {
    /// 项目唯一标识符 (可选，user_id 和 project_id 至少需要一个)
    #[param(example = "proj_456")]
    #[schema(example = "proj_456")]
    #[serde(default)]
    pub project_id: Option<String>,

    /// 用户唯一标识符 (可选，user_id 和 project_id 至少需要一个)
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    #[serde(default)]
    pub user_id: Option<String>,

    // === 新增字段 (多租户隔离支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "pod_tenant_123")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "tenant_abc")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "space_xyz")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "tenant")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "computer-agent-runner")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,
}

/// 查询容器状态响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodStatusResponse {
    /// 容器是否存活 (true=存在且运行中，false=不存在或未运行)
    #[schema(example = true)]
    pub alive: bool,

    /// 容器状态描述 (running/stopped/not_found)
    #[schema(example = "running")]
    pub status: String,

    /// 容器 ID (如果存在)
    #[schema(example = "abc123def456")]
    pub container_id: Option<String>,

    /// 容器名称 (如果存在)
    #[schema(example = "computer-agent-runner-user_123")]
    pub container_name: Option<String>,

    /// 查询时间戳 (Unix 毫秒)
    #[schema(example = 1702700000000_u64)]
    pub timestamp: u64,

    /// 提示消息
    #[schema(example = "容器正在运行中")]
    pub message: String,
}

/// VNC 状态查询参数
#[derive(Debug, Clone, Deserialize, IntoParams, ToSchema)]
pub struct VncStatusQuery {
    /// 用户唯一标识符（可选，与 project_id 至少填一个）
    #[param(example = "user_123")]
    #[schema(example = "user_123")]
    pub user_id: Option<String>,

    /// 项目唯一标识符（可选，与 user_id 至少填一个）
    #[param(example = "proj_456")]
    #[schema(example = "proj_456")]
    pub project_id: Option<String>,

    // === 新增字段 (多租户隔离支持) ===
    /// 容器唯一标识，若传值则使用此 ID 标识容器
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "pod_tenant_123")]
    #[schema(example = "pod_tenant_123")]
    pub pod_id: Option<String>,

    /// 租户 ID，用于多租户场景下的数据隔离
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "tenant_abc")]
    #[schema(example = "tenant_abc")]
    pub tenant_id: Option<String>,

    /// 空间 ID，用于区分租户下的不同空间
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "shared_types::flexible_string::flexible_string"
    )]
    #[param(example = "space_xyz")]
    #[schema(example = "space_xyz")]
    pub space_id: Option<String>,

    /// 隔离类型，控制容器共享粒度和数据目录结构
    #[serde(skip_serializing_if = "Option::is_none")]
    #[param(example = "tenant")]
    #[schema(example = "tenant")]
    pub isolation_type: Option<String>,

    /// 服务类型，决定创建哪种类型的容器
    /// - "computer-agent-runner" (默认): ComputerAgentRunner 容器，标识符为 user_id
    /// - "web-agent-runner": WebAgentRunner 容器，标识符为 project_id
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[param(example = "computer-agent-runner")]
    #[schema(example = "computer-agent-runner")]
    pub service_type: Option<String>,
}
