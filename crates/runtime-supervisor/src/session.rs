//! Unified in-process owner session (app-cli runtime recovery v2, plan §5).
//!
//! The process that holds the owner lock hosts the native control channel,
//! the discovery journal, generation receipts and the reconciliation fence,
//! plus the business orchestration driven through a caller-supplied factory.
//! A fenced or stopped workspace keeps management alive: Status/Stop/Recover
//! answer on the control channel and the caller's management API keeps
//! serving, because neither depends on business recovery any more.
//!
//! Honest boundaries (plan §5.1): a hung business driver inside this process
//! cannot be force-killed from the same process. The session stops it
//! cooperatively within the graceful budget, then records a bounded problem
//! instead of manufacturing success; precise termination stays with the
//! retained driver handle or an external supervisor.
use crate::{
    CleanupCommand, cleanup,
    control::{self, Discovery, Envelope, FailureCode, Phase, Problem, Reply, Snapshot},
    epoch,
    monitor::{Owner, detach_previous_container_control, initial_recovery_launch},
    record::{self, Intent},
    worker::WorkerControl,
};
use anyhow::{Context, Result};
use std::{
    collections::VecDeque,
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{net::TcpListener, sync::mpsc, sync::oneshot};
use tokio_util::sync::CancellationToken;

/// The business driver's completion future: `Ok(None)` means "session ended
/// without a failure exit code".
pub type BusinessEnd = Pin<Box<dyn Future<Output = Result<Option<i32>>> + Send>>;

/// Reconciliation outcome for authorizing the first local business launch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FenceState {
    /// No abandoned local generation blocks this owner.
    Clear,
    /// Cleanup of an abandoned generation is pending or failing; business
    /// launch is withheld while management stays available.
    Fenced { problem: Problem },
}

pub struct SessionOptions {
    pub binding: control::Binding,
    pub policy: crate::Policy,
    /// Same-binary cleanup adapter for engine-owned programs.
    pub cleanup_adapter: Option<CleanupCommand>,
    /// Re-run the business after it exits on its own while the intent is Run
    /// (serve semantics). Foreground `run` passes false and propagates exits.
    pub restart_on_exit: bool,
    pub shutdown: CancellationToken,
}

/// One business launch requested from the session.
pub struct BusinessLaunch {
    pub generation: String,
    /// This generation's work root（work/<id>）. The session installs it as
    /// the process-level command scope before the factory runs, so every
    /// nested spawn（服务、预检、迁移、Pingap）inherits the same managed
    /// execution range（recovery v2 R1）.
    pub work_root: PathBuf,
    /// The first launch in this owner process consumes fresh platform inputs
    /// (one-shot deploy declarations); relaunches after an interruption are
    /// recovery launches and must not replay them.
    pub fresh: bool,
}

/// The currently installed business session scope（R1）. Exposed for
/// engine-receipt publication and diagnostics; never authority to signal.
#[derive(Clone, Debug)]
pub struct SessionScope {
    pub work_root: PathBuf,
    pub generation: String,
    pub supervisor_id: String,
}

static CURRENT_SCOPE: std::sync::RwLock<Option<SessionScope>> = std::sync::RwLock::new(None);

