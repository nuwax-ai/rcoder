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
    publication::{self, ProcessHost, ProcessInstance},
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
    busy: AtomicBool,
    control_pending: AtomicBool,
    fault_cleanup: std::sync::Mutex<Option<Arc<FaultCleanup>>>,
    settled: tokio::sync::Notify,
    status: AtomicU8,
    control_epoch: AtomicU64,
    epoch_fence: std::sync::Mutex<()>,
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
        cancel: Option<tokio_util::sync::CancellationToken>,
        deadline: tokio::time::Instant,
        epoch: u64,
        reply: oneshot::Sender<Result<ConfirmedPublication>>,
    },
    Standby {
        operation_id: Option<String>,
        deadline: tokio::time::Instant,
        epoch: u64,
        watch_was_paused: bool,
        release_control: bool,
        publication: tokio::sync::MutexGuard<'static, ()>,
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
    Verify {
        expected: ConfirmedPublication,
        deadline: tokio::time::Instant,
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
        busy: AtomicBool::new(false),
        control_pending: AtomicBool::new(false),
        fault_cleanup: std::sync::Mutex::new(None),
        settled: tokio::sync::Notify::new(),
        status: AtomicU8::new(ABSENT),
        control_epoch: AtomicU64::new(0),
        epoch_fence: std::sync::Mutex::new(()),
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

pub(crate) fn owner_is_closing() -> Result<bool> {
    Ok(controller()?.is_some_and(|owner| {
        owner.stopping.load(Ordering::Acquire) || owner.entry_closing.load(Ordering::Acquire)
    }))
}

/// Read-only exact Child check. A busy writer or pending control request is
/// reported as unobserved; this does not enqueue another write or open a scope.
pub(crate) async fn verify_entry_confirmation(
    expected: &ConfirmedPublication,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let owner = controller()?.context("native proxy has no retained owner")?;
    ensure!(
        !owner.stopping.load(Ordering::Acquire)
            && !owner.entry_closing.load(Ordering::Acquire)
            && !owner.busy.load(Ordering::Acquire)
            && tokio::time::Instant::now() < deadline,
        "native proxy is busy or its owner is closing"
    );
    let _read_gate = compiler::try_publication_guard()?;
    let (reply, result) = oneshot::channel();
    owner
        .tx
        .try_send(Request::Verify {
            expected: expected.clone(),
            deadline,
            reply,
        })
        .map_err(|error| anyhow::anyhow!("native physical observation unavailable: {error}"))?;
    tokio::time::timeout_at(deadline, result)
        .await
        .context("native physical observation deadline exhausted")?
        .context("native physical observation actor unavailable")?
}

pub(crate) async fn reload(outcome: &CompileOutcome) -> Result<ConfirmedPublication> {
    dispatch_reload(outcome, None).await
}

pub(crate) async fn reload_with_cancel(
    outcome: &CompileOutcome,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<ConfirmedPublication> {
    dispatch_reload(outcome, Some(cancel.clone())).await
}

async fn dispatch_reload(
    outcome: &CompileOutcome,
    cancel: Option<tokio_util::sync::CancellationToken>,
) -> Result<ConfirmedPublication> {
    ensure!(
        !cancel.as_ref().is_some_and(|token| token.is_cancelled()),
        "proxy operation cancelled before dispatch"
    );
    let owner = controller()?.context("resident reload requires an owner capability")?;
    if owner.control_pending.load(Ordering::Acquire) {
        return Err(compiler::proxy_busy_error());
    }
    let epoch = owner.control_epoch.load(Ordering::Acquire);
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
    if owner.busy.swap(true, Ordering::AcqRel) {
        return Err(compiler::proxy_busy_error());
    }
    // Reject an already running read/write before replacing the serving view.
    // While busy=true, new readonly requests cannot race this dispatch barrier.
    let dispatch_gate = match compiler::try_publication_guard() {
        Ok(guard) => guard,
        Err(error) => {
            owner.busy.store(false, Ordering::Release);
            return Err(error);
        }
    };
    drop(dispatch_gate);
    let (reply, result) = oneshot::channel();
    if let Err(error) = owner.tx.try_send(Request::Ensure {
        outcome: outcome.clone(),
        cancel,
        deadline: tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET,
        epoch,
        reply,
    }) {
        owner.busy.store(false, Ordering::Release);
        return Err(error).context("resident owner actor unavailable");
    }
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
    publish_standby_for_operation_until(deadline, None).await
}

pub(crate) async fn publish_standby_for_operation_until(
    deadline: tokio::time::Instant,
    operation_id: Option<String>,
) -> Result<()> {
    dispatch_standby(deadline, operation_id, None).await
}

async fn dispatch_standby(
    deadline: tokio::time::Instant,
    operation_id: Option<String>,
    fault: Option<&Arc<FaultCleanup>>,
) -> Result<()> {
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
    // This entry is called only after lifecycle Stop/control admission. Revoke
    // the background writer and wait for its exact cleanup within that original
    // budget; ordinary reload requests still fail Busy and never queue.
    let mut control =
        control_publication_until(&owner, deadline, fault, runtime_supervisor::current_scope())
            .await?;
    let epoch = owner.control_epoch.load(Ordering::Acquire);
    let operation_id = Some(operation_id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()));
    let publication = control
        .publication
        .take()
        .context("Stop publication barrier missing")?;
    let (reply, result) = oneshot::channel();
    if let Err(error) = owner.tx.try_send(Request::Standby {
        operation_id,
        deadline,
        epoch,
        watch_was_paused: control.watch_was_paused,
        release_control: control.release_control,
        publication,
        reply,
    }) {
        return Err(error).context("resident owner actor unavailable");
    }
    control.dispatched = true;
    result
        .await
        .context("resident drain response unavailable")?
}

/// Only an admitted lifecycle control may wait for an earlier operation's
/// cleanup. This reserves priority before waiting; it is not a reload queue.
pub(crate) struct ControlPublication {
    owner: Arc<Controller>,
    publication: Option<tokio::sync::MutexGuard<'static, ()>>,
    watch_was_paused: bool,
    owns_busy: bool,
    dispatched: bool,
    release_control: bool,
}

struct FaultCleanup {
    scope: Option<runtime_supervisor::SessionScope>,
    result: std::sync::Mutex<Option<Result<(), String>>>,
    done: tokio::sync::Notify,
}

fn same_execution(
    left: &Option<runtime_supervisor::SessionScope>,
    right: &Option<runtime_supervisor::SessionScope>,
) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.work_root == right.work_root
                && left.generation == right.generation
                && left.supervisor_id == right.supervisor_id
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

/// Completion of one captured business failure cleanup, not new authority or
/// a second request queue. Its outcome is published only after all retained
/// old Children and the proxy drain/closure have settled.
pub(crate) struct FaultCleanupGuard {
    owner: Arc<Controller>,
    fault: Arc<FaultCleanup>,
    finished: bool,
}

pub(crate) fn begin_fault_cleanup(
    scope: Option<runtime_supervisor::SessionScope>,
) -> Result<Option<FaultCleanupGuard>> {
    let Some(owner) = controller()? else {
        return Ok(None);
    };
    let mut slot = owner
        .fault_cleanup
        .lock()
        .map_err(|_| anyhow::anyhow!("fault cleanup context poisoned"))?;
    if owner.control_pending.swap(true, Ordering::AcqRel) {
        // An admitted user control owns the drain. The old supervisor may only
        // settle its captured business Children; it must not close that proxy.
        ensure!(
            slot.is_none(),
            "another captured fault cleanup is already active"
        );
        return Ok(None);
    }
    let fault = Arc::new(FaultCleanup {
        scope,
        result: std::sync::Mutex::new(None),
        done: tokio::sync::Notify::new(),
    });
    *slot = Some(fault.clone());
    drop(slot);
    Ok(Some(FaultCleanupGuard {
        owner,
        fault,
        finished: false,
    }))
}

impl FaultCleanupGuard {
    pub(crate) async fn drain(&self, deadline: tokio::time::Instant) -> Result<()> {
        dispatch_standby(deadline, None, Some(&self.fault)).await
    }
    pub(crate) fn finish(mut self, result: &Result<()>) {
        self.complete(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        );
    }
    fn complete(&mut self, mut result: Result<(), String>) {
        if self.finished {
            return;
        }
        self.finished = true;
        let mut slot = match self.owner.fault_cleanup.lock() {
            Ok(slot) => slot,
            Err(poisoned) => {
                result = Err("captured fault cleanup context poisoned; outcome unconfirmed".into());
                poisoned.into_inner()
            }
        };
        let mut completed = match self.fault.result.lock() {
            Ok(completed) => completed,
            Err(poisoned) => {
                result = Err("captured fault cleanup outcome poisoned; outcome unconfirmed".into());
                poisoned.into_inner()
            }
        };
        if result.is_ok()
            && slot
                .as_ref()
                .is_some_and(|held| Arc::ptr_eq(held, &self.fault))
        {
            *slot = None;
            self.owner.control_pending.store(false, Ordering::Release);
        }
        let failure = result.as_ref().err().cloned();
        *completed = Some(result);
        drop(completed);
        drop(slot);
        if let Some(message) = failure {
            let protection = unconfirmed_error(
                &self.owner,
                format!("captured business fault cleanup unconfirmed: {message}"),
            );
            tracing::error!(error=%format!("{protection:#}"),"original business cleanup remains protected");
        }
        self.fault.done.notify_waiters();
        self.owner.settled.notify_waiters();
    }
}

impl Drop for FaultCleanupGuard {
    fn drop(&mut self) {
        if !self.finished {
            if let Err(error) = revoke_epoch(&self.owner) {
                tracing::error!(error=%format!("{error:#}"),"fault cleanup abandonment revocation failed");
            }
            self.complete(Err(
                "original supervisor ended before confirming its captured cleanup".into(),
            ));
        }
    }
}

enum ControlClaim {
    Claimed,
    OwnFault,
    Wait(Arc<FaultCleanup>),
}

fn claim_control(
    owner: &Controller,
    own_fault: Option<&Arc<FaultCleanup>>,
    expected: &Option<runtime_supervisor::SessionScope>,
) -> Result<ControlClaim> {
    let slot = owner
        .fault_cleanup
        .lock()
        .map_err(|_| unconfirmed_error(owner, "fault cleanup context poisoned".into()))?;
    if let Some(fault) = own_fault {
        ensure!(
            owner.control_pending.load(Ordering::Acquire)
                && slot.as_ref().is_some_and(|held| Arc::ptr_eq(held, fault)),
            "captured fault cleanup is no longer the active owner execution"
        );
        return Ok(ControlClaim::OwnFault);
    }
    if !owner.control_pending.swap(true, Ordering::AcqRel) {
        return Ok(ControlClaim::Claimed);
    }
    let Some(fault) = slot.as_ref() else {
        return Err(compiler::proxy_busy_error());
    };
    let complete = fault
        .result
        .lock()
        .map_err(|_| unconfirmed_error(owner, "fault cleanup outcome poisoned".into()))?;
    if matches!(complete.as_ref(), Some(Err(_))) {
        return Ok(ControlClaim::Wait(fault.clone()));
    }
    if !same_execution(&fault.scope, expected) {
        return Err(compiler::proxy_busy_error());
    }
    Ok(ControlClaim::Wait(fault.clone()))
}

fn retire_fault_after_verified_quiescence(
    owner: &Controller,
    fault: &Arc<FaultCleanup>,
) -> Result<bool> {
    let Some(scope) = fault.scope.as_ref() else {
        return Ok(false);
    };
    owner.scope.verify_owner_for_cleanup()?;
    let control_root = scope
        .work_root
        .parent()
        .and_then(std::path::Path::parent)
        .context("captured fault execution root missing")?;
    let owner_root = owner
        .scope
        .root()
        .parent()
        .and_then(std::path::Path::parent)
        .context("captured resident owner root missing")?;
    ensure!(
        control_root == owner_root && scope.supervisor_id == owner.scope.identity().owner_instance,
        "fault cleanup evidence belongs to another owner range"
    );
    // This existing aggregate receipt includes worker, command-tree and engine
    // cleanup. Missing, locked, foreign or Unknown evidence never retires here.
    if runtime_supervisor::verify_local_quiescent_for_supervisor(
        control_root,
        &scope.generation,
        &scope.supervisor_id,
    )?
    .is_none()
    {
        return Ok(false);
    }
    let mut slot = owner.fault_cleanup.lock().map_err(|_| {
        unconfirmed_error(owner, "fault cleanup retirement context poisoned".into())
    })?;
    if slot.as_ref().is_some_and(|held| Arc::ptr_eq(held, fault)) {
        *slot = None;
        owner.control_pending.store(false, Ordering::Release);
        owner.settled.notify_waiters();
    }
    Ok(true)
}

pub(crate) async fn control_publication(
    deadline: tokio::time::Instant,
) -> Result<ControlPublication> {
    let owner = controller()?.context("proxy control requires an owner capability")?;
    control_publication_until(&owner, deadline, None, runtime_supervisor::current_scope()).await
}

pub(crate) fn control_epoch() -> Result<u64> {
    Ok(controller()?
        .context("proxy control owner missing")?
        .control_epoch
        .load(Ordering::Acquire))
}

fn with_epoch_commit<T>(
    owner: &Controller,
    deadline: tokio::time::Instant,
    epoch: u64,
    publish: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let _epoch_fence = owner
        .epoch_fence
        .lock()
        .map_err(|_| anyhow::anyhow!("proxy epoch commit fence poisoned"))?;
    ensure!(
        tokio::time::Instant::now() < deadline
            && owner.control_epoch.load(Ordering::Acquire) == epoch
            && !owner.stopping.load(Ordering::Acquire)
            && !owner.entry_closing.load(Ordering::Acquire),
        "proxy confirmation expired or revoked before success commit"
    );
    publish()
}

pub(crate) fn commit_current_epoch<T>(
    deadline: tokio::time::Instant,
    epoch: u64,
    publish: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let owner = controller()?.context("proxy commit owner missing")?;
    with_epoch_commit(&owner, deadline, epoch, publish)
}

fn revoke_epoch(owner: &Controller) -> Result<u64> {
    let _epoch_fence = owner
        .epoch_fence
        .lock()
        .map_err(|_| anyhow::anyhow!("proxy epoch revocation fence poisoned"))?;
    let epoch = owner.control_epoch.fetch_add(1, Ordering::AcqRel) + 1;
    owner.interrupted.notify_waiters();
    Ok(epoch)
}

pub(crate) async fn control_revoked(epoch: u64) {
    let Ok(Some(owner)) = controller() else {
        return;
    };
    loop {
        let interrupted = owner.interrupted.notified();
        tokio::pin!(interrupted);
        interrupted.as_mut().enable();
        if owner.control_epoch.load(Ordering::Acquire) != epoch
            || owner.stopping.load(Ordering::Acquire)
        {
            return;
        }
        interrupted.await;
    }
}

impl Drop for ControlPublication {
    fn drop(&mut self) {
        if self.dispatched {
            return;
        }
        self.publication.take();
        if self.owns_busy {
            finish_writer(&self.owner);
        }
        if self.release_control {
            self.owner.control_pending.store(false, Ordering::Release);
        }
        if !self.owner.stopping.load(Ordering::Acquire)
            && !self.owner.entry_closing.load(Ordering::Acquire)
        {
            self.owner
                .pause_watch
                .store(self.watch_was_paused, Ordering::Release);
        }
    }
}

fn finish_writer(owner: &Controller) {
    owner.busy.store(false, Ordering::Release);
    owner.settled.notify_waiters();
}

async fn control_publication_until(
    owner: &Arc<Controller>,
    deadline: tokio::time::Instant,
    own_fault: Option<&Arc<FaultCleanup>>,
    expected_scope: Option<runtime_supervisor::SessionScope>,
) -> Result<ControlPublication> {
    loop {
        ensure!(
            tokio::time::Instant::now() < deadline,
            "control deadline exhausted before captured cleanup admission"
        );
        let fault = match claim_control(owner, own_fault, &expected_scope)? {
            ControlClaim::Claimed | ControlClaim::OwnFault => break,
            ControlClaim::Wait(fault) => fault,
        };
        loop {
            let done = fault.done.notified();
            tokio::pin!(done);
            done.as_mut().enable();
            let result = fault
                .result
                .lock()
                .map_err(|_| anyhow::anyhow!("fault cleanup outcome poisoned"))?
                .clone();
            match result {
                Some(Ok(())) => break,
                Some(Err(message)) => {
                    match retire_fault_after_verified_quiescence(owner, &fault) {
                        Ok(true) => break,
                        Ok(false) => {}
                        Err(error) => {
                            return Err(unconfirmed_error(
                                owner,
                                format!(
                                    "original cleanup failed: {message}; captured retirement evidence unavailable: {error:#}"
                                ),
                            ));
                        }
                    }
                    return Err(unconfirmed_error(
                        owner,
                        format!("original captured business cleanup failed: {message}"),
                    ));
                }
                None => tokio::time::timeout_at(deadline, done).await.map_err(|_| {
                    unconfirmed_error(
                        owner,
                        "original captured business cleanup did not settle before control deadline"
                            .into(),
                    )
                })?,
            }
        }
    }
    let watch_was_paused = owner.pause_watch.swap(true, Ordering::AcqRel);
    let mut control = ControlPublication {
        owner: owner.clone(),
        publication: None,
        watch_was_paused,
        owns_busy: false,
        dispatched: false,
        release_control: own_fault.is_none(),
    };
    revoke_epoch(owner)?;
    loop {
        let settled = owner.settled.notified();
        tokio::pin!(settled);
        settled.as_mut().enable();
        if owner
            .busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            control.owns_busy = true;
            break;
        }
        tokio::time::timeout_at(deadline, settled)
            .await
            .map_err(|_| {
                unconfirmed_error(
                    owner,
                    "prior proxy writer cleanup remains pending before Stop".into(),
                )
            })?;
    }
    let guard = tokio::time::timeout_at(deadline, compiler::publication_guard())
        .await
        .map_err(|_| {
            unconfirmed_error(
                owner,
                "proxy observation or recovery remains unsettled before Stop".into(),
            )
        })?;
    ensure!(
        !owner.stopping.load(Ordering::Acquire) && !owner.entry_closing.load(Ordering::Acquire),
        "proxy owner closed while preparing Stop"
    );
    control.publication = Some(guard);
    Ok(control)
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
    let epoch = revoke_epoch(&owner)?;
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
    ensure!(
        tokio::time::Instant::now() < deadline
            && !owner.stopping.load(Ordering::Acquire)
            && owner.control_epoch.load(Ordering::Acquire) == epoch,
        "resident request expired or revoked before publication dispatch"
    );
    compiler::try_publication_guard()
}

/// Admission closes before enqueueing shutdown, preventing a pending Spawn from
/// crossing it. The actor retains an unconfirmed Child and the owner lease; a
/// failed stop persists Unknown and remains retryable by the native owner loop.
pub(crate) async fn shutdown() -> Result<()> {
    let Some(owner) = controller()? else {
        return Ok(());
    };
    owner.stopping.store(true, Ordering::Release);
    revoke_epoch(&owner)?;
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

impl ResidentProxy {
    async fn run(mut self, mut rx: mpsc::Receiver<Request>) {
        let mut tick = tokio::time::interval(Duration::from_millis(200));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                request = rx.recv() => {
                    let Some(request) = request else { return; };
                    match request {
                        Request::Ensure { outcome, cancel, deadline, epoch, reply } => {
                            let result = match acquire_publication(&self.owner, deadline, epoch).await {
                                Ok(_publication) => self.ensure(outcome, cancel.as_ref(), deadline, epoch, &reply).await,
                                Err(error) => Err(error),
                            };
                            finish_writer(&self.owner);
                            drop(reply.send(result));
                        }
                        Request::Standby { operation_id, deadline, epoch, watch_was_paused, release_control, publication, reply } => {
                            // The admitted control owns this exact barrier from
                            // dispatch through actor completion; no readonly
                            // probe can claim the release/enqueue interval.
                            let result = async {
                                ensure!(tokio::time::Instant::now()<deadline
                                    && !self.owner.stopping.load(Ordering::Acquire)
                                    && self.owner.control_epoch.load(Ordering::Acquire)==epoch,
                                    "resident control expired or revoked before dispatch");
                                self.standby(operation_id, deadline, epoch, &reply).await
                            }.await;
                            drop(publication);
                            if !self.owner.stopping.load(Ordering::Acquire)
                                && !self.owner.entry_closing.load(Ordering::Acquire) {
                                self.owner.pause_watch.store(if result.is_ok() {false} else {watch_was_paused},Ordering::Release);
                            }
                            if release_control {self.owner.control_pending.store(false,Ordering::Release);}
                            finish_writer(&self.owner);
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
                        Request::Verify { expected, deadline, reply } => {
                            if reply.is_closed() {continue;}
                            let result=self.verify_retained_confirmation(&expected,deadline).await;
                            drop(reply.send(result));
                        }
                    }
                }
                _ = tick.tick() => {
                    // External supervisord control does not send an actor
                    // request. Keep the timer live so restored flags are seen;
                    // select guards are evaluated only when entering the loop.
                    if self.owner.stopping.load(Ordering::Acquire)
                        || self.owner.pause_watch.load(Ordering::Acquire)
                        || self.owner.control_pending.load(Ordering::Acquire)
                        || self.owner.busy.load(Ordering::Acquire) {continue;}
                    let deadline = tokio::time::Instant::now() + admin_probe::CONFIRM_BUDGET;
                    if self.confirmed.is_none() {
                        if tokio::time::Instant::now()<self.next_restart {continue;}
                        self.next_restart=tokio::time::Instant::now()+Duration::from_secs(1);
                        if let Some((expected,held))=compiler::confirmed_publication() {
                            match crate::services::supervisord_host::SupervisordHost::from_env().await {
                                Ok(Some(host))=>if let Err(error)=host.reconcile_official_incarnation(&self.owner.runtime_root,&expected,&held,deadline).await {
                                    tracing::error!(error=%format!("{error:#}"),"official supervisord proxy reconfirmation failed");
                                },
                                Ok(None)=>{},
                                Err(error)=>tracing::error!(error=%format!("{error:#}"),"official proxy host observation failed"),
                            }
                        }
                        continue;
                    }
                    if !matches!(self.observe_exit(), Ok(true)) { continue; }
                    let epoch = self.owner.control_epoch.load(Ordering::Acquire);
                    if let Ok(_publication) = acquire_publication(&self.owner, deadline, epoch).await
                        && let Err(error) = self.watch(deadline, epoch).await {
                        tracing::error!(error = %format!("{error:#}"), "resident supervision remains protected");
                    }
                }
            }
        }
    }

    fn unconfirmed(&self, message: String) -> anyhow::Error {
        unconfirmed_error(&self.owner, message)
    }

    #[cfg(test)]
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

    async fn spawn(
        &mut self,
        outcome: &CompileOutcome,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
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
            .arg(&outcome.config_path)
            .env("PINGAP_ADMIN_ADDR", &endpoint.addr)
            .env("PINGAP_ADMIN_USER", &endpoint.user)
            .env("PINGAP_ADMIN_PASSWORD", &endpoint.password);
        crate::proxy::official_env::sanitize(&mut command);
        command
            .env("PINGAP_ADMIN_ADDR", &endpoint.addr)
            .env("PINGAP_ADMIN_USER", &endpoint.user)
            .env("PINGAP_ADMIN_PASSWORD", &endpoint.password);
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

    async fn pin(
        &mut self,
        outcome: CompileOutcome,
        receipt: ConfirmedPublication,
        bytes: &[u8],
        deadline: tokio::time::Instant,
        epoch: u64,
    ) -> Result<()> {
        ensure!(
            uuid::Uuid::parse_str(&outcome.publication_id).is_ok(),
            "invalid resident publication identity"
        );
        // The verified snapshot is exactly the path used by the retained Child.
        let pinned = outcome.config_path.clone();
        ensure!(
            publication::validate_immutable_until(&outcome, deadline).await? == bytes,
            "verified immutable proxy snapshot changed before commit"
        );
        self.owner
            .scope
            .record_publication_until(
                serde_json::json!({
                    "config_path": pinned, "publication_id": outcome.publication_id,
                    "config_digest": outcome.config_digest, "config_hash": outcome.expected_hash,
                    "process_id": receipt.process_id, "process_instance_id": receipt.instance_id,
                    "confirmation_contract": "official_restart_http_v1",
                }),
                deadline,
            )
            .await?;
        let file =
            compiler::prepare_official_confirmation(&self.owner.runtime_root, &outcome, &receipt)?;
        let owner = self.owner.clone();
        with_epoch_commit(&owner, deadline, epoch, || {
            file.publish()?;
            ensure!(
                tokio::time::Instant::now() < deadline,
                "native proxy confirmation commit deadline exhausted"
            );
            compiler::record_confirmed_publication(&outcome, &receipt)?;
            self.confirmed = Some(Confirmed {
                outcome,
                pinned,
                receipt,
            });
            Ok(())
        })
    }

    async fn verify_retained_confirmation(
        &mut self,
        expected: &ConfirmedPublication,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        ensure!(
            tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire)
                && !self.owner.entry_closing.load(Ordering::Acquire),
            "native physical observation expired or was cancelled"
        );
        self.owner.scope.verify_owner_for_cleanup()?;
        let held = self
            .confirmed
            .as_ref()
            .context("native proxy has no retained verified snapshot")?;
        let (current, receipt) =
            compiler::confirmed_publication().context("native current confirmation missing")?;
        ensure!(
            held.receipt == *expected
                && receipt == *expected
                && held.outcome.config_path == current.config_path
                && held.outcome.config_digest == expected.config_digest
                && held.outcome.publication_id == expected.publication_id,
            "native confirmation does not belong to the retained Child and snapshot"
        );
        publication::validate_immutable_until(&held.outcome, deadline).await?;
        let child = self
            .child
            .as_mut()
            .context("native proxy has no retained Child")?;
        ensure!(
            child.id() == Some(expected.process_id) && child.try_wait_root()?.is_none(),
            "native retained Child exited or changed identity"
        );
        ensure!(
            tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire)
                && !self.owner.entry_closing.load(Ordering::Acquire),
            "native physical observation became stale"
        );
        Ok(())
    }

    async fn ensure(
        &mut self,
        outcome: CompileOutcome,
        cancel: Option<&tokio_util::sync::CancellationToken>,
        deadline: tokio::time::Instant,
        epoch: u64,
        reply: &oneshot::Sender<Result<ConfirmedPublication>>,
    ) -> Result<ConfirmedPublication> {
        ensure!(
            !reply.is_closed()
                && tokio::time::Instant::now() < deadline
                && !self.owner.stopping.load(Ordering::Acquire)
                && !self.owner.entry_closing.load(Ordering::Acquire),
            "resident request expired or was cancelled before proxy replacement"
        );
        ensure!(
            !cancel.is_some_and(|token| token.is_cancelled()),
            "proxy operation cancelled before candidate preparation"
        );
        let outcome = publication::freeze_candidate(self.owner.scope.root(), &outcome).await?;
        let previous = self.confirmed.as_ref().map(|value| value.outcome.clone());
        let mut host = NativeHost {
            proxy: self,
            acceptance: Some(reply),
            cancel,
            epoch,
        };
        let result =
            publication::restart_verified(&mut host, &outcome, previous.as_ref(), deadline).await;
        if result.is_ok() {
            self.owner.pause_watch.store(false, Ordering::Release);
        } else if self.child.is_none()
            && result
                .as_ref()
                .err()
                .is_some_and(|error| !error.is::<crate::supervisor::ShutdownUnconfirmed>())
        {
            self.confirmed = None;
            self.owner.pause_watch.store(true, Ordering::Release);
        }
        result
    }

    async fn standby(
        &mut self,
        operation_id: Option<String>,
        deadline: tokio::time::Instant,
        epoch: u64,
        reply: &oneshot::Sender<Result<()>>,
    ) -> Result<()> {
        ensure!(
            !reply.is_closed() && tokio::time::Instant::now() < deadline,
            "resident drain expired before proxy replacement"
        );
        if self.child.is_none() && self.confirmed.is_none() {
            guardian::recover(self.owner.scope.root())
                .context("prove no resident entry exists before drain")?;
            return Ok(());
        }
        let topology = self
            .confirmed
            .as_ref()
            .map(|value| value.pinned.clone())
            .unwrap_or_else(|| compiler::active_config_path(&self.owner.runtime_root));
        let mut outcome =
            publication::standby_candidate(self.owner.scope.root(), &topology).await?;
        outcome.operation_id = operation_id;
        let bytes = publication::validate_immutable(&outcome).await?;
        let graph = pingap_config::PingapConfig::new(&bytes, true)?;
        compiler::validate_candidate_until(
            &self.args.pingap_bin,
            &graph,
            &self.owner.runtime_root,
            deadline,
        )
        .await?;
        ensure!(
            !reply.is_closed(),
            "resident drain was cancelled before stopping the old instance"
        );
        let previous = self.confirmed.as_ref().map(|value| value.outcome.clone());
        let mut host = NativeHost {
            proxy: self,
            acceptance: None,
            cancel: None,
            epoch,
        };
        publication::restart_verified(&mut host, &outcome, previous.as_ref(), deadline)
            .await
            .map(|_| ())
    }

    async fn restore_with_deadline(
        &mut self,
        deadline: tokio::time::Instant,
        epoch: u64,
    ) -> Result<()> {
        let previous = self
            .confirmed
            .as_ref()
            .map(|value| value.outcome.clone())
            .context("resident has no verified snapshot to restore")?;
        let mut host = NativeHost {
            proxy: self,
            acceptance: None,
            cancel: None,
            epoch,
        };
        publication::restart_verified(&mut host, &previous, None, deadline)
            .await
            .map(|_| ())
    }

    async fn watch(&mut self, deadline: tokio::time::Instant, epoch: u64) -> Result<()> {
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
        self.restore_with_deadline(deadline, epoch).await
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

struct NativeHost<'a> {
    proxy: &'a mut ResidentProxy,
    acceptance: Option<&'a oneshot::Sender<Result<ConfirmedPublication>>>,
    cancel: Option<&'a tokio_util::sync::CancellationToken>,
    epoch: u64,
}

impl NativeHost<'_> {
    fn admitted(&self, deadline: tokio::time::Instant) -> Result<()> {
        ensure!(
            tokio::time::Instant::now() < deadline
                && !self.proxy.owner.stopping.load(Ordering::Acquire)
                && !self.proxy.owner.entry_closing.load(Ordering::Acquire)
                && self.proxy.owner.control_epoch.load(Ordering::Acquire) == self.epoch
                && self.acceptance.is_none_or(|reply| !reply.is_closed())
                && !self.cancel.is_some_and(|token| token.is_cancelled()),
            "proxy operation expired or was cancelled"
        );
        Ok(())
    }
}

