//! rcoder 侧 publish/build 异步任务:`PublishTaskStore` + `PublishTask` + 进度事件。
//!
//! 结构参考:file-server `BuildTaskStore`(`crates/file-server/src/service/userapp/tasks.rs`)
//! 与 rcoder `SessionStreamRegistry`(`grpc/session_stream_registry.rs`,broadcast+ring+seq)。
//! agent-runner 的 build 进度经 [`client`] 透传给前端(rcoder SSE),叠加 rcoder 发布阶段(Stage)。

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};

use chrono::Utc;
use serde::Serialize;
use serde_json::Value;
use tokio::sync::{Mutex, Notify, broadcast};

/// 历史事件 ring 容量(断线重连 seq replay)。
const RING_CAP: usize = 1000;
/// broadcast 通道容量(实时 SSE fan-out)。
const BROADCAST_CAP: usize = 256;
/// 终态任务在内存中保留 24h，便于前端重连查询。
const TERMINAL_TASK_TTL_SECS: i64 = 24 * 60 * 60;
/// 防止异常调用方无限创建任务。达上限时优先淘汰最旧终态任务。
const MAX_RETAINED_TASKS: usize = 1_000;

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
    /// 透传 agent-runner build 进度(Building/BuildOk/BuildFail,data=原始 JSON)。
    BuildProgress { data: Value },
    /// 任务完成(build 产 release_id;publish 发布 Active)。
    Completed { release_id: String },
    /// 任务失败。
    Failed { error: String },
    /// 任务被取消。
    Cancelled,
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

struct TaskInner {
    app_id: String,
    project_id: String,
    kind: PublishTaskKind,
    status: PublishTaskStatus,
    stage: Option<String>,
    release_id: Option<String>,
    error: Option<String>,
    created_at: i64,
    updated_at: i64,
}

/// 单个异步任务:状态 + 进度事件流 + cancel。
pub struct PublishTask {
    pub id: PublishTaskId,
    inner: Mutex<TaskInner>,
    tx: broadcast::Sender<(u64, PublishEvent)>,
    history: Mutex<VecDeque<(u64, PublishEvent)>>,
    /// 串行化状态转移、seq、history 与 broadcast，避免终态被并发覆盖。
    event_lock: Mutex<()>,
    seq: AtomicU64,
    cancelled: AtomicBool,
    cancel_notify: Notify,
    terminal_at: AtomicI64,
    created_at: i64,
    remote_build: Mutex<Option<RemoteBuildTask>>,
}

#[derive(Clone)]
pub struct RemoteBuildTask {
    pub addr: String,
    pub task_id: String,
}

