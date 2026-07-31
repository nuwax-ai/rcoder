//! UserApp 异步编译/发布任务:BuildTaskStore(DashMap)+ 状态机 + 进度事件。
//!
//! 设计参考 app_manager `ReleaseStatus`(Prepared/PendingStart/Active/Failed)状态机;
//! 进度事件用 `broadcast`(实时 SSE 推送)+ `VecDeque` ring(seq replay,断线重连)。
//! 无新重依赖:broadcast(tokio)、DashMap、VecDeque(std)。
//!
//! 任务生命周期:Pending(创建)→ Running(spawn 执行)→ Completed/Failed/Cancelled。
//! cancel 通过 `cancel()` 置位 + 外部 kill 进程组(`kill_process_group`)。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use chrono::Utc;
use serde::Serialize;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

/// 历史事件 ring 容量(断线重连 seq replay)。
const RING_CAP: usize = 1000;
/// broadcast 通道容量(实时 SSE fan-out)。
const BROADCAST_CAP: usize = 256;

pub type BuildTaskId = String;

/// 任务类型:仅编译(发布编排已移 rcoder 侧,agent-runner 只负责 build)。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BuildTaskKind {
    Build,
}

/// 任务状态(镜像 app_manager ReleaseStatus 语义)。
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BuildTaskStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// 任务快照(GET /tasks/{id} 返回)。
#[derive(Debug, Clone, Serialize)]
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
    pub error: Option<String>,
    pub seq: u64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 进度事件(SSE 推送 + ring 持久)。每个事件自增 seq。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase", tag = "event")]
pub enum BuildProgressEvent {
    /// 进入新阶段(publish: GitCommit/Prepare/Activate/CreateApp/WaitReady/Confirm)
    Stage { stage: String },
    /// 开始编译某服务
    Building { service: String },
    /// 某服务编译成功
    BuildOk { service: String },
    /// 某服务编译失败
    BuildFail { service: String, error: String },
    /// 一行日志(实时 tail,可选)
    Log { service: String, line: String },
    /// 任务完成(build 产 release_id + 包摘要;rcoder publish 经快照读取做 prepare)。
    Completed {
        release_id: String,
        sha256: String,
        size_bytes: u64,
        file_name: String,
    },
    /// 任务失败
    Failed { error: String },
    /// 任务被取消
    Cancelled,
}

struct TaskInner {
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
    error: Option<String>,
    /// workspace 根 (build/publish 工作区): logs/SSE 查询路径解析用。预 resolve 后存入。
    workspace_root: Option<PathBuf>,
    created_at: i64,
    updated_at: i64,
}

