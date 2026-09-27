use crate::{
    control::{self, Action, Discovery, Envelope, Reply, Snapshot},
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
    /// Evidence must cover every old generation before publishing completion.
    pub async fn stop_offline(self, request: &control::Request) -> Result<Snapshot> {
        // Parent death closes pipes first; guardians still need a bounded drain
        // window. Wait only for a retained OS lock, never reinterpret corruption
        // or missing cleanup evidence as success.
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            match self.try_stop_offline(request) {
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
    fn try_stop_offline(&self, request: &control::Request) -> Result<Snapshot> {
        ensure!(
            !request.request_id.is_empty() && request.request_id.len() <= 128,
            "invalid control request identity"
        );
        ensure!(
            matches!(request.action, Action::Shutdown | Action::StopWork),
            "offline action must stop execution"
        );
        let mut discovery: Discovery = record::read(&self.root.join("supervisor.json"))?;
        ensure!(discovery.version == 1, "unsupported supervisor receipt");
        if let Some((original, snapshot)) = discovery
            .requests
            .iter()
            .find(|(r, _)| r.request_id == request.request_id)
        {
            ensure!(
                original == request,
                "request identity already used with different parameters"
            );
            if matches!(snapshot.phase.as_str(), "ready" | "stopped") {
                return Ok(snapshot.clone());
            }
        }
        ensure!(
            request.expected_generation.is_none()
                || request.expected_generation == discovery.snapshot.generation,
            "execution generation changed"
        );
        record::reconcile(&self.root)?;
        if let Some(id) = &discovery.snapshot.operation_id {
            for (original, snapshot) in &mut discovery.requests {
                if &original.request_id == id {
                    snapshot.phase = "recovery_required".into();
                    snapshot.error = Some("supervisor exited before control completed".into());
                }
            }
        }
        discovery.snapshot.intent = if request.action == Action::StopWork {
            Intent::Stopped
        } else {
            Intent::Shutdown
        };
        discovery.snapshot.phase = "stopped".into();
        discovery.snapshot.error = None;
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

    pub async fn run(self, options: Options) -> Result<i32> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let old: Option<Discovery> = if self.root.join("supervisor.json").try_exists()? {
            Some(record::read(&self.root.join("supervisor.json"))?)
        } else {
            None
        };
        if let Some(old) = &old {
            ensure!(old.version == 1, "unsupported supervisor receipt");
        }
        let instance = uuid::Uuid::new_v4().to_string();
        let binding = options.binding.clone().unwrap_or(control::Binding {
            component: "runtime".into(),
            resource: self.root.clone(),
        });
        if let Some(old) = &old {
            ensure!(
                old.snapshot.binding == binding,
                "supervisor scope belongs to another resource"
            );
        }
        // Resume an accepted control after parent death. Explicit launch after
        // a completed shutdown starts a new session; it cannot discard a Stop
        // still waiting for quiescence or for the worker's durable acknowledgement.
        let pending = old
            .as_ref()
            .filter(|d| d.snapshot.phase != "stopped")
            .and_then(|d| d.snapshot.operation_id.clone());
        let intent = old.as_ref().map_or(Intent::Run, |d| {
            if pending.is_some() || d.snapshot.intent == Intent::Stopped {
                d.snapshot.intent
            } else {
                Intent::Run
            }
        });
        let mut state = State {
            root: self.root.clone(),
            policy: options.policy.clone(),
            discovery: Discovery {
                version: 1,
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
                    phase: "reconciling".into(),
                    intent,
                    operation_id: pending.clone(),
                    error: None,
                },
                requests: old.map_or_else(Vec::new, |d| d.requests),
            },
            recovery_launch: pending.is_some(),
            child: None,
            stopping: None,
            probe: None,
            stop_ack: None,
            next_probe: Instant::now(),
            started: Instant::now(),
            last_tick: Instant::now(),
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
                    drop(incoming.reply.send(Reply { instance: state.discovery.instance.clone(), snapshot, error: outcome.err().map(|e| format!("{e:#}")) }));
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
        ensure!(
            envelope.version == 1
                && envelope.instance == self.discovery.instance
                && envelope.token == self.discovery.token,
            "supervisor request identity mismatch"
        );
        let request = &envelope.request;
        ensure!(
            !request.request_id.is_empty() && request.request_id.len() <= 128,
            "invalid control request identity"
        );
        if request.action == Action::Status {
            return Ok(self.discovery.snapshot.clone());
        }
        if let Some((original, snapshot)) = self
            .discovery
            .requests
            .iter()
            .find(|(r, _)| r.request_id == request.request_id)
        {
            ensure!(
                original.action == request.action
                    && original.expected_generation == request.expected_generation,
                "request identity already used with different parameters"
            );
            return Ok(snapshot.clone());
        }
        ensure!(
            request.expected_generation.is_none()
                || request.expected_generation == self.discovery.snapshot.generation,
            "execution generation changed; inspect current owner before retrying"
        );
        ensure!(
            self.discovery.snapshot.operation_id.is_none(),
            "supervisor is busy with operation {} ({})",
            self.discovery
                .snapshot
                .operation_id
                .as_deref()
                .unwrap_or("unknown"),
            self.discovery.snapshot.phase
        );
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
        self.discovery.snapshot.phase = "stopping".into();
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
        self.discovery.snapshot.intent = intent;
        self.discovery.snapshot.phase = "stopping".into();
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
            if let Some(declared) = now.checked_add(observation.shutdown_grace) {
                *deadline = (*deadline).max(declared);
            }
        }
        if let Some(child) = &mut self.child {
            if self.stopping.is_some_and(|deadline| now >= deadline) {
                child.lease.take();
                self.discovery.snapshot.phase = "cleanup_pending".into();
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
                    self.discovery.snapshot.phase = "cleanup_pending".into();
                    self.discovery.snapshot.error = Some(format!("{error:#}"));
                    self.persist()?;
                    return Ok(None);
                }
                if self.discovery.snapshot.intent == Intent::Shutdown
                    || !restart_on_exit
                    || self.initial_launch
                {
                    self.discovery.snapshot.phase = "stopped".into();
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
                let message = format!("{error:#}");
                if self.discovery.snapshot.error.as_deref() != Some(&message) {
                    self.discovery.snapshot.phase = "cleanup_pending".into();
                    self.discovery.snapshot.error = Some(message);
                    self.persist()?;
                }
                return Ok(None);
            }
            if self.discovery.snapshot.intent == Intent::Shutdown {
                self.discovery.snapshot.phase = "stopped".into();
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
                if self.discovery.snapshot.phase != "recovery_required" {
                    self.discovery.snapshot.phase = "recovery_required".into();
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
                },
            )?;
            self.discovery.snapshot.generation = Some(id);
            self.discovery.snapshot.phase = "starting".into();
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
                if observation.ready && self.discovery.snapshot.phase != "ready" {
                    self.discovery.snapshot.phase = "ready".into();
                    self.discovery.snapshot.error = None;
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
}

async fn worker_shutdown(work: PathBuf) -> Result<worker::Observation> {
    worker::challenge(&work, true).await
}
