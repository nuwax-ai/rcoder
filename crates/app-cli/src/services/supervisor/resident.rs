//! Owner resident proxy. One actor retains the exact Child, immutable confirmed
//! configuration and owner capability across business StopWork/generation cleanup.
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use process_utils::{
    command_authority::ResidentScope,
    guardian::{self, OwnedChild},
    managed_tree::StopOutcome,
};
use tokio::sync::{mpsc, oneshot};

use super::RuntimeArgs;
use crate::proxy::{
    admin_probe,
    apply_status::ConfirmedPublication,
    compiler::{self, CompileOutcome},
};

const ABSENT: u8 = 0;
const LIVE: u8 = 1;
const UNKNOWN: u8 = 2;
const CLOSED: u8 = 3;

struct Controller {
    scope: ResidentScope,
    runtime_root: PathBuf,
    workspace: PathBuf,
    tx: mpsc::Sender<Request>,
    stopping: AtomicBool,
    status: AtomicU8,
    control_epoch: AtomicU64,
    entry_closing: AtomicBool,
    pause_watch: AtomicBool,
    interrupted: tokio::sync::Notify,
}

static RESIDENT: std::sync::Mutex<Option<Arc<Controller>>> = std::sync::Mutex::new(None);

#[derive(Clone)]
struct Confirmed {
    outcome: CompileOutcome,
    pinned: PathBuf,
    receipt: ConfirmedPublication,
}

struct ResidentProxy {
    owner: Arc<Controller>,
    args: RuntimeArgs,
    child: Option<OwnedChild>,
    confirmed: Option<Confirmed>,
    next_restart: tokio::time::Instant,
}

