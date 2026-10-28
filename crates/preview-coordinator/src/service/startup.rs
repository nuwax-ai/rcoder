//! A request waits for an admitted startup; it never owns/cancels its execution.
//! One owner retains admission, bounded strict-port retries and terminal write
//! evidence across unavailable replies, until committed or superseded.
use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::watch;

const RECONCILE_DELAY: Duration = Duration::from_millis(100);

type OwnerKey = String;
#[derive(Clone, Default)]
pub(super) struct StartupOwners(Arc<Mutex<BTreeMap<OwnerKey, Arc<OwnedStartAttempt>>>>);

#[derive(Clone)]
pub(super) enum StartupResult {
    Ready(PreviewStartEnvelope),
    Error(Arc<PreviewCoordinationError>),
}

#[derive(Clone, Copy)]
enum StartupPhase {
    Admitting,
    Idle,
    Running,
    Finished,
}

pub(super) struct OwnedStartAttempt {
    input: AcceptStartInput,
    cancelled: AtomicBool,
    phase: Mutex<StartupPhase>,
    admitted: Mutex<Option<PreviewInstanceRecord>>,
    // True once the logical chain can no longer spawn: a final outcome, or
    // cancellation after admission and while no native future remains running.
    // Persistence reconciliation may continue after this execution proof.
    execution_done: watch::Sender<bool>,
    result: watch::Sender<Option<StartupResult>>,
}

impl OwnedStartAttempt {
    fn cancel(&self) -> Result<(), PreviewCoordinationError> {
        let phase = self
            .phase
            .lock()
            .map_err(|_| unavailable("startup phase unavailable"))?;
        // The same lock guards the next native attempt. Once Idle is cancelled,
        // no pending port-CAS reply can authorize another physical start.
        self.cancelled.store(true, Ordering::Release);
        match *phase {
            StartupPhase::Idle | StartupPhase::Finished => {
                self.execution_done.send_replace(true);
            }
            StartupPhase::Admitting | StartupPhase::Running => {}
        }
        Ok(())
    }
    fn admitted(&self, row: &PreviewInstanceRecord) -> Result<(), PreviewCoordinationError> {
        *self
            .admitted
            .lock()
            .map_err(|_| unavailable("startup admission ownership unavailable"))? =
            Some(row.clone());
        self.native_idle()
    }
    fn begin_native(&self) -> Result<bool, PreviewCoordinationError> {
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| unavailable("startup phase unavailable"))?;
        if self.cancelled() {
            return Ok(false);
        }
        *phase = StartupPhase::Running;
        self.execution_done.send_replace(false);
        Ok(true)
    }
    fn native_idle(&self) -> Result<(), PreviewCoordinationError> {
        let mut phase = self
            .phase
            .lock()
            .map_err(|_| unavailable("startup phase unavailable"))?;
        *phase = StartupPhase::Idle;
        if self.cancelled() {
            self.execution_done.send_replace(true);
        }
        Ok(())
    }
    pub(super) fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn execution_finished(&self) {
        if let Ok(mut phase) = self.phase.lock() {
            *phase = StartupPhase::Finished;
        }
        self.execution_done.send_replace(true);
    }
    async fn wait_execution(&self) -> Result<(), PreviewCoordinationError> {
        let mut completion = self.execution_done.subscribe();
        loop {
            if *completion.borrow() {
                return Ok(());
            }
            completion
                .changed()
                .await
                .map_err(|_| unavailable("startup execution observation closed"))?;
        }
    }
    #[cfg(test)]
    pub(super) fn observer_count(&self) -> usize {
        self.result.receiver_count()
    }
    pub(super) async fn wait_result(&self) -> Result<StartupResult, PreviewCoordinationError> {
        let mut result = self.result.subscribe();
        loop {
            if let Some(value) = result.borrow().clone() {
                return Ok(value);
            }
            result
                .changed()
                .await
                .map_err(|_| unavailable("startup owner ended without an outcome"))?;
        }
    }
}

