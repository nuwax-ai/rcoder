//! UserApp 异步编译/发布任务:BuildTaskStore(Mutex<HashMap>)+ 状态机 + 进度事件。
//!
//! 设计参考 app_manager `ReleaseStatus`(Prepared/PendingStart/Active/Failed)状态机;
//! 进度事件用 `broadcast`(实时 SSE 推送)+ `VecDeque` ring(seq replay,断线重连)。
//! 无新重依赖:broadcast(tokio)、VecDeque(std)。
//!
//! 任务生命周期:Pending(创建)→ Running(spawn 执行)→ Completed/Failed/Cancelled。
//! cancel 通过 `cancel()` 置位 + 外部 kill 进程组(`kill_process_group`)。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};

use chrono::Utc;
use serde::Serialize;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

// 进度事件类型复用 shared_types(file-server 发送 ↔ rcoder 接收,统一 wire)。
pub use shared_types::BuildProgressEvent;

/// 历史事件 ring 容量(断线重连 seq replay)。
const RING_CAP: usize = 1000;
/// broadcast 通道容量(实时 SSE fan-out)。
const BROADCAST_CAP: usize = 256;
const TERMINAL_TASK_TTL_SECS: i64 = 24 * 60 * 60;
const MAX_RETAINED_TASKS: usize = 1_000;

pub type BuildTaskId = String;

/// 任务类型。Build = 发布打包（zip 制品）；DevStart/DevRestart = 开发闭环
/// （manifest 同核编译成功后启动/重启 dev 服务——**启停前必先编译**，新代码
/// 才生效；Completed 的制品四字段为占位空值，调用方按 status/error 消费，
/// 端口经 `GET /api/v1/userapp/dev/list` 查询）。纯开发编译不设接口——与
/// Build 同核无增量，用 `/api/v1/userapp/build`。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum BuildTaskKind {
    Build,
    DevStart,
    DevRestart,
}

/// 任务状态(镜像 app_manager ReleaseStatus 语义)。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq, utoipa::ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum BuildTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 任务快照(GET /tasks/{id} 返回)。
#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct BuildTaskSnapshot {
    pub id: BuildTaskId,
    pub app_id: String,
    pub kind: BuildTaskKind,
    pub status: BuildTaskStatus,
    pub stage: Option<String>,
    pub current_service: Option<String>,
    pub release_id: Option<String>,
    pub sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub file_name: Option<String>,
    /// 相对 workspace 根的产物路径(`builds/workspace-package-{releaseId}.zip`)——
    /// 任务创建时预生成(pending 期即有值),Java 取包 URL 直接拼段。
    pub artifact_path: Option<String>,
    pub error: Option<String>,
    pub seq: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 任务可变状态:全部收在【一把】`state` 锁后(status/seq/history 同步变更,保证一致性)。
struct TaskState {
    app_id: String,
    kind: BuildTaskKind,
    status: BuildTaskStatus,
    stage: Option<String>,
    current_service: Option<String>,
    release_id: Option<String>,
    /// build 产物包摘要(完成时填;rcoder publish 经 GET /tasks/{id} 读取做 prepare)。
    sha256: Option<String>,
    size_bytes: Option<u64>,
    file_name: Option<String>,
    /// 相对 workspace 根的产物路径(start_build_task 预生成时 set;Completed 一致覆盖)。
    artifact_path: Option<String>,
    error: Option<String>,
    /// workspace 根 (build/publish 工作区): logs/SSE 查询路径解析用。预 resolve 后存入。
    workspace_root: Option<PathBuf>,
    seq: u64,
    history: VecDeque<(u64, BuildProgressEvent)>,
    updated_at: i64,
}

/// 单个异步任务:状态 + 进度事件流 + cancel + build 进程 pid。
///
/// 并发模型:所有可变状态在【一把】`state` 锁后;`emit`/`subscribe` 各取一次锁、互不嵌套调用
/// (共享临界逻辑在自由函数 [`publish_mut`],它吃 `&mut TaskState`、无 `&self`,类型上无法再去
/// 取 state 锁),从结构上根除锁序/重入死锁。`cancelled`/`terminal_at`/`created_at`/`pid`
/// 保持在锁外:供 cancel/store/同步 on_pid 回调无锁读写。
pub struct BuildTask {
    pub id: BuildTaskId,
    /// 所属 app（锁外不可变：供 store 在 map 锁内无锁做 per-app 扫描——
    /// dev_stop 联动取消在途任务用，与 rcoder PublishTask 的 app_id 同款设计）
    pub app_id: String,
    /// 任务类型（锁外不可变，同上）
    pub kind: BuildTaskKind,
    state: Mutex<TaskState>,
    tx: broadcast::Sender<(u64, BuildProgressEvent)>,
    cancelled: AtomicBool,
    terminal_at: AtomicI64,
    created_at: i64,
    /// 当前 build 子进程 pid (cancel 时 kill_process_group 用)。
    /// AtomicU32 (0 = 未设置) 而非 Mutex: build_generic 的 on_pid 回调是同步的, 需同步写。
    pid: AtomicU32,
}

