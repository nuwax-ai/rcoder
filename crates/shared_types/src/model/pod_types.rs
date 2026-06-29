//! Pod 容器管理相关类型定义
//!
//! 提供容器数量统计的响应类型，供 rcoder 和 agent_runner 共享使用。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 容器数量响应
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountResponse {
    /// 当前运行的容器总数
    #[schema(example = 5)]
    pub total_count: u32,

    /// 按服务类型分类的容器数量
    pub by_service_type: PodCountByServiceType,

    /// 统计时间戳 (Unix 毫秒)
    #[schema(example = 1702700000000_u64)]
    pub timestamp: u64,
}

/// 按服务类型分类的容器数量
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct PodCountByServiceType {
    /// RCoder 类型容器数量
    #[schema(example = 2)]
    pub rcoder: u32,

    /// ComputerAgentRunner 类型容器数量
    #[schema(example = 3)]
    pub computer_agent_runner: u32,
}

/// VNC 状态响应
///
/// 供 rcoder 的 `/computer/pod/vnc-status` 与 agent_runner 的
/// `/computer/agent/vnc-status` 共享，保证前端拿到一致的字段结构。
///
/// `uptime_seconds` / `container_id` 为可选：rcoder 聚合 gRPC 结果与容器信息时填充；
/// agent_runner 在宿主机直接探测时无法提供（填 None）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct VncStatusResponse {
    /// VNC 是否已就绪（Xvnc 进程 + 5900 端口 RFB 握手）
    #[schema(example = true)]
    pub vnc_ready: bool,

    /// noVNC 是否已就绪（6080 端口）
    #[schema(example = true)]
    pub novnc_ready: bool,

    /// 状态描述消息
    #[schema(example = "VNC 服务已就绪")]
    pub message: String,

    /// 容器启动时长（秒）；agent_runner 直接探测时为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = 120)]
    pub uptime_seconds: Option<i64>,

    /// 容器 ID；agent_runner 直接探测时为 None
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "abc123def456")]
    pub container_id: Option<String>,
}
