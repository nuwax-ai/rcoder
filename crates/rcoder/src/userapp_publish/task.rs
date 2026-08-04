//! 单个 publish/build 异步任务:状态机 + 进度事件流 + 取消。
//!
//! 所有可变状态收在【一把】`state` 锁后;`emit`/`request_cancel`/`subscribe` 各取一次锁、互不嵌套
//! (共享临界逻辑在自由函数 `publish_mut`)。结构参考 file-server `BuildTaskStore`
//! (`crates/file-server/src/service/userapp/tasks.rs`)与 rcoder `SessionStreamRegistry`
//! (`grpc/session_stream_registry.rs`,broadcast+ring+seq)。对外契约类型(事件/状态/快照)见
//! `super::types`,全局任务表见 `super::store`。agent-runner 的 build 进度经 `super::client`
//! 透传给前端(rcoder SSE),叠加发布阶段(Stage)。

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use chrono::Utc;
use tokio::sync::{Mutex, Notify, broadcast};

use super::types::{
    CancelAttempt, PublishEvent, PublishTaskId, PublishTaskKind, PublishTaskSnapshot,
    PublishTaskStatus,
};

/// 历史事件 ring 容量(断线重连 seq replay)。
const RING_CAP: usize = 1000;
/// 单个 build 进度事件(`BuildProgress.data`)序列化字节上限:超大对象会撑大 ring
/// (1000 事件 × N MB)。超限在入 ring/broadcast 前替换为截断标记(#11)。
const MAX_EVENT_BYTES: usize = 64 * 1024;
/// broadcast 通道容量(实时 SSE fan-out)。
const BROADCAST_CAP: usize = 256;

/// 任务可变状态:全部收在【一把】`state` 锁后(status/seq/history 同步变更,保证一致性)。
struct TaskState {
    app_id: String,
    project_id: String,
    kind: PublishTaskKind,
    status: PublishTaskStatus,
    stage: Option<String>,
    release_id: Option<String>,
    error: Option<String>,
    seq: u64,
    history: VecDeque<(u64, PublishEvent)>,
    updated_at: i64,
}

#[derive(Clone)]
pub struct RemoteBuildTask {
    pub addr: String,
    pub task_id: String,
}

/// 单个异步任务:状态 + 进度事件流 + cancel。
///
/// 并发模型:所有可变状态在【一把】`state` 锁后;`emit`/`request_cancel`/`subscribe` 各取一次锁、
/// 互不嵌套调用(共享临界逻辑在自由函数 `publish_mut`,它吃 `&mut TaskState`、无 `&self`,
/// 类型上无法再去取 state 锁),从结构上根除锁序/重入死锁。`cancelled`/`terminal_at`/`created_at`
/// 保持在锁外:供 orchestrator/store 无锁读。
pub struct PublishTask {
    pub id: PublishTaskId,
    state: Mutex<TaskState>,
    tx: broadcast::Sender<(u64, PublishEvent)>,
    cancelled: AtomicBool,
    cancel_notify: Notify,
    terminal_at: AtomicI64,
    created_at: i64,
    remote_build: Mutex<Option<RemoteBuildTask>>,
}