enum Request {
    Ensure {
        outcome: CompileOutcome,
        deadline: tokio::time::Instant,
        epoch: u64,
        reply: oneshot::Sender<Result<ConfirmedPublication>>,
    },
    Standby {
        deadline: tokio::time::Instant,
        epoch: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    CloseEntry {
        deadline: tokio::time::Instant,
        epoch: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    Shutdown {
        reply: oneshot::Sender<Result<()>>,
    },
}

fn controller() -> Result<Option<Arc<Controller>>> {
    RESIDENT
        .lock()
        .map(|slot| slot.clone())
        .map_err(|_| anyhow::anyhow!("resident controller lock poisoned"))
}

fn unconfirmed_error(owner: &Controller, mut message: String) -> anyhow::Error {
    owner.status.store(UNKNOWN, Ordering::Release);
    if let Err(error) = owner.scope.record_unknown(&message) {
        message.push_str(&format!("; persist Unknown failed: {error:#}"));
    }
    crate::supervisor::ShutdownUnconfirmed(message).into()
}

fn forward_deadline(deadline: tokio::time::Instant, reserve: Duration) -> tokio::time::Instant {
    let now = tokio::time::Instant::now();
    now + deadline
        .saturating_duration_since(now)
        .saturating_sub(reserve)
}

/// Install the capability while the management owner is locked, before any
/// business generation is created. No network or process wait holds this lock.
pub(crate) fn bind_owner(scope: ResidentScope, args: &RuntimeArgs) -> Result<()> {
    let args = args.for_management()?;
    let workspace = runtime_state_layout::resolve_project_origin(&args.workspace)?;
    let binding_workspace = scope
        .identity()
        .binding
        .get("resource")
        .and_then(serde_json::Value::as_str)
        .context("resident workspace identity missing")?;
    ensure!(
        runtime_state_layout::resolve_project_origin(std::path::Path::new(binding_workspace))?
            == workspace,
        "resident owner belongs to another workspace"
    );
    let runtime_root = compiler::runtime_root(&args.log_dir);
    let (tx, rx) = mpsc::channel(16);
    let owner = Arc::new(Controller {
        scope,
        runtime_root,
        workspace,
        tx,
        stopping: AtomicBool::new(false),
        status: AtomicU8::new(ABSENT),
        control_epoch: AtomicU64::new(0),
        entry_closing: AtomicBool::new(false),
        pause_watch: AtomicBool::new(false),
        interrupted: tokio::sync::Notify::new(),
    });
    {
        let mut slot = RESIDENT
            .lock()
            .map_err(|_| anyhow::anyhow!("resident controller lock poisoned"))?;
        ensure!(slot.is_none(), "resident owner context already installed");
        *slot = Some(owner.clone());
    }
    let resident = ResidentProxy {
        owner,
        args,
        child: None,
        confirmed: None,
        next_restart: tokio::time::Instant::now(),
    };
    tokio::spawn(resident.run(rx));
    Ok(())
}

/// Compatibility entry for orchestration's serve/foreground distinction. The
/// path is diagnostic only; actual spawn accepts the sealed owner capability.
pub(crate) fn owner_scope_root() -> Result<Option<PathBuf>> {
    Ok(controller()?.map(|owner| owner.scope.root().to_path_buf()))
}

pub(crate) fn owner_scope() -> Result<Option<ResidentScope>> {
    Ok(controller()?.map(|owner| owner.scope.clone()))
}

pub(crate) async fn reload(outcome: &CompileOutcome) -> Result<ConfirmedPublication> {
    let owner = controller()?.context("resident reload requires an owner capability")?;
    ensure!(
        !owner.stopping.load(Ordering::Acquire),
        "resident owner is shutting down"
    );
    if owner.entry_closing.load(Ordering::Acquire) {
        return Err(unconfirmed_error(
            &owner,
            "resident entry closure is still unconfirmed".into(),
        ));
    }
    let (reply, result) = oneshot::channel();
    owner
        .tx
        .send(Request::Ensure {
            outcome: outcome.clone(),
            deadline: tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET,
            epoch: owner.control_epoch.load(Ordering::Acquire),
            reply,
        })
        .await
        .context("resident owner actor unavailable")?;
    result
        .await
        .context("resident reload response unavailable")?
}

pub(super) async fn ensure_resident(args: &RuntimeArgs, outcome: &CompileOutcome) -> Result<()> {
    let owner = controller()?.context("resident proxy requires an initialized owner capability")?;
    ensure!(
        !owner.stopping.load(Ordering::Acquire),
        "resident owner is shutting down"
    );
    let normalized = args.for_management()?;
    ensure!(
        runtime_state_layout::resolve_project_origin(&normalized.workspace)? == owner.workspace
            && compiler::runtime_root(&args.log_dir) == owner.runtime_root,
        "resident request belongs to another workspace or runtime range"
    );
    reload(outcome).await.map(|_| ())
}

/// Admin timeouts/401/malformed data remain errors. They never authorize
/// respawn, skip the drain, or stop a still-live business process.
pub(crate) async fn publish_standby_if_serving() -> Result<()> {
    publish_standby_until(tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET).await
}

pub(crate) async fn publish_standby_until(deadline: tokio::time::Instant) -> Result<()> {
    let Some(owner) = controller()? else {
        return Ok(());
    };
    ensure!(
        !owner.stopping.load(Ordering::Acquire),
        "resident owner is shutting down"
    );
    if owner.entry_closing.load(Ordering::Acquire) {
        return Err(unconfirmed_error(
            &owner,
            "resident entry closure is still unconfirmed".into(),
        ));
    }
    let (reply, result) = oneshot::channel();
    owner
        .tx
        .send(Request::Standby {
            deadline,
            epoch: owner.control_epoch.load(Ordering::Acquire),
            reply,
        })
        .await
        .context("resident owner actor unavailable")?;
    result
        .await
        .context("resident drain response unavailable")?
}

/// Physical liveness of the retained child, independent of the admin plane.
/// Unknown is treated conservatively as possibly serving for preflight gates.
pub(crate) async fn is_serving() -> bool {
    match controller() {
        Ok(owner) => owner
            .is_some_and(|owner| matches!(owner.status.load(Ordering::Acquire), LIVE | UNKNOWN)),
        Err(_) => true, // Unknown must keep compatibility/drain protections.
    }
}

pub(crate) async fn close_entry() -> Result<()> {
    let Some(owner) = controller()? else {
        return Ok(());
    };
    ensure!(
        !owner.stopping.load(Ordering::Acquire),
        "resident owner is shutting down"
    );
    owner.entry_closing.store(true, Ordering::Release);
    owner.pause_watch.store(true, Ordering::Release);
    let epoch = owner.control_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    owner.interrupted.notify_waiters();
    let deadline = tokio::time::Instant::now()
        + Duration::from_secs(crate::supervision::STOP_GRACE_SECONDS + 6);
    let (reply, result) = oneshot::channel();
    if let Err(error) = owner
        .tx
        .send(Request::CloseEntry {
            deadline,
            epoch,
            reply,
        })
        .await
    {
        return Err(unconfirmed_error(
            &owner,
            format!("resident entry close actor unavailable: {error}"),
        ));
    }
    result.await.map_err(|error| {
        unconfirmed_error(
            &owner,
            format!("resident entry close response unavailable: {error}"),
        )
    })?
}

async fn acquire_publication(
    owner: &Controller,
    deadline: tokio::time::Instant,
    epoch: u64,
) -> Result<tokio::sync::MutexGuard<'static, ()>> {
    let interrupted = owner.interrupted.notified();
    tokio::pin!(interrupted);
    ensure!(
        !owner.stopping.load(Ordering::Acquire)
            && owner.control_epoch.load(Ordering::Acquire) == epoch,
        "resident request revoked before publication dispatch"
    );
    tokio::select! {
        guard = tokio::time::timeout_at(deadline, compiler::publication_guard()) => guard.context("resident request expired before publication dispatch"),
        _ = &mut interrupted => Err(anyhow::anyhow!("resident request revoked before publication dispatch")),
    }
}

/// Admission closes before enqueueing shutdown, preventing a pending Spawn from
/// crossing it. The actor retains an unconfirmed Child and the owner lease; a
/// failed stop persists Unknown and remains retryable by the native owner loop.
pub(crate) async fn shutdown() -> Result<()> {
    let Some(owner) = controller()? else {
        return Ok(());
    };
    owner.stopping.store(true, Ordering::Release);
    owner.control_epoch.fetch_add(1, Ordering::AcqRel);
    owner.interrupted.notify_waiters();
    if let Err(error) = owner.scope.close() {
        return Err(unconfirmed_error(
            &owner,
            format!("resident admission closure failed: {error:#}"),
        ));
    }
    let (reply, result) = oneshot::channel();
    if let Err(error) = owner.tx.send(Request::Shutdown { reply }).await {
        return Err(unconfirmed_error(
            &owner,
            format!("resident owner actor unavailable during shutdown: {error}"),
        ));
    }
    result.await.map_err(|error| {
        unconfirmed_error(
            &owner,
            format!("resident shutdown response unavailable: {error}"),
        )
    })?
}

fn freeze_private(path: &std::path::Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let parent = path
        .parent()
        .context("immutable publication parent missing")?;
    process_utils::command_context::create_durable_directory(parent)?;
    ensure!(
        std::fs::canonicalize(parent)? == parent,
        "immutable publication directory resolves outside owner range"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut options = std::fs::File::options();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(path) {
        Ok(mut file) => {
            file.write_all(bytes)?;
            file.flush()?;
            file.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure!(
                std::fs::symlink_metadata(path)?.file_type().is_file(),
                "immutable publication is not a regular owned file"
            );
            ensure!(
                std::fs::read(path)? == bytes,
                "publication UUID already belongs to different immutable content"
            );
        }
        Err(error) => return Err(error).context("create immutable private publication"),
    }
    #[cfg(unix)]
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

impl ResidentProxy {
    async fn run(mut self, mut rx: mpsc::Receiver<Request>) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                request = rx.recv() => {
                    let Some(request) = request else { return; };
                    match request {
                        Request::Ensure { outcome, deadline, epoch, reply } => {
                            let result = match acquire_publication(&self.owner, deadline, epoch).await {
                                Ok(_publication) => self.ensure(outcome, deadline, &reply).await,
                                Err(error) => Err(error),
                            };
                            drop(reply.send(result));
                        }
                        Request::Standby { deadline, epoch, reply } => {
                            let result = match acquire_publication(&self.owner, deadline, epoch).await {
                                Ok(_publication) => self.standby(deadline, &reply).await,
                                Err(error) => Err(error),
                            };
                            drop(reply.send(result));
                        }
                        Request::CloseEntry { deadline, epoch, reply } => {
                            let result = match acquire_publication(&self.owner, deadline, epoch).await {
                                Ok(_publication) => self.close_entry(deadline).await,
                                Err(error) => Err(self.unconfirmed(format!("resident close entry admission unconfirmed: {error:#}"))),
                            };
                            drop(reply.send(result));
                        }
                        Request::Shutdown { reply } => { let result = self.stop().await; drop(reply.send(result)); }
                    }
                }
                _ = tick.tick(), if !self.owner.stopping.load(Ordering::Acquire) && !self.owner.pause_watch.load(Ordering::Acquire) => {
                    if self.confirmed.is_none() || !matches!(self.observe_exit(), Ok(true)) { continue; }
                    let deadline = tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET;
                    let epoch = self.owner.control_epoch.load(Ordering::Acquire);
                    if let Ok(_publication) = acquire_publication(&self.owner, deadline, epoch).await
                        && let Err(error) = self.watch(deadline).await {
                        tracing::error!(error = %format!("{error:#}"), "resident supervision remains protected");
                    }
                }
            }
        }
    }