impl StartupOwners {
    fn join_or_insert(
        &self,
        owner: Arc<OwnedStartAttempt>,
    ) -> Result<(Arc<OwnedStartAttempt>, bool), PreviewCoordinationError> {
        let mut owners = self
            .0
            .lock()
            .map_err(|_| unavailable("startup owner registry unavailable"))?;
        if let Some(current) = owners.get(&owner.input.preview_key) {
            if current.input.project_id != owner.input.project_id
                || current.input.project_path != owner.input.project_path
            {
                return Err(conflict(
                    "preview key already has a different project startup",
                ));
            }
            if let Some(requested) = &owner.input.base_path {
                // A default base follows the final port. Its temporary
                // allocation cannot satisfy a follower's explicit fixed base.
                if current.input.base_path.as_ref() != Some(requested) {
                    return Err(conflict(
                        "preview startup already uses a different base path",
                    ));
                }
            }
            return Ok((current.clone(), false));
        }
        owners.insert(owner.input.preview_key.clone(), owner.clone());
        Ok((owner, true))
    }
    pub(super) fn get(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<Option<Arc<OwnedStartAttempt>>, PreviewCoordinationError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| unavailable("startup owner registry unavailable"))?
            .get(key)
            .filter(|owner| owner.input.instance_id == instance)
            .cloned())
    }
    fn project(
        &self,
        project: &str,
    ) -> Result<Vec<Arc<OwnedStartAttempt>>, PreviewCoordinationError> {
        Ok(self
            .0
            .lock()
            .map_err(|_| unavailable("startup owner registry unavailable"))?
            .values()
            .filter(|owner| owner.input.project_id == project)
            .cloned()
            .collect())
    }
    fn remove(&self, owner: &Arc<OwnedStartAttempt>) {
        if let Ok(mut owners) = self.0.lock() {
            let key = owner.input.preview_key.clone();
            if owners
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, owner))
            {
                owners.remove(&key);
            }
        }
    }
}

#[derive(Clone)]
struct StartupResources {
    store: Arc<dyn shared_types::PreviewLifecycleStore>,
    executor: Arc<dyn shared_types::PreviewExecutor>,
    route_cache: Arc<RouteCache>,
}
struct Completion {
    result: StartupResult,
}
impl Completion {
    fn error(error: PreviewCoordinationError) -> Self {
        Self {
            result: StartupResult::Error(Arc::new(error)),
        }
    }
}

pub(super) fn copy_error(error: &PreviewCoordinationError) -> PreviewCoordinationError {
    match error {
        PreviewCoordinationError::Unavailable(detail) => unavailable(detail),
        PreviewCoordinationError::Conflict(detail) => conflict(detail),
        PreviewCoordinationError::Invalid(detail) => invalid(detail),
        PreviewCoordinationError::Business { code, message } => {
            PreviewCoordinationError::Business {
                code: code.clone(),
                message: message.clone(),
            }
        }
    }
}