impl BuildTask {
    fn new(app_id: String, kind: BuildTaskKind) -> Arc<Self> {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAP);
        let now = Utc::now().timestamp();
        Arc::new(Self {
            id: Uuid::now_v7().simple().to_string(),
            app_id: app_id.clone(),
            kind,
            state: Mutex::new(TaskState {
                app_id,
                kind,
                status: BuildTaskStatus::Pending,
                stage: None,
                current_service: None,
                release_id: None,
                sha256: None,
                size_bytes: None,
                file_name: None,
                artifact_path: None,
                error: None,
                workspace_root: None,
                seq: 0,
                history: VecDeque::with_capacity(RING_CAP),
                updated_at: now,
            }),
            tx,
            cancelled: AtomicBool::new(false),
            terminal_at: AtomicI64::new(0),
            created_at: now,
            pid: AtomicU32::new(0),
        })
    }

    /// 当前快照(查询用)。
    pub async fn snapshot(&self) -> BuildTaskSnapshot {
        let s = self.state.lock().await;
        BuildTaskSnapshot {
            id: self.id.clone(),
            app_id: s.app_id.clone(),
            kind: s.kind,
            status: s.status,
            stage: s.stage.clone(),
            current_service: s.current_service.clone(),
            release_id: s.release_id.clone(),
            sha256: s.sha256.clone(),
            size_bytes: s.size_bytes,
            file_name: s.file_name.clone(),
            artifact_path: s.artifact_path.clone(),
            error: s.error.clone(),
            seq: s.seq,
            created_at: self.created_at,
            updated_at: s.updated_at,
        }
    }

    /// 当前状态(轻量,无完整快照)。
    pub async fn status(&self) -> BuildTaskStatus {
        self.state.lock().await.status
    }

    /// 记录 workspace 根 (start_build_task 预 resolve 后存入),供 logs/SSE 解析日志目录。
    pub async fn set_workspace_root(&self, root: PathBuf) {
        self.state.lock().await.workspace_root = Some(root);
    }

    /// 预置产物路径与 release_id（start_build_task 预生成 release_id 后存入,
    /// 快照 pending 期即可见;Completed 事件携带一致值覆盖,两处同源不漂移）。
    pub async fn set_artifact_path(&self, release_id: String, artifact_path: String) {
        let mut s = self.state.lock().await;
        s.release_id = Some(release_id);
        s.artifact_path = Some(artifact_path);
    }

    /// workspace 根; 任务 resolve 前为 None (logs handler 据此判断日志目录是否就绪)。
    pub async fn workspace_root(&self) -> Option<PathBuf> {
        self.state.lock().await.workspace_root.clone()
    }

    /// 发进度事件:取一次 state 锁 → [`publish_mut`] → 释放 → broadcast。
    /// 已 Completed/Failed/Cancelled 的任务丢弃后续事件(终态)。
    /// 发进度事件:取一次 state 锁 → [`publish_mut`] → broadcast → 释放。
    /// 已 Completed/Failed/Cancelled 的任务丢弃后续事件(终态)。
    pub async fn emit(&self, event: BuildProgressEvent) {
        let terminal = {
            let mut s = self.state.lock().await;
            let Some((seq, event, terminal)) = publish_mut(&mut s, event) else {
                return;
            };
            // broadcast 必须在持 state 锁内:与 subscribe 的"创建 receiver + 读 replay"互斥串行,
            // 否则同一事件可能既进 replay 又被 receiver 收到 → 重复(broadcast::send 非阻塞,持锁安全)。
            // 进度事件无订阅者时 send 失败属预期(接收方可能已退出)；记 debug 便于诊断
            if let Err(send_err) = self.tx.send((seq, event)) {
                tracing::warn!("progress event send failed (consumer gone): {send_err}");
            }
            terminal
        };
        if terminal {
            self.terminal_at
                .store(Utc::now().timestamp(), Ordering::Release);
        }
    }

    pub async fn is_terminal(&self) -> bool {
        matches!(
            self.state.lock().await.status,
            BuildTaskStatus::Completed | BuildTaskStatus::Failed | BuildTaskStatus::Cancelled
        )
    }

    /// 订阅:回放 ring 里 seq >= from_seq 的历史 + 实时 broadcast receiver。
    /// 一把 state 锁兜住"创建 receiver + 取 replay",保证两者之间无 emit 缝隙。
    /// 供 SSE 断线重连(带 from_seq)+ 首次订阅(from_seq=0)。
    pub async fn subscribe(
        &self,
        from_seq: u64,
    ) -> (
        Vec<(u64, BuildProgressEvent)>,
        broadcast::Receiver<(u64, BuildProgressEvent)>,
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

    /// 记录当前 build 子进程 pid (build_generic spawn 后经 on_pid 回调同步写入)。
    pub fn set_pid(&self, pid: u32) {
        self.pid.store(pid, Ordering::Relaxed);
    }

    /// 清除记录的 build pid。子进程退出(或超时被 kill)后调用,缩短 stale-pid 窗口(#2 防御性加固)。
    pub fn clear_pid(&self) {
        self.pid.store(0, Ordering::Relaxed);
    }

    /// 当前 build child 进程 pid (cancel 时 kill_process_group 用); 0 = 未设置。
    pub fn pid(&self) -> Option<u32> {
        match self.pid.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// 标记取消(外部再 kill 进程组);build 循环可检 is_cancelled 主动退出。
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

fn is_terminal_status(status: BuildTaskStatus) -> bool {
    matches!(
        status,
        BuildTaskStatus::Completed | BuildTaskStatus::Failed | BuildTaskStatus::Cancelled
    )
}

/// 临界逻辑(自由函数,吃 `&mut TaskState`、无 `&self`):终态检查 → apply → seq → ring。
/// 返回 `(seq, event, terminal)`:event 供调用方在【持 state 锁时】做 broadcast(必须持锁,
/// 与 subscribe 串行,防同一事件既进 replay 又进 broadcast);任务已终态返回 None。
/// 没有 `&self` → 类型上无法再去取 state 锁 → 根除"持锁时调 emit"的重入死锁(与 rcoder PublishTask 同构)。
fn publish_mut(
    state: &mut TaskState,
    event: BuildProgressEvent,
) -> Option<(u64, BuildProgressEvent, bool)> {
    if is_terminal_status(state.status) {
        return None;
    }
    apply_event(state, &event);
    let terminal = is_terminal_status(state.status);
    let seq = state.seq;
    state.seq += 1;
    if state.history.len() >= RING_CAP {
        state.history.pop_front();
    }
    state.history.push_back((seq, event.clone()));
    Some((seq, event, terminal))
}

fn apply_event(state: &mut TaskState, event: &BuildProgressEvent) {
    state.updated_at = Utc::now().timestamp();
    match event {
        BuildProgressEvent::Stage { stage } => {
            state.stage = Some(stage.clone());
            state.status = BuildTaskStatus::Running;
        }
        BuildProgressEvent::Building { service } => {
            state.current_service = Some(service.clone());
            state.status = BuildTaskStatus::Running;
        }
        BuildProgressEvent::BuildOk { .. } => state.current_service = None,
        BuildProgressEvent::BuildFail { service, error } => {
            state.current_service = Some(service.clone());
            state.error = Some(error.clone());
        }
        BuildProgressEvent::Log { .. } => {}
        BuildProgressEvent::Completed {
            release_id,
            sha256,
            size_bytes,
            file_name,
            artifact_path,
        } => {
            state.release_id = Some(release_id.clone());
            state.sha256 = Some(sha256.clone());
            state.size_bytes = Some(*size_bytes);
            state.file_name = Some(file_name.clone());
            state.artifact_path = Some(artifact_path.clone());
            state.status = BuildTaskStatus::Completed;
        }
        BuildProgressEvent::Failed { error } => {
            state.error = Some(error.clone());
            state.status = BuildTaskStatus::Failed;
        }
        BuildProgressEvent::Cancelled => state.status = BuildTaskStatus::Cancelled,
    }
}

/// 全局任务表(Mutex<HashMap>,内存;build 短期不需持久化,发布产物由 app_manager release index 持久)。
/// 用 tokio::sync::Mutex(无 poison,符合禁止 unwrap/expect);并发度低(任务数有限)。
pub struct BuildTaskStore {
    map: Mutex<HashMap<BuildTaskId, Arc<BuildTask>>>,
    max_retained_tasks: usize,
}

/// `BuildTaskStore::create` 容量耗尽错误(硬上限:全活跃任务达上限且无终态任务可淘汰)。
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum BuildTaskStoreError {
    #[error("build task capacity exhausted (limit={limit}); wait for an active build to finish")]
    CapacityExceeded { limit: usize },
}

impl Default for BuildTaskStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildTaskStore {
    pub fn new() -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_retained_tasks: MAX_RETAINED_TASKS,
        }
    }

    #[cfg(test)]
    fn with_max_retained_tasks(max_retained_tasks: usize) -> Self {
        Self {
            map: Mutex::new(HashMap::new()),
            max_retained_tasks,
        }
    }

    pub async fn create(
        &self,
        app_id: String,
        kind: BuildTaskKind,
    ) -> Result<Arc<BuildTask>, BuildTaskStoreError> {
        let task = BuildTask::new(app_id, kind);
        let now = Utc::now().timestamp();
        let mut map = self.map.lock().await;
        map.retain(|_, existing| {
            let terminal_at = existing.terminal_at.load(Ordering::Acquire);
            terminal_at == 0 || now.saturating_sub(terminal_at) < TERMINAL_TASK_TTL_SECS
        });
        // 硬上限:全活跃任务达上限时,优先淘汰最旧终态任务;无终态可淘汰则拒绝(不再越过上限插入,#12)。
        while map.len() >= self.max_retained_tasks {
            let Some(oldest_terminal_id) = map
                .values()
                .filter(|existing| existing.terminal_at.load(Ordering::Acquire) > 0)
                .min_by_key(|existing| existing.created_at)
                .map(|existing| existing.id.clone())
            else {
                return Err(BuildTaskStoreError::CapacityExceeded {
                    limit: self.max_retained_tasks,
                });
            };
            map.remove(&oldest_terminal_id);
        }
        map.insert(task.id.clone(), task.clone());
        Ok(task)
    }

    pub async fn get(&self, id: &str) -> Option<Arc<BuildTask>> {
        self.map.lock().await.get(id).cloned()
    }

    /// 该 app 的在途（非终态）任务列表——dev_stop 联动取消用：不取消的话，
    /// 编译中的 start/restart 任务会在编译完成后把刚停的服务重新拉起，
    /// 停止意图被异步任务推翻。map 锁内原子读 terminal_at + 锁外不可变
    /// app_id，无锁嵌套。
    pub async fn active_tasks_for_app(&self, app_id: &str) -> Vec<Arc<BuildTask>> {
        self.map
            .lock()
            .await
            .values()
            .filter(|t| t.terminal_at.load(Ordering::Acquire) == 0 && t.app_id == app_id)
            .cloned()
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn subscription_has_no_gap_between_replay_and_live_events() {
        let task = BuildTask::new("app-a".into(), BuildTaskKind::Build);
        let (replay, mut receiver) = task.subscribe(0).await;
        assert!(replay.is_empty());

        task.emit(BuildProgressEvent::Stage {
            stage: "Build".into(),
        })
        .await;
        let (seq, event) = receiver.recv().await.expect("live event");
        assert_eq!(seq, 0);
        assert!(matches!(event, BuildProgressEvent::Stage { .. }));
    }

    #[tokio::test]
    async fn concurrent_terminal_events_commit_exactly_once() {
        let task = BuildTask::new("app-a".into(), BuildTaskKind::Build);
        let completed = task.emit(BuildProgressEvent::Completed {
            release_id: "release-1".into(),
            sha256: "a".repeat(64),
            size_bytes: 1,
            file_name: "release-1.zip".into(),
            artifact_path: "builds/release-1.zip".into(),
        });
        let failed = task.emit(BuildProgressEvent::Failed {
            error: "late failure".into(),
        });
        tokio::join!(completed, failed);

        let snapshot = task.snapshot().await;
        assert_eq!(snapshot.seq, 1);
        assert!(matches!(
            snapshot.status,
            BuildTaskStatus::Completed | BuildTaskStatus::Failed
        ));
        let (replay, _) = task.subscribe(0).await;
        assert_eq!(replay.len(), 1);
    }

    #[tokio::test]
    async fn store_rejects_new_task_when_all_capacity_is_active() {
        let store = BuildTaskStore::with_max_retained_tasks(2);
        for app_id in ["app-a", "app-b"] {
            store
                .create(app_id.into(), BuildTaskKind::Build)
                .await
                .expect("active task within capacity");
        }

        let result = store.create("app-c".into(), BuildTaskKind::Build).await;
        let error = match result {
            Ok(_) => panic!("active tasks must never be silently evicted"),
            Err(error) => error,
        };
        assert_eq!(error, BuildTaskStoreError::CapacityExceeded { limit: 2 });
        assert_eq!(store.map.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn store_evicts_terminal_task_before_rejecting_new_task() {
        let store = BuildTaskStore::with_max_retained_tasks(1);
        let completed = store
            .create("app-a".into(), BuildTaskKind::Build)
            .await
            .expect("first task");
        completed
            .emit(BuildProgressEvent::Completed {
                release_id: "release-a".into(),
                sha256: "a".repeat(64),
                size_bytes: 1,
                file_name: "release-a.zip".into(),
                artifact_path: "builds/release-a.zip".into(),
            })
            .await;

        let replacement = store
            .create("app-b".into(), BuildTaskKind::Build)
            .await
            .expect("terminal task should be evicted");
        let map = store.map.lock().await;
        assert_eq!(map.len(), 1);
        assert!(map.contains_key(&replacement.id));
        assert!(!map.contains_key(&completed.id));
    }
}