    fn unconfirmed(&self, message: String) -> anyhow::Error {
        unconfirmed_error(&self.owner, message)
    }

    async fn reject_initial_attempt(
        &mut self,
        error: anyhow::Error,
        deadline: tokio::time::Instant,
    ) -> Result<ConfirmedPublication> {
        let cleanup = async {
            compiler::wait_for_file_replacements().await;
            if let Some(child) = self.child.as_mut() {
                ensure!(
                    child.stop(Duration::ZERO).await != StopOutcome::Unconfirmed,
                    "initial resident exact Child cleanup is unconfirmed"
                );
            }
            guardian::recover(self.owner.scope.root())
                .context("confirm initial resident guardian cleanup")?;
            self.owner.scope.record_running_until(deadline).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match cleanup {
            Ok(()) => {
                self.child.take();
                self.owner.status.store(ABSENT, Ordering::Release);
                Err(anyhow::anyhow!(
                    "resident bootstrap rejected; exact startup tree cleanup confirmed: {error:#}"
                ))
            }
            Err(cleanup_error) => Err(self.unconfirmed(format!(
                "resident bootstrap rejected: {error:#}; cleanup unknown: {cleanup_error:#}"
            ))),
        }
    }

    fn observe_exit(&mut self) -> Result<bool> {
        let Some(child) = self.child.as_mut() else {
            return Ok(true);
        };
        match child.try_wait_root() {
            Ok(Some(_)) => Ok(true),
            Ok(None) => {
                self.owner.status.store(LIVE, Ordering::Release);
                Ok(false)
            }
            Err(error) => Err(self.unconfirmed(format!("observe exact resident Child: {error}"))),
        }
    }

    async fn retire_dead_child(&mut self) -> Result<()> {
        if let Some(child) = self.child.as_mut() {
            ensure!(
                child.stop(Duration::ZERO).await != StopOutcome::Unconfirmed,
                "resident root exited but its exact guardian/tree cleanup is unconfirmed"
            );
        }
        // A previous failed spawn retains a durable guardian receipt. A new
        // spawn cannot bypass its still-running or unknown range.
        guardian::recover(self.owner.scope.root())
            .context("confirm prior resident tree cleanup")?;
        self.child.take();
        self.owner.status.store(ABSENT, Ordering::Release);
        Ok(())
    }

    async fn spawn(&mut self, deadline: tokio::time::Instant) -> Result<()> {
        ensure!(
            !self.owner.stopping.load(Ordering::Acquire),
            "resident owner is shutting down"
        );
        ensure!(
            self.child.is_none(),
            "resident spawn requires the original Child to be physically settled"
        );
        let endpoint = admin_probe::ensure_admin_endpoint()?.clone();
        let mut command = tokio::process::Command::new(&self.args.pingap_bin);
        command
            .arg("-c")
            .arg(compiler::active_config_path(&self.owner.runtime_root))
            .arg("--autoreload")
            .env("PINGAP_ADMIN_ADDR", endpoint.addr)
            .env("PINGAP_ADMIN_USER", endpoint.user)
            .env("PINGAP_ADMIN_PASSWORD", endpoint.password);
        // Output inherits owner stdout/stderr; no unread pipe can backpressure
        // the resident on a log flood.
        self.owner.status.store(UNKNOWN, Ordering::Release);
        let child = match guardian::spawn_guarded_owner_checked_until(
            command,
            &self.owner.scope,
            false,
            deadline,
        )
        .await
        {
            Ok(child) => child,
            Err(failure) => {
                self.child = failure.child;
                return Err(self.unconfirmed(format!(
                    "resident spawn/guardian acknowledgement unknown: {:#}",
                    failure.source
                )));
            }
        };
        self.child = Some(child);
        self.owner.status.store(LIVE, Ordering::Release);
        Ok(())
    }

    async fn confirm(
        &mut self,
        outcome: &CompileOutcome,
        deadline: tokio::time::Instant,
    ) -> Result<ConfirmedPublication> {
        let endpoint = admin_probe::ensure_admin_endpoint()?.clone();
        ensure!(
            !self.observe_exit()?,
            "resident exited before publication confirmation"
        );
        let child_pid = self
            .child
            .as_ref()
            .and_then(OwnedChild::id)
            .context("resident command PID missing")?;
        let confirmed = admin_probe::wait_for_publication(
            &endpoint,
            outcome,
            deadline.saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
        .context("confirm resident complete application and entry HTTP")?;
        ensure!(
            confirmed.process_id == child_pid,
            "authenticated admin application belongs to another process"
        );
        ensure!(
            !self.observe_exit()?,
            "resident exited during publication confirmation"
        );
        ensure!(
            !self.owner.stopping.load(Ordering::Acquire)
                && !self.owner.entry_closing.load(Ordering::Acquire),
            "resident owner or entry closed during publication confirmation"
        );
        self.owner.scope.record_running_until(deadline).await?;
        Ok(confirmed)
    }

    async fn pin(
        &mut self,
        outcome: CompileOutcome,
        receipt: ConfirmedPublication,
        bytes: &[u8],
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(&outcome.publication_id).is_ok(),
            "invalid resident publication identity"
        );
        let directory = self.owner.scope.root().join("confirmed");
        let pinned = directory.join(format!("{}.toml", outcome.publication_id));
        freeze_private(&pinned, bytes)?;
        let outcome = CompileOutcome {
            config_path: pinned.clone(),
            ..outcome
        };
        self.owner
            .scope
            .record_publication_until(
                serde_json::json!({
                    "config_path": pinned, "publication_id": outcome.publication_id,
                    "config_digest": outcome.config_digest, "config_hash": outcome.expected_hash,
                    "process_id": receipt.process_id, "process_instance_id": receipt.instance_id,
                    "attempt_id": receipt.attempt_id,
                }),
                deadline,
            )
            .await?;
        compiler::record_confirmed_publication(&outcome, &receipt)?;
        self.confirmed = Some(Confirmed {
            outcome,
            pinned,
            receipt,
        });
        Ok(())
    }

    async fn restore_after_rejection(
        &mut self,
        error: anyhow::Error,
        previous: Option<Confirmed>,
        deadline: tokio::time::Instant,
    ) -> Result<ConfirmedPublication> {
        let restore = async {
            ensure!(
                !self.owner.stopping.load(Ordering::Acquire)
                    && !self.owner.entry_closing.load(Ordering::Acquire),
                "owner or entry shutdown prevents another rollback publication"
            );
            let previous = previous.context("resident has no confirmed old graph to restore")?;
            let bytes = tokio::fs::read(&previous.pinned)
                .await
                .context("read immutable old applied graph")?;
            let graph = pingap_config::PingapConfig::new(&bytes, true)?;
            let endpoint = admin_probe::ensure_admin_endpoint()?;
            let target = compiler::active_config_path(&self.owner.runtime_root);
            let (restored, receipt) = compiler::rollback_publication(
                &self.owner.runtime_root,
                &target,
                graph,
                endpoint,
                &previous.receipt,
                &previous.outcome.business_probes,
                deadline,
            )
            .await?;
            ensure!(
                !self.observe_exit()?
                    && self.child.as_ref().and_then(OwnedChild::id) == Some(receipt.process_id),
                "resident changed during rollback confirmation"
            );
            let bytes = tokio::fs::read(&restored.config_path).await?;
            self.pin(restored, receipt, &bytes, deadline).await
        }
        .await;
        match restore {
            Ok(()) => Err(error).context("resident publication rejected; previous complete graph restored, business retained"),
            Err(restore_error) => {
                Err(self.unconfirmed(format!("publication rejected: {error:#}; complete old graph restoration unknown: {restore_error:#}")))
            }
        }
    }

    async fn ensure(
        &mut self,
        outcome: CompileOutcome,
        deadline: tokio::time::Instant,
        reply: &oneshot::Sender<Result<ConfirmedPublication>>,
    ) -> Result<ConfirmedPublication> {
        ensure!(
            !self.owner.stopping.load(Ordering::Acquire),
            "resident owner is shutting down"
        );
        ensure!(
            !reply.is_closed() && tokio::time::Instant::now() < deadline,
            "resident request expired before publication dispatch"
        );
        let bytes = tokio::fs::read(&outcome.config_path)
            .await
            .context("read immutable resident candidate")?;
        let graph = pingap_config::PingapConfig::new(&bytes, true)?;
        ensure!(
            compiler::configuration_digest(&graph)? == outcome.config_digest
                && admin_probe::hashes_match(&graph.hash()?, &outcome.expected_hash),
            "resident candidate differs from its authorized graph"
        );
        if let Some(previous) = self.confirmed.as_ref() {
            compiler::validate_hot_reload_compatible(
                &previous.pinned,
                std::str::from_utf8(&bytes)?,
            )?;
        }
        ensure!(
            uuid::Uuid::parse_str(&outcome.publication_id).is_ok(),
            "invalid resident publication identity"
        );
        let frozen_dir = self.owner.scope.root().join("authorized");
        let frozen = frozen_dir.join(format!("{}.toml", outcome.publication_id));
        freeze_private(&frozen, &bytes)?;
        let outcome = CompileOutcome {
            config_path: frozen,
            ..outcome
        };
        ensure!(
            !reply.is_closed()
                && tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire)
                && !self.owner.entry_closing.load(Ordering::Acquire),
            "resident request expired or closed before publication dispatch"
        );
        let previous = self.confirmed.clone();
        let result = async {
            compiler::publish_active_until(
                &self.owner.runtime_root,
                &outcome.config_path,
                deadline,
            )
            .await?;
            if self.observe_exit()? {
                self.retire_dead_child().await?;
                self.spawn(deadline).await?;
            }
            let reserve = if previous.is_some() {
                Duration::from_secs(6)
            } else {
                Duration::ZERO
            };
            let receipt = self
                .confirm(&outcome, forward_deadline(deadline, reserve))
                .await?;
            self.pin(outcome, receipt.clone(), &bytes, deadline).await?;
            Ok(receipt)
        }
        .await;
        match result {
            Ok(receipt) => {
                self.owner.pause_watch.store(false, Ordering::Release);
                Ok(receipt)
            }
            Err(error) if previous.is_none() => self.reject_initial_attempt(error, deadline).await,
            Err(error) => {
                self.restore_after_rejection(error, previous, deadline)
                    .await
            }
        }
    }

