//! 发布/构建任务的请求与响应 DTO（从 handler.rs 拆出；HTTP 处理与路由在 handler.rs）。

use serde::Deserialize;

use super::types::{PublishTaskKind, PublishTaskSnapshot, PublishTaskStatus};

/// build 请求体:agent-runner project_id(定位 build 目标)。
#[derive(Debug, Deserialize, utoipa::ToSchema)]
pub struct PublishBody {
    pub project_id: String,
    /// owner 用户 ID（补记 userapp_metadata；显式传优先于 create-workspace 注册值）
    #[serde(default)]
    pub user_id: Option<String>,
}

/// tasks/query 请求体(分页 + 可选过滤;POST body 承载,与 /apps/query 惯例一致)。
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct QueryPublishTasksRequest {
    /// 页码,从 1 起,默认 1
    pub page: Option<u32>,
    /// 每页数量,1..=100,默认 20
    pub page_size: Option<u32>,
    /// 过滤条件
    pub filters: Option<PublishTaskFilters>,
}

/// 任务过滤(app_ids 精确集合 / kind / 只看未终态)。
#[derive(Debug, Default, Deserialize, utoipa::ToSchema)]
pub struct PublishTaskFilters {
    /// 按 app_id 集合过滤(None=全部)
    pub app_ids: Option<Vec<String>>,
    /// build | publish(None=全部)
    pub kind: Option<PublishTaskKind>,
    /// 只看未终态任务(对账:该 app 现在有没有在跑的任务)
    pub active_only: Option<bool>,
}

#[derive(Debug, Deserialize, utoipa::IntoParams)]
pub struct StreamQuery {
    #[serde(default)]
    pub from_seq: u64,
}

// ---- 类型化响应(HttpResult.data 载荷;错误链已是 HttpResult shape 零改动)----

/// publish / build 立即返回(task 已创建,后台 spawn)。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct PublishTaskData {
    pub task_id: String,
    pub status: String,
}

/// get_task 返回(任务快照)。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct GetTaskData {
    pub task: PublishTaskSnapshot,
}

/// cancel_task 返回(Accepted 时 already_terminal=None;AlreadyTerminal 时 Some(true))。
#[derive(Debug, serde::Serialize, utoipa::ToSchema)]
pub struct CancelTaskData {
    pub task_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub already_terminal: Option<bool>,
    pub status: PublishTaskStatus,
}
