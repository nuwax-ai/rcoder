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
use anyhow::{Context, Result, ensure};
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
    /// The first launch in this owner process consumes fresh platform inputs
    /// (one-shot deploy declarations); relaunches after an interruption are
    /// recovery launches and must not replay them.
    pub fresh: bool,
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
        let mut discovery = self.discovery.lock().expect("discovery lock poisoned");
        let result = update(&mut discovery)?;
        record::save(&self.root.join("supervisor.json"), &*discovery)?;
        Ok(result)
    }

    fn snapshot(&self) -> Snapshot {
        self.discovery
            .lock()
            .expect("discovery lock poisoned")
            .snapshot
            .clone()
    }

    fn intent(&self) -> Intent {
        self.snapshot().intent
    }

    fn set_fence(&self, state: FenceState) {
        *self.fence.lock().expect("fence lock poisoned") = state;
        self.fence_changed.notify_waiters();
    }

    fn fence(&self) -> FenceState {
        self.fence.lock().expect("fence lock poisoned").clone()
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
        self.core.with_discovery(|discovery| {
            if discovery.snapshot.generation.as_deref() == Some(self.generation.as_str())
                && !matches!(discovery.snapshot.phase, Phase::Stopped)
            {
                discovery.snapshot.phase = Phase::Ready;
                discovery.snapshot.error = None;
                discovery.snapshot.problem = None;
                discovery.snapshot.operation_id = None;
                if discovery.snapshot.intent == Intent::Stopped {
                    discovery.snapshot.intent = Intent::Run;
                }
            }
            Ok(())
        })?;
        *self
            .core
            .management_ready
            .lock()
            .expect("ready lock poisoned") = Some(self.generation.clone());
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
        let mut state = self
            .run
            .lock()
            .expect("owner session run slot poisoned")
            .take()
            .context("owner session already running")?;
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
}