#[async_trait::async_trait]
impl ProcessHost for NativeHost<'_> {
    async fn stop_exact(&mut self, _deadline: tokio::time::Instant) -> Result<()> {
        // Cleanup is never cancelled: it settles the retained original Child.
        if let Some(child) = self.proxy.child.as_mut() {
            ensure!(
                child
                    .stop(Duration::from_secs(crate::supervision::STOP_GRACE_SECONDS))
                    .await
                    != StopOutcome::Unconfirmed,
                "resident exact process cleanup remains unconfirmed"
            );
        }
        guardian::recover(self.proxy.owner.scope.root())
            .context("confirm resident descendant cleanup")?;
        self.proxy.child.take();
        self.proxy.owner.status.store(ABSENT, Ordering::Release);
        compiler::clear_current_confirmation()?;
        if !self.proxy.owner.stopping.load(Ordering::Acquire) {
            self.proxy.owner.scope.record_running()?;
        }
        Ok(())
    }

    async fn start_exact(
        &mut self,
        candidate: &CompileOutcome,
        deadline: tokio::time::Instant,
    ) -> Result<ProcessInstance> {
        self.admitted(deadline)?;
        self.proxy.spawn(candidate, deadline).await?;
        let pid = self
            .proxy
            .child
            .as_ref()
            .and_then(OwnedChild::id)
            .context("resident command PID missing")?;
        Ok(ProcessInstance {
            pid,
            id: uuid::Uuid::new_v4().to_string(),
        })
    }

    async fn verify_exact(
        &mut self,
        expected: &ProcessInstance,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.admitted(deadline)?;
        ensure!(
            self.proxy.child.as_ref().and_then(OwnedChild::id) == Some(expected.pid)
                && !self.proxy.observe_exit()?,
            "resident process exited or changed during verification"
        );
        Ok(())
    }

    async fn confirm(
        &mut self,
        candidate: &CompileOutcome,
        instance: &ProcessInstance,
        deadline: tokio::time::Instant,
    ) -> Result<ConfirmedPublication> {
        self.admitted(deadline)?;
        let endpoint = admin_probe::ensure_admin_endpoint()?.clone();
        let cancellation = async {
            match self.cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            ()=cancellation=>Err(anyhow::anyhow!("proxy confirmation cancelled by its operation")),
            result=admin_probe::wait_for_official_publication(&endpoint,candidate,instance.pid,&instance.id,
                deadline.saturating_duration_since(tokio::time::Instant::now()))=>result,
            ()=control_revoked(self.epoch)=>Err(anyhow::anyhow!("proxy confirmation revoked by Stop or Shutdown")),
        }
    }

    async fn commit(
        &mut self,
        candidate: &CompileOutcome,
        receipt: &ConfirmedPublication,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        self.admitted(deadline)?;
        let bytes = publication::validate_immutable_until(candidate, deadline).await?;
        // Persist the diagnostic view before the final confirmation. Active
        // replacement temporarily marks uncertainty; final pin clears it.
        compiler::publish_active_until(
            &self.proxy.owner.runtime_root,
            &candidate.config_path,
            deadline,
        )
        .await?;
        self.admitted(deadline)?;
        self.proxy
            .pin(
                candidate.clone(),
                receipt.clone(),
                &bytes,
                deadline,
                self.epoch,
            )
            .await?;
        Ok(())
    }

    fn mark_unknown(&self, message: String) -> anyhow::Error {
        self.proxy.unconfirmed(message)
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
            busy: AtomicBool::new(false),
            control_pending: AtomicBool::new(false),
            fault_cleanup: std::sync::Mutex::new(None),
            settled: tokio::sync::Notify::new(),
            status: AtomicU8::new(ABSENT),
            control_epoch: AtomicU64::new(0),
            epoch_fence: std::sync::Mutex::new(()),
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
    async fn busy_and_cancelled_reply_cannot_publish_after_gate_release() {
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
            serving_release_id: None,
            operation_id: None,
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
            busy: AtomicBool::new(false),
            control_pending: AtomicBool::new(false),
            fault_cleanup: std::sync::Mutex::new(None),
            settled: tokio::sync::Notify::new(),
            status: AtomicU8::new(ABSENT),
            control_epoch: AtomicU64::new(0),
            epoch_fence: std::sync::Mutex::new(()),
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
            cancel: None,
            outcome: outcome.clone(),
            epoch: 0,
            deadline: tokio::time::Instant::now() + Duration::from_millis(10),
            reply,
        })
        .await
        .unwrap();
        let (cancelled, receive_cancelled) = oneshot::channel();
        tx.send(Request::Ensure {
            cancel: None,
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
        assert!(format!("{error:#}").contains("operation is already in progress"));
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
    /// Real Child/OS liveness with local fixture config; this does not claim
    /// Pingap HTTP acceptance. The observer cannot create or revive a process.
    #[cfg(unix)]
    #[tokio::test]
    async fn readonly_native_confirmation_requires_retained_cookie_and_live_child() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, mut proxy) = owned_proxy(root.path());
        let id = uuid::Uuid::new_v4().to_string();
        let (content, _) = compiler::build_standby_config(&id).unwrap();
        let graph = pingap_config::PingapConfig::new(content.as_bytes(), true).unwrap();
        let outcome =
            compiler::fixture_persist_candidate(&proxy.owner.runtime_root, &graph, &id, 503)
                .await
                .unwrap();
        let outcome = publication::freeze_candidate(proxy.owner.scope.root(), &outcome)
            .await
            .unwrap();
        let mut command = tokio::process::Command::new("sleep");
        command.arg("60");
        let child = process_utils::managed_tree::spawn_managed(command).unwrap();
        let pid = child.id().unwrap();
        proxy.child = Some(OwnedChild::Direct(child));
        let receipt = ConfirmedPublication {
            process_id: pid,
            instance_id: uuid::Uuid::new_v4().to_string(),
            publication_id: outcome.publication_id.clone(),
            operation_id: outcome.operation_id.clone(),
            config_hash: outcome.expected_hash.clone(),
            config_digest: outcome.config_digest.clone(),
            attempt_id: 1,
        };
        let bytes = publication::validate_immutable(&outcome).await.unwrap();
        proxy
            .pin(
                outcome,
                receipt.clone(),
                &bytes,
                tokio::time::Instant::now() + Duration::from_secs(5),
                proxy.owner.control_epoch.load(Ordering::Acquire),
            )
            .await
            .unwrap();
        let claim_path = proxy.owner.scope.root().join("resident-scope.json");
        let original_claim = std::fs::read(&claim_path).unwrap();
        proxy
            .verify_retained_confirmation(
                &receipt,
                tokio::time::Instant::now() + Duration::from_secs(2),
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&claim_path).unwrap(),
            original_claim,
            "read-only confirmation rewrote owner state"
        );
        let mut foreign = receipt.clone();
        foreign.instance_id = uuid::Uuid::new_v4().to_string();
        assert!(
            proxy
                .verify_retained_confirmation(
                    &foreign,
                    tokio::time::Instant::now() + Duration::from_secs(2)
                )
                .await
                .is_err()
        );
        assert!(
            proxy.child.as_mut().unwrap().stop(Duration::ZERO).await != StopOutcome::Unconfirmed
        );
        assert!(
            proxy
                .verify_retained_confirmation(
                    &receipt,
                    tokio::time::Instant::now() + Duration::from_secs(2)
                )
                .await
                .is_err(),
            "dead Child must not reuse a previously healthy nonce or config hash"
        );
        assert!(
            proxy
                .verify_retained_confirmation(
                    &receipt,
                    tokio::time::Instant::now() - Duration::from_millis(1)
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(claim_path).unwrap(), original_claim);
    }
    /// Actual actor entry and real HTTP/Child, with an explicitly controlled
    /// already-confirmed baseline. This checks pre-mutation conservation only;
    /// official Pingap acceptance is provided by the isolated binary tests.
    #[cfg(unix)]
    #[tokio::test]
    async fn native_preflight_rejection_keeps_http_and_serving_view_without_success() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, mut proxy) = owned_proxy(root.path());
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mut command = tokio::process::Command::new("python3");
        command.arg("-u").arg("-c").arg(format!("from http.server import BaseHTTPRequestHandler,HTTPServer\nclass H(BaseHTTPRequestHandler):\n def do_GET(self):\n  self.send_response(200);self.end_headers();self.wfile.write(b'old-proxy-http')\nHTTPServer(('127.0.0.1',{port}),H).serve_forever()\n"));
        let child = process_utils::managed_tree::spawn_managed(command).unwrap();
        let pid = child.id().unwrap();
        proxy.child = Some(OwnedChild::Direct(child));
        proxy.args.pingap_bin = root.path().join("missing-pingap");
        let id = uuid::Uuid::new_v4().to_string();
        let (content, _) = compiler::build_standby_config(&id).unwrap();
        let graph = pingap_config::PingapConfig::new(content.as_bytes(), true).unwrap();
        let mut outcome =
            compiler::fixture_persist_candidate(&proxy.owner.runtime_root, &graph, &id, 200)
                .await
                .unwrap();
        outcome
            .entry_probes
            .iter_mut()
            .for_each(|probe| probe.expected_status = 200);
        let outcome = publication::freeze_candidate(proxy.owner.scope.root(), &outcome)
            .await
            .unwrap();
        let receipt = ConfirmedPublication {
            process_id: pid,
            instance_id: uuid::Uuid::new_v4().to_string(),
            publication_id: outcome.publication_id.clone(),
            operation_id: outcome.operation_id.clone(),
            config_hash: outcome.expected_hash.clone(),
            config_digest: outcome.config_digest.clone(),
            attempt_id: 1,
        };
        let bytes = publication::validate_immutable(&outcome).await.unwrap();
        proxy
            .pin(
                outcome.clone(),
                receipt,
                &bytes,
                tokio::time::Instant::now() + Duration::from_secs(5),
                proxy.owner.control_epoch.load(Ordering::Acquire),
            )
            .await
            .unwrap();
        let previous = compiler::official_confirmation().unwrap();
        assert_eq!(previous.phase, shared_types::ProxyPhase::Serving);
        let (tx, rx) = mpsc::channel(16);
        let owner = Arc::new(Controller {
            scope: proxy.owner.scope.clone(),
            runtime_root: proxy.owner.runtime_root.clone(),
            workspace: proxy.owner.workspace.clone(),
            tx,
            stopping: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            control_pending: AtomicBool::new(false),
            fault_cleanup: std::sync::Mutex::new(None),
            settled: tokio::sync::Notify::new(),
            status: AtomicU8::new(LIVE),
            control_epoch: AtomicU64::new(0),
            epoch_fence: std::sync::Mutex::new(()),
            entry_closing: AtomicBool::new(false),
            pause_watch: AtomicBool::new(false),
            interrupted: tokio::sync::Notify::new(),
        });
        proxy.owner = owner.clone();
        *RESIDENT.lock().unwrap() = Some(owner.clone());
        let actor = tokio::spawn(proxy.run(rx));
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let url = format!("http://127.0.0.1:{port}/");
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if client.get(&url).send().await.is_ok() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let mut rejected = outcome;
        rejected.config_path = root.path().join("absent-candidate.toml");
        assert!(
            reload(&rejected).await.is_err(),
            "rejected immutable read must stay failed"
        );
        assert_eq!(compiler::official_confirmation().as_ref(), Some(&previous));
        assert!(
            publish_standby_if_serving().await.is_err(),
            "failed native preflight must stay failed"
        );
        assert_eq!(
            compiler::official_confirmation().as_ref(),
            Some(&previous),
            "standby preflight left the serving view stuck Preparing"
        );
        assert!(!owner.busy.load(Ordering::Acquire));
        assert_eq!(
            client.get(&url).send().await.unwrap().text().await.unwrap(),
            "old-proxy-http"
        );
        shutdown().await.unwrap();
        actor.abort();
    }

    #[tokio::test]
    async fn empty_native_standby_does_not_publish_preparing_or_fake_serving() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, mut proxy) = owned_proxy(root.path());
        let (tx, rx) = mpsc::channel(16);
        let owner = Arc::new(Controller {
            scope: proxy.owner.scope.clone(),
            runtime_root: proxy.owner.runtime_root.clone(),
            workspace: proxy.owner.workspace.clone(),
            tx,
            stopping: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            control_pending: AtomicBool::new(false),
            fault_cleanup: std::sync::Mutex::new(None),
            settled: tokio::sync::Notify::new(),
            status: AtomicU8::new(ABSENT),
            control_epoch: AtomicU64::new(0),
            epoch_fence: std::sync::Mutex::new(()),
            entry_closing: AtomicBool::new(false),
            pause_watch: AtomicBool::new(false),
            interrupted: tokio::sync::Notify::new(),
        });
        proxy.owner = owner.clone();
        *RESIDENT.lock().unwrap() = Some(owner);
        let actor = tokio::spawn(proxy.run(rx));
        assert!(compiler::official_confirmation().is_none());
        publish_standby_if_serving().await.unwrap();
        assert!(compiler::official_confirmation().is_none());
        shutdown().await.unwrap();
        actor.abort();
    }

    #[test]
    fn admitted_stop_waits_for_prior_publication_then_settles_without_busy_failure() {
        let _environment = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let temporary = tempfile::tempdir().unwrap();
                let root = temporary.path().canonicalize().unwrap();
                let (_lease, proxy) = owned_proxy(&root);
                let args = RuntimeArgs {
                    workspace: root.clone(),
                    log_dir: root.join("logs"),
                    ..Default::default()
                };
                bind_owner(proxy.owner.scope.clone(), &args).unwrap();
                let gate = compiler::publication_guard().await;
                let stop = publish_standby_for_operation_until(
                    tokio::time::Instant::now() + Duration::from_secs(2),
                    Some("admitted-stop-original-id".into()),
                );
                tokio::pin!(stop);
                // The old implementation returns Busy here even though Stop was
                // accepted; this is a stage barrier, not a timing-only final assertion.
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), &mut stop)
                        .await
                        .is_err(),
                    "admitted Stop rejected the prior publication instead of settling it"
                );
                let owner = controller().unwrap().unwrap();
                assert!(owner.busy.load(Ordering::Acquire));
                assert!(owner.pause_watch.load(Ordering::Acquire));
                assert!(compiler::try_publication_guard().is_err());
                drop(gate);
                stop.await.unwrap();
                assert!(!owner.busy.load(Ordering::Acquire));
                assert!(compiler::try_publication_guard().is_ok());
                assert!(compiler::official_confirmation().is_none());
                shutdown().await.unwrap();
            });
    }

    #[tokio::test]
    async fn revoked_native_epoch_cannot_late_commit_after_a_missed_notification() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, mut proxy) = owned_proxy(root.path());
        let epoch = proxy.owner.control_epoch.load(Ordering::Acquire);
        // Stop may run before the confirmation future registers its Notify.
        // The epoch is authoritative even when no waiter saw that notification.
        proxy.owner.control_epoch.fetch_add(1, Ordering::AcqRel);
        proxy.owner.interrupted.notify_waiters();
        let host = NativeHost {
            proxy: &mut proxy,
            acceptance: None,
            cancel: None,
            epoch,
        };
        assert!(
            host.admitted(tokio::time::Instant::now() + Duration::from_secs(1))
                .is_err()
        );
    }

    #[test]
    fn revoked_epoch_discards_prepared_success_before_sidecar_or_memory_publication() {
        let _environment = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let root = tempfile::tempdir().unwrap();
                let (_lease, proxy) = owned_proxy(root.path());
                compiler::clear_current_confirmation().unwrap();
                let id = uuid::Uuid::new_v4().to_string();
                let (text, _) = compiler::build_standby_config(&id).unwrap();
                let graph = pingap_config::PingapConfig::new(text.as_bytes(), true).unwrap();
                let outcome = compiler::fixture_persist_candidate(
                    &proxy.owner.runtime_root,
                    &graph,
                    &id,
                    503,
                )
                .await
                .unwrap();
                let receipt = ConfirmedPublication {
                    process_id: std::process::id(),
                    instance_id: uuid::Uuid::new_v4().to_string(),
                    publication_id: id,
                    operation_id: outcome.operation_id.clone(),
                    config_hash: outcome.expected_hash.clone(),
                    config_digest: outcome.config_digest.clone(),
                    attempt_id: 1,
                };
                let epoch = proxy.owner.control_epoch.load(Ordering::Acquire);
                let file = compiler::prepare_official_confirmation(
                    &proxy.owner.runtime_root,
                    &outcome,
                    &receipt,
                )
                .unwrap();
                revoke_epoch(&proxy.owner).unwrap();
                let result = with_epoch_commit(
                    &proxy.owner,
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    epoch,
                    || {
                        file.publish()?;
                        compiler::record_confirmed_publication(&outcome, &receipt)
                    },
                );
                assert!(result.is_err(), "revoked writer published a late success");
                assert!(
                    !proxy
                        .owner
                        .runtime_root
                        .join("official-proxy-confirmation.json")
                        .exists()
                );
                assert!(compiler::official_confirmation().is_none());
                assert!(compiler::confirmed_publication().is_none());
            });
    }

    #[cfg(unix)]
    #[test]
    fn watchdog_resumes_after_external_control_flags_without_any_actor_message() {
        let _environment = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let temporary = tempfile::tempdir().unwrap();
                let (_lease, mut proxy) = owned_proxy(temporary.path());
                let mut command = tokio::process::Command::new("sleep");
                command.arg("60");
                let child = process_utils::managed_tree::spawn_managed(command).unwrap();
                let pid = child.id().unwrap();
                let id = uuid::Uuid::new_v4().to_string();
                let (text, _) = compiler::build_standby_config(&id).unwrap();
                let graph = pingap_config::PingapConfig::new(text.as_bytes(), true).unwrap();
                let outcome = compiler::fixture_persist_candidate(
                    &proxy.owner.runtime_root,
                    &graph,
                    &id,
                    503,
                )
                .await
                .unwrap();
                let receipt = ConfirmedPublication {
                    process_id: pid,
                    instance_id: uuid::Uuid::new_v4().to_string(),
                    publication_id: id,
                    operation_id: outcome.operation_id.clone(),
                    config_hash: outcome.expected_hash.clone(),
                    config_digest: outcome.config_digest.clone(),
                    attempt_id: 1,
                };
                proxy.child = Some(OwnedChild::Direct(child));
                proxy.confirmed = Some(Confirmed {
                    pinned: outcome.config_path.clone(),
                    outcome,
                    receipt,
                });
                // Exercise observation/resumption, not a fake replacement
                // executable: an existing retry backoff defers all spawns.
                proxy.next_restart = tokio::time::Instant::now() + Duration::from_secs(60);
                let (tx, rx) = mpsc::channel(16);
                let owner = Arc::new(Controller {
                    scope: proxy.owner.scope.clone(),
                    runtime_root: proxy.owner.runtime_root.clone(),
                    workspace: proxy.owner.workspace.clone(),
                    tx,
                    stopping: AtomicBool::new(false),
                    busy: AtomicBool::new(true),
                    control_pending: AtomicBool::new(false),
                    fault_cleanup: std::sync::Mutex::new(None),
                    settled: tokio::sync::Notify::new(),
                    status: AtomicU8::new(LIVE),
                    control_epoch: AtomicU64::new(0),
                    epoch_fence: std::sync::Mutex::new(()),
                    entry_closing: AtomicBool::new(false),
                    pause_watch: AtomicBool::new(true),
                    interrupted: tokio::sync::Notify::new(),
                });
                proxy.owner = owner.clone();
                let actor = tokio::spawn(proxy.run(rx));
                // Current-thread executor polls the actor while both gates are
                // closed. No request, query or sender notification will wake it.
                tokio::task::yield_now().await;
                assert!(
                    std::process::Command::new("kill")
                        .args(["-KILL", &pid.to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
                tokio::time::sleep(Duration::from_millis(250)).await;
                assert_eq!(
                    owner.status.load(Ordering::Acquire),
                    LIVE,
                    "paused watcher performed process IO"
                );
                assert!(
                    process_utils::process_exists(pid).unwrap(),
                    "paused actor already reaped the captured Child"
                );
                owner.pause_watch.store(false, Ordering::Release);
                finish_writer(&owner);
                let result = tokio::time::timeout(Duration::from_secs(2), async {
                    while process_utils::process_exists(pid).unwrap() {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await;
                // This is real actor scheduling with an owned Child, not a fake
                // Pingap success. The dead Child must be observed without a poke.
                assert!(
                    result.is_ok(),
                    "watchdog stayed asleep after external control resumed without an actor message"
                );
                assert!(
                    !process_utils::process_exists(pid).unwrap(),
                    "resumed actor did not reap the exact dead Child"
                );
                actor.abort();
                let _ = actor.await;
            });
    }

    #[cfg(unix)]
    #[test]
    fn admitted_source_waits_for_captured_fault_child_cleanup_without_new_queue() {
        let _environment = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let root = tempfile::tempdir().unwrap();
                let (_lease, proxy) = owned_proxy(root.path());
                bind_owner(
                    proxy.owner.scope.clone(),
                    &RuntimeArgs {
                        workspace: root.path().canonicalize().unwrap(),
                        log_dir: root.path().join("logs"),
                        ..Default::default()
                    },
                )
                .unwrap();
                let fault = begin_fault_cleanup(runtime_supervisor::current_scope())
                    .unwrap()
                    .unwrap();
                let mut command = tokio::process::Command::new("sleep");
                command.arg("60");
                let mut old = process_utils::managed_tree::spawn_managed(command).unwrap();
                let pid = old.id().unwrap();
                let next = publish_standby_for_operation_until(
                    tokio::time::Instant::now() + Duration::from_secs(3),
                    Some("accepted-source-recovery".into()),
                );
                tokio::pin!(next);
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), &mut next)
                        .await
                        .is_err(),
                    "accepted recovery rejected an unfinished fault drain"
                );
                assert!(process_utils::process_exists(pid).unwrap());
                assert!(old.stop(Duration::ZERO).await != StopOutcome::Unconfirmed);
                assert!(!process_utils::process_exists(pid).unwrap());
                fault.finish(&Ok(()));
                next.await.unwrap();
                assert!(
                    compiler::official_confirmation().is_none(),
                    "empty actor manufactured proxy success"
                );
                shutdown().await.unwrap();
            });
    }

    #[test]
    fn fault_unknown_protects_waiting_and_fresh_controls_until_verified_retirement() {
        let _environment = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let root = tempfile::tempdir().unwrap();
                let (_lease, proxy) = owned_proxy(root.path());
                bind_owner(
                    proxy.owner.scope.clone(),
                    &RuntimeArgs {
                        workspace: root.path().canonicalize().unwrap(),
                        log_dir: root.path().join("logs"),
                        ..Default::default()
                    },
                )
                .unwrap();
                let fault = begin_fault_cleanup(runtime_supervisor::current_scope())
                    .unwrap()
                    .unwrap();
                let next = publish_standby_for_operation_until(
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    Some("waiting-source".into()),
                );
                tokio::pin!(next);
                assert!(
                    tokio::time::timeout(Duration::from_millis(20), &mut next)
                        .await
                        .is_err()
                );
                let owner = controller().unwrap().unwrap();
                let epoch = owner.control_epoch.load(Ordering::Acquire);
                fault.finish(&Err(crate::supervisor::ShutdownUnconfirmed(
                    "controlled captured Child cleanup unknown".into(),
                )
                .into()));
                assert!(
                    next.await
                        .unwrap_err()
                        .is::<crate::supervisor::ShutdownUnconfirmed>()
                );
                let fresh = publish_standby_for_operation_until(
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    Some("fresh-source".into()),
                )
                .await
                .unwrap_err();
                assert!(fresh.is::<crate::supervisor::ShutdownUnconfirmed>());
                assert!(owner.control_pending.load(Ordering::Acquire));
                assert!(owner.fault_cleanup.lock().unwrap().is_some());
                assert_eq!(
                    owner.control_epoch.load(Ordering::Acquire),
                    epoch,
                    "Unknown admitted a fresh writer"
                );
                shutdown().await.unwrap();
            });
    }

    #[test]
    fn fault_wait_keeps_original_budget_and_rejects_other_execution() {
        let _environment = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let root = tempfile::tempdir().unwrap();
                let (_lease, proxy) = owned_proxy(root.path());
                bind_owner(
                    proxy.owner.scope.clone(),
                    &RuntimeArgs {
                        workspace: root.path().canonicalize().unwrap(),
                        log_dir: root.path().join("logs"),
                        ..Default::default()
                    },
                )
                .unwrap();
                let scope = runtime_supervisor::SessionScope {
                    work_root: root.path().join("work/original"),
                    generation: uuid::Uuid::new_v4().to_string(),
                    supervisor_id: proxy.owner.scope.identity().owner_instance.clone(),
                };
                let fault = begin_fault_cleanup(Some(scope.clone())).unwrap().unwrap();
                let owner = controller().unwrap().unwrap();
                let other = runtime_supervisor::SessionScope {
                    generation: uuid::Uuid::new_v4().to_string(),
                    ..scope.clone()
                };
                let different = control_publication_until(
                    &owner,
                    tokio::time::Instant::now() + Duration::from_secs(1),
                    None,
                    Some(other),
                )
                .await;
                assert!(
                    different
                        .err()
                        .unwrap()
                        .is::<compiler::ProxyOperationBusy>()
                );
                let begin = tokio::time::Instant::now();
                let waited = control_publication_until(
                    &owner,
                    begin + Duration::from_millis(20),
                    None,
                    Some(scope),
                )
                .await;
                assert!(
                    waited
                        .err()
                        .unwrap()
                        .is::<crate::supervisor::ShutdownUnconfirmed>()
                );
                assert!(
                    begin.elapsed() < Duration::from_millis(250),
                    "fault wait acquired another complete budget"
                );
                fault.finish(&Ok(()));
                shutdown().await.unwrap();
            });
    }
}
