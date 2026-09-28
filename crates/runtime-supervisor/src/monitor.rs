use crate::{
    control::{self, Action, Discovery, Envelope, FailureCode, Phase, Problem, Reply, Snapshot},
    guardian,
    record::{self, Generation, GenerationPhase, Intent},
    worker,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::VecDeque,
    ffi::OsString,
    fs::File,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub struct Policy {
    pub probe_interval: Duration,
    pub probe_timeout: Duration,
    pub unresponsive_for: Duration,
    pub initialization_timeout: Duration,
    pub graceful_stop: Duration,
    /// Allow a worker to extend grace for non-interactive shutdowns.
    pub negotiate_shutdown_grace: bool,
    pub restart_limit: usize,
    pub restart_window: Duration,
}
impl Default for Policy {
    fn default() -> Self {
        Self {
            probe_interval: Duration::from_secs(2),
            probe_timeout: Duration::from_secs(2),
            unresponsive_for: Duration::from_secs(15),
            initialization_timeout: Duration::from_secs(30),
            graceful_stop: Duration::from_secs(10),
            negotiate_shutdown_grace: true,
            restart_limit: 3,
            restart_window: Duration::from_secs(600),
        }
    }
}
pub struct Options {
    pub binding: Option<control::Binding>,
    pub worker_args: Vec<OsString>,
    /// One-shot deployment inputs must not be replayed on automatic recovery.
    pub recovery_remove_env: Vec<OsString>,
    /// Same-binary cleanup adapter for programs owned by an external engine.
    /// The guardian runs it after worker/command exit, before acknowledging stop.
    pub external_cleanup_args: Option<Vec<OsString>>,
    pub policy: Policy,
    pub shutdown: CancellationToken,
    /// One-shot foreground commands preserve their normal exit behavior.
    pub restart_on_exit: bool,
}
impl Options {
    pub fn new(worker_args: Vec<OsString>) -> Self {
        Self {
            binding: None,
            worker_args,
            recovery_remove_env: Vec::new(),
            external_cleanup_args: None,
            policy: Policy::default(),
            shutdown: CancellationToken::new(),
            restart_on_exit: true,
        }
    }
}
pub struct Owner {
    _lock: File,
    root: PathBuf,
}
impl Owner {
    /// Explicit shutdown after a supervisor crash, while holding its owner lock.
    /// Evidence must cover every local generation before publishing completion.
    pub async fn stop_offline(
        self,
        binding: &control::Binding,
        request: &control::Request,
    ) -> Result<Snapshot> {
        // Parent death closes pipes first; guardians still need a bounded drain
        // window. Wait only for a retained OS lock, never reinterpret corruption
        // or missing cleanup evidence as success.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.try_stop_offline(binding, request) {
                Err(error)
                    if error
                        .downcast_ref::<std::fs::TryLockError>()
                        .is_some_and(|e| matches!(e, std::fs::TryLockError::WouldBlock))
                        && Instant::now() < deadline =>
                {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                result => return result,
            }
        }
    }
    fn try_stop_offline(
        &self,
        binding: &control::Binding,
        request: &control::Request,
    ) -> Result<Snapshot> {
        ensure!(
            !request.request_id.is_empty() && request.request_id.len() <= 128,
            "invalid control request identity"
        );
        ensure!(
            matches!(request.action, Action::Shutdown | Action::StopWork),
            "offline action must stop execution"
        );
        let (old, _) = self.load_discovery(binding)?;
        let mut discovery = old.unwrap_or_else(|| inactive_discovery(binding.clone()));
        if let Some((original, snapshot)) = discovery
            .requests
            .iter()
            .find(|(r, _)| r.request_id == request.request_id)
        {
            ensure!(
                original == request,
                "request identity already used with different parameters"
            );
            if matches!(snapshot.phase, Phase::Ready | Phase::Stopped) {
                return Ok(snapshot.clone());
            }
        }
        ensure!(
            request.expected_generation.is_none()
                || request.expected_generation == discovery.snapshot.generation,
            "execution generation changed"
        );
        record::reconcile(&self.root)?;
        detach_previous_container_control(&self.root, &mut discovery)?;
        if let Some(id) = &discovery.snapshot.operation_id {
            for (original, snapshot) in &mut discovery.requests {
                if &original.request_id == id {
                    snapshot.phase = Phase::RecoveryRequired;
                    snapshot.error = Some("supervisor exited before control completed".into());
                }
            }
        }
        discovery.snapshot.intent = if request.action == Action::StopWork {
            Intent::Stopped
        } else {
            Intent::Shutdown
        };
        discovery.snapshot.phase = Phase::Stopped;
        discovery.snapshot.error = None;
        discovery.snapshot.problem = None;
        discovery.snapshot.operation_id = Some(request.request_id.clone());
        discovery
            .requests
            .retain(|(r, _)| r.request_id != request.request_id);
        discovery
            .requests
            .push((request.clone(), discovery.snapshot.clone()));
        record::save(&self.root.join("supervisor.json"), &discovery)?;
        Ok(discovery.snapshot)
    }
    pub fn try_acquire(root: &Path) -> Result<Option<Self>> {
        process_utils::command_context::create_durable_directory(root)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("owner.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                _lock: file,
                root: std::fs::canonicalize(root).context("canonicalize supervisor scope")?,
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Discovery locates the management endpoint; generation receipts prove
    /// process cleanup. Only the owner-lock holder may rebuild discovery.
    fn load_discovery(&self, binding: &control::Binding) -> Result<(Option<Discovery>, bool)> {
        let path = self.root.join("supervisor.json");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // Also covers a crash between preserving a damaged record and
                // publishing its replacement. Never replay one-shot deployment
                // inputs just because discovery was lost.
                return Ok((None, self.root.join("work").try_exists()?));
            }
            Err(error) => return Err(error).context("read supervisor discovery"),
        };
        let old: Discovery = match serde_json::from_slice(&bytes) {
            Ok(old) => old,
            Err(error) => {
                // A newer schema may add fields rejected by Discovery's strict
                // decoder. Inspect readable identity/version before classifying
                // the record as damaged, rather than erasing a valid new schema.
                if let Ok(raw) = serde_json::from_slice::<serde_json::Value>(&bytes) {
                    if let Some(version) = raw.get("version").and_then(serde_json::Value::as_u64) {
                        ensure!(
                            version == 1 || version == u64::from(control::CONTROL_VERSION),
                            "unsupported supervisor receipt"
                        );
                    }
                    if let Some(raw_binding) = raw.pointer("/snapshot/binding")
                        && let Ok(recorded) =
                            serde_json::from_value::<control::Binding>(raw_binding.clone())
                    {
                        ensure!(
                            &recorded == binding,
                            "supervisor scope belongs to another resource"
                        );
                    }
                }
                preserve_discovery(&path)?;
                tracing::warn!(%error, "rebuilding damaged supervisor discovery under owner lock");
                return Ok((None, true));
            }
        };
        ensure!(
            matches!(old.version, 1 | control::CONTROL_VERSION),
            "unsupported supervisor receipt"
        );
        ensure!(
            &old.snapshot.binding == binding,
            "supervisor scope belongs to another resource"
        );
        Ok((Some(old), false))
    }

    pub async fn run(self, options: Options) -> Result<i32> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let instance = uuid::Uuid::new_v4().to_string();
        let binding = options.binding.clone().unwrap_or(control::Binding {
            component: "runtime".into(),
            resource: self.root.clone(),
        });
        let (mut old, rebuilt) = self.load_discovery(&binding)?;
        if let Some(old) = &mut old {
            detach_previous_container_control(&self.root, old)?;
        }
        let current = match crate::domain::PhysicalDomain::from_env() {
            Ok(domain) => domain,
            Err(error) => {
                tracing::warn!(%error, "execution domain unreadable; first launch keeps recovery semantics");
                None
            }
        };
        let recovery_launch = initial_recovery_launch(&self.root, old.as_ref(), current.as_ref());
        // Resume an accepted control after parent death. Explicit launch after
        // a completed shutdown starts a new session; it cannot discard a Stop
        // still waiting for quiescence or for the worker's durable acknowledgement.
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
        let mut state = State {
            root: self.root.clone(),
            policy: options.policy.clone(),
            discovery: Discovery {
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
                    binding,
                    supervisor_id: instance,
                    generation: old.as_ref().and_then(|d| d.snapshot.generation.clone()),
                    phase: Phase::Reconciling,
                    intent,
                    operation_id: pending.clone(),
                    error: None,
                    problem: None,
                },
                requests: old.map_or_else(Vec::new, |d| d.requests),
            },
            recovery_launch,
            child: None,
            stopping: None,
            probe: None,
            stop_ack: None,
            next_probe: Instant::now(),
            started: Instant::now(),
            last_tick: Instant::now(),
            next_discovery_check: Instant::now() + Duration::from_secs(2),
            failed_since: None,
            failures: 0,
            restarts: VecDeque::new(),
            restart_allowed: true,
            observed_ready: false,
            initial_launch: true,
        };
        state.persist()?;
        let (tx, mut rx) = mpsc::channel::<Incoming>(32);
        let listener_task = tokio::spawn(accept(listener, tx));
        let mut listener_guard = AbortOnDrop(listener_task);
        let launch = guardian::Launch {
            program: std::env::current_exe()?,
            args: options.worker_args,
            cwd: std::env::current_dir()?,
            remove_env: Vec::new(),
            external_cleanup_args: options.external_cleanup_args,
        };
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                result = &mut listener_guard.0 => {
                    // Never keep a live worker behind a dead control endpoint.
                    // Dropping State closes the exact guardian pipe; its retained
                    // Child and command guardians finish cleanup independently.
                    return Err(result.context("supervisor listener task failed")?
                        .err().unwrap_or_else(|| anyhow::anyhow!("supervisor listener exited")));
                }
                _ = tick.tick() => {
                    if let Some(code) = state.tick(&launch, options.restart_on_exit, &options.recovery_remove_env).await? { return Ok(code); }
                }
                Some(incoming) = rx.recv() => {
                    let outcome = state.request(&incoming.request);
                    let snapshot = outcome.as_ref().ok().cloned().unwrap_or_else(|| state.discovery.snapshot.clone());
                    drop(incoming.reply.send(Reply { instance: state.discovery.instance.clone(), snapshot, error: outcome.err().as_ref().map(Problem::from_error) }));
                }
                _ = options.shutdown.cancelled(), if state.discovery.snapshot.intent != Intent::Shutdown => {
                    // Physical stop still proceeds if recording the intent fails.
                    if let Err(error) = state.begin_stop(Intent::Shutdown) {
                        state.discovery.snapshot.error = Some(format!("persist shutdown intent: {error:#}"));
                        state.restart_allowed = false;
                    }
                }
            }
        }
    }
}