impl PublishTask {
    pub(super) fn new(app_id: String, project_id: String, kind: PublishTaskKind) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAP);
        let now = Utc::now().timestamp();
        Arc::new(Self {
            id: uuid::Uuid::now_v7().simple().to_string(),
            state: Mutex::new(TaskState {
                app_id,
                project_id,
                kind,
                status: PublishTaskStatus::Pending,
                stage: None,
                release_id: None,
                error: None,
                seq: 0,
                history: VecDeque::with_capacity(RING_CAP),
                updated_at: now,
            }),
            tx,
            cancelled: AtomicBool::new(false),
            cancel_notify: Notify::new(),
            terminal_at: AtomicI64::new(0),
            created_at: now,
            remote_build: Mutex::new(None),
        })
    }

    /// 当前快照(查询用)。
    pub async fn snapshot(&self) -> PublishTaskSnapshot {
        let s = self.state.lock().await;
        PublishTaskSnapshot {
            id: self.id.clone(),
            app_id: s.app_id.clone(),
            project_id: s.project_id.clone(),
            kind: s.kind,
            status: s.status,
            stage: s.stage.clone(),
            release_id: s.release_id.clone(),
            error: s.error.clone(),
            seq: s.seq,
            created_at: self.created_at,
            updated_at: s.updated_at,
        }
    }

    pub async fn is_terminal(&self) -> bool {
        is_terminal_status(self.state.lock().await.status)
    }

    /// 当前状态(轻量,无完整快照)。
    pub async fn status(&self) -> PublishTaskStatus {
        self.state.lock().await.status
    }

    /// 发进度事件:取一次 state 锁 → `publish_mut` → broadcast → 释放。终态后丢弃后续事件。
    pub async fn emit(&self, event: PublishEvent) {
        let terminal = {
            let mut s = self.state.lock().await;
            let Some((seq, event, terminal)) = publish_mut(&mut s, event) else {
                return;
            };
            // broadcast 必须在持 state 锁内:与 subscribe 的"创建 receiver + 读 replay"互斥串行,
            // 否则同一事件可能既进 replay 又被 receiver 收到 → 重复(broadcast::send 非阻塞,持锁安全)。
            let _ = self.tx.send((seq, event));
            terminal
        };
        if terminal {
            self.terminal_at
                .store(Utc::now().timestamp(), Ordering::Release);
        }
    }

    /// 原子取消请求:取一次 state 锁 → check 终态 → `publish_mut`(置 Cancelling) → 释放 → 置 flag+notify。
    /// 与 `emit` 是【平级兄弟】(都只调 `publish_mut`,互不调用),不再有"持锁时调 emit"的重入死锁。
    /// 返回 Accepted 表示已进入非终态 Cancelling,终态 Cancelled/Failed 由 orchestrator 完成远端
    /// 取消/回滚后收敛;返回 AlreadyTerminal 时任务已终态,携带实际状态供调用方如实回传(#5)。
    pub async fn request_cancel(&self) -> CancelAttempt {
        let mut s = self.state.lock().await;
        if is_terminal_status(s.status) {
            return CancelAttempt::AlreadyTerminal(s.status);
        }
        // check + 转移 + 广播在同一把锁内原子完成(同 emit,防 replay/broadcast 重复;
        // 也不会被并发终态在 check 与转移之间抢入)。
        let Some((seq, event, _)) = publish_mut(&mut s, PublishEvent::Cancelling) else {
            unreachable!("status was checked non-terminal above");
        };
        let _ = self.tx.send((seq, event));
        drop(s);
        // 唤醒 orchestrator 的 cancellation_notified 等待者(置 flag + notify)。
        self.cancel();
        CancelAttempt::Accepted
    }

    /// 订阅:回放 ring 里 seq >= from_seq 的历史 + 实时 broadcast receiver。
    /// 一把 state 锁兜住"创建 receiver + 取 replay",保证两者之间无 emit 缝隙。
    pub async fn subscribe(
        &self,
        from_seq: u64,
    ) -> (
        Vec<(u64, PublishEvent)>,
        broadcast::Receiver<(u64, PublishEvent)>,
    ) {
        let s = self.state.lock().await;
        let receiver = self.tx.subscribe();
        let replay = s
            .history
            .iter()
            .filter(|(seq, _)| *seq >= from_seq)
            .cloned()
            .collect();
        (replay, receiver)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 标记取消(供 orchestrator 的 cancellation_notified 感知;终态 emit 由顶层统一做)。
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

    /// 终态时间戳(0=未终态)。Acquire 载入供 store 的 TTL/淘汰决策无锁读。
    pub(super) fn terminal_at(&self) -> i64 {
        self.terminal_at.load(Ordering::Acquire)
    }

    /// 创建时间。供 store 淘汰"最旧终态任务"。
    pub(super) fn created_at(&self) -> i64 {
        self.created_at
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

/// 临界逻辑(自由函数,吃 `&mut TaskState`、无 `&self`):终态检查 → #11 截断 → apply → seq → ring。
/// 返回 `(seq, event, terminal)`:event 供调用方在【持 state 锁期间】做 broadcast(必须在锁内,
/// 与 subscribe 的"创建 receiver + 读 replay"串行,防同一事件既进 replay 又进 broadcast);
/// 任务已终态返回 None。没有 `&self` → 类型上无法再去取 state 锁 → 根除"持锁时调 emit"的重入死锁。
fn publish_mut(state: &mut TaskState, event: PublishEvent) -> Option<(u64, PublishEvent, bool)> {
    if is_terminal_status(state.status) {
        return None;
    }
    apply_event(state, &event);
    let terminal = is_terminal_status(state.status);
    let seq = state.seq;
    state.seq += 1;
    // 单事件字节上限:超大的 build 进度(如巨长 error)会撑大 ring(1000 事件 × N MB)。
    // 超限不入 ring(仍 broadcast 给实时客户端)—— 不影响终态(PublishEvent::Completed/Failed
    // 是 orchestrator 单独 emit 的另一事件,不受此限,故迟重连者仍能收到终态)(#11)。
    if let PublishEvent::BuildProgress { .. } = &event
        && let Ok(bytes) = serde_json::to_vec(&event)
        && bytes.len() > MAX_EVENT_BYTES
    {
        tracing::warn!(
            seq,
            orig_len = bytes.len(),
            max_bytes = MAX_EVENT_BYTES,
            "build progress event exceeds byte cap, skipped from ring (still broadcast)"
        );
        return Some((seq, event, terminal));
    }
    if state.history.len() >= RING_CAP {
        state.history.pop_front();
    }
    state.history.push_back((seq, event.clone()));
    Some((seq, event, terminal))
}

fn apply_event(state: &mut TaskState, event: &PublishEvent) {
    state.updated_at = Utc::now().timestamp();
    match event {
        PublishEvent::Stage { stage } => {
            state.stage = Some(stage.clone());
            state.status = PublishTaskStatus::Running;
        }
        PublishEvent::BuildProgress { .. } => state.status = PublishTaskStatus::Running,
        PublishEvent::Cancelling => state.status = PublishTaskStatus::Cancelling,
        PublishEvent::Completed { release_id } => {
            state.release_id = Some(release_id.clone());
            state.status = PublishTaskStatus::Completed;
        }
        PublishEvent::Failed { error } => {
            state.error = Some(error.clone());
            state.status = PublishTaskStatus::Failed;
        }
        PublishEvent::Cancelled => state.status = PublishTaskStatus::Cancelled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::BuildProgressEvent;

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

    #[tokio::test]
    async fn request_cancel_transitions_running_to_cancelling() {
        let task = PublishTask::new("app-a".into(), "app-a".into(), PublishTaskKind::Publish);
        task.emit(PublishEvent::Stage {
            stage: "Build".into(),
        })
        .await;
        assert_eq!(task.status().await, PublishTaskStatus::Running);

        let attempt = task.request_cancel().await;
        assert_eq!(attempt, CancelAttempt::Accepted);
        assert_eq!(task.status().await, PublishTaskStatus::Cancelling);
        assert!(!task.is_terminal().await, "Cancelling must be non-terminal");
        assert!(task.is_cancelled(), "cancelled flag must be set");
        // Cancelling 事件入历史(SSE 客户端可见"取消中")。
        let (replay, _) = task.subscribe(0).await;
        assert!(
            replay
                .iter()
                .any(|(_, ev)| matches!(ev, PublishEvent::Cancelling))
        );
    }

    #[tokio::test]
    async fn request_cancel_returns_already_terminal_for_terminal_task() {
        let task = PublishTask::new("app-a".into(), "app-a".into(), PublishTaskKind::Publish);
        task.emit(PublishEvent::Completed {
            release_id: "r1".into(),
        })
        .await;

        let attempt = task.request_cancel().await;
        assert_eq!(
            attempt,
            CancelAttempt::AlreadyTerminal(PublishTaskStatus::Completed)
        );
        assert_eq!(task.status().await, PublishTaskStatus::Completed);
    }

    #[tokio::test]
    async fn oversized_build_progress_event_skipped_from_ring() {
        let task = PublishTask::new("app-a".into(), "app-a".into(), PublishTaskKind::Publish);
        // 超长 error 字段 → 序列化超 MAX_EVENT_BYTES
        let huge = BuildProgressEvent::Failed {
            error: "x".repeat(MAX_EVENT_BYTES + 4096),
        };
        task.emit(PublishEvent::BuildProgress { data: huge }).await;

        let (replay, _) = task.subscribe(0).await;
        // 超限事件不入 ring(仍 broadcast,但 replay 不含)—— ring 内存有界(#11)。
        assert!(
            replay
                .iter()
                .all(|(_, ev)| !matches!(ev, PublishEvent::BuildProgress { .. })),
            "oversized build progress event must be skipped from ring"
        );
    }

    /// 并发 emit + request_cancel 不应死锁/卡住(回归守护:state 单锁 + publish_mut 无重入)。
    #[tokio::test]
    async fn concurrent_emit_and_request_cancel_do_not_deadlock() {
        let task = Arc::new(PublishTask::new(
            "app-a".into(),
            "app-a".into(),
            PublishTaskKind::Publish,
        ));
        let t = Arc::clone(&task);
        let emitter = tokio::spawn(async move {
            for i in 0..200_u32 {
                t.emit(PublishEvent::BuildProgress {
                    data: BuildProgressEvent::Building {
                        service: i.to_string(),
                    },
                })
                .await;
            }
        });
        let t = Arc::clone(&task);
        let canceller = tokio::spawn(async move { t.request_cancel().await });
        // 若死锁,timeout 触发失败。
        let (_, cancel_result) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            (emitter.await, canceller.await)
        })
        .await
        .expect("emit + request_cancel must not deadlock");
        let _ = cancel_result.expect("canceller task panicked");
        // 收敛:任务最终进入某个终态或 Cancelling,且不卡。
        let _ = task.status().await;
    }

    /// 并发回归守护:多 emit + 多 subscribe 并发,任一订阅者不应在 replay 与 broadcast 中
    /// 收到同一 seq 两次。这条不变量依赖"broadcast 必须在持 state 锁内、与 subscribe 串行"——
    /// 若 tx.send 被移到锁外,会在 [ring push, send] 窗口内让某订阅者既从 replay 又从 broadcast
    /// 拿到同一事件(重复)。测试在多线程下反复触发该窗口,有 bug 必然失败。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn subscribers_never_receive_duplicate_seq_under_concurrency() {
        let task = Arc::new(PublishTask::new(
            "app-a".into(),
            "app-a".into(),
            PublishTaskKind::Publish,
        ));
        let mut handles = Vec::new();

        // 生产者:持续 emit 进度事件。
        let producer = Arc::clone(&task);
        handles.push(tokio::spawn(async move {
            for i in 0..300_u64 {
                producer
                    .emit(PublishEvent::BuildProgress {
                        data: BuildProgressEvent::Building {
                            service: i.to_string(),
                        },
                    })
                    .await;
            }
        }));

        // 4 个订阅者:各自 replay + 消费广播,断言无 seq 在 replay∪broadcast 中重复。
        for _ in 0..4 {
            let t = Arc::clone(&task);
            handles.push(tokio::spawn(async move {
                let (replay, mut rx) = t.subscribe(0).await;
                let mut seen = std::collections::HashSet::new();
                for (seq, _) in replay {
                    assert!(seen.insert(seq), "duplicate seq {seq} from replay");
                }
                loop {
                    match tokio::time::timeout(std::time::Duration::from_millis(30), rx.recv())
                        .await
                    {
                        Ok(Ok((seq, _))) => {
                            assert!(seen.insert(seq), "duplicate seq {seq} from broadcast")
                        }
                        Ok(Err(broadcast::error::RecvError::Lagged(_))) => continue,
                        _ => break, // Closed 或超时(生产者停 + 静默)
                    }
                }
            }));
        }

        for h in handles {
            h.await.unwrap();
        }
    }
}