impl PreviewCoordinator {
    /// The registered owner retains the unresolved native start/port outcome,
    /// even after its request stops waiting. Heartbeat cannot publish it early.
    pub(crate) fn has_startup_owner(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<bool, PreviewCoordinationError> {
        self.startup_owners
            .get(key, instance)
            .map(|owner| owner.is_some())
    }

    /// Register before the first admission await. Even a cancelled PG reply
    /// receiver cannot orphan a committed Starting row without an executor owner.
    pub(super) fn start_owned(
        &self,
        input: AcceptStartInput,
    ) -> Result<Arc<OwnedStartAttempt>, PreviewCoordinationError> {
        let (execution_done, _) = watch::channel(false);
        let (result, _) = watch::channel(None);
        let owner = Arc::new(OwnedStartAttempt {
            input,
            cancelled: AtomicBool::new(false),
            phase: Mutex::new(StartupPhase::Admitting),
            admitted: Mutex::new(None),
            execution_done,
            result,
        });
        let (owner, inserted) = self.startup_owners.join_or_insert(owner)?;
        if !inserted {
            return Ok(owner);
        }
        let resources = StartupResources {
            store: self.store.clone(),
            executor: self.executor.clone(),
            route_cache: self.route_cache.clone(),
        };
        let task_owner = owner.clone();
        let worker_resources = resources.clone();
        let task = tokio::spawn(async move { worker_resources.run(&task_owner).await });
        let observed = owner.clone();
        let owners = self.startup_owners.clone();
        tokio::spawn(async move {
            match task.await {
                Ok(completion) => {
                    observed.execution_finished();
                    // A definitive result authorizes an immediate fresh entry.
                    // Retire this exact owner before waking its waiting callers.
                    owners.remove(&observed);
                    observed.result.send_replace(Some(completion.result));
                }
                Err(error) => {
                    // A panic/cancelled task is not physical cleanup evidence.
                    // Keep the original owner observable to Stop, and never
                    // manufacture a failed admission or replay native start.
                    observed.execution_finished();
                    let detail = format!("preview startup owner outcome unknown: {error}");
                    observed
                        .result
                        .send_replace(Some(StartupResult::Error(Arc::new(unavailable(&detail)))));
                    // Reconcile only the original instance after the failed
                    // future has ended. The panic itself is never a terminal
                    // proof; current exact native cleanup must confirm absence.
                    tracing::warn!(operation_id = %observed.input.operation_id, "{detail}");
                    loop {
                        let admitted = observed.admitted.lock().ok().and_then(|row| row.clone());
                        let row = match admitted {
                            Some(row) => Some(row),
                            None => match resources.store.get(&observed.input.preview_key).await {
                                Ok(Some(row)) if row.instance_id == observed.input.instance_id => {
                                    Some(row)
                                }
                                Ok(Some(_)) => {
                                    owners.remove(&observed);
                                    break;
                                }
                                Ok(None)
                                | Err(
                                    PreviewStoreError::Unavailable(_)
                                    | PreviewStoreError::Conflict(_)
                                    | PreviewStoreError::Invalid(_),
                                ) => None,
                            },
                        };
                        if let Some(row) = row {
                            let completion = resources
                                .unknown(&observed, &row, unavailable(&detail))
                                .await;
                            observed.result.send_replace(Some(completion.result));
                            owners.remove(&observed);
                            break;
                        }
                        tokio::time::sleep(RECONCILE_DELAY).await;
                    }
                }
            }
        });
        Ok(owner)
    }

    /// The native future is not aborted. A stop intent prevents a later spawn
    /// before execution starts, or drains that exact instance after it finishes.
    pub(super) async fn stop_local_startup(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<ExecutorStopOutcome, PreviewCoordinationError> {
        let owner = self.startup_owners.get(key, instance)?;
        if let Some(owner) = &owner {
            owner.cancel()?;
        }
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.config.remote_dispatch_timeout_secs);
        let outcome = tokio::time::timeout_at(deadline, async {
            if let Some(owner) = &owner {
                // Stop an already registered PID immediately, so readiness can
                // observe its exit. NotRegistered is only an early observation:
                // preparation may still register after it, so wait and confirm.
                if let Err(error) = self.executor.stop_local(key, instance).await {
                    tracing::debug!("early exact startup stop remains unconfirmed: {error}");
                }
                owner.wait_execution().await?;
            }
            self.executor
                .stop_local(key, instance)
                .await
                .map_err(|error| unavailable(error.to_string()))
        })
        .await
        .map_err(|_| {
            unavailable(
                "preview stop is still waiting for the original startup execution and cleanup",
            )
        })??;
        // Current exact executor observation follows completion of the original
        // future. A historical Failed string cannot invalidate NotRegistered.
        match outcome {
            ExecutorStopOutcome::Stopped | ExecutorStopOutcome::NotRegistered => {
                if let Some(owner) = &owner {
                    self.startup_owners.remove(owner);
                }
            }
            ExecutorStopOutcome::IdentityMismatch => {}
        }
        Ok(outcome)
    }

    /// Covers starts accepted by the local owner but whose admission reply is
    /// still pending, before list_active can discover their durable row.
    pub(super) async fn stop_project_startups(
        &self,
        project: &str,
    ) -> Result<(), PreviewCoordinationError> {
        let owners = self.startup_owners.project(project)?;
        for owner in &owners {
            owner.cancel()?;
        }
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(self.config.remote_dispatch_timeout_secs);
        for owner in owners {
            // The same original deadline covers early stop, execution drain and
            // the post-completion confirmation, across every captured owner.
            let early_target = owner
                .admitted
                .lock()
                .map_err(|_| unavailable("startup admission ownership unavailable"))?
                .clone();
            if let Some(target) = early_target {
                let early = tokio::time::timeout_at(
                    deadline,
                    self.executor
                        .stop_local(&target.preview_key, &target.instance_id),
                )
                .await
                .map_err(|_| {
                    unavailable("preview stop is still waiting for early startup cleanup")
                })?;
                if let Err(error) = early {
                    tracing::debug!("early project startup cleanup unconfirmed: {error}");
                }
            }
            tokio::time::timeout_at(deadline, owner.wait_execution())
                .await
                .map_err(|_| {
                    unavailable(
                        "preview stop is still waiting for the original admission/start owner",
                    )
                })??;
            let admitted = owner
                .admitted
                .lock()
                .map_err(|_| unavailable("startup admission ownership unavailable"))?
                .clone();
            let Some(target) = admitted else {
                // ExistingReady/Blocked belongs to a different real instance.
                // This attempt never owned a native execution; let list_active
                // and coordinated_stop handle that authoritative instance.
                self.startup_owners.remove(&owner);
                continue;
            };
            let outcome = tokio::time::timeout_at(
                deadline,
                self.executor
                    .stop_local(&target.preview_key, &target.instance_id),
            )
            .await
            .map_err(|_| unavailable("preview stop is still waiting for original startup cleanup"))?
            .map_err(|error| unavailable(error.to_string()))?;
            match outcome {
                ExecutorStopOutcome::Stopped | ExecutorStopOutcome::NotRegistered => {
                    self.startup_owners.remove(&owner)
                }
                ExecutorStopOutcome::IdentityMismatch => {
                    return Err(conflict(
                        "startup stop belongs to a superseded local registration",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl StartupResources {
    async fn run(&self, owner: &Arc<OwnedStartAttempt>) -> Completion {
        if owner.cancelled() {
            owner.execution_finished();
            return Completion::error(conflict("preview start was stopped before admission"));
        }
        let record = loop {
            match self.store.accept_start(owner.input.clone()).await {
                Ok(AcceptStartOutcome::Admitted(row)) => break row,
                Ok(AcceptStartOutcome::Blocked(row))
                    if row.instance_id == owner.input.instance_id
                        && row.operation_id == owner.input.operation_id
                        && row.state == PreviewInstanceState::Starting =>
                {
                    break row;
                }
                Ok(AcceptStartOutcome::ExistingReady(row)) => {
                    owner.execution_finished();
                    if let Err(error) = PreviewCoordinator::check_requested_base(
                        &row,
                        owner.input.base_path.as_deref(),
                    ) {
                        return Completion::error(error);
                    }
                    return Completion {
                        result: StartupResult::Ready(PreviewCoordinator::start_envelope(
                            &row,
                            "Development server started",
                        )),
                    };
                }
                Ok(AcceptStartOutcome::Blocked(row)) => {
                    owner.execution_finished();
                    return Completion::error(conflict(format!(
                        "preview operation in progress: state={:?}",
                        row.state
                    )));
                }
                Err(PreviewStoreError::Unavailable(detail)) => {
                    // Retry the same immutable admission, never a new operation.
                    // PG may have committed despite losing its acknowledgement.
                    tracing::debug!(operation_id = %owner.input.operation_id, "preview admission awaiting reconciliation: {detail}");
                    tokio::time::sleep(RECONCILE_DELAY).await;
                }
                Err(error @ (PreviewStoreError::Conflict(_) | PreviewStoreError::Invalid(_))) => {
                    owner.execution_finished();
                    return Completion::error(store_error(error));
                }
            }
        };
        if let Err(error) = owner.admitted(&record) {
            return Completion::error(error);
        }
        let mut record = record;
        for attempt in 0..START_PORT_ATTEMPTS {
            if owner.cancelled() {
                return self.cancelled(owner, &record).await;
            }
            // A delayed metadata reply is not permission to spawn. Revalidate
            // the exact original operation/revision immediately before each
            // physical attempt; remote Stop uses the owner's phase lock too.
            if let Err(error) = self.confirm_start(owner, &record).await {
                if owner.cancelled() {
                    return self.cancelled(owner, &record).await;
                }
                return self.fail(owner, &record, error).await;
            }
            let Some(port) = record.port else {
                return self
                    .fail(owner, &record, invalid("admitted record lacks port"))
                    .await;
            };
            let ticket = ExecutorStartTicket {
                preview_key: record.preview_key.clone(),
                log_key: record.project_id.clone(),
                project_path: record.project_path.clone(),
                port,
                base_path: record.base_path.clone(),
                instance_id: record.instance_id.clone(),
            };
            match owner.begin_native() {
                Ok(true) => {}
                Ok(false) => return self.cancelled(owner, &record).await,
                Err(error) => return Completion::error(error),
            }
            let started = self.executor.start_local(&ticket).await;
            if let Err(error) = owner.native_idle() {
                return self.unknown(owner, &record, error).await;
            }
            if owner.cancelled() {
                return self.cancelled(owner, &record).await;
            }
            match started {
                Ok((pid, actual_port)) => {
                    return self.publish(owner, &record, pid, actual_port).await;
                }
                Err(shared_types::PreviewExecutorError::PortInUse(detail)) => {
                    if attempt + 1 == START_PORT_ATTEMPTS {
                        return self
                            .fail(
                                owner,
                                &record,
                                unavailable(format!(
                                    "preview start exhausted port retry budget: {detail}"
                                )),
                            )
                            .await;
                    }
                    // The row stays Starting under one operation and instance.
                    // Only a committed exact port CAS permits the next attempt.
                    match self.retry_port(owner, &record).await {
                        Ok(next) => {
                            if let Err(error) = owner.admitted(&next) {
                                return Completion::error(error);
                            }
                            record = next;
                        }
                        Err(error) => {
                            if owner.cancelled() {
                                return self.cancelled(owner, &record).await;
                            }
                            return self.fail(owner, &record, error).await;
                        }
                    }
                }
                Err(shared_types::PreviewExecutorError::Business { code, message }) => {
                    return self
                        .fail(
                            owner,
                            &record,
                            PreviewCoordinationError::Business { code, message },
                        )
                        .await;
                }
                Err(shared_types::PreviewExecutorError::Failed(detail)) => {
                    return self.unknown(owner, &record, unavailable(detail)).await;
                }
            }
        }
        Completion::error(unavailable("preview start exhausted port retry budget"))
    }

    async fn confirm_start(
        &self,
        owner: &OwnedStartAttempt,
        record: &PreviewInstanceRecord,
    ) -> Result<(), PreviewCoordinationError> {
        loop {
            if owner.cancelled() {
                return Err(conflict("preview startup was stopped"));
            }
            match self.store.get(&record.preview_key).await {
                Ok(Some(current))
                    if current.instance_id == record.instance_id
                        && current.operation_id == record.operation_id
                        && current.revision == record.revision
                        && current.port == record.port
                        && current.state == PreviewInstanceState::Starting
                        && current.pid.is_none() =>
                {
                    return Ok(());
                }
                Ok(Some(_)) | Ok(None) => {
                    return Err(conflict(
                        "startup authority changed before native execution",
                    ));
                }
                Err(PreviewStoreError::Unavailable(_)) => tokio::time::sleep(RECONCILE_DELAY).await,
                Err(error @ (PreviewStoreError::Conflict(_) | PreviewStoreError::Invalid(_))) => {
                    return Err(store_error(error));
                }
            }
        }
    }

    async fn retry_port(
        &self,
        owner: &OwnedStartAttempt,
        record: &PreviewInstanceRecord,
    ) -> Result<PreviewInstanceRecord, PreviewCoordinationError> {
        let previous_port = record
            .port
            .ok_or_else(|| invalid("admitted record lacks port"))?;
        let occupied = loop {
            if owner.cancelled() {
                return Err(conflict("preview startup was stopped"));
            }
            match self.store.active_ports().await {
                Ok(ports) => break ports,
                Err(PreviewStoreError::Unavailable(_)) => tokio::time::sleep(RECONCILE_DELAY).await,
                Err(error @ (PreviewStoreError::Conflict(_) | PreviewStoreError::Invalid(_))) => {
                    return Err(store_error(error));
                }
            }
        };
        let mut next_port = PreviewCoordinator::next_candidate(previous_port);
        while occupied.contains(&next_port) {
            next_port = PreviewCoordinator::next_candidate(next_port);
            if next_port == previous_port {
                return Err(unavailable("preview port pool exhausted"));
            }
        }
        let input = shared_types::PreviewStartPortRetry {
            preview_key: record.preview_key.clone(),
            operation_id: record.operation_id.clone(),
            instance_id: record.instance_id.clone(),
            revision: record.revision,
            previous_port,
            next_port,
        };
        loop {
            if owner.cancelled() {
                return Err(conflict("preview startup was stopped"));
            }
            match self.store.retry_start_port(input.clone()).await {
                Ok(row) => return Ok(row),
                Err(PreviewStoreError::Unavailable(detail)) => {
                    tracing::debug!(operation_id = %record.operation_id, "preview port CAS awaiting reconciliation: {detail}");
                    tokio::time::sleep(RECONCILE_DELAY).await;
                }
                Err(error @ (PreviewStoreError::Conflict(_) | PreviewStoreError::Invalid(_))) => {
                    return Err(store_error(error));
                }
            }
        }
    }

    async fn publish(
        &self,
        owner: &Arc<OwnedStartAttempt>,
        record: &PreviewInstanceRecord,
        pid: i64,
        port: u16,
    ) -> Completion {
        let base = record
            .base_path
            .as_deref()
            .map(normalize_base_path)
            .unwrap_or_else(|| default_base(port));
        loop {
            if owner.cancelled() {
                return self.cancelled(owner, record).await;
            }
            match self
                .store
                .publish_running(
                    &record.preview_key,
                    &record.operation_id,
                    record.revision,
                    pid,
                    port,
                    Some(&base),
                )
                .await
            {
                Ok(row) => {
                    if owner.cancelled() {
                        return self.cancelled(owner, record).await;
                    }
                    self.route_cache.invalidate(port);
                    return Completion {
                        result: StartupResult::Ready(PreviewCoordinator::start_envelope(
                            &row,
                            "Development server started",
                        )),
                    };
                }
                Err(PreviewStoreError::Unavailable(detail)) => {
                    tracing::debug!(operation_id = %record.operation_id, "preview publication awaiting reconciliation: {detail}");
                    tokio::time::sleep(RECONCILE_DELAY).await;
                }
                Err(error @ (PreviewStoreError::Conflict(_) | PreviewStoreError::Invalid(_))) => {
                    // A superseded start can only drain its own native instance.
                    // Native stop checks the registration before touching a PID.
                    let failure = store_error(error);
                    match self
                        .executor
                        .stop_local(&record.preview_key, &record.instance_id)
                        .await
                    {
                        Ok(ExecutorStopOutcome::Stopped | ExecutorStopOutcome::NotRegistered) => {
                            self.settle_no_process(
                                record,
                                "startup superseded and its exact process was drained",
                            )
                            .await;
                            return Completion::error(failure);
                        }
                        Ok(ExecutorStopOutcome::IdentityMismatch) => {
                            return Completion::error(failure);
                        }
                        Err(cleanup) => {
                            return self
                                .unknown(
                                    owner,
                                    record,
                                    unavailable(format!(
                                        "{failure}; original startup cleanup pending: {cleanup}"
                                    )),
                                )
                                .await;
                        }
                    }
                }
            }
        }
    }

    async fn fail(
        &self,
        owner: &Arc<OwnedStartAttempt>,
        record: &PreviewInstanceRecord,
        error: PreviewCoordinationError,
    ) -> Completion {
        let detail = error.to_string();
        loop {
            if owner.cancelled() {
                return self.cancelled(owner, record).await;
            }
            match self
                .store
                .mark_failed(
                    &record.preview_key,
                    &record.instance_id,
                    record.revision,
                    &detail,
                )
                .await
            {
                Ok(_) => {
                    return Completion::error(error);
                }
                Err(PreviewStoreError::Unavailable(message)) => {
                    tracing::debug!(operation_id = %record.operation_id, "preview failure terminal awaiting reconciliation: {message}")
                }
                Err(PreviewStoreError::Conflict(_) | PreviewStoreError::Invalid(_)) => {}
            }
            match self.store.get(&record.preview_key).await {
                Ok(Some(row))
                    if row.instance_id == record.instance_id
                        && row.operation_id == record.operation_id
                        && row.revision == record.revision
                        && row.state == PreviewInstanceState::Failed =>
                {
                    return Completion::error(error);
                }
                Ok(Some(row))
                    if row.instance_id != record.instance_id
                        || row.operation_id != record.operation_id
                        || row.revision != record.revision =>
                {
                    return Completion::error(conflict(
                        "startup terminal was superseded by another operation",
                    ));
                }
                Ok(Some(row)) if row.state == PreviewInstanceState::Unknown => {
                    self.settle_no_process(record, &detail).await;
                    return Completion::error(error);
                }
                Ok(Some(row)) if row.state == PreviewInstanceState::Stopped => {
                    return Completion::error(error);
                }
                // This branch already owns a definitive no-execution result.
                // A retired authority row is not a historical startup gate.
                Ok(None) => return Completion::error(error),
                Ok(Some(_))
                | Err(
                    PreviewStoreError::Unavailable(_)
                    | PreviewStoreError::Conflict(_)
                    | PreviewStoreError::Invalid(_),
                ) => {}
            }
            tokio::time::sleep(RECONCILE_DELAY).await;
        }
    }

    async fn cancelled(
        &self,
        owner: &Arc<OwnedStartAttempt>,
        record: &PreviewInstanceRecord,
    ) -> Completion {
        match self
            .executor
            .stop_local(&record.preview_key, &record.instance_id)
            .await
        {
            Ok(ExecutorStopOutcome::Stopped | ExecutorStopOutcome::NotRegistered) => {
                self.settle_no_process(
                    record,
                    "startup stop intent completed after original execution and exact cleanup",
                )
                .await;
                Completion::error(conflict("preview startup was stopped before completion"))
            }
            Ok(ExecutorStopOutcome::IdentityMismatch) => Completion::error(conflict(
                "startup registration was superseded; successor was preserved",
            )),
            Err(error) => {
                self.unknown(
                    owner,
                    record,
                    unavailable(format!("startup stop cleanup pending: {error}")),
                )
                .await
            }
        }
    }

    /// Persist the already observed absence, not a new stop/start side effect.
    /// Re-read after every lost reply, preserving a newer instance/operation.
    async fn settle_no_process(&self, record: &PreviewInstanceRecord, evidence: &str) {
        loop {
            let current = match self.store.get(&record.preview_key).await {
                Ok(Some(row)) if row.instance_id == record.instance_id => row,
                Ok(Some(_)) | Ok(None) => return,
                Err(error) => {
                    tracing::debug!("preview exact cleanup awaiting store observation: {error}");
                    tokio::time::sleep(RECONCILE_DELAY).await;
                    continue;
                }
            };
            let result = match current.state {
                PreviewInstanceState::Starting | PreviewInstanceState::Ready => {
                    if current.operation_id != record.operation_id
                        || current.revision < record.revision
                    {
                        return;
                    }
                    self.store
                        .mark_failed(
                            &record.preview_key,
                            &record.instance_id,
                            current.revision,
                            evidence,
                        )
                        .await
                }
                PreviewInstanceState::Stopping => {
                    self.store
                        .mark_stopped(
                            &current.preview_key,
                            &current.operation_id,
                            current.revision,
                        )
                        .await
                }
                PreviewInstanceState::Unknown => {
                    self.store
                        .resolve_unknown_stopped(
                            &current.preview_key,
                            &current.instance_id,
                            evidence,
                        )
                        .await
                }
                PreviewInstanceState::Stopped | PreviewInstanceState::Failed => return,
            };
            match result {
                Ok(_) => {
                    if let Some(port) = record.port {
                        self.route_cache.invalidate(port);
                    }
                    return;
                }
                Err(
                    PreviewStoreError::Unavailable(_)
                    | PreviewStoreError::Conflict(_)
                    | PreviewStoreError::Invalid(_),
                ) => tokio::time::sleep(RECONCILE_DELAY).await,
            }
        }
    }

    async fn unknown(
        &self,
        owner: &Arc<OwnedStartAttempt>,
        record: &PreviewInstanceRecord,
        error: PreviewCoordinationError,
    ) -> Completion {
        // Report the real unknown result to its waiter, but retain this owner
        // and the original execution identity for active cleanup reconciliation.
        // The original start future has finished; only exact idempotent Stop
        // and conditional evidence writes are retried, never native start.
        owner
            .result
            .send_replace(Some(StartupResult::Error(Arc::new(copy_error(&error)))));
        loop {
            match self
                .executor
                .stop_local(&record.preview_key, &record.instance_id)
                .await
            {
                Ok(ExecutorStopOutcome::Stopped | ExecutorStopOutcome::NotRegistered) => {
                    self.settle_no_process(record, &error.to_string()).await;
                    return Completion::error(error);
                }
                Ok(ExecutorStopOutcome::IdentityMismatch) => {
                    // A successor's registration never authorizes stopping it.
                    // Keep only an actually unresolved original instance fenced.
                    if let Ok(Some(current)) = self.store.get(&record.preview_key).await
                        && current.instance_id != record.instance_id
                    {
                        return Completion::error(error);
                    }
                }
                Err(cleanup) => {
                    tracing::debug!(operation_id = %record.operation_id, "original preview cleanup still unconfirmed: {cleanup}")
                }
            }
            tokio::time::sleep(RECONCILE_DELAY).await;
        }
    }
}