/// Read the current business session scope installed by the running owner.
pub fn current_scope() -> Option<SessionScope> {
    match CURRENT_SCOPE.read() {
        Ok(scope) => scope.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

fn install_scope(scope: Option<SessionScope>) {
    let mut slot = match CURRENT_SCOPE.write() {
        Ok(slot) => slot,
        Err(poisoned) => poisoned.into_inner(),
    };
    process_utils::command_authority::set_session_work_root(
        scope.as_ref().map(|scope| scope.work_root.clone()),
    );
    *slot = scope;
}

/// A launched business: its control adapter and driver end. The driver
/// resolves when the whole business session (recovery + orchestration loop)
/// ends; `Ok(None)` means "session ended without a failure exit code".
pub struct BusinessRun {
    pub control: Arc<dyn WorkerControl>,
    pub end: BusinessEnd,
}

pub type BusinessFactory = Box<dyn FnMut(BusinessLaunch) -> Result<BusinessRun> + Send>;

struct ActiveBusiness {
    generation: String,
    value: record::Generation,
    control: Arc<dyn WorkerControl>,
    end: BusinessEnd,
    stopping: Option<tokio::time::Instant>,
    /// Held for the business lifetime, mirroring the root guardian's lock in
    /// process mode: late guardians and cleanup callbacks use it for
    /// authorization and quiescence proof.
    _generation_lock: std::fs::File,
}

struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);
impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct Incoming {
    request: Envelope,
    reply: oneshot::Sender<Reply>,
}

struct SessionCore {
    root: PathBuf,
    policy: crate::Policy,
    discovery: Mutex<Discovery>,
    fence: Mutex<FenceState>,
    fence_changed: tokio::sync::Notify,
    relaunch_requested: tokio::sync::Notify,
    cleanup_adapter: Option<CleanupCommand>,
    restarts: Mutex<VecDeque<tokio::time::Instant>>,
    /// Latest generation whose business reported management readiness.
    management_ready: Mutex<Option<String>>,
}

impl SessionCore {
    fn with_discovery<T>(&self, update: impl FnOnce(&mut Discovery) -> Result<T>) -> Result<T> {
        // 持锁窗口只覆盖 clone 与 swap（内存操作）；durable 落盘
        //（fsync，共享盘上可达数十毫秒）在锁外执行——控制连接的快照
        // 读取不被持久化 I/O 阻塞（复核 §6）。写者串行由写路径单任务
        //（run loop / mark_ready）保证；并发写者最多造成一次后写覆盖
        // 先写的同代快照，与旧实现（锁内 save）语义等价。
        let mut next = {
            let discovery = self
                .discovery
                .lock()
                .map_err(|_| anyhow::anyhow!("discovery lock poisoned"))?;
            discovery.clone()
        };
        let result = update(&mut next)?;
        // Replay parity（R2.4）：受理中操作的每次快照推进同步进 requests
        // 登记，同请求重试因此反映实际进度与终态（monitor::persist 同款）。
        if let Some(id) = next.snapshot.operation_id.clone() {
            for (request, snapshot) in &mut next.requests {
                if request.request_id == id {
                    *snapshot = next.snapshot.clone();
                }
            }
        }
        record::save(&self.root.join("supervisor.json"), &next)?;
        match self.discovery.lock() {
            Ok(mut discovery) => *discovery = next,
            Err(poisoned) => *poisoned.into_inner() = next,
        }
        Ok(result)
    }

    fn snapshot(&self) -> Snapshot {
        match self.discovery.lock() {
            Ok(discovery) => discovery.snapshot.clone(),
            Err(poisoned) => poisoned.into_inner().snapshot.clone(),
        }
    }

    fn intent(&self) -> Intent {
        self.snapshot().intent
    }

    fn set_fence(&self, state: FenceState) {
        let mut fence = match self.fence.lock() {
            Ok(fence) => fence,
            Err(poisoned) => poisoned.into_inner(),
        };
        *fence = state;
        drop(fence);
        self.fence_changed.notify_waiters();
    }

    fn fence(&self) -> FenceState {
        match self.fence.lock() {
            Ok(fence) => fence.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn record_problem(&self, error: &anyhow::Error) -> Result<()> {
        let waiting = error
            .downcast_ref::<std::fs::TryLockError>()
            .is_some_and(|e| matches!(e, std::fs::TryLockError::WouldBlock));
        let problem = Problem {
            code: if waiting {
                FailureCode::CleanupInProgress
            } else {
                FailureCode::CleanupUnconfirmed
            },
            message: format!("{error:#}"),
        };
        let phase = if waiting {
            Phase::CleanupPending
        } else {
            Phase::RecoveryRequired
        };
        tracing::error!(
            root = %self.root.display(),
            phase = ?phase,
            "unified supervisor reconciliation fence: {}",
            problem.message
        );
        self.with_discovery(|discovery| {
            if discovery.snapshot.problem.as_ref() != Some(&problem) {
                discovery.snapshot.phase = phase;
                discovery.snapshot.error = Some(problem.message.clone());
                discovery.snapshot.problem = Some(problem.clone());
            }
            Ok(())
        })?;
        self.set_fence(FenceState::Fenced { problem });
        Ok(())
    }

    fn mark_pending_cleanup(&self, message: String) -> Result<()> {
        let problem = Problem {
            code: FailureCode::CleanupInProgress,
            message,
        };
        self.with_discovery(|discovery| {
            discovery.snapshot.phase = Phase::CleanupPending;
            discovery.snapshot.problem = Some(problem.clone());
            discovery.snapshot.error = None;
            Ok(())
        })?;
        self.set_fence(FenceState::Fenced { problem });
        Ok(())
    }
}

/// Guard that publishes management readiness for one generation. Dropping it
/// without [`Self::mark_ready`] records that the session never completed
/// management initialization.
pub struct BusinessReadyGuard {
    core: Arc<SessionCore>,
    generation: String,
    armed: bool,
}
impl BusinessReadyGuard {
    pub fn mark_ready(mut self) -> Result<()> {
        self.armed = true;
        let current = self.core.with_discovery(|discovery| {
            if discovery.snapshot.generation.as_deref() != Some(self.generation.as_str()) {
                return Ok(false);
            }
            // Management readiness is an observation, not a new Run intent.
            // A Stop accepted during initialization keeps its control identity.
            if discovery.snapshot.phase == Phase::Starting {
                discovery.snapshot.phase = Phase::Ready;
                discovery.snapshot.error = None;
                discovery.snapshot.problem = None;
                discovery.snapshot.operation_id = None;
            }
            Ok(true)
        })?;
        if current {
            *self
                .core
                .management_ready
                .lock()
                .map_err(|_| anyhow::anyhow!("ready lock poisoned"))? =
                Some(self.generation.clone());
        }
        Ok(())
    }
}
impl Drop for BusinessReadyGuard {
    fn drop(&mut self) {
        if !self.armed {
            tracing::warn!(
                generation = %self.generation,
                "business session ended before reporting management readiness"
            );
        }
    }
}

pub struct OwnerSession {
    core: Arc<SessionCore>,
    _owner: Owner,
    run: Mutex<Option<RunState>>,
}

impl OwnerSession {
    /// Bind the native control listener and publish discovery while holding
    /// the owner lock. Reconciliation of abandoned generations starts during
    /// [`OwnerSession::run`]. This never launches or stops business work, so
    /// callers may bind their management API first.
    pub async fn start(owner: Owner, options: SessionOptions) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let instance = uuid::Uuid::new_v4().to_string();
        let root = owner.scope_root().to_path_buf();
        let (mut old, rebuilt) = owner.load_discovery(&options.binding)?;
        let current = crate::domain::PhysicalDomain::from_env()
            .context("read execution domain before supervisor launch")?;
        if let Some(old) = &mut old {
            detach_previous_container_control(&root, old, current.as_ref())?;
        }
        let recovery_launch = initial_recovery_launch(&root, old.as_ref(), current.as_ref());
        let pending = old
            .as_ref()
            .filter(|d| !matches!(d.snapshot.phase, Phase::Stopped | Phase::RecoveryRequired))
            .and_then(|d| d.snapshot.operation_id.clone());
        let intent = old.as_ref().map_or(
            if rebuilt {
                Intent::Stopped
            } else {
                Intent::Run
            },
            |d| {
                if pending.is_some() || d.snapshot.intent == Intent::Stopped {
                    d.snapshot.intent
                } else {
                    Intent::Run
                }
            },
        );
        let discovery = Discovery {
            version: control::CONTROL_VERSION,
            instance: instance.clone(),
            address: listener.local_addr()?.to_string(),
            token: format!(
                "{}{}",
                uuid::Uuid::new_v4().simple(),
                uuid::Uuid::new_v4().simple()
            ),
            snapshot: Snapshot {
                version: 1,
                binding: options.binding.clone(),
                supervisor_id: instance,
                generation: old.as_ref().and_then(|d| d.snapshot.generation.clone()),
                phase: Phase::Reconciling,
                intent,
                operation_id: pending,
                error: None,
                problem: None,
            },
            requests: old.map_or_else(Vec::new, |d| d.requests),
        };
        tracing::info!(
            root = %root.display(),
            supervisor = %discovery.instance,
            control = %discovery.address,
            intent = ?intent,
            "unified in-process supervisor owner starting"
        );
        record::save(&root.join("supervisor.json"), &discovery)?;
        let core = Arc::new(SessionCore {
            root: root.clone(),
            policy: options.policy,
            discovery: Mutex::new(discovery),
            fence: Mutex::new(FenceState::Clear),
            fence_changed: tokio::sync::Notify::new(),
            relaunch_requested: tokio::sync::Notify::new(),
            cleanup_adapter: options.cleanup_adapter,
            restarts: Mutex::new(VecDeque::new()),
            management_ready: Mutex::new(None),
        });
        let (tx, incoming) = mpsc::channel::<Incoming>(32);
        let run = RunState {
            _listener_guard: AbortOnDrop(tokio::spawn(accept(listener, tx))),
            incoming,
            core: core.clone(),
            business: None,
            cleanup: None,
            next_cleanup: tokio::time::Instant::now(),
            next_discovery_check: tokio::time::Instant::now(),
            first_launch: !recovery_launch,
            restart_on_exit: options.restart_on_exit,
            shutdown: options.shutdown,
        };
        Ok(Self {
            core,
            _owner: owner,
            run: Mutex::new(Some(run)),
        })
    }

    /// Current reconciliation fence for business authorization.
    pub fn fence(&self) -> FenceState {
        self.core.fence()
    }

    /// Wait until the startup fence clears, or the budget expires. A fenced
    /// result is a diagnosis, not a failure to manage.
    pub async fn wait_fence(&self, budget: Duration) -> FenceState {
        let deadline = tokio::time::Instant::now() + budget;
        loop {
            let state = self.core.fence();
            if state == FenceState::Clear {
                return state;
            }
            if tokio::time::Instant::now() >= deadline {
                return state;
            }
            let changed = self.core.fence_changed.notified();
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return self.core.fence();
            }
        }
    }

    /// Ask the run loop to evaluate a business (re)launch. Callers bridge
    /// explicit management requests (for example an admitted runtime Start
    /// while business is stopped) into the session's intent loop.
    pub fn request_relaunch(&self) {
        self.core.relaunch_requested.notify_one();
    }

    pub fn snapshot(&self) -> Snapshot {
        self.core.snapshot()
    }

    /// Guard that publishes management readiness for the current generation.
    pub fn ready_guard(self: &Arc<Self>, generation: &str) -> BusinessReadyGuard {
        BusinessReadyGuard {
            core: self.core.clone(),
            generation: generation.to_string(),
            armed: false,
        }
    }

    /// Drive the control loop: reconcile retries, native requests, business
    /// (re)launches, cooperative stops and shutdown. Returns the process exit
    /// code implied by the terminal intent.
    pub async fn run(self: Arc<Self>, mut factory: BusinessFactory) -> Result<i32> {
        let mut state = match self.run.lock() {
            Ok(mut slot) => slot.take().context("owner session already running")?,
            Err(poisoned) => poisoned
                .into_inner()
                .take()
                .context("owner session run slot poisoned mid-start")?,
        };
        state.seed_fence()?;
        loop {
            if state.business.is_none() {
                match state.drive_idle(&mut factory).await? {
                    IdleFlow::Launched => {}
                    // Shutdown while idle has no business to drain: the
                    // pending cleanup task (if any) is resolved by the
                    // successor's reconciliation on the same receipts.
                    IdleFlow::Shutdown => return Ok(0),
                }
            }
            if let Some(exit) = state.drive_business().await? {
                return Ok(exit);
            }
        }
    }
}

enum IdleFlow {
    Launched,
    Shutdown,
}

enum IdleEvent {
    Tick,
    Request(Incoming),
    Shutdown,
    Relaunch,
}

enum BusinessEvent {
    Driver(Result<Option<i32>>),
    StopDeadline,
    Request(Incoming),
    ShutdownToken,
}

/// A background cleanup for one generation, foreign (startup fence) or the
/// owner's own just-ended business（R2.3：清理任务化，原生控制持续响应）.
/// The task owns the mutable receipt so a failure hands it back for a
/// bounded retry within the same owner（R2.1：不再依赖把本 owner PID 当
/// 旧 worker 的 reconcile 路径重试）.
struct CleanupSlot {
    generation: String,
    root: PathBuf,
    lock: std::fs::File,
    /// Receipt retained between a failed attempt and its bounded retry.
    value: Option<record::Generation>,
    /// The task returns the（possibly mutated）receipt alongside its result
    /// so a failure hands the authoritative state back for the retry.
    task: Option<AbortOnDrop<(record::Generation, Result<()>)>>,
    next_attempt: tokio::time::Instant,
}

struct RunState {
    /// Aborting the accept task when the run state drops keeps the control
    /// endpoint from outliving its owner session.
    _listener_guard: AbortOnDrop<Result<()>>,
    incoming: mpsc::Receiver<Incoming>,
    core: Arc<SessionCore>,
    business: Option<ActiveBusiness>,
    cleanup: Option<CleanupSlot>,
    next_cleanup: tokio::time::Instant,
    next_discovery_check: tokio::time::Instant,
    first_launch: bool,
    restart_on_exit: bool,
    shutdown: CancellationToken,
}

impl RunState {
    /// One initial synchronous reconcile so callers observe the startup fence
    /// immediately; retries continue while idle.
    fn seed_fence(&mut self) -> Result<()> {
        let instance = self.core.snapshot().supervisor_id;
        match record::reconcile_foreign(&self.core.root, &instance) {
            Ok(None) => {
                self.core.set_fence(FenceState::Clear);
                Ok(())
            }
            Ok(Some(target)) => {
                self.core.mark_pending_cleanup(format!(
                    "verifying abandoned generation {} command and engine cleanup",
                    target.value.id
                ))?;
                self.start_cleanup(target.value, target.root, target._lock);
                Ok(())
            }
            Err(error) => self.core.record_problem(&error),
        }
    }

    fn start_cleanup(&mut self, value: record::Generation, root: PathBuf, lock: std::fs::File) {
        self.spawn_cleanup_stage(value, root, lock);
    }

    fn spawn_cleanup_stage(
        &mut self,
        value: record::Generation,
        root: PathBuf,
        lock: std::fs::File,
    ) {
        let adapter = self.core.cleanup_adapter.clone();
        let id = value.id.clone();
        let mut owned = value;
        self.cleanup = Some(CleanupSlot {
            generation: id,
            root: root.clone(),
            lock,
            task: Some(AbortOnDrop(tokio::spawn(async move {
                let result = cleanup::once(&root, &mut owned, adapter.as_ref()).await;
                (owned, result)
            }))),
            value: None,
            next_attempt: tokio::time::Instant::now(),
        });
    }

    /// Poll the background cleanup slot. Control stays responsive throughout
    ///（R2.3）；success closes the fence and publishes the terminal snapshot
    /// for accepted Stop/Shutdown（R2.2）；failure re-arms a bounded retry
    /// within the same owner with the handed-back receipt（R2.1）.
    async fn poll_cleanup(&mut self) -> Result<()> {
        let mut slot = match self.cleanup.take() {
            Some(slot) => slot,
            None => return Ok(()),
        };
        let running = match &mut slot.task {
            Some(task) if !task.0.is_finished() => {
                self.cleanup = Some(slot);
                return Ok(());
            }
            running => running.take(),
        };
        match running {
            Some(mut task) => {
                let (returned, result) = (&mut task.0).await.context("cleanup task failed")?;
                match result {
                    Ok(()) => {
                        self.core.set_fence(FenceState::Clear);
                        let generation = slot.generation.clone();
                        let intent = self.core.intent();
                        self.core.with_discovery(|discovery| {
                            if discovery.snapshot.generation.as_deref() == Some(generation.as_str())
                            {
                                discovery.snapshot.generation = None;
                            }
                            if matches!(
                                discovery.snapshot.phase,
                                Phase::CleanupPending | Phase::RecoveryRequired | Phase::Stopping
                            ) && matches!(intent, Intent::Stopped | Intent::Shutdown)
                            {
                                // 终态只在清理成功后发布（R2.2）：受理的
                                // Stop/Shutdown 以确认的清理回执收尾，同请求
                                // 重放读取同一终态快照。
                                discovery.snapshot.phase = Phase::Stopped;
                                discovery.snapshot.problem = None;
                                discovery.snapshot.error = None;
                            } else if matches!(
                                discovery.snapshot.phase,
                                Phase::CleanupPending | Phase::RecoveryRequired
                            ) {
                                discovery.snapshot.problem = None;
                                discovery.snapshot.error = None;
                            }
                            Ok(())
                        })?;
                        tracing::info!(
                            generation = %generation,
                            "generation cleanup confirmed; fence cleared"
                        );
                    }
                    Err(error) => {
                        // Receipt handed back by the task drives the bounded
                        // retry; never publish Stopped or success here.
                        slot.value = Some(returned);
                        slot.next_attempt = tokio::time::Instant::now() + Duration::from_secs(2);
                        self.cleanup = Some(slot);
                        self.core.record_problem(&error)?;
                    }
                }
            }
            None => {
                // Backoff elapsed → respawn the retry stage from the retained
                // receipt; the generation lock transfers with the slot.
                if tokio::time::Instant::now() >= slot.next_attempt && slot.value.is_some() {
                    let CleanupSlot {
                        value, root, lock, ..
                    } = slot;
                    self.spawn_cleanup_stage(value.expect("checked Some above"), root, lock);
                } else {
                    self.cleanup = Some(slot);
                }
            }
        }
        Ok(())
    }

    fn cleanup_pending(&self) -> bool {
        self.cleanup.is_some()
    }

    /// Idle-loop reconciliation step: poll the background cleanup, then look
    /// for newly abandoned FOREIGN generations（own generations are tracked
    /// in the slot machinery, R2.1）.
    async fn reconcile_idle(&mut self) -> Result<()> {
        self.poll_cleanup().await?;
        if self.cleanup.is_some() {
            return Ok(());
        }
        if tokio::time::Instant::now() < self.next_cleanup {
            return Ok(());
        }
        let instance = self.core.snapshot().supervisor_id;
        match record::reconcile_foreign(&self.core.root, &instance) {
            Ok(None) => {
                self.core.set_fence(FenceState::Clear);
            }
            Ok(Some(target)) => {
                self.core.mark_pending_cleanup(format!(
                    "verifying abandoned generation {} command and engine cleanup",
                    target.value.id
                ))?;
                self.start_cleanup(target.value, target.root, target._lock);
            }
            Err(error) => {
                self.core.record_problem(&error)?;
                self.next_cleanup = tokio::time::Instant::now() + Duration::from_secs(2);
            }
        }
        Ok(())
    }

    fn repair_discovery(&mut self) -> Result<()> {
        let path = self.core.root.join("supervisor.json");
        let current = {
            let discovery = match self.core.discovery.lock() {
                Ok(discovery) => discovery,
                Err(poisoned) => poisoned.into_inner(),
            };
            serde_json::to_vec(&*discovery)?
        };
        match std::fs::read(&path) {
            Ok(bytes) if bytes == current => return Ok(()),
            Ok(bytes) => crate::monitor::preserve_discovery(&path, &bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("check supervisor discovery"),
        }
        self.core.with_discovery(|_| Ok(()))
    }

    /// Idle loop while no business runs: wait for fence clearance, native
    /// requests, relaunch or shutdown. A Stopped intent still launches the
    /// management session (idle serve); the business body suppresses actual
    /// business start from the snapshot intent.
    async fn drive_idle(&mut self, factory: &mut BusinessFactory) -> Result<IdleFlow> {
        loop {
            // Shutdown completes only after the background cleanup settles
            //（R2.2：不因进程退出把未确认清理写成 Stopped）。
            if self.shutdown.is_cancelled() && !self.cleanup_pending() {
                self.core.with_discovery(|discovery| {
                    if discovery.snapshot.intent != Intent::Shutdown {
                        discovery.snapshot.intent = Intent::Shutdown;
                    }
                    if discovery.snapshot.phase != Phase::Stopped
                        && discovery.snapshot.problem.is_none()
                    {
                        discovery.snapshot.phase = Phase::Stopped;
                    }
                    Ok(())
                })?;
                return Ok(IdleFlow::Shutdown);
            }
            if !self.shutdown.is_cancelled()
                && self.fence_cleared()
                && !self.cleanup_pending()
                && self.relaunch_permitted()
                && self.launch(factory)?
            {
                return Ok(IdleFlow::Launched);
            }
            let shutdown_latched = self.core.intent() == Intent::Shutdown;
            let event = {
                let mut tick = tokio::time::interval(Duration::from_millis(200));
                tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                // Consume the immediate first tick so timing comes from the
                // select below, not a busy loop.
                tick.tick().await;
                let relaunch = self.core.relaunch_requested.notified();
                tokio::pin!(relaunch);
                tokio::select! {
                    _ = tick.tick() => IdleEvent::Tick,
                    Some(incoming) = self.incoming.recv() => IdleEvent::Request(incoming),
                    // 已取消的令牌每次立即就绪；闩锁后禁用本分支，让 tick
                    // 正常推进清理轮询（否则 poll_cleanup 被分支饥饿饿死）。
                    () = self.shutdown.cancelled(), if !shutdown_latched => {
                        IdleEvent::Shutdown
                    }
                    _ = &mut relaunch => IdleEvent::Relaunch,
                }
            };
            match event {
                IdleEvent::Tick => {
                    if tokio::time::Instant::now() >= self.next_discovery_check {
                        self.repair_discovery()?;
                        self.next_discovery_check =
                            tokio::time::Instant::now() + Duration::from_secs(2);
                    }
                    self.reconcile_idle().await?;
                }
                IdleEvent::Request(incoming) => {
                    self.handle_incoming(incoming)?;
                }
                IdleEvent::Shutdown => {
                    // 闩锁 shutdown 意图一次；此后清理由 tick 轮询推进，
                    // 顶部出口在清理收敛后返回。
                    self.core.with_discovery(|discovery| {
                        if discovery.snapshot.intent != Intent::Shutdown {
                            discovery.snapshot.intent = Intent::Shutdown;
                        }
                        Ok(())
                    })?;
                }
                IdleEvent::Relaunch => {
                    // 显式请求（运行操作受理/Recover）触发的新一轮业务
                    // 启动重置重启预算：预算只约束自动重试风暴，不应把
                    // 用户请求挡在门外（停摆的 owner 无法消费已受理操作）。
                    if let Ok(mut restarts) = self.core.restarts.lock() {
                        restarts.clear();
                    }
                }
            }
        }
    }

    fn fence_cleared(&self) -> bool {
        matches!(self.core.fence(), FenceState::Clear)
    }

    fn relaunch_permitted(&self) -> bool {
        let mut restarts = match self.core.restarts.lock() {
            Ok(restarts) => restarts,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = tokio::time::Instant::now();
        while restarts
            .front()
            .is_some_and(|at| now.duration_since(*at) > self.core.policy.restart_window)
        {
            restarts.pop_front();
        }
        restarts.len() < self.core.policy.restart_limit
    }

    /// Attempt one business launch. Returns false when the factory refused;
    /// the idle loop retries after its own backoff.
    fn launch(&mut self, factory: &mut BusinessFactory) -> Result<bool> {
        let intent = self.core.intent();
        let id = uuid::Uuid::new_v4().to_string();
        let work = record::work_root(&self.core.root, &id)?;
        process_utils::command_context::create_durable_directory(&work)?;
        process_utils::command_authority::Gate::try_acquire(&work)?.initialize()?;
        let generation_lock = record::lock(&work.join("generation.lock"))?;
        let supervisor = self.core.snapshot().supervisor_id;
        let value = record::Generation {
            version: 1,
            id: id.clone(),
            supervisor: supervisor.clone(),
            token: uuid::Uuid::new_v4().to_string(),
            intent,
            phase: record::GenerationPhase::Pending,
            worker_pid: Some(std::process::id()),
            exit_code: None,
            error: None,
            physical_domain: crate::domain::PhysicalDomain::from_env()?,
            process_epoch: epoch::current(),
        };
        record::save(&work.join("generation.json"), &value)?;
        let fresh = self.first_launch;
        // R1：安装进程级会话命令范围——业务会话内全部嵌套 spawn 的受管
        // 命令（服务/预检/迁移/Pingap）据此落 generation 作用域。
        install_scope(Some(SessionScope {
            work_root: work.clone(),
            generation: id.clone(),
            supervisor_id: supervisor,
        }));
        self.core.with_discovery(|discovery| {
            discovery.snapshot.generation = Some(id.clone());
            discovery.snapshot.phase = Phase::Starting;
            discovery.snapshot.error = None;
            discovery.snapshot.problem = None;
            Ok(())
        })?;
        let run = match factory(BusinessLaunch {
            generation: id.clone(),
            work_root: work.clone(),
            fresh,
        }) {
            Ok(run) => run,
            Err(error) => {
                // The factory refused this launch; revoke the pending
                // authorization so nothing may consume it later.
                let mut revoked = value;
                revoked.phase = record::GenerationPhase::Revoked;
                record::save(&work.join("generation.json"), &revoked)?;
                self.core.with_discovery(|discovery| {
                    discovery.snapshot.generation = None;
                    discovery.snapshot.phase = Phase::RecoveryRequired;
                    discovery.snapshot.error = Some(format!("business launch failed: {error:#}"));
                    Ok(())
                })?;
                // 计入重启预算：否则工厂持续失败时 drive_idle 以 200ms
                // 间隔无限重试（忙循环 + 日志风暴）。预算耗尽后仅在显式
                // 请求（Recover/relaunch 通知）时再尝试。
                self.core
                    .restarts
                    .lock()
                    .map_err(|_| anyhow::anyhow!("restart lock poisoned"))?
                    .push_back(tokio::time::Instant::now());
                install_scope(None);
                return Ok(false);
            }
        };
        let mut running = value;
        running.phase = record::GenerationPhase::Running;
        record::save(&work.join("generation.json"), &running)?;
        tracing::info!(
            generation = %id,
            fresh,
            "unified owner business session launched"
        );
        self.business = Some(ActiveBusiness {
            generation: id,
            value: running,
            control: run.control,
            end: run.end,
            stopping: None,
            _generation_lock: generation_lock,
        });
        self.first_launch = false;
        Ok(true)
    }

    /// Business-active loop: native requests, cooperative stop deadlines and
    /// driver completion. Returns `Some(exit)` when the process should exit.
    async fn drive_business(&mut self) -> Result<Option<i32>> {
        loop {
            if self.shutdown.is_cancelled() && self.core.intent() != Intent::Shutdown {
                self.begin_stop(Intent::Shutdown)?;
            }
            let event = {
                let business = self
                    .business
                    .as_mut()
                    .context("business loop without a business")?;
                let stop_deadline = business.stopping;
                let driver = business.end.as_mut();
                let stopping_sleep = async {
                    match stop_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::pin!(stopping_sleep);
                let shutdown = self.shutdown.cancelled();
                tokio::pin!(shutdown);
                let already_shutting = self.core.intent() == Intent::Shutdown;
                tokio::select! {
                    outcome = driver => BusinessEvent::Driver(outcome),
                    _ = &mut stopping_sleep => BusinessEvent::StopDeadline,
                    Some(incoming) = self.incoming.recv() => BusinessEvent::Request(incoming),
                    // R2.6：shutdown 令牌直接唤醒本 select——业务 future
                    // 长期 pending 时不再依赖外部偶合驱动停止。guard 防止
                    // 已取消令牌对 select 的分支饥饿（每次立即就绪会饿死
                    // driver/停止期限的推进）。
                    () = &mut shutdown, if !already_shutting => {
                        BusinessEvent::ShutdownToken
                    }
                }
            };
            match event {
                BusinessEvent::Driver(outcome) => {
                    return self.business_ended(outcome).await;
                }
                BusinessEvent::StopDeadline => {
                    self.stop_deadline_exceeded()?;
                }
                BusinessEvent::Request(incoming) => {
                    self.handle_incoming(incoming)?;
                }
                BusinessEvent::ShutdownToken => {
                    if self.core.intent() != Intent::Shutdown {
                        self.begin_stop(Intent::Shutdown)?;
                    }
                }
            }
        }
    }

    /// The business driver ended（R2 rework）. The generation's physical
    /// cleanup runs as a background task that retains the generation lock;
    /// native control keeps answering throughout. Terminal Stop/Shutdown
    /// snapshots are published only from the confirmed cleanup result
    ///（poll_cleanup）, never from here.
    async fn business_ended(&mut self, outcome: Result<Option<i32>>) -> Result<Option<i32>> {
        let exit = match outcome {
            Ok(code) => code,
            Err(error) => {
                tracing::error!(%error, "unified owner business session failed");
                Some(1)
            }
        };
        let business = self
            .business
            .take()
            .context("business end without an active business")?;
        let ActiveBusiness {
            generation,
            mut value,
            _generation_lock: lock,
            ..
        } = business;
        let was_ready = self
            .core
            .management_ready
            .lock()
            .map_err(|_| anyhow::anyhow!("ready lock poisoned"))?
            .as_deref()
            == Some(generation.as_str());
        if !was_ready {
            tracing::warn!(
                generation = %generation,
                exit = ?exit,
                "business session ended before management readiness"
            );
        }
        // R1：业务结束即卸下会话命令范围（清理引擎子进程经自身 env 运行）。
        install_scope(None);
        let intent = self.core.intent();
        // Hand the whole generation（receipt + lock）to the background
        // cleanup; the slot machinery retries transient engine failures
        // inside this owner instead of wedging on the live owner PID.
        let root = record::work_root(&self.core.root, &generation)?;
        self.core.mark_pending_cleanup(format!(
            "confirming generation {generation} command and engine cleanup after business end"
        ))?;
        // The in-process business driver completing IS this owner's evidence
        // that the execution ended; cleanup confirms the physical range.
        value.phase = record::GenerationPhase::Draining;
        self.spawn_cleanup_stage(value, root, lock);
        if intent == Intent::Shutdown {
            // Exit decision waits for the cleanup slot（drive_idle gates the
            // Shutdown return on it）——清理未确认不发布 Stopped/成功。
            return Ok(None);
        }
        if !self.restart_on_exit {
            // Foreground run: propagate the exit code; the generation stays
            // Draining on disk（honest）for the successor to reconcile.
            self.core.with_discovery(|discovery| {
                if let Some(code) = exit {
                    discovery.snapshot.error = Some(format!("execution process exited ({code})"));
                }
                Ok(())
            })?;
            return Ok(Some(exit.unwrap_or(0)));
        }
        // serve/recovery semantics（R3）：初始化失败也保持 owner 存活——
        // 后台清理关闭 fence 后按重启预算重试；预算耗尽停在
        // RecoveryRequired，管理与 Stop 全程在线。
        {
            let mut restarts = self
                .core
                .restarts
                .lock()
                .map_err(|_| anyhow::anyhow!("restart lock poisoned"))?;
            restarts.push_back(tokio::time::Instant::now());
        }
        if !self.relaunch_permitted() {
            self.core.with_discovery(|discovery| {
                discovery.snapshot.phase = Phase::RecoveryRequired;
                discovery.snapshot.generation = None;
                discovery.snapshot.error = Some("business restart budget exhausted".into());
                Ok(())
            })?;
            return Ok(None);
        }
        self.core.with_discovery(|discovery| {
            if let Some(code) = exit {
                discovery.snapshot.error = Some(format!("execution process exited ({code})"));
            }
            Ok(())
        })?;
        Ok(None)
    }

    fn stop_deadline_exceeded(&mut self) -> Result<()> {
        let business = self
            .business
            .as_mut()
            .context("stop deadline without a business")?;
        let message = format!(
            "business session did not stop within the graceful budget; \
             generation {} remains draining",
            business.generation
        );
        tracing::error!(%message, "cooperative business stop exceeded its budget");
        self.core.with_discovery(|discovery| {
            discovery.snapshot.phase = Phase::CleanupPending;
            discovery.snapshot.problem = Some(Problem {
                code: FailureCode::CleanupInProgress,
                message: message.clone(),
            });
            Ok(())
        })?;
        // Re-arm a bounded re-evaluation instead of spinning; the hung-driver
        // boundary is documented in the module header.
        business.stopping = Some(tokio::time::Instant::now() + Duration::from_secs(2));
        Ok(())
    }

    fn begin_stop(&mut self, intent: Intent) -> Result<()> {
        self.core.with_discovery(|discovery| {
            discovery.snapshot.intent = intent;
            discovery.snapshot.phase = Phase::Stopping;
            discovery.snapshot.problem = None;
            Ok(())
        })?;
        if let Some(business) = &mut self.business
            && business.stopping.is_none()
        {
            // Close admission independently of a possibly hung business.
            let work = record::work_root(&self.core.root, &business.generation)?;
            if let Err(error) = process_utils::command_authority::Gate::try_acquire(&work)
                .and_then(|gate| gate.close())
            {
                self.core.with_discovery(|discovery| {
                    discovery.snapshot.error =
                        Some(format!("command admission closure pending: {error:#}"));
                    Ok(())
                })?;
            }
            let grace = if self.core.policy.negotiate_shutdown_grace {
                self.core
                    .policy
                    .graceful_stop
                    .max(business.control.shutdown_grace())
            } else {
                self.core.policy.graceful_stop
            };
            business.stopping = Some(tokio::time::Instant::now() + grace);
            let control = business.control.clone();
            tokio::spawn(async move {
                if let Err(error) = control.shutdown().await {
                    tracing::warn!(%error, "business control shutdown path failed");
                }
            });
        }
        Ok(())
    }

    fn handle_incoming(&mut self, incoming: Incoming) -> Result<()> {
        let outcome = self.handle_request(&incoming.request);
        let snapshot = match &outcome {
            Ok(snapshot) => snapshot.clone(),
            Err(_) => self.core.snapshot(),
        };
        let instance = match self.core.discovery.lock() {
            Ok(discovery) => discovery.instance.clone(),
            Err(poisoned) => poisoned.into_inner().instance.clone(),
        };
        drop(incoming.reply.send(Reply {
            instance,
            snapshot,
            error: outcome.err().as_ref().map(Problem::from_error),
        }));
        // A Recover accepted while idle must re-evaluate a launch; while a
        // business runs it first stops the current session cooperatively.
        if self.business.is_none()
            && matches!(incoming.request.request.action, control::Action::Recover)
        {
            self.core.relaunch_requested.notify_one();
        }
        Ok(())
    }

    fn handle_request(&mut self, envelope: &Envelope) -> Result<Snapshot> {
        {
            let discovery = match self.core.discovery.lock() {
                Ok(discovery) => discovery,
                Err(poisoned) => poisoned.into_inner(),
            };
            if envelope.version != control::CONTROL_VERSION
                || envelope.instance != discovery.instance
                || envelope.token != discovery.token
            {
                return Err(Problem {
                    code: FailureCode::IdentityChanged,
                    message: "supervisor request identity mismatch".into(),
                }
                .into());
            }
        }
        let request = &envelope.request;
        if request.request_id.is_empty() || request.request_id.len() > 128 {
            return Err(Problem {
                code: FailureCode::InvalidRequest,
                message: "invalid control request identity".into(),
            }
            .into());
        }
        let snapshot = self.core.snapshot();
        if request.action == control::Action::Status {
            return Ok(snapshot);
        }
        {
            let discovery = match self.core.discovery.lock() {
                Ok(discovery) => discovery,
                Err(poisoned) => poisoned.into_inner(),
            };
            if let Some((original, recorded)) = discovery
                .requests
                .iter()
                .find(|(r, _)| r.request_id == request.request_id)
                .map(|(r, s)| (r.clone(), s.clone()))
            {
                if original.action != request.action
                    || original.expected_generation != request.expected_generation
                {
                    return Err(Problem {
                        code: FailureCode::IdentityChanged,
                        message: "request identity already used with different parameters".into(),
                    }
                    .into());
                }
                return Ok(recorded);
            }
        }
        if request.expected_generation.is_some()
            && request.expected_generation != snapshot.generation
        {
            return Err(Problem {
                code: FailureCode::IdentityChanged,
                message: "execution generation changed".into(),
            }
            .into());
        }
        if let Some(operation) = &snapshot.operation_id
            && (snapshot.phase != Phase::RecoveryRequired || self.business.is_some())
        {
            return Err(Problem {
                code: FailureCode::Busy,
                message: format!(
                    "supervisor is busy with operation {operation} ({})",
                    snapshot.phase
                ),
            }
            .into());
        }
        let intent = match request.action {
            control::Action::Recover => snapshot.intent,
            control::Action::StopWork => Intent::Stopped,
            control::Action::Shutdown => Intent::Shutdown,
            control::Action::Status => return Ok(snapshot),
        };
        self.core.with_discovery(|discovery| {
            discovery.snapshot.operation_id = Some(request.request_id.clone());
            discovery
                .requests
                .push((request.clone(), discovery.snapshot.clone()));
            discovery.snapshot.intent = intent;
            discovery.snapshot.phase = Phase::Stopping;
            Ok(())
        })?;
        if let Ok(mut restarts) = self.core.restarts.lock() {
            restarts.clear();
        }
        self.next_cleanup = tokio::time::Instant::now();
        self.begin_stop(intent)?;
        Ok(self.core.snapshot())
    }
}

async fn accept(listener: TcpListener, tx: mpsc::Sender<Incoming>) -> Result<()> {
    let slots = Arc::new(tokio::sync::Semaphore::new(32));
    loop {
        let (mut stream, _) = listener.accept().await?;
        let Ok(permit) = slots.clone().try_acquire_owned() else {
            continue;
        };
        let tx = tx.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let result = async {
                let request: Envelope = control::receive(&mut stream).await?;
                let (reply, receiver) = oneshot::channel();
                tx.try_send(Incoming { request, reply })
                    .context("supervisor control capacity reached")?;
                let response = tokio::time::timeout(Duration::from_secs(3), receiver).await??;
                control::send(&mut stream, &response).await
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, "supervisor control connection ended");
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{Action, Request};

    async fn session(root: &std::path::Path) -> Arc<OwnerSession> {
        Arc::new(
            OwnerSession::start(
                Owner::try_acquire(root).unwrap().unwrap(),
                SessionOptions {
                    binding: control::Binding {
                        component: "app-cli".into(),
                        resource: root.to_path_buf(),
                    },
                    policy: crate::Policy::default(),
                    cleanup_adapter: None,
                    restart_on_exit: true,
                    shutdown: CancellationToken::new(),
                },
            )
            .await
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn failed_discovery_commit_does_not_publish_unaccepted_state() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path()).await;
        let before = serde_json::to_value(session.snapshot()).unwrap();
        // Deterministic publication failure, independent of user permissions.
        std::fs::remove_file(root.path().join("supervisor.json")).unwrap();
        std::fs::create_dir(root.path().join("supervisor.json")).unwrap();
        assert!(
            session
                .core
                .with_discovery(|discovery| {
                    discovery.snapshot.intent = Intent::Stopped;
                    discovery.snapshot.phase = Phase::Stopping;
                    discovery.snapshot.operation_id = Some("unaccepted".into());
                    Ok(())
                })
                .is_err()
        );
        assert_eq!(serde_json::to_value(session.snapshot()).unwrap(), before);
    }

    #[tokio::test]
    async fn late_ready_report_preserves_stop_and_current_generation() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path()).await;
        session
            .core
            .with_discovery(|discovery| {
                discovery.snapshot.generation = Some("current".into());
                discovery.snapshot.intent = Intent::Stopped;
                discovery.snapshot.phase = Phase::Stopping;
                discovery.snapshot.operation_id = Some("stop".into());
                Ok(())
            })
            .unwrap();
        let before = serde_json::to_value(session.snapshot()).unwrap();
        session.ready_guard("current").mark_ready().unwrap();
        assert_eq!(serde_json::to_value(session.snapshot()).unwrap(), before);
        session.ready_guard("older").mark_ready().unwrap();
        assert_eq!(
            session.core.management_ready.lock().unwrap().as_deref(),
            Some("current"),
            "a stale callback must not replace the current readiness identity"
        );
        session
            .core
            .with_discovery(|discovery| {
                discovery.snapshot.phase = Phase::Starting;
                discovery.snapshot.operation_id = None;
                Ok(())
            })
            .unwrap();
        session.ready_guard("current").mark_ready().unwrap();
        assert_eq!(session.snapshot().phase, Phase::Ready);
        assert_eq!(session.snapshot().intent, Intent::Stopped);
    }

    #[tokio::test]
    async fn control_rejections_keep_the_native_wire_error_codes() {
        let root = tempfile::tempdir().unwrap();
        let session = session(root.path()).await;
        let mut state = session.run.lock().unwrap().take().unwrap();
        let discovery = session.core.discovery.lock().unwrap().clone();
        let envelope = || Envelope {
            version: control::CONTROL_VERSION,
            instance: discovery.instance.clone(),
            token: discovery.token.clone(),
            request: Request::new(Action::StopWork),
        };
        let mut wrong_owner = envelope();
        wrong_owner.instance = "another-owner".into();
        let mut invalid_request = envelope();
        invalid_request.request.request_id.clear();
        let mut wrong_generation = envelope();
        wrong_generation.request.expected_generation = Some("another-generation".into());
        for (request, expected) in [
            (wrong_owner, FailureCode::IdentityChanged),
            (invalid_request, FailureCode::InvalidRequest),
            (wrong_generation, FailureCode::IdentityChanged),
        ] {
            let error = state.handle_request(&request).unwrap_err();
            assert_eq!(Problem::from_error(&error).code, expected);
        }
        session
            .core
            .with_discovery(|discovery| {
                discovery.snapshot.operation_id = Some("already-stopping".into());
                discovery.snapshot.phase = Phase::Stopping;
                Ok(())
            })
            .unwrap();
        let error = state.handle_request(&envelope()).unwrap_err();
        assert_eq!(Problem::from_error(&error).code, FailureCode::Busy);
    }
}
