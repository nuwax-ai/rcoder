//! UserApp publish/build 的对外契约类型:任务种类/状态/进度事件/快照/错误。
//!
//! 纯数据(serde + utoipa),无并发;handler(HTTP/SSE)、orchestrator(emit 事件)、
//! store/task(状态机)统一引用。加性演进(新增可选字段)沿用 `Option + serde(default)` 范式,
//! 见 workspace-manifest crate 顶部"配置演进策略"。

use serde::Serialize;
use shared_types::BuildProgressEvent;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PublishTaskStoreError {
    #[error("publish task capacity exhausted (limit={limit}); wait for an active task to finish")]
    CapacityExceeded { limit: usize },
    /// 同一 app 已有活跃 publish/build 任务(U2 并发早拒绝)。携带活跃任务 id 便于排障。
    #[error(
        "app {app_id} already has an active publish/build task (task_id={task_id}); wait for it to finish or cancel it"
    )]
    AppBusy { app_id: String, task_id: String },
    /// 持久化后端故障（M5：PG 模式 create/落库失败；不降级为纯内存，如实 500）
    #[error("publish task persistence backend: {0}")]
    Backend(String),
}

impl PublishTaskKind {
    /// PG 行的 kind 字符串（与 serde lowercase 一致）
    pub(crate) fn as_pg_str(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Publish => "publish",
        }
    }
}

impl PublishTaskStatus {
    /// PG 行的 state 字符串（与 serde lowercase 一致）
    pub(crate) fn as_pg_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Cancelling => "cancelling",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// PG 行 state 字符串 → 状态（未知值按 Failed 收敛，附告警由调用方处理）
    pub(crate) fn from_pg_str(s: &str) -> Self {
        match s {
            "pending" => Self::Pending,
            "running" => Self::Running,
            "cancelling" => Self::Cancelling,
            "completed" => Self::Completed,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }
}

pub type PublishTaskId = String;

/// 任务类型:仅触发 agent-runner build / 全流程发布。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PublishTaskKind {
    Build,
    Publish,
}

/// 任务状态。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PublishTaskStatus {
    Pending,
    Running,
    /// 取消已请求、远端取消/回滚进行中(非终态)。终态由 orchestrator 收敛为 Cancelled/Failed。
    Cancelling,
    Completed,
    Failed,
    Cancelled,
}

/// 进度事件(给前端 SSE)。agent-runner build 进度原样透传(`BuildProgress.data`)。
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase", tag = "event")]
pub enum PublishEvent {
    /// 进入新发布阶段(publish: EnsureApp/Prepare/Activate/WaitReady/Confirm)。
    Stage { stage: String },
    /// 透传 agent-runner build 进度(Building/BuildOk/BuildFail,data=类型化事件)。
    BuildProgress { data: BuildProgressEvent },
    /// 取消已请求(非终态):任务进入 Cancelling,通知前端"取消中"。终态 Cancelled/Failed 由 orchestrator emit。
    Cancelling,
    /// 任务完成(build 产 release_id;publish 发布 Active)。
    Completed { release_id: String },
    /// 任务失败。
    Failed { error: String },
    /// 任务被取消。
    Cancelled,
}

/// `request_cancel` 的结果(原子取消请求)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelAttempt {
    /// 已接受:任务转入 Cancelling(非终态),orchestrator 将做远端取消/回滚后 emit 终态。
    Accepted,
    /// 任务已终态,不可取消。携带实际终态供调用方如实回传(避免 #5 撒谎窗口)。
    AlreadyTerminal(PublishTaskStatus),
}

/// 任务快照(GET /tasks/{id} 返回)。
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PublishTaskSnapshot {
    pub id: PublishTaskId,
    pub app_id: String,
    pub project_id: String,
    pub kind: PublishTaskKind,
    pub status: PublishTaskStatus,
    pub stage: Option<String>,
    pub release_id: Option<String>,
    pub error: Option<String>,
    pub seq: u64,
    pub created_at: i64,
    pub updated_at: i64,
}
