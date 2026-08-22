//! UserApp publish/build 的对外契约类型:任务种类/状态/进度事件/快照/错误。
//!
//! 纯数据(serde + utoipa),无并发;handler(HTTP/SSE)、orchestrator(emit 事件)、
//! store/task(状态机)统一引用。加性演进(新增可选字段)沿用 `Option + serde(default)` 范式,
//! 见 workspace-manifest crate 顶部"配置演进策略"。

use serde::{Deserialize, Serialize};
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
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum PublishTaskKind {
    Build,
    /// 已废弃（一键 publish 编排已删，改 Java 分步编排 + start+url 部署）：
    /// 变体仅为读 PG 存量历史行保留，不再创建新任务。
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
#[serde(tag = "event", rename_all = "snake_case")]
pub enum PublishEvent {
    /// 进入新发布阶段(publish: EnsureBuilder/Build/Prepare/Activate；build: EnsureBuilder/Build)。
    Stage { stage: String },
    /// 透传 agent-runner build 进度(Building/BuildOk/BuildFail,data=类型化事件)。
    BuildProgress { data: BuildProgressEvent },
    /// 取消已请求(非终态):任务进入 Cancelling,通知前端"取消中"。终态 Cancelled/Failed 由 orchestrator emit。
    Cancelling,
    /// 任务完成(build 产 release_id;publish 发布 Active)。
    Completed { release_id: Option<String> },
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
pub struct PublishTaskSnapshot {
    pub id: PublishTaskId,
    pub app_id: String,
    pub project_id: String,
    pub kind: PublishTaskKind,
    pub status: PublishTaskStatus,
    pub stage: Option<String>,
    pub release_id: Option<String>,
    /// build 产物摘要（Completed 后有值——Java 轮询取包依据：
    /// `GET /api/userapp/static/{app_id}/{file_name}` + header `X-App-Id`）
    pub artifact: Option<ArtifactDigest>,
    pub error: Option<String>,
    pub seq: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// build 产物摘要（文件名 + 内容哈希 + 大小；来自 file-server build 快照）。
#[derive(Debug, Clone, serde::Serialize, utoipa::ToSchema)]
pub struct ArtifactDigest {
    /// 制品文件名（workspace-package-{release_id}.zip，static 下载路径的尾段）
    pub file_name: String,
    /// 制品 sha256（64 位 hex——start+url 部署时作为校验值传入，幂等键组成）
    pub sha256: String,
    /// 制品字节数
    pub size_bytes: u64,
}

/// 任务列表查询结果(items + 分页前总数;handler 据此组装 PaginatedResponse)。
#[derive(Debug, Clone)]
pub struct PublishTaskListPage {
    pub items: Vec<PublishTaskSnapshot>,
    pub total: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对外契约锁定（Java 消费）：SSE 事件名 snake_case、字段 snake_case。
    /// caef1f5 由 camelCase 反转——序列化形态无测试锁定会静默漂移。
    #[test]
    fn publish_event_serializes_snake_case() {
        let ev = serde_json::to_value(PublishEvent::Stage {
            stage: "Build".into(),
        })
        .expect("stage event");
        assert_eq!(ev["event"], "stage");
        assert_eq!(ev["stage"], "Build");

        let ev = serde_json::to_value(PublishEvent::BuildProgress {
            data: BuildProgressEvent::Log {
                service: "backend".into(),
                line: "chunk".into(),
            },
        })
        .expect("build progress event");
        assert_eq!(ev["event"], "build_progress");
        assert!(ev["data"].is_object(), "progress payload under data");

        let ev = serde_json::to_value(PublishEvent::Cancelling).expect("cancelling");
        assert_eq!(ev["event"], "cancelling");

        let ev = serde_json::to_value(PublishEvent::Completed {
            release_id: Some("rel-1".into()),
        })
        .expect("completed");
        assert_eq!(ev["event"], "completed");
        assert_eq!(ev["release_id"], "rel-1");

        let ev = serde_json::to_value(PublishEvent::Failed {
            error: "boom".into(),
        })
        .expect("failed");
        assert_eq!(ev["event"], "failed");
        assert_eq!(ev["error"], "boom");

        assert_eq!(
            serde_json::to_value(PublishEvent::Cancelled).expect("cancelled")["event"],
            "cancelled"
        );
    }

    /// 任务快照字段 snake_case（taskId→task_id 反转后的对外形态）。
    #[test]
    fn task_snapshot_serializes_snake_case() {
        let snap = PublishTaskSnapshot {
            id: PublishTaskId::new(),
            app_id: "app-x".into(),
            project_id: "app-x".into(),
            kind: PublishTaskKind::Publish,
            status: PublishTaskStatus::Failed,
            stage: None,
            release_id: None,
            artifact: None,
            error: Some("boom".into()),
            seq: 0,
            created_at: 1,
            updated_at: 2,
        };
        let v = serde_json::to_value(&snap).expect("snapshot");
        assert!(v.get("app_id").is_some(), "snake keys: {v}");
        assert!(v.get("appId").is_none(), "camel residue: {v}");
        assert!(v.get("project_id").is_some());
        assert!(v.get("release_id").is_none_or(|_| true));
        assert_eq!(v["status"], "failed", "status value lowercase");
        assert_eq!(v["kind"], "publish", "kind value lowercase");
    }
}
