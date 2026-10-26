//! Early handoff transactions: no runtime I/O and no age-based authorization.
use super::{compute, compute_execution, repo, storage};
use crate::db::schema::Backend;
use shared_types::*;
use toasty::Executor;

async fn current(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let identity = ComputeExecutorIdentity {
        app_id: snapshot.app_id.clone(),
        lifecycle_id: snapshot.lifecycle_id.clone(),
        scope: snapshot.scope,
        operation_id: snapshot.operation_id.clone(),
        generation: snapshot.generation,
        executor_id: snapshot
            .executor_id
            .clone()
            .ok_or(UserAppStoreError::VersionConflict)?,
    };
    let actual = compute::current(tx, &identity).await?;
    if actual != *snapshot || !actual.can_recover_compute_drain() {
        return Err(UserAppStoreError::VersionConflict);
    }
    Ok(actual)
}

pub(super) async fn reserve(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    let actual = current(tx, backend, snapshot).await?;
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='recovery_required',error_code='COMPUTE_RECOVERING',error_message='Continuing original compute operation after early execution handoff',revision=revision+1,updated_at_us=$3 WHERE operation_id=$1 AND revision=$2 AND state=$5 AND executor_id=$4"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(chrono::Utc::now().timestamp_micros()).bind(snapshot.executor_id.as_deref().ok_or(UserAppStoreError::VersionConflict)?).bind(if actual.state == ComputeControlState::Running { "running" } else { "recovery_required" }).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

pub(super) async fn capture(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    receipt: &UserAppOperationLeaseReceipt,
    attempt: &UserAppExecutionContext,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    attempt
        .validate_identity(&snapshot.app_id)
        .map_err(UserAppStoreError::InvalidOperation)?;
    let actual = current(tx, backend, snapshot).await?;
    if actual.state != ComputeControlState::RecoveryRequired {
        return Err(UserAppStoreError::VersionConflict);
    }
    receipt
        .validate()
        .map_err(UserAppStoreError::InvalidOperation)?;
    let token = match receipt {
        UserAppOperationLeaseReceipt::Docker { token, .. }
        | UserAppOperationLeaseReceipt::Kubernetes { token, .. } => token,
    };
    if receipt.service_type()
        != &compute_lease_family(snapshot.scope).map_err(UserAppStoreError::InvalidOperation)?
        || attempt.lifecycle_id != snapshot.lifecycle_id
        || attempt.operation_id != snapshot.operation_id
        || attempt.request_fingerprint != snapshot.request_fingerprint
        || attempt.executor_id != *token
    {
        return Err(UserAppStoreError::InvalidOperation(
            "Recovered compute lease belongs to another attempt".into(),
        ));
    }
    if let Some(previous) = &actual.lease {
        return if previous == receipt {
            Ok(actual)
        } else {
            Err(UserAppStoreError::VersionConflict)
        };
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET lease_json=$3,revision=revision+1,updated_at_us=$4 WHERE operation_id=$1 AND revision=$2 AND state='recovery_required' AND lease_json IS NULL"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(serde_json::to_string(receipt).map_err(storage)?).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

pub(super) async fn resume(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    released: Option<&UserAppOperationLeaseReceipt>,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    let actual = current(tx, backend, snapshot).await?;
    if actual.state != ComputeControlState::RecoveryRequired || actual.lease.as_ref() != released {
        return Err(UserAppStoreError::VersionConflict);
    }
    compute_execution::drained(tx, &actual).await?;
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='pending',executor_id=NULL,lease_json=NULL,stage='accepted',error_code=NULL,error_message=NULL,revision=revision+1,updated_at_us=$3 WHERE operation_id=$1 AND revision=$2 AND state='recovery_required'"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

pub(super) async fn problem(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    code: &str,
    message: &str,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    let actual = current(tx, backend, snapshot).await?;
    if actual.state != ComputeControlState::RecoveryRequired {
        return Err(UserAppStoreError::VersionConflict);
    }
    if actual.error_code.as_deref() == Some(code)
        && actual.error_message.as_deref() == Some(message)
    {
        return Ok(actual);
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET error_code=$3,error_message=$4,revision=revision+1,updated_at_us=$5 WHERE operation_id=$1 AND revision=$2 AND state='recovery_required'"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(code).bind(message).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

/// 恢复器判定操作不可恢复（如 image-roll 的滚动目标缺席）：recovery_required →
/// failed 终态收口。与 problem() 同款 CAS（快照身份 + revision），不改
/// checkpoint（保留原始证据），写 terminal_at_us。只接受 recovery_required
/// 来源；Running 记录的终态走 compute_execution::advance 的执行器路径。
pub(super) async fn fail_unrecoverable(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    code: &str,
    message: &str,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    let actual = current(tx, backend, snapshot).await?;
    if actual.state.is_terminal() {
        return Err(UserAppStoreError::VersionConflict);
    }
    let now = chrono::Utc::now().timestamp_micros();
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='failed',error_code=$3,error_message=$4,revision=revision+1,updated_at_us=$5,terminal_at_us=$5 WHERE operation_id=$1 AND revision=$2 AND state='recovery_required'"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(code).bind(message).bind(now).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

/// Bind the compute control's total-operation deadline into the shared
/// operation deadline table (keyed by operation id). Idempotent: an existing
/// row is returned unchanged, so retried claims and the recovery scan can
/// never extend an operation's budget. Terminal or moved-on records refuse.
pub(super) async fn bind_deadline(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    deadline_epoch_ms: i64,
) -> Result<i64, UserAppStoreError> {
    use crate::db::models;
    if deadline_epoch_ms <= 0 {
        return Err(UserAppStoreError::InvalidOperation(
            "Compute control deadline must be positive".into(),
        ));
    }
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    if let Some(row) =
        models::ComputeControlDeadline::filter_by_operation_id(&snapshot.operation_id)
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
    {
        if row.app_id != snapshot.app_id || row.lifecycle_id != snapshot.lifecycle_id {
            return Err(UserAppStoreError::LifecycleConflict);
        }
        return Ok(row.deadline_ms);
    }
    let actual = compute::get(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if actual.lifecycle_id != snapshot.lifecycle_id
        || actual.generation != snapshot.generation
        || actual.scope != snapshot.scope
        || actual.state.is_terminal()
    {
        return Err(UserAppStoreError::VersionConflict);
    }
    if let Err(error) = models::ComputeControlDeadline::create()
        .operation_id(&snapshot.operation_id)
        .app_id(&snapshot.app_id)
        .lifecycle_id(&snapshot.lifecycle_id)
        .deadline_ms(deadline_epoch_ms)
        .exec(tx)
        .await
    {
        // A concurrent bind (another replica's scan) can win the operation-id
        // key between the read above and this insert; its row is equivalent
        // exactly when it names the same control.
        if let Some(row) =
            models::ComputeControlDeadline::filter_by_operation_id(&snapshot.operation_id)
                .first()
                .exec(tx)
                .await
                .map_err(storage)?
            && row.app_id == snapshot.app_id
            && row.lifecycle_id == snapshot.lifecycle_id
        {
            return Ok(row.deadline_ms);
        }
        return Err(storage(error));
    }
    Ok(deadline_epoch_ms)
}

/// Deadline observation for the compute control table: keyed by the control's
/// operation id and filtered by app.
pub(super) async fn read_deadline(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<i64>, UserAppStoreError> {
    use crate::db::models;
    Ok(
        models::ComputeControlDeadline::filter_by_operation_id(operation_id)
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
            .filter(|row| row.app_id == app_id)
            .map(|row| row.deadline_ms),
    )
}

/// Deadline settlement: the bound total deadline expired while the control was
/// still non-terminal. Full record equality guards the settler against a stale
/// page, the deadline row is re-verified inside this transaction (a missing or
/// unexpired row never settles), and the terminal CAS keeps the last durable
/// stage, checkpoint, lease and executor so the failure stays diagnosable and
/// the terminal lease-cleanup scan still owns the physical receipt.
///
/// The quiet-window anchor is re-verified against the settler's expected value
/// in the SAME transaction: a concurrent clear (the lease renewed again) or
/// re-anchor (a later death) instantly revokes every older observer, so a
/// stale settler can never terminalize a control whose lease came back to
/// life after its observation.
///
/// Deliberately NOT compute::current: settlement is the last-resort unblock,
/// so it must not inherit continuation preconditions (the intent head still
/// pointing at this control, an active application, a present executor). A
/// record whose head drifted or whose app went inactive is exactly the
/// permanently-fenced shape the deadline exists to settle.
pub(super) async fn expire(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    code: &str,
    message: &str,
    expected_unheld_observed_ms: i64,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    use crate::db::models;
    repo::claim_app(tx, backend, &snapshot.app_id).await?;
    let actual = compute::get(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if actual != *snapshot || actual.state.is_terminal() {
        return Err(UserAppStoreError::VersionConflict);
    }
    let row = models::ComputeControlDeadline::filter_by_operation_id(&snapshot.operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
        .filter(|row| row.app_id == snapshot.app_id);
    if !row
        .as_ref()
        .is_some_and(|row| row.deadline_ms <= chrono::Utc::now().timestamp_millis())
    {
        return Err(UserAppStoreError::InvalidOperation(
            "Compute control deadline has not expired".into(),
        ));
    }
    // Hold the anchor row's lock from the re-verification until this
    // transaction commits: a concurrent clear (the lease renewed again) or
    // re-anchor either commits BEFORE this check — matching zero rows and
    // refusing the settler — or waits on the row lock until after the
    // terminal CAS, where clearing is a harmless post-terminal cleanup. The
    // value-preserving self-update keeps the anchor evidence intact.
    let verified = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_control_deadlines SET unheld_observed_ms=unheld_observed_ms WHERE operation_id=$1 AND unheld_observed_ms=$2"))
        .bind(&snapshot.operation_id).bind(expected_unheld_observed_ms).exec(tx).await.map_err(storage)?;
    if verified != 1 {
        return Err(UserAppStoreError::VersionConflict);
    }
    #[cfg(all(test, feature = "pg"))]
    {
        super::transaction_fault_tests::after_expire_anchor_lock().await;
    }
    let now = chrono::Utc::now().timestamp_micros();
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_controls SET state='failed',error_code=$3,error_message=$4,revision=revision+1,updated_at_us=$5,terminal_at_us=$5 WHERE operation_id=$1 AND revision=$2 AND state IN ('running','recovery_required')"))
        .bind(&snapshot.operation_id).bind(snapshot.revision).bind(code).bind(message).bind(now).exec(tx).await.map_err(storage)?;
    updated(tx, snapshot, changed).await
}

async fn updated(
    tx: &mut dyn Executor,
    snapshot: &ComputeControlRecord,
    changed: u64,
) -> Result<ComputeControlRecord, UserAppStoreError> {
    if changed != 1 {
        return Err(UserAppStoreError::VersionConflict);
    }
    compute::get(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)
}

/// Anchor (idempotently) the settlement quiet window at the first durable
/// observation that the physical lease stopped being renewed. An existing
/// anchor is returned unchanged: the window always measures from the FIRST
/// unheld observation, never slides with later ones. Terminal or moved-on
/// records refuse, exactly like the binding.
pub(super) async fn mark_unheld(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
    observed_ms: i64,
) -> Result<i64, UserAppStoreError> {
    use crate::db::models;
    // No claim_app: this runs every scan round while the quiet window elapses
    // and must not churn the application row. The anchor row's own
    // set-if-NULL CAS is the serialization point; the control-side staleness
    // guard is the settle CAS's record equality.
    let Some(row) = models::ComputeControlDeadline::filter_by_operation_id(&snapshot.operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
    else {
        return Err(UserAppStoreError::InvalidOperation(
            "Unheld observation requires a bound compute control deadline".into(),
        ));
    };
    if row.app_id != snapshot.app_id || row.lifecycle_id != snapshot.lifecycle_id {
        return Err(UserAppStoreError::LifecycleConflict);
    }
    if let Some(anchor) = row.unheld_observed_ms {
        return Ok(anchor);
    }
    let actual = compute::get(tx, &snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if actual != *snapshot || actual.state.is_terminal() {
        return Err(UserAppStoreError::VersionConflict);
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_control_deadlines SET unheld_observed_ms=$2 WHERE operation_id=$1 AND unheld_observed_ms IS NULL"))
        .bind(&snapshot.operation_id).bind(observed_ms).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        // A concurrent scan anchored first; its earlier anchor is the one the
        // quiet window must keep measuring from.
        return Ok(
            models::ComputeControlDeadline::filter_by_operation_id(&snapshot.operation_id)
                .first()
                .exec(tx)
                .await
                .map_err(storage)?
                .ok_or(UserAppStoreError::NotFound)?
                .unheld_observed_ms
                .ok_or(UserAppStoreError::VersionConflict)?,
        );
    }
    Ok(observed_ms)
}

/// Drop the quiet-window anchor: the lease is held (renewed) again, so a later
/// unheld transition must re-anchor from its own first observation. Idempotent.
pub(super) async fn clear_unheld(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &ComputeControlRecord,
) -> Result<(), UserAppStoreError> {
    // No claim_app (same per-round rationale as mark_unheld); only touch the
    // row when an anchor exists: a still-renewed record hits this every scan
    // round and must not churn the table.
    let changed = toasty::sql::statement(repo::sql(backend,
        "UPDATE userapp_compute_control_deadlines SET unheld_observed_ms=NULL WHERE operation_id=$1 AND unheld_observed_ms IS NOT NULL"))
        .bind(&snapshot.operation_id).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Ok(());
    }
    Ok(())
}