    async fn standby(
        &mut self,
        deadline: tokio::time::Instant,
        reply: &oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        ensure!(
            !reply.is_closed()
                && tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire),
            "resident drain expired before dispatch"
        );
        if self.child.is_none() && self.confirmed.is_none() {
            guardian::recover(self.owner.scope.root())
                .context("prove no resident entry exists before drain")?;
            return Ok(());
        }
        let recovery = async {
            if self.observe_exit()? {
                self.retire_dead_child().await?;
                self.restore_with_deadline(deadline).await?;
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(error) = recovery {
            return Err(self.unconfirmed(format!(
                "resident drain recovery outcome unknown: {error:#}"
            )));
        }
        ensure!(
            !reply.is_closed()
                && tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire),
            "resident drain expired before publication dispatch"
        );
        let previous = self.confirmed.clone();
        let result = async {
            let publication = uuid::Uuid::new_v4().to_string();
            let outcome = compiler::publish_standby(&self.owner.runtime_root, &publication).await?;
            let bytes = tokio::fs::read(&outcome.config_path).await?;
            let receipt = self
                .confirm(&outcome, forward_deadline(deadline, Duration::from_secs(6)))
                .await?;
            self.pin(outcome, receipt, &bytes, deadline).await
        }
        .await;
        match result {
            Ok(()) => Ok(()),
            Err(error) => self
                .restore_after_rejection(error, previous, deadline)
                .await
                .map(|_| ()),
        }
    }