struct RunState {
    /// Aborting the accept task when the run state drops keeps the control
    /// endpoint from outliving its owner session.
    _listener_guard: AbortOnDrop<Result<()>>,
    incoming: mpsc::Receiver<Incoming>,
    core: Arc<SessionCore>,
    business: Option<ActiveBusiness>,
    cleanup: Option<AbortOnDrop<Result<()>>>,
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
        match record::reconcile(&self.core.root) {
            Ok(None) => {
                self.core.set_fence(FenceState::Clear);
                Ok(())
            }
            Ok(Some(target)) => {
                self.core.mark_pending_cleanup(format!(
                    "verifying abandoned generation {} command and engine cleanup",
                    target.value.id
                ))?;
                self.spawn_cleanup(target);
                Ok(())
            }
            Err(error) => self.core.record_problem(&error),
        }
    }

    fn spawn_cleanup(&mut self, target: record::AbandonedGeneration) {
        let adapter = self.core.cleanup_adapter.clone();
        self.cleanup = Some(AbortOnDrop(tokio::spawn(async move {
            cleanup::abandoned(target, adapter.as_ref()).await
        })));
    }

    async fn reconcile_idle(&mut self) -> Result<()> {
        if let Some(task) = &self.cleanup
            && !task.0.is_finished()
        {
            return Ok(());
        }
        if let Some(mut task) = self.cleanup.take() {
            let result = (&mut task.0)
                .await
                .context("abandoned cleanup task failed")
                .and_then(|result| result);
            match result {
                Ok(()) => {
                    self.core.set_fence(FenceState::Clear);
                    self.core.with_discovery(|discovery| {
                        if matches!(
                            discovery.snapshot.phase,
                            Phase::CleanupPending | Phase::RecoveryRequired
                        ) {
                            discovery.snapshot.problem = None;
                            discovery.snapshot.error = None;
                        }
                        Ok(())
                    })?;
                }
                Err(error) => {
                    self.core.record_problem(&error)?;
                    self.next_cleanup = tokio::time::Instant::now() + Duration::from_secs(2);
                    return Ok(());
                }
            }
        }
        if tokio::time::Instant::now() < self.next_cleanup {
            return Ok(());
        }
        match record::reconcile(&self.core.root) {
            Ok(None) => {
                self.core.set_fence(FenceState::Clear);
            }
            Ok(Some(target)) => {
                self.core.mark_pending_cleanup(format!(
                    "verifying abandoned generation {} command and engine cleanup",
                    target.value.id
                ))?;
                self.spawn_cleanup(target);
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
        let current =
            serde_json::to_vec(&*self.core.discovery.lock().expect("discovery lock poisoned"))?;
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
            if self.shutdown.is_cancelled() {
                self.core.with_discovery(|discovery| {
                    if discovery.snapshot.intent != Intent::Shutdown {
                        discovery.snapshot.intent = Intent::Shutdown;
                        discovery.snapshot.phase = Phase::Stopped;
                    }
                    Ok(())
                })?;
                return Ok(IdleFlow::Shutdown);
            }
            if self.fence_cleared() && self.relaunch_permitted() && self.launch(factory)? {
                return Ok(IdleFlow::Launched);
            }
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
                    () = self.shutdown.cancelled() => IdleEvent::Shutdown,
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
                    self.core.with_discovery(|discovery| {
                        if discovery.snapshot.intent != Intent::Shutdown {
                            discovery.snapshot.intent = Intent::Shutdown;
                            discovery.snapshot.phase = Phase::Stopped;
                        }
                        Ok(())
                    })?;
                    return Ok(IdleFlow::Shutdown);
                }
                IdleEvent::Relaunch => {}
            }
        }
    }

    fn fence_cleared(&self) -> bool {
        matches!(self.core.fence(), FenceState::Clear)
    }

    fn relaunch_permitted(&self) -> bool {
        let mut restarts = self.core.restarts.lock().expect("restart lock poisoned");
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
            supervisor,
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
        self.core.with_discovery(|discovery| {
            discovery.snapshot.generation = Some(id.clone());
            discovery.snapshot.phase = Phase::Starting;
            discovery.snapshot.error = None;
            discovery.snapshot.problem = None;
            Ok(())
        })?;
        let run = match factory(BusinessLaunch {
            generation: id.clone(),
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
                tokio::select! {
                    outcome = driver => BusinessEvent::Driver(outcome),
                    _ = &mut stopping_sleep => BusinessEvent::StopDeadline,
                    Some(incoming) = self.incoming.recv() => BusinessEvent::Request(incoming),
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
            }
        }
    }

    /// The business driver ended. Write cleanup receipts, then decide between
    /// relaunch, idle and process exit.
    async fn business_ended(&mut self, outcome: Result<Option<i32>>) -> Result<Option<i32>> {
        let exit = match outcome {
            Ok(code) => code,
            Err(error) => {
                tracing::error!(%error, "unified owner business session failed");
                Some(1)
            }
        };
        let mut business = self
            .business
            .take()
            .context("business end without an active business")?;
        let was_ready = self
            .core
            .management_ready
            .lock()
            .expect("ready lock poisoned")
            .as_deref()
            == Some(business.generation.as_str());
        if !was_ready {
            tracing::warn!(
                generation = %business.generation,
                exit = ?exit,
                "business session ended before management readiness"
            );
        }
        // One task owns the generation lock throughout cleanup; a replacement
        // cannot launch while receipts are still being written.
        let work = record::work_root(&self.core.root, &business.generation)?;
        let adapter = self.core.cleanup_adapter.clone();
        match cleanup::once(&work, &mut business.value, adapter.as_ref()).await {
            Ok(()) => {}
            Err(error) => {
                cleanup::record_failure(&work, &mut business.value, &error);
                self.core.record_problem(&error)?;
            }
        }
        let intent = self.core.intent();
        if intent == Intent::Shutdown {
            self.core.with_discovery(|discovery| {
                discovery.snapshot.phase = Phase::Stopped;
                discovery.snapshot.generation = None;
                discovery.snapshot.error = None;
                discovery.snapshot.problem = None;
                Ok(())
            })?;
            return Ok(Some(if business.stopping.is_some() {
                0
            } else {
                exit.unwrap_or(0)
            }));
        }
        if !self.restart_on_exit || !was_ready {
            // Foreground runs propagate exits; a session that never reached
            // management readiness fails fast instead of restart-looping.
            self.core.with_discovery(|discovery| {
                discovery.snapshot.phase = Phase::Stopped;
                discovery.snapshot.generation = None;
                if let Some(code) = exit {
                    discovery.snapshot.error = Some(format!("execution process exited ({code})"));
                }
                Ok(())
            })?;
            return Ok(Some(exit.unwrap_or(0)));
        }
        {
            let mut restarts = self.core.restarts.lock().expect("restart lock poisoned");
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
        let instance = self
            .core
            .discovery
            .lock()
            .expect("discovery lock poisoned")
            .instance
            .clone();
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
            let discovery = self.core.discovery.lock().expect("discovery lock poisoned");
            ensure!(
                envelope.version == control::CONTROL_VERSION
                    && envelope.instance == discovery.instance
                    && envelope.token == discovery.token,
                "supervisor request identity mismatch"
            );
        }
        let request = &envelope.request;
        ensure!(
            !request.request_id.is_empty() && request.request_id.len() <= 128,
            "invalid control request identity"
        );
        let snapshot = self.core.snapshot();
        if request.action == control::Action::Status {
            return Ok(snapshot);
        }
        {
            let discovery = self.core.discovery.lock().expect("discovery lock poisoned");
            if let Some((original, recorded)) = discovery
                .requests
                .iter()
                .find(|(r, _)| r.request_id == request.request_id)
                .map(|(r, s)| (r.clone(), s.clone()))
            {
                ensure!(
                    original.action == request.action
                        && original.expected_generation == request.expected_generation,
                    "request identity already used with different parameters"
                );
                return Ok(recorded);
            }
        }
        ensure!(
            request.expected_generation.is_none()
                || request.expected_generation == snapshot.generation,
            "execution generation changed"
        );
        if let Some(operation) = &snapshot.operation_id {
            ensure!(
                snapshot.phase == Phase::RecoveryRequired && self.business.is_none(),
                "supervisor is busy with operation {operation} ({})",
                snapshot.phase
            );
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
        self.core
            .restarts
            .lock()
            .expect("restart lock poisoned")
            .clear();
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