impl PublishTask {
    fn new(app_id: String, project_id: String, kind: PublishTaskKind) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAP);
        let now = Utc::now().timestamp();
        Arc::new(Self {
            id: uuid::Uuid::now_v7().simple().to_string(),
            inner: Mutex::new(TaskInner {
                app_id,
                project_id,
                kind,
                status: PublishTaskStatus::Pending,
                stage: None,
                release_id: None,
                error: None,
                created_at: now,
                updated_at: now,
            }),
            tx,
            history: Mutex::new(VecDeque::with_capacity(RING_CAP)),
            event_lock: Mutex::new(()),
            seq: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
            terminal_at: AtomicI64::new(0),
            created_at: now,
            remote_build: Mutex::new(None),
        })
    }

    /// 当前快照(查询用)。
    pub async fn snapshot(&self) -> PublishTaskSnapshot {
        let inner = self.inner.lock().await;
        PublishTaskSnapshot {
            id: self.id.clone(),
            app_id: inner.app_id.clone(),
            project_id: inner.project_id.clone(),
            kind: inner.kind,
            status: inner.status,
            stage: inner.stage.clone(),
            release_id: inner.release_id.clone(),
            error: inner.error.clone(),
            seq: self.seq.load(Ordering::Relaxed),
            created_at: inner.created_at,
            updated_at: inner.updated_at,
        }
    }

    pub async fn is_terminal(&self) -> bool {
        matches!(
            self.inner.lock().await.status,
            PublishTaskStatus::Completed | PublishTaskStatus::Failed | PublishTaskStatus::Cancelled
        )
    }

    /// 发进度事件:apply 状态副作用 → seq++ → ring → broadcast。终态后丢弃后续事件。
    pub async fn emit(&self, event: PublishEvent) {
        let _event_guard = self.event_lock.lock().await;
        let mut inner = self.inner.lock().await;
        if is_terminal_status(inner.status) {
            return;
        }
        apply_event(&mut inner, &event);
        let terminal = is_terminal_status(inner.status);
        let s = self.seq.fetch_add(1, Ordering::Relaxed);
        // event_lock 已保证事件顺序，状态更新完成后无需在等待 history 锁时继续持有 inner。
        drop(inner);
        {
            let mut h = self.history.lock().await;
            if h.len() >= RING_CAP {
                h.pop_front();
            }
            h.push_back((s, event.clone()));
        }
        let _ = self.tx.send((s, event));
        if terminal {
            self.terminal_at
                .store(Utc::now().timestamp(), Ordering::Release);
        }
    }

    /// 订阅:回放 ring 里 seq >= from_seq 的历史 + 实时 broadcast receiver。
    pub async fn subscribe(
        &self,
        from_seq: u64,
    ) -> (
        Vec<(u64, PublishEvent)>,
        broadcast::Receiver<(u64, PublishEvent)>,
    ) {
        let _event_guard = self.event_lock.lock().await;
        // 必须先创建 receiver，再取 replay。event_lock 保证两者之间无 emit。
        let receiver = self.tx.subscribe();
        let replay = self
            .history
            .lock()
            .await
            .iter()
            .filter(|(s, _)| *s >= from_seq)
            .cloned()
            .collect();
        (replay, receiver)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 标记取消(外部再调 agent-runner cancel_build + 顶层 emit Cancelled)。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.cancel_notify.notify_waiters();
    }

    pub async fn cancellation_notified(&self) {
        loop {
            let notified = self.cancel_notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    pub async fn set_remote_build(&self, addr: String, task_id: String) {
        *self.remote_build.lock().await = Some(RemoteBuildTask { addr, task_id });
    }

    pub async fn remote_build(&self) -> Option<RemoteBuildTask> {
        self.remote_build.lock().await.clone()
    }
}

fn is_terminal_status(status: PublishTaskStatus) -> bool {
    matches!(
        status,
        PublishTaskStatus::Completed | PublishTaskStatus::Failed | PublishTaskStatus::Cancelled
    )
}

fn apply_event(inner: &mut TaskInner, event: &PublishEvent) {
    inner.updated_at = Utc::now().timestamp();
    match event {
        PublishEvent::Stage { stage } => {
            inner.stage = Some(stage.clone());
            inner.status = PublishTaskStatus::Running;
        }
        PublishEvent::BuildProgress { .. } => inner.status = PublishTaskStatus::Running,
        PublishEvent::Completed { release_id } => {
            inner.release_id = Some(release_id.clone());
            inner.status = PublishTaskStatus::Completed;
        }
        PublishEvent::Failed { error } => {
            inner.error = Some(error.clone());
            inner.status = PublishTaskStatus::Failed;
        }
        PublishEvent::Cancelled => inner.status = PublishTaskStatus::Cancelled,
    }
}

/// 全局任务表(内存;短期。发布产物由 app_manager release index 持久)。
pub struct PublishTaskStore {
    map: Mutex<HashMap<PublishTaskId, Arc<PublishTask>>>,
}

impl PublishTaskStore {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
        }
    }

    pub async fn create(
        &self,
        app_id: String,
        project_id: String,
        kind: PublishTaskKind,
    ) -> Arc<PublishTask> {
        let task = PublishTask::new(app_id, project_id, kind);
        let now = Utc::now().timestamp();
        let mut map = self.map.lock().await;
        map.retain(|_, existing| {
            let terminal_at = existing.terminal_at.load(Ordering::Acquire);
            terminal_at == 0 || now.saturating_sub(terminal_at) < TERMINAL_TASK_TTL_SECS
        });
        if map.len() >= MAX_RETAINED_TASKS
            && let Some(oldest_terminal_id) = map
                .values()
                .filter(|existing| existing.terminal_at.load(Ordering::Acquire) > 0)
                .min_by_key(|existing| existing.created_at)
                .map(|existing| existing.id.clone())
        {
            map.remove(&oldest_terminal_id);
        }
        map.insert(task.id.clone(), task.clone());
        task
    }

    pub async fn get(&self, id: &str) -> Option<Arc<PublishTask>> {
        self.map.lock().await.get(id).cloned()
    }
}

impl Default for PublishTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscription_has_no_gap_between_replay_and_live_events() {
        let task = PublishTask::new("app-a".into(), "app-a".into(), PublishTaskKind::Publish);
        let (replay, mut receiver) = task.subscribe(0).await;
        assert!(replay.is_empty());

        task.emit(PublishEvent::Stage {
            stage: "Build".into(),
        })
        .await;
        let (seq, event) = receiver.recv().await.expect("live event");
        assert_eq!(seq, 0);
        assert!(matches!(event, PublishEvent::Stage { .. }));
    }

    #[tokio::test]
    async fn concurrent_terminal_events_commit_exactly_once() {
        let task = PublishTask::new("app-a".into(), "app-a".into(), PublishTaskKind::Publish);
        let completed = task.emit(PublishEvent::Completed {
            release_id: "release-1".into(),
        });
        let failed = task.emit(PublishEvent::Failed {
            error: "late failure".into(),
        });
        tokio::join!(completed, failed);

        let snapshot = task.snapshot().await;
        assert_eq!(snapshot.seq, 1);
        assert!(matches!(
            snapshot.status,
            PublishTaskStatus::Completed | PublishTaskStatus::Failed
        ));
        let (replay, _) = task.subscribe(0).await;
        assert_eq!(replay.len(), 1);
    }

    #[tokio::test]
    async fn cancellation_notification_is_not_lost_when_cancelled_first() {
        let task = PublishTask::new("app-a".into(), "app-a".into(), PublishTaskKind::Build);
        task.cancel();
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            task.cancellation_notified(),
        )
        .await
        .expect("pre-existing cancellation must be observed");
    }
}