    async fn restore_with_deadline(&mut self, deadline: tokio::time::Instant) -> Result<()> {
        ensure!(
            tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire)
                && !self.owner.entry_closing.load(Ordering::Acquire),
            "resident restoration budget expired or entry closed before mutation"
        );
        let previous = self
            .confirmed
            .clone()
            .context("resident has no confirmed configuration to restore")?;
        let bytes = tokio::fs::read(&previous.pinned).await?;
        ensure!(
            tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire)
                && !self.owner.entry_closing.load(Ordering::Acquire),
            "resident restoration revoked before publication dispatch"
        );
        compiler::publish_active_until(&self.owner.runtime_root, &previous.pinned, deadline)
            .await?;
        self.spawn(deadline).await?;
        let receipt = self.confirm(&previous.outcome, deadline).await?;
        self.pin(previous.outcome, receipt, &bytes, deadline).await
    }

    async fn watch(&mut self, deadline: tokio::time::Instant) -> Result<()> {
        if tokio::time::Instant::now() >= deadline || self.owner.stopping.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.confirmed.is_none() || tokio::time::Instant::now() < self.next_restart {
            return Ok(());
        }
        if !self.observe_exit()? {
            return Ok(());
        }
        self.next_restart = tokio::time::Instant::now() + Duration::from_secs(1);
        self.retire_dead_child().await?;
        self.restore_with_deadline(deadline).await
    }

    async fn close_entry(&mut self, deadline: tokio::time::Instant) -> Result<()> {
        compiler::wait_for_file_replacements().await;
        let result = async {
            if let Some(child) = self.child.as_mut() {
                ensure!(
                    child
                        .stop(Duration::from_secs(crate::supervision::STOP_GRACE_SECONDS))
                        .await
                        != StopOutcome::Unconfirmed,
                    "resident entry exact Child cleanup unconfirmed"
                );
            }
            guardian::recover(self.owner.scope.root())?;
            self.owner.scope.record_running_until(deadline).await?;
            compiler::clear_current_confirmation()?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        match result {
            Ok(()) => {
                self.child.take();
                self.confirmed = None;
                self.owner.status.store(ABSENT, Ordering::Release);
                self.owner.entry_closing.store(false, Ordering::Release);
                Ok(())
            }
            Err(error) => Err(self.unconfirmed(format!(
                "resident entry physical closure unconfirmed: {error:#}"
            ))),
        }
    }

    async fn stop(&mut self) -> Result<()> {
        compiler::wait_for_file_replacements().await;
        if let Some(child) = self.child.as_mut() {
            let grace = Duration::from_secs(crate::supervision::STOP_GRACE_SECONDS);
            if child.stop(grace).await == StopOutcome::Unconfirmed {
                return Err(self.unconfirmed(
                    "resident owner shutdown physical cleanup unconfirmed; original Child retained"
                        .into(),
                ));
            }
        }
        if let Err(error) = self.owner.scope.record_quiescent() {
            return Err(self.unconfirmed(format!(
                "persist resident owner physical cleanup confirmation: {error:#}"
            )));
        }
        self.child.take();
        self.owner.status.store(CLOSED, Ordering::Release);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned_proxy(
        root: &std::path::Path,
    ) -> (process_utils::command_authority::OwnerLease, ResidentProxy) {
        let root = root.canonicalize().unwrap();
        let lease = process_utils::command_authority::OwnerLease::try_acquire(&root)
            .unwrap()
            .unwrap();
        let identity = process_utils::command_authority::ResidentIdentity {
            application_id: "resident-result-fixture".into(),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            binding: serde_json::json!({"component":"app-cli","resource":root}),
            physical_domain: None,
            process_epoch: None,
        };
        std::fs::write(root.join("supervisor.json"), serde_json::to_vec(&serde_json::json!({
            "instance":identity.owner_instance,"snapshot":{"supervisor_id":identity.owner_instance,"binding":identity.binding}
        })).unwrap()).unwrap();
        let scope = ResidentScope::initialize(&lease, identity).unwrap();
        let (tx, _rx) = mpsc::channel(1);
        let owner = Arc::new(Controller {
            scope,
            runtime_root: root.join("proxy-runtime"),
            workspace: root,
            tx,
            stopping: AtomicBool::new(false),
            status: AtomicU8::new(ABSENT),
            control_epoch: AtomicU64::new(0),
            entry_closing: AtomicBool::new(false),
            pause_watch: AtomicBool::new(false),
            interrupted: tokio::sync::Notify::new(),
        });
        (
            lease,
            ResidentProxy {
                owner,
                args: RuntimeArgs::default(),
                child: None,
                confirmed: None,
                next_restart: tokio::time::Instant::now(),
            },
        )
    }

    #[test]
    fn unknown_persistence_error_cannot_downgrade_protected_result() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, proxy) = owned_proxy(root.path());
        let path = proxy.owner.scope.root().join("resident-scope.json");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let error = proxy.unconfirmed("controlled physical uncertainty".into());
        assert!(
            error
                .downcast_ref::<crate::supervisor::ShutdownUnconfirmed>()
                .is_some()
        );
        assert!(format!("{error:#}").contains("persist Unknown failed"));
        assert_eq!(proxy.owner.status.load(Ordering::Acquire), UNKNOWN);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejected_bootstrap_and_business_close_keep_owner_admission_for_source_retry() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, mut proxy) = owned_proxy(root.path());
        let mut command = tokio::process::Command::new("sleep");
        command.arg("60");
        let child = process_utils::managed_tree::spawn_managed(command).unwrap();
        let pid = child.id().unwrap();
        proxy.child = Some(OwnedChild::Direct(child));
        let failure = proxy
            .reject_initial_attempt(
                anyhow::anyhow!("controlled business HTTP refusal"),
                tokio::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(
            failure
                .downcast_ref::<crate::supervisor::ShutdownUnconfirmed>()
                .is_none()
        );
        assert!(proxy.child.is_none());
        assert!(
            !process_utils::process_exists(pid).unwrap(),
            "failed bootstrap still owns a live entry"
        );
        proxy.owner.entry_closing.store(true, Ordering::Release);
        proxy.owner.pause_watch.store(true, Ordering::Release);
        proxy
            .close_entry(tokio::time::Instant::now() + Duration::from_secs(5))
            .await
            .unwrap();
        assert!(!proxy.owner.stopping.load(Ordering::Acquire));
        assert!(!proxy.owner.entry_closing.load(Ordering::Acquire));
        assert!(proxy.confirmed.is_none());
        proxy
            .owner
            .scope
            .record_running_until(tokio::time::Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn queued_deadline_and_cancelled_reply_cannot_publish_late_standby() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let lease = process_utils::command_authority::OwnerLease::try_acquire(&root)
            .unwrap()
            .unwrap();
        let identity = process_utils::command_authority::ResidentIdentity {
            application_id: "queued-fixture".into(),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            binding: serde_json::json!({"component":"app-cli","resource":root}),
            physical_domain: None,
            process_epoch: None,
        };
        std::fs::write(root.join("supervisor.json"), serde_json::to_vec(&serde_json::json!({
            "instance":identity.owner_instance,"snapshot":{"supervisor_id":identity.owner_instance,"binding":identity.binding}
        })).unwrap()).unwrap();
        let scope = ResidentScope::initialize(&lease, identity).unwrap();
        let runtime_root = root.join("proxy-runtime");
        std::fs::create_dir_all(runtime_root.join("active")).unwrap();
        let active = compiler::active_config_path(&runtime_root);
        std::fs::write(&active, "old-confirmed-sentinel").unwrap();
        let publication = uuid::Uuid::new_v4().to_string();
        let (content, hash) = compiler::build_standby_config(&publication).unwrap();
        let graph = pingap_config::PingapConfig::new(content.as_bytes(), true).unwrap();
        let candidate = runtime_root.join("candidate.toml");
        std::fs::write(&candidate, content).unwrap();
        let outcome = CompileOutcome {
            config_path: candidate,
            expected_hash: hash,
            publication_id: publication,
            config_digest: compiler::configuration_digest(&graph).unwrap(),
            entry_probes: vec![],
            business_probes: vec![],
        };
        let (tx, rx) = mpsc::channel(16);
        let owner = Arc::new(Controller {
            scope: scope.clone(),
            runtime_root,
            workspace: root.clone(),
            tx: tx.clone(),
            stopping: AtomicBool::new(false),
            status: AtomicU8::new(ABSENT),
            control_epoch: AtomicU64::new(0),
            entry_closing: AtomicBool::new(false),
            pause_watch: AtomicBool::new(false),
            interrupted: tokio::sync::Notify::new(),
        });
        let resident = ResidentProxy {
            owner,
            args: RuntimeArgs::default(),
            child: None,
            confirmed: None,
            next_restart: tokio::time::Instant::now(),
        };
        let gate = compiler::publication_guard().await;
        let task = tokio::spawn(resident.run(rx));
        let (reply, receive) = oneshot::channel();
        tx.send(Request::Ensure {
            outcome: outcome.clone(),
            epoch: 0,
            deadline: tokio::time::Instant::now() + Duration::from_millis(10),
            reply,
        })
        .await
        .unwrap();
        let (cancelled, receive_cancelled) = oneshot::channel();
        tx.send(Request::Ensure {
            outcome,
            epoch: 0,
            deadline: tokio::time::Instant::now() + Duration::from_secs(2),
            reply: cancelled,
        })
        .await
        .unwrap();
        drop(receive_cancelled);
        tokio::time::sleep(Duration::from_millis(25)).await;
        drop(gate);
        let error = receive.await.unwrap().unwrap_err();
        assert!(format!("{error:#}").contains("expired before publication dispatch"));
        let (reply, receive) = oneshot::channel();
        tx.send(Request::Shutdown { reply }).await.unwrap();
        receive.await.unwrap().unwrap();
        assert_eq!(
            std::fs::read_to_string(&active).unwrap(),
            "old-confirmed-sentinel"
        );
        assert!(
            !scope.root().join("guardians").exists(),
            "late cancelled request spawned a process"
        );
        task.abort();
        let _ = task.await;
    }
}