/// 单个异步任务:状态 + 进度事件流 + cancel + build 进程 pid。
pub struct BuildTask {
    pub id: BuildTaskId,
    inner: Mutex<TaskInner>,
    tx: broadcast::Sender<BuildProgressEvent>,
    history: Mutex<VecDeque<(u64, BuildProgressEvent)>>,
    seq: AtomicU64,
    cancelled: AtomicBool,
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
            inner: Mutex::new(TaskInner {
                app_id,
                kind,
                status: BuildTaskStatus::Pending,
                stage: None,
                current_service: None,
                release_id: None,
                sha256: None,
                size_bytes: None,
                file_name: None,
                error: None,
                workspace_root: None,
                created_at: now,
                updated_at: now,
            }),
            tx,
            history: Mutex::new(VecDeque::with_capacity(RING_CAP)),
            seq: AtomicU64::new(0),
            cancelled: AtomicBool::new(false),
            pid: AtomicU32::new(0),
        })
    }

    /// 当前快照(查询用)。
    pub async fn snapshot(&self) -> BuildTaskSnapshot {
        let inner = self.inner.lock().await;
        BuildTaskSnapshot {
            id: self.id.clone(),
            app_id: inner.app_id.clone(),
            kind: inner.kind,
            status: inner.status,
            stage: inner.stage.clone(),
            current_service: inner.current_service.clone(),
            release_id: inner.release_id.clone(),
            sha256: inner.sha256.clone(),
            size_bytes: inner.size_bytes,
            file_name: inner.file_name.clone(),
            error: inner.error.clone(),
            seq: self.seq.load(Ordering::Relaxed),
            created_at: inner.created_at,
            updated_at: inner.updated_at,
        }
    }

    /// 当前状态(轻量,无完整快照)。
    pub async fn status(&self) -> BuildTaskStatus {
        self.inner.lock().await.status
    }

    /// 记录 workspace 根 (start_build_task 预 resolve 后存入),供 logs/SSE 解析日志目录。
    pub async fn set_workspace_root(&self, root: PathBuf) {
        self.inner.lock().await.workspace_root = Some(root);
    }

    /// workspace 根; 任务 resolve 前为 None (logs handler 据此判断日志目录是否就绪)。
    pub async fn workspace_root(&self) -> Option<PathBuf> {
        self.inner.lock().await.workspace_root.clone()
    }

    /// 发进度事件:apply 状态副作用 → seq++ → ring → broadcast。
    /// 已 Completed/Failed/Cancelled 的任务丢弃后续事件(终态)。
    pub async fn emit(&self, event: BuildProgressEvent) {
        if self.is_terminal().await {
            return;
        }
        self.apply_event(&event).await;
        let s = self.seq.fetch_add(1, Ordering::Relaxed);
        {
            let mut h = self.history.lock().await;
            if h.len() >= RING_CAP {
                h.pop_front();
            }
            h.push_back((s, event.clone()));
        }
        // 无订阅者时 send 报错,属正常,忽略。
        let _ = self.tx.send(event);
    }

    async fn apply_event(&self, event: &BuildProgressEvent) {
        let mut inner = self.inner.lock().await;
        inner.updated_at = Utc::now().timestamp();
        match event {
            BuildProgressEvent::Stage { stage } => {
                inner.stage = Some(stage.clone());
                inner.status = BuildTaskStatus::Running;
            }
            BuildProgressEvent::Building { service } => {
                inner.current_service = Some(service.clone());
                inner.status = BuildTaskStatus::Running;
            }
            BuildProgressEvent::BuildOk { .. } => {
                inner.current_service = None;
            }
            BuildProgressEvent::BuildFail { service, error } => {
                inner.current_service = Some(service.clone());
                inner.error = Some(error.clone());
            }
            BuildProgressEvent::Log { .. } => {}
            BuildProgressEvent::Completed {
                release_id,
                sha256,
                size_bytes,
                file_name,
            } => {
                inner.release_id = Some(release_id.clone());
                inner.sha256 = Some(sha256.clone());
                inner.size_bytes = Some(*size_bytes);
                inner.file_name = Some(file_name.clone());
                inner.status = BuildTaskStatus::Completed;
            }
            BuildProgressEvent::Failed { error } => {
                inner.error = Some(error.clone());
                inner.status = BuildTaskStatus::Failed;
            }
            BuildProgressEvent::Cancelled => {
                inner.status = BuildTaskStatus::Cancelled;
            }
        }
    }

    pub async fn is_terminal(&self) -> bool {
        matches!(
            self.inner.lock().await.status,
            BuildTaskStatus::Completed | BuildTaskStatus::Failed | BuildTaskStatus::Cancelled
        )
    }

    /// 订阅:回放 ring 里 seq >= from_seq 的历史 + 实时 broadcast receiver。
    /// 供 SSE 断线重连(带 from_seq)+ 首次订阅(from_seq=0)。
    pub async fn subscribe(
        &self,
        from_seq: u64,
    ) -> (
        Vec<(u64, BuildProgressEvent)>,
        broadcast::Receiver<BuildProgressEvent>,
    ) {
        let replay = self
            .history
            .lock()
            .await
            .iter()
            .filter(|(s, _)| *s >= from_seq)
            .cloned()
            .collect();
        (replay, self.tx.subscribe())
    }

    /// 记录当前 build 子进程 pid (build_generic spawn 后经 on_pid 回调同步写入)。
    pub fn set_pid(&self, pid: u32) {
        self.pid.store(pid, Ordering::Relaxed);
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

/// 全局任务表(Mutex<HashMap>,内存;build 短期不需持久化,发布产物由 app_manager release index 持久)。
/// 用 tokio::sync::Mutex(无 poison,符合禁止 unwrap/expect);并发度低(任务数有限)。
pub struct BuildTaskStore {
    map: Mutex<HashMap<BuildTaskId, Arc<BuildTask>>>,
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
        }
    }

    pub async fn create(&self, app_id: String, kind: BuildTaskKind) -> Arc<BuildTask> {
        let task = BuildTask::new(app_id, kind);
        self.map.lock().await.insert(task.id.clone(), task.clone());
        task
    }

    pub async fn get(&self, id: &str) -> Option<Arc<BuildTask>> {
        self.map.lock().await.get(id).cloned()
    }
}