fn preserve_discovery(path: &Path) -> Result<()> {
    let backup = path.with_extension(format!("corrupt-{}.json", uuid::Uuid::new_v4().simple()));
    std::fs::rename(path, &backup).context("preserve damaged supervisor discovery")?;
    tracing::warn!(backup = %backup.display(), "preserved replaced supervisor discovery");
    Ok(())
}

fn inactive_discovery(binding: control::Binding) -> Discovery {
    let instance = uuid::Uuid::new_v4().to_string();
    Discovery {
        version: control::CONTROL_VERSION,
        instance: instance.clone(),
        address: String::new(),
        token: String::new(),
        snapshot: Snapshot {
            version: 1,
            binding,
            supervisor_id: instance,
            generation: None,
            phase: Phase::Reconciling,
            intent: Intent::Stopped,
            operation_id: None,
            error: None,
            problem: None,
        },
        requests: Vec::new(),
    }
}

/// First-launch recovery classification, container-scoped. Records that belong
/// to a previous container were reset by [`detach_previous_container_control`]
/// before this runs: those processes died with the container, so the first
/// launch here is an explicit platform launch and one-shot deployment inputs
/// redeclared in the environment are fresh intent, not a replay. Leftovers
/// from THIS container (owner crash, in-place container restart) keep
/// recovery semantics and never replay those inputs.
fn initial_recovery_launch(
    root: &Path,
    old: Option<&Discovery>,
    current: Option<&crate::domain::PhysicalDomain>,
) -> bool {
    match old {
        Some(discovery) => discovery.snapshot.phase != Phase::Stopped,
        None => record::current_container_work_exists_with(root, current),
    }
}

