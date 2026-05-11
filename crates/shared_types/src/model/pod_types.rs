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