/// A control request targets one container's captured process tree. Preserve
/// its unknown result as history, but never replay an old Shutdown or let its
/// pending operation occupy the replacement container's management slot.
fn detach_previous_container_control(root: &Path, discovery: &mut Discovery) -> Result<()> {
    let Some(id) = discovery.snapshot.generation.as_deref() else {
        return Ok(());
    };
    if !record::belongs_to_previous_container(root, id)? {
        return Ok(());
    }
    for (_, snapshot) in &mut discovery.requests {
        if !matches!(snapshot.phase, Phase::Ready | Phase::Stopped) {
            snapshot.phase = Phase::RecoveryRequired;
            snapshot.error = Some("control result belongs to a previous container".into());
            snapshot.problem = Some(Problem {
                code: FailureCode::IdentityChanged,
                message: "container replaced before control completion; original result preserved"
                    .into(),
            });
        }
    }
    discovery.snapshot.generation = None;
    discovery.snapshot.operation_id = None;
    discovery.snapshot.phase = Phase::Stopped;
    discovery.snapshot.error = None;
    discovery.snapshot.problem = None;
    if discovery.snapshot.intent == Intent::Shutdown {
        discovery.snapshot.intent = Intent::Run;
    }
    Ok(())
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
                let request = control::receive(&mut stream).await?;
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
struct State {
    root: PathBuf,
    policy: Policy,
    discovery: Discovery,
    recovery_launch: bool,
    child: Option<guardian::Child>,
    stopping: Option<Instant>,
    probe: Option<tokio::task::JoinHandle<Result<worker::Observation>>>,
    stop_ack: Option<tokio::task::JoinHandle<Result<worker::Observation>>>,
    next_probe: Instant,
    started: Instant,
    last_tick: Instant,
    next_discovery_check: Instant,
    failed_since: Option<Instant>,
    failures: u32,
    restarts: VecDeque<Instant>,
    restart_allowed: bool,
    observed_ready: bool,
    initial_launch: bool,
}
impl State {
    fn work(&self) -> Result<PathBuf> {
        record::work_root(
            &self.root,
            self.discovery
                .snapshot
                .generation
                .as_deref()
                .context("worker generation missing")?,
        )
    }
    fn persist(&mut self) -> Result<()> {
        if let Some(id) = &self.discovery.snapshot.operation_id {
            for (request, snapshot) in &mut self.discovery.requests {
                if &request.request_id == id {
                    *snapshot = self.discovery.snapshot.clone();
                }
            }
        }
        record::save(&self.root.join("supervisor.json"), &self.discovery)
    }
    fn request(&mut self, envelope: &Envelope) -> Result<Snapshot> {
        if envelope.version != control::CONTROL_VERSION
            || envelope.instance != self.discovery.instance
            || envelope.token != self.discovery.token
        {
            return Err(Problem {
                code: FailureCode::IdentityChanged,
                message: "supervisor request identity mismatch".into(),
            }
            .into());
        }
        let request = &envelope.request;
        if request.request_id.is_empty() || request.request_id.len() > 128 {
            return Err(Problem {
                code: FailureCode::InvalidRequest,
                message: "invalid control request identity".into(),
            }
            .into());
        }
        if request.action == Action::Status {
            return Ok(self.discovery.snapshot.clone());
        }
        if let Some((original, snapshot)) = self
            .discovery
            .requests
            .iter()
            .find(|(r, _)| r.request_id == request.request_id)
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
            return Ok(snapshot.clone());
        }
        if request.expected_generation.is_some()
            && request.expected_generation != self.discovery.snapshot.generation
        {
            return Err(Problem {
                code: FailureCode::IdentityChanged,
                message: "execution generation changed; inspect current owner before retrying"
                    .into(),
            }
            .into());
        }
        // A failed attempt remains replayable as failure, but cannot reserve
        // the control slot forever. A retry still has to reconcile the exact
        // process receipts before it can start a successor.
        if let Some(operation) = &self.discovery.snapshot.operation_id
            && (self.discovery.snapshot.phase != Phase::RecoveryRequired || self.child.is_some())
        {
            return Err(Problem {
                code: FailureCode::Busy,
                message: format!(
                    "supervisor is busy with operation {operation} ({})",
                    self.discovery.snapshot.phase
                ),
            }
            .into());
        }
        let intent = match request.action {
            Action::Recover => self.discovery.snapshot.intent,
            Action::StopWork => Intent::Stopped,
            Action::Shutdown => Intent::Shutdown,
            Action::Status => return Ok(self.discovery.snapshot.clone()),
        };
        let previous = self.discovery.clone();
        self.discovery.snapshot.operation_id = Some(request.request_id.clone());
        self.discovery
            .requests
            .push((request.clone(), self.discovery.snapshot.clone()));
        // Receipt failure cannot manufacture an accepted queued operation.
        self.discovery.snapshot.intent = intent;
        self.discovery.snapshot.phase = Phase::Stopping;
        if let Err(error) = self.persist() {
            self.discovery = previous;
            return Err(error);
        }
        self.restarts.clear();
        self.restart_allowed = true;
        self.begin_stop(intent)?;
        Ok(self.discovery.snapshot.clone())
    }
    fn begin_stop(&mut self, intent: Intent) -> Result<()> {
        self.discovery.snapshot.problem = None;
        self.discovery.snapshot.intent = intent;
        self.discovery.snapshot.phase = Phase::Stopping;
        if let Some(task) = self.probe.take() {
            task.abort();
        }
        if self.stopping.is_none() {
            self.stopping = Some(Instant::now() + self.policy.graceful_stop);
            if self.child.is_some() {
                let work = self.work()?;
                // Close admission independently of the possibly hung worker.
                // A busy guardian cannot prevent Stop acceptance; retry while
                // its already captured process is being stopped.
                if let Err(error) = process_utils::command_authority::Gate::try_acquire(&work)
                    .and_then(|gate| gate.close())
                {
                    self.discovery.snapshot.error =
                        Some(format!("command admission closure pending: {error:#}"));
                }
                self.stop_ack = Some(tokio::spawn(worker_shutdown(work)));
            }
        }
        self.persist()
    }
    fn record_cleanup_problem(&mut self, error: &anyhow::Error) -> Result<()> {
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
        if self.discovery.snapshot.problem.as_ref() != Some(&problem) {
            self.discovery.snapshot.phase = if waiting {
                Phase::CleanupPending
            } else {
                Phase::RecoveryRequired
            };
            self.discovery.snapshot.error = Some(problem.message.clone());
            self.discovery.snapshot.problem = Some(problem);
            self.persist()?;
        }
        Ok(())
    }
    async fn tick(
        &mut self,
        launch: &guardian::Launch,
        restart_on_exit: bool,
        recovery_remove_env: &[OsString],
    ) -> Result<Option<i32>> {
        let now = Instant::now();
        if now.duration_since(self.last_tick) > self.policy.probe_interval * 4 {
            self.failed_since = None;
            self.failures = 0;
            if let Some(task) = self.probe.take() {
                task.abort();
            }
            self.next_probe = now; // fresh challenge after sleep/scheduling starvation
        }
        self.last_tick = now;
        if now >= self.next_discovery_check {
            self.repair_discovery()?;
            self.next_discovery_check = now + Duration::from_secs(2);
        }
        if self.stopping.is_some() && self.child.is_some() {
            let work = self.work()?;
            if let Err(error) = process_utils::command_authority::Gate::try_acquire(&work)
                .and_then(|gate| gate.close())
            {
                let message = format!("command admission closure pending: {error:#}");
                if self.discovery.snapshot.error.as_deref() != Some(&message) {
                    self.discovery.snapshot.error = Some(message);
                    self.persist()?;
                }
            }
        }
        if self
            .stop_ack
            .as_ref()
            .is_some_and(|task| task.is_finished())
            && let Some(task) = self.stop_ack.take()
            && let Ok(Ok(observation)) = task.await
            && let Some(deadline) = self.stopping.as_mut()
        {
            // A responsive adapter may already have a longer, service-derived
            // cleanup budget. Negotiate once; a hung adapter gets the fallback.
            if self.policy.negotiate_shutdown_grace
                && let Some(declared) = now.checked_add(observation.shutdown_grace)
            {
                *deadline = (*deadline).max(declared);
            }
        }
        if self.child.is_some() {
            // 收据可见性（2026-09-28）：共享挂载上周期读可能短暂 NotFound。
            // 一次缺失不退出监督循环（`?` 会终止整个 Owner::run，丢掉 Child
            // 与控制入口），也不得当作已清理；短窗内重读，持续缺失上报
            // Problem 并保留监督。非 NotFound（损坏/权限）照常传播。
            let generation = match record::generation(&self.work()?) {
                Ok(generation) => Some(generation),
                Err(error) if process_utils::observe::is_not_found(&error) => {
                    tracing::warn!(error = %error, "generation receipt briefly unreadable");
                    None
                }
                Err(error) => return Err(error),
            };
            if let Some(generation) = generation
                && generation.phase == GenerationPhase::Draining
            {
                let problem = Problem {
                    code: FailureCode::CleanupInProgress,
                    message: generation.error.unwrap_or_else(|| {
                        "guardian is verifying command and external engine cleanup".into()
                    }),
                };
                if self.discovery.snapshot.problem.as_ref() != Some(&problem)
                    || self.discovery.snapshot.phase != Phase::CleanupPending
                {
                    self.discovery.snapshot.phase = Phase::CleanupPending;
                    self.discovery.snapshot.error = Some(problem.message.clone());
                    self.discovery.snapshot.problem = Some(problem);
                    self.persist()?;
                }
            }
        }
        if let Some(child) = &mut self.child {
            if self.stopping.is_some_and(|deadline| now >= deadline) {
                child.lease.take();
                self.discovery.snapshot.phase = Phase::CleanupPending;
            }
            if let Some(status) = child.process.try_wait()? {
                let exit = status.code().unwrap_or(1);
                self.child.take();
                if let Some(task) = self.probe.take() {
                    task.abort();
                }
                if let Some(task) = self.stop_ack.take() {
                    task.abort();
                }
                let id = self
                    .discovery
                    .snapshot
                    .generation
                    .clone()
                    .context("exited generation missing")?;
                if let Err(error) = record::verify_quiescent(&self.root, &id) {
                    self.record_cleanup_problem(&error)?;
                    return Ok(None);
                }
                if self.discovery.snapshot.intent == Intent::Shutdown
                    || !restart_on_exit
                    || self.initial_launch
                {
                    self.discovery.snapshot.phase = Phase::Stopped;
                    self.discovery.snapshot.problem = None;
                    self.discovery.snapshot.error = None;
                    self.persist()?;
                    return Ok(Some(if self.stopping.is_some() { 0 } else { exit }));
                }
                if self.stopping.is_none() {
                    self.restarts.push_back(now);
                    self.discovery.snapshot.error =
                        Some(format!("execution process exited ({exit})"));
                }
            }
        }
        if self.child.is_none() {
            if let Err(error) = record::reconcile(&self.root) {
                self.record_cleanup_problem(&error)?;
                return Ok(None);
            }
            self.discovery.snapshot.problem = None;
            if self.discovery.snapshot.intent == Intent::Shutdown {
                self.discovery.snapshot.phase = Phase::Stopped;
                self.persist()?;
                return Ok(Some(0));
            }
            while self
                .restarts
                .front()
                .is_some_and(|t| now.duration_since(*t) > self.policy.restart_window)
            {
                self.restarts.pop_front();
            }
            if !self.restart_allowed || self.restarts.len() >= self.policy.restart_limit {
                if self.discovery.snapshot.phase != Phase::RecoveryRequired {
                    self.discovery.snapshot.phase = Phase::RecoveryRequired;
                    self.persist()?;
                }
                return Ok(None);
            }
            let replacing = self.recovery_launch;
            let id = uuid::Uuid::new_v4().to_string();
            let work = record::work_root(&self.root, &id)?;
            process_utils::command_context::create_durable_directory(&work)?;
            process_utils::command_authority::Gate::try_acquire(&work)?.initialize()?;
            record::save(
                &work.join("generation.json"),
                &Generation {
                    version: 1,
                    id: id.clone(),
                    supervisor: self.discovery.instance.clone(),
                    token: uuid::Uuid::new_v4().to_string(),
                    intent: self.discovery.snapshot.intent,
                    phase: GenerationPhase::Pending,
                    worker_pid: None,
                    exit_code: None,
                    error: None,
                    physical_domain: crate::domain::PhysicalDomain::from_env()?,
                    process_epoch: crate::epoch::current(),
                },
            )?;
            self.discovery.snapshot.generation = Some(id);
            self.discovery.snapshot.phase = Phase::Starting;
            self.persist()?;
            let recovery_launch = guardian::Launch {
                program: launch.program.clone(),
                args: launch.args.clone(),
                cwd: launch.cwd.clone(),
                remove_env: if replacing {
                    recovery_remove_env.to_vec()
                } else {
                    launch.remove_env.clone()
                },
                external_cleanup_args: launch.external_cleanup_args.clone(),
            };
            self.child = Some(guardian::spawn(&work, &recovery_launch).await?);
            self.recovery_launch = true;
            self.started = now;
            self.stopping = None;
            self.observed_ready = false;
            self.failed_since = None;
            self.failures = 0;
            self.next_probe = now;
        }
        if self.stopping.is_some() {
            return Ok(None);
        }
        if self.probe.as_ref().is_some_and(|task| task.is_finished())
            && let Some(task) = self.probe.take()
        {
            let outcome = task
                .await
                .context("worker challenge task failed")
                .and_then(|r| r);
            if let Ok(observation) = &outcome {
                self.initial_launch = false;
                self.failures = 0;
                self.failed_since = None;
                self.observed_ready = true;
                if observation.ready && self.discovery.snapshot.phase != Phase::Ready {
                    self.discovery.snapshot.phase = Phase::Ready;
                    self.discovery.snapshot.error = None;
                    self.discovery.snapshot.problem = None;
                    self.persist()?;
                    // The operation is now terminal. Preserve its final snapshot for replay.
                    self.discovery.snapshot.operation_id = None;
                    // Stopped is a one-time bootstrap instruction. The CLI
                    // has durably consumed it before publishing control;
                    // subsequent explicit starts use its business store.
                    if self.discovery.snapshot.intent == Intent::Stopped {
                        self.discovery.snapshot.intent = Intent::Run;
                    }
                    self.persist()?;
                }
            } else if self.observed_ready
                || now.duration_since(self.started) >= self.policy.initialization_timeout
            {
                self.failures += 1;
                let since = *self.failed_since.get_or_insert(now);
                if self.failures >= 3 && now.duration_since(since) >= self.policy.unresponsive_for {
                    self.restarts.push_back(now);
                    self.discovery.snapshot.error = outcome
                        .err()
                        .map(|e| format!("control unresponsive: {e:#}"));
                    self.begin_stop(self.discovery.snapshot.intent)?;
                }
            }
        }
        if self.probe.is_none() && self.stopping.is_none() && now >= self.next_probe {
            let work = self.work()?;
            let budget = self.policy.probe_timeout;
            self.probe = Some(tokio::spawn(async move {
                tokio::time::timeout(budget, worker::challenge(&work, false))
                    .await
                    .context("worker challenge timed out")?
            }));
            self.next_probe = now + self.policy.probe_interval;
        }
        Ok(None)
    }

    fn repair_discovery(&mut self) -> Result<()> {
        // The live owner is authoritative for endpoint discovery. Repair from
        // memory without rotating identity, stopping work, or trusting a stale
        // PID. Contenders never rewrite this file while owner.lock is held.
        let path = self.root.join("supervisor.json");
        match std::fs::read(&path) {
            Ok(bytes) if bytes == serde_json::to_vec(&self.discovery)? => return Ok(()),
            Ok(_) => preserve_discovery(&path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("check supervisor discovery"),
        }
        self.persist()
    }
}

async fn worker_shutdown(work: PathBuf) -> Result<worker::Observation> {
    worker::challenge(&work, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebuilding_discovery_preserves_binding_version_and_io_errors() {
        let dir = tempfile::tempdir().unwrap();
        let owner = Owner::try_acquire(dir.path()).unwrap().unwrap();
        let binding = control::Binding {
            component: "test".into(),
            resource: owner.root.clone(),
        };
        let path = dir.path().join("supervisor.json");
        let mut saved = inactive_discovery(binding.clone());
        saved.snapshot.binding.component = "another-cli".into();
        record::save(&path, &saved).unwrap();
        assert!(owner.load_discovery(&binding).is_err());
        assert_eq!(
            record::read::<Discovery>(&path)
                .unwrap()
                .snapshot
                .binding
                .component,
            "another-cli"
        );
        saved.snapshot.binding = binding.clone();
        saved.version = 999;
        record::save(&path, &saved).unwrap();
        assert!(owner.load_discovery(&binding).is_err());
        assert_eq!(record::read::<Discovery>(&path).unwrap().version, 999);
        let mut future = serde_json::to_value(&saved).unwrap();
        future["new_protocol_field"] = true.into();
        record::save(&path, &future).unwrap();
        assert!(owner.load_discovery(&binding).is_err());
        assert_eq!(record::read::<serde_json::Value>(&path).unwrap(), future);
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            owner.load_discovery(&binding).is_err(),
            "I/O error is not corrupt JSON"
        );
        assert!(path.is_dir());
    }

    fn work_generation(scope: &Path, domain: Option<crate::domain::PhysicalDomain>) -> String {
        let id = uuid::Uuid::new_v4().to_string();
        let root = record::work_root(scope, &id).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        record::save(
            &root.join("generation.json"),
            &Generation {
                version: 1,
                id: id.clone(),
                supervisor: "sup".into(),
                token: "test".into(),
                intent: Intent::Run,
                phase: GenerationPhase::Running,
                worker_pid: None,
                exit_code: None,
                error: None,
                physical_domain: domain,
                process_epoch: None,
            },
        )
        .unwrap();
        id
    }

    #[test]
    fn same_container_leftovers_keep_recovery_launch() {
        let dir = tempfile::tempdir().unwrap();
        let binding = control::Binding {
            component: "app-cli".into(),
            resource: dir.path().to_path_buf(),
        };
        let mut discovery = inactive_discovery(binding);
        discovery.snapshot.phase = Phase::Ready;
        assert!(initial_recovery_launch(dir.path(), Some(&discovery), None));
        discovery.snapshot.phase = Phase::Stopped;
        assert!(!initial_recovery_launch(dir.path(), Some(&discovery), None));
    }

    /// belongs_to_previous_container 经 from_env 解析当前容器身份；测试用
    /// env 守卫注入（nextest 每用例独立进程，无并发污染）。
    struct DomainGuard;
    impl DomainGuard {
        #[allow(unsafe_code)]
        fn new(instance: &str) -> Self {
            unsafe {
                std::env::set_var(
                    crate::domain::DOMAIN_ENV,
                    "{\"authority\":\"k8s\",\"volume\":\"workspace-pvc\",\"instance\":\"\",\
                     \"instance_source_env\":\"RCODER_PHYSICAL_POD_UID\"}",
                );
                std::env::set_var("RCODER_PHYSICAL_POD_UID", instance);
            }
            Self
        }
    }
    impl Drop for DomainGuard {
        #[allow(unsafe_code)]
        fn drop(&mut self) {
            unsafe {
                std::env::remove_var(crate::domain::DOMAIN_ENV);
                std::env::remove_var("RCODER_PHYSICAL_POD_UID");
            }
        }
    }

    #[test]
    fn replaced_container_leftovers_classify_as_explicit_launch() {
        // 事故复现：上一容器死在 phase=Running，work 记录属于上一容器。
        // detach 归零后首启是显式平台启动——不得剥离部署声明 env。
        let _guard = DomainGuard::new("pod-new");
        let dir = tempfile::tempdir().unwrap();
        let previous = crate::domain::PhysicalDomain {
            authority: "k8s".into(),
            instance_source_env: None,
            instance: "pod-old".into(),
            volume: "workspace-pvc".into(),
        };
        let id = work_generation(dir.path(), Some(previous));
        let binding = control::Binding {
            component: "app-cli".into(),
            resource: dir.path().to_path_buf(),
        };
        let mut discovery = inactive_discovery(binding);
        discovery.snapshot.phase = Phase::Ready;
        discovery.snapshot.generation = Some(id);
        detach_previous_container_control(dir.path(), &mut discovery).unwrap();
        assert_eq!(discovery.snapshot.phase, Phase::Stopped);
        assert!(!initial_recovery_launch(
            dir.path(),
            Some(&discovery),
            Some(&crate::domain::PhysicalDomain {
                authority: "k8s".into(),
                instance_source_env: None,
                instance: "pod-new".into(),
                volume: "workspace-pvc".into(),
            })
        ));
    }

    #[test]
    fn same_pod_restart_keeps_recovery_launch() {
        // 同 Pod 容器重启：domain instance 未变，detach 不归零，保持恢复语义
        // （防一次性部署输入随 kubelet 重启重放）。
        let _guard = DomainGuard::new("pod-same");
        let dir = tempfile::tempdir().unwrap();
        let same = crate::domain::PhysicalDomain {
            authority: "k8s".into(),
            instance_source_env: None,
            instance: "pod-same".into(),
            volume: "workspace-pvc".into(),
        };
        let id = work_generation(dir.path(), Some(same.clone()));
        let binding = control::Binding {
            component: "app-cli".into(),
            resource: dir.path().to_path_buf(),
        };
        let mut discovery = inactive_discovery(binding);
        discovery.snapshot.phase = Phase::Ready;
        discovery.snapshot.generation = Some(id);
        detach_previous_container_control(dir.path(), &mut discovery).unwrap();
        assert_eq!(discovery.snapshot.phase, Phase::Ready);
        assert!(initial_recovery_launch(
            dir.path(),
            Some(&discovery),
            Some(&same)
        ));
    }
}
