//! Priority admission only. Never claims that superseded runtime writes ended.
use super::{codec, repo};
use crate::{
    db::{models, schema::Backend},
    userapp_lifecycle::{domain, storage},
};
use shared_types::*;
use toasty::Executor;
type Error = UserAppStoreError;
fn invalid(message: &str) -> Error {
    Error::InvalidOperation(message.into())
}
fn action(value: ComputeControlAction) -> &'static str {
    match value {
        ComputeControlAction::Stop => "stop",
        ComputeControlAction::Restart => "restart",
    }
}
pub(super) fn decode(row: models::ComputeControl) -> Result<ComputeControlRecord, Error> {
    if row.generation < 1 || row.revision < 1 {
        return Err(invalid("Invalid compute control revision"));
    }
    let scope = codec::scope(&row.scope).map_err(storage)?;
    let lease: Option<UserAppOperationLeaseReceipt> = row
        .lease_json
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map_err(storage)?;
    if let Some(receipt) = &lease {
        receipt.validate().map_err(Error::InvalidOperation)?;
        let expected = match scope {
            UserAppOperationScope::Dev => ServiceType::UserappBuilder,
            UserAppOperationScope::Prod => ServiceType::Userapp,
            UserAppOperationScope::Application => {
                return Err(invalid("Invalid compute receipt scope"));
            }
        };
        if receipt.service_type() != &expected
            || row.executor_id.as_deref().is_none_or(str::is_empty)
        {
            return Err(invalid("Invalid compute receipt owner"));
        }
    }
    Ok(ComputeControlRecord {
        app_id: row.app_id,
        lifecycle_id: row.lifecycle_id,
        request_id: row.request_id,
        scope,
        operation_id: row.operation_id,
        request_fingerprint: row.request_fingerprint,
        generation: row.generation,
        revision: row.revision,
        action: match row.action.as_str() {
            "stop" => ComputeControlAction::Stop,
            "restart" => ComputeControlAction::Restart,
            _ => return Err(invalid("Invalid compute action")),
        },
        state: match row.state.as_str() {
            "pending" => ComputeControlState::Pending,
            "running" => ComputeControlState::Running,
            "recovery_required" => ComputeControlState::RecoveryRequired,
            "succeeded" => ComputeControlState::Succeeded,
            "failed" => ComputeControlState::Failed,
            "superseded" => ComputeControlState::Superseded,
            _ => return Err(invalid("Invalid compute state")),
        },
        executor_id: row.executor_id,
        stage: row.stage,
        checkpoint: serde_json::from_str(&row.checkpoint_json).map_err(storage)?,
        error_code: row.error_code,
        error_message: row.error_message,
        lease,
        interrupted_operations: serde_json::from_str(&row.evidence_json).map_err(storage)?,
    })
}
pub(super) async fn get(
    tx: &mut dyn Executor,
    app_id: &str,
    id: &str,
) -> Result<Option<ComputeControlRecord>, Error> {
    let row = models::ComputeControl::filter_by_operation_id(id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?;
    row.filter(|row| row.app_id == app_id)
        .map(decode)
        .transpose()
}
pub(super) async fn admit(
    tx: &mut dyn Executor,
    backend: Backend,
    request: &ComputeControlRequest,
    idle_only: bool,
) -> Result<ComputeControlRecord, Error> {
    for (value, name) in [
        (&request.app_id, "app_id"),
        (&request.lifecycle_id, "lifecycle_id"),
        (&request.operation_id, "operation_id"),
        (&request.request_id, "request_id"),
    ] {
        validate_identifier(value, name).map_err(Error::InvalidOperation)?;
    }
    if request.scope == UserAppOperationScope::Application {
        return Err(invalid("Compute control requires dev or prod scope"));
    }
    if request.request_fingerprint.len() != 64
        || !request
            .request_fingerprint
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
    {
        return Err(invalid("Invalid compute request fingerprint"));
    }
    repo::claim_app(tx, backend, &request.app_id).await?;
    let app = repo::app(tx, &request.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    domain::validate_active(&app)?;
    if app.lifecycle_id != request.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    let scope = codec::scope_name(request.scope);
    let fields = models::ComputeControl::fields();
    if let Some(replay) = models::ComputeControl::filter(
        fields
            .app_id()
            .eq(&request.app_id)
            .and(fields.scope().eq(scope))
            .and(fields.request_id().eq(&request.request_id)),
    )
    .first()
    .exec(tx)
    .await
    .map_err(storage)?
    {
        if replay.lifecycle_id != request.lifecycle_id
            || replay.request_fingerprint != request.request_fingerprint
            || replay.action != action(request.action)
        {
            return Err(invalid(
                "Compute request identity was reused with different input",
            ));
        }
        return decode(replay);
    }
    let active = repo::active(tx, &app).await?;
    if idle_only
        && (request.scope != UserAppOperationScope::Dev
            || request.action != ComputeControlAction::Restart
            || active.dev.is_some()
            || active.application.is_some())
    {
        return Err(Error::VersionConflict);
    }
    // An application-wide deletion cannot be undone by physical recovery.
    if let Some(operation) = active.application.as_ref()
        && operation.kind.ends_lifecycle()
    {
        return Err(Error::LifecycleConflict);
    }
    let head = models::ComputeIntent::filter_by_app_id_and_scope(&request.app_id, scope)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?;
    if idle_only
        && head
            .as_ref()
            .is_some_and(|head| head.desired_state == "stopped")
    {
        return Err(Error::VersionConflict);
    }
    let now = chrono::Utc::now().timestamp_micros();
    let mut interrupted = Vec::new();
    let mut supersedes = None;
    let (generation, revision) = if let Some(head) = &head {
        if head.lifecycle_id != request.lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        if let Some(id) = &head.control_operation_id {
            let old = get(tx, &request.app_id, id)
                .await?
                .ok_or_else(|| invalid("Compute intent target is missing"))?;
            interrupted.extend(old.interrupted_operations.clone());
            if !old.state.is_terminal() {
                if !((request.action == ComputeControlAction::Stop
                    && old.action == ComputeControlAction::Restart)
                    || (!idle_only
                        && request.action == ComputeControlAction::Restart
                        && old.request_id.starts_with("auto-repair-")))
                {
                    return Err(control_conflict(old));
                }
                interrupted.push(old.operation_id.clone());
                supersedes = Some(old.operation_id);
            }
        }
        (
            next_generation(tx, backend, &request.app_id, scope).await?,
            head.revision
                .checked_add(1)
                .ok_or_else(|| invalid("Compute revision exhausted"))?,
        )
    } else {
        (1, 1)
    };
    for scope in [request.scope, UserAppOperationScope::Application] {
        if let Some(id) = app.active_operations.slot(scope) {
            interrupted.push(id.to_owned());
        }
    }
    interrupted.sort();
    interrupted.dedup();
    if let Some(id) = supersedes {
        // Superseded means loss of authority, NOT confirmation of physical cleanup.
        toasty::sql::statement(repo::sql(backend,"UPDATE userapp_compute_controls SET state='superseded',revision=revision+1,updated_at_us=$2,terminal_at_us=$2 WHERE operation_id=$1 AND state IN ('pending','running','recovery_required')"))
            .bind(id).bind(now).exec(tx).await.map_err(storage)?;
    }
    toasty::sql::statement(repo::sql(backend,"INSERT INTO userapp_compute_controls (operation_id,app_id,lifecycle_id,scope,generation,revision,action,state,request_id,request_fingerprint,stage,checkpoint_json,evidence_json,created_at_us,updated_at_us) VALUES ($1,$2,$3,$4,$5,1,$6,'pending',$7,$8,'accepted',$9,$10,$11,$11)"))
        .bind(&request.operation_id).bind(&request.app_id).bind(&request.lifecycle_id).bind(scope).bind(generation)
        .bind(action(request.action)).bind(&request.request_id).bind(&request.request_fingerprint)
        .bind(serde_json::json!({"restart_image_roll":request.restart_image_roll}).to_string())
        .bind(serde_json::to_string(&interrupted).map_err(storage)?).bind(now).exec(tx).await.map_err(storage)?;
    let desired = if request.action == ComputeControlAction::Stop {
        "stopped"
    } else {
        "running"
    };
    if let Some(head) = head {
        let changed = toasty::sql::statement(repo::sql(backend,"UPDATE userapp_compute_intents SET generation=$4,revision=$5,desired_state=$6,control_operation_id=$7,updated_at_us=$8 WHERE app_id=$1 AND scope=$2 AND lifecycle_id=$3 AND revision=$9 AND generation=$10"))
            .bind(&request.app_id).bind(scope).bind(&request.lifecycle_id).bind(generation).bind(revision).bind(desired).bind(&request.operation_id).bind(now).bind(head.revision).bind(head.generation).exec(tx).await.map_err(storage)?;
        if changed != 1 {
            return Err(Error::VersionConflict);
        }
    } else {
        toasty::sql::statement(repo::sql(backend,"INSERT INTO userapp_compute_intents (app_id,scope,lifecycle_id,generation,revision,desired_state,control_operation_id,updated_at_us) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)"))
            .bind(&request.app_id).bind(scope).bind(&request.lifecycle_id).bind(generation).bind(revision).bind(desired).bind(&request.operation_id).bind(now).exec(tx).await.map_err(storage)?;
    }
    // 拍板 2026-09-23：手动 stop 与闲置回收统一——有请求即唤醒。stop 不再
    // 把持久 runtime_policy.wake_on_traffic 翻成 false（历史行中的 false 仅
    // 为存量展示值，不参与唤醒闸门）。
    get(tx, &request.app_id, &request.operation_id)
        .await?
        .ok_or(Error::NotFound)
}

pub(super) async fn current(
    tx: &mut dyn Executor,
    identity: &ComputeExecutorIdentity,
) -> Result<ComputeControlRecord, Error> {
    if identity.scope == UserAppOperationScope::Application || identity.generation < 1 {
        return Err(invalid("Invalid compute executor scope or generation"));
    }
    validate_identifier(&identity.executor_id, "executor_id").map_err(Error::InvalidOperation)?;
    let app = repo::app(tx, &identity.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    domain::validate_active(&app)?;
    if app.lifecycle_id != identity.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    let head = models::ComputeIntent::filter_by_app_id_and_scope(
        &identity.app_id,
        codec::scope_name(identity.scope),
    )
    .first()
    .exec(tx)
    .await
    .map_err(storage)?
    .ok_or(Error::NotFound)?;
    if head.lifecycle_id != identity.lifecycle_id
        || head.generation != identity.generation
        || head.control_operation_id.as_deref() != Some(identity.operation_id.as_str())
    {
        return Err(Error::VersionConflict);
    }
    let record = get(tx, &identity.app_id, &identity.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if record.lifecycle_id != identity.lifecycle_id
        || record.scope != identity.scope
        || record.generation != identity.generation
        || record.state.is_terminal()
    {
        return Err(Error::VersionConflict);
    }
    Ok(record)
}
pub(super) async fn check(
    tx: &mut dyn Executor,
    identity: &ComputeExecutorIdentity,
) -> Result<(), Error> {
    let record = current(tx, identity).await?;
    if record.state != ComputeControlState::Running
        || record.executor_id.as_deref() != Some(identity.executor_id.as_str())
    {
        return Err(Error::VersionConflict);
    }
    Ok(())
}
pub(super) async fn claim(
    tx: &mut dyn Executor,
    backend: Backend,
    identity: &ComputeExecutorIdentity,
    revision: i64,
) -> Result<ComputeControlRecord, Error> {
    repo::claim_app(tx, backend, &identity.app_id).await?;
    let record = current(tx, identity).await?;
    if record.revision != revision
        || record.state != ComputeControlState::Pending
        || record.executor_id.is_some()
    {
        return Err(Error::VersionConflict);
    }
    let next = revision
        .checked_add(1)
        .ok_or_else(|| invalid("Compute revision exhausted"))?;
    let changed = toasty::sql::statement(repo::sql(backend,"UPDATE userapp_compute_controls SET executor_id=$2,state='running',revision=$3,stage='draining_previous',updated_at_us=$4 WHERE operation_id=$1 AND revision=$5 AND executor_id IS NULL AND state='pending' AND lifecycle_id=$6 AND generation=$7"))
        .bind(&identity.operation_id).bind(&identity.executor_id).bind(next).bind(chrono::Utc::now().timestamp_micros()).bind(revision).bind(&identity.lifecycle_id).bind(identity.generation).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    cancel_unclaimed_business(tx, backend, &record).await?;
    get(tx, &identity.app_id, &identity.operation_id)
        .await?
        .ok_or(Error::NotFound)
}

/// Pending revision 1 has never granted execution authority. Retire only that
/// exact state while holding the same short application CAS transaction as the
/// compute claim. Running/WaitingRetry/uncertain operations require runtime
/// reconciliation and must not be inferred drained from elapsed time.
async fn cancel_unclaimed_business(
    tx: &mut dyn Executor,
    backend: Backend,
    control: &ComputeControlRecord,
) -> Result<(), Error> {
    let mut app = repo::app(tx, &control.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    let before_app = app.clone();
    for id in &control.interrupted_operations {
        let Some(mut old) = repo::operation(tx, &control.app_id, id).await? else {
            continue;
        };
        if old.lifecycle_id != control.lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        if old.state != UserAppOperationState::Pending {
            continue;
        }
        if old.revision != 1
            || old.executor_id.is_some()
            || old.kind.ends_lifecycle()
            || !domain::slot_matches_operation(&app, &old)
            || models::OperationLease::filter_by_operation_id(id)
                .first()
                .exec(tx)
                .await
                .map_err(storage)?
                .is_some()
        {
            return Err(invalid(
                "Unclaimed operation has inconsistent execution evidence",
            ));
        }
        super::configuration::validate_terminal(tx, &old, UserAppOperationState::Failed).await?;
        let before = old.clone();
        old.state = UserAppOperationState::Failed;
        old.revision += 1;
        old.step = "cancelled_before_execution".into();
        old.error_code = Some("ERR_OPERATION_CANCELLED".into());
        old.error_message =
            Some("Superseded by an explicit compute control before execution".into());
        old.checkpoint = serde_json::json!({
            "compute_operation_id": control.operation_id,
            "no_execution_claim": true,
        });
        repo::save_operation(tx, backend, &old, &before).await?;
        app.active_operations.set(old.scope, None);
        models::OperationInput::delete_by_operation_id(tx, id)
            .await
            .map_err(storage)?;
    }
    // save_slots compares the captured operation id, never clears a replacement.
    repo::save_slots(tx, backend, &app, &before_app).await?;
    Ok(())
}

/// Business progress may report uncertainty after interruption, but must never
/// commit success or issue a fresh mutation under superseded authority.
pub(super) async fn check_business_authority(
    tx: &mut dyn Executor,
    operation: &UserAppOperationRecord,
) -> Result<(), Error> {
    let scopes: &[UserAppOperationScope] = match operation.scope {
        UserAppOperationScope::Application => {
            &[UserAppOperationScope::Dev, UserAppOperationScope::Prod]
        }
        UserAppOperationScope::Dev => &[UserAppOperationScope::Dev],
        UserAppOperationScope::Prod => &[UserAppOperationScope::Prod],
    };
    for scope in scopes {
        let head = models::ComputeIntent::filter_by_app_id_and_scope(
            &operation.app_id,
            codec::scope_name(*scope),
        )
        .first()
        .exec(tx)
        .await
        .map_err(storage)?;
        let Some(head) = head else {
            continue;
        };
        if head.lifecycle_id != operation.lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        if let Some(id) = head.control_operation_id {
            let control = get(tx, &operation.app_id, &id)
                .await?
                .ok_or_else(|| invalid("Compute intent target is missing"))?;
            if control
                .interrupted_operations
                .iter()
                .any(|id| id == &operation.operation_id)
            {
                return Err(Error::VersionConflict);
            }
        }
    }
    Ok(())
}

/// New business work must not slip into an empty business slot while a priority
/// control owns the scope. Called inside the same root-CAS transaction as admit.
pub(super) async fn guard_admission(
    tx: &mut dyn Executor,
    app: &UserAppLifecycleRecord,
    scope: UserAppOperationScope,
) -> Result<(), Error> {
    let scopes: &[UserAppOperationScope] = match scope {
        UserAppOperationScope::Application => {
            &[UserAppOperationScope::Dev, UserAppOperationScope::Prod]
        }
        UserAppOperationScope::Dev => &[UserAppOperationScope::Dev],
        UserAppOperationScope::Prod => &[UserAppOperationScope::Prod],
    };
    for scope in scopes {
        let head = models::ComputeIntent::filter_by_app_id_and_scope(
            &app.app_id,
            codec::scope_name(*scope),
        )
        .first()
        .exec(tx)
        .await
        .map_err(storage)?;
        let Some(head) = head else {
            continue;
        };
        if head.lifecycle_id != app.lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        if let Some(id) = head.control_operation_id {
            let control = get(tx, &app.app_id, &id)
                .await?
                .ok_or_else(|| invalid("Compute intent target is missing"))?;
            if !control.state.is_terminal() {
                return Err(control_conflict(control));
            }
        }
    }
    Ok(())
}

async fn next_generation(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    scope: &str,
) -> Result<i64, Error> {
    let rows = repo::strings(toasty::sql::query(repo::sql(backend,
        "SELECT CAST(COALESCE(MAX(generation),0) AS TEXT) FROM userapp_compute_controls WHERE app_id=$1 AND scope=$2"))
        .bind(app_id).bind(scope).exec(tx).await.map_err(storage)?)?;
    rows.first()
        .ok_or_else(|| invalid("Missing compute generation"))?
        .parse::<i64>()
        .map_err(storage)?
        .checked_add(1)
        .ok_or_else(|| invalid("Compute generation exhausted"))
}

fn control_conflict(control: ComputeControlRecord) -> Error {
    Error::OperationInProgress(UserAppOperationBlocker {
        operation_id: control.operation_id,
        scope: control.scope,
        kind: match (control.scope, control.action) {
            (UserAppOperationScope::Dev, ComputeControlAction::Stop) => {
                UserAppOperationKind::StopBuilder
            }
            (UserAppOperationScope::Dev, ComputeControlAction::Restart) => {
                UserAppOperationKind::RestartBuilder
            }
            (_, ComputeControlAction::Stop) => UserAppOperationKind::Stop,
            (_, ComputeControlAction::Restart) => UserAppOperationKind::Restart,
        },
        state: match control.state {
            ComputeControlState::Pending => UserAppOperationState::Pending,
            ComputeControlState::RecoveryRequired => UserAppOperationState::RecoveryRequired,
            _ => UserAppOperationState::Running,
        },
        step: control.stage,
    })
}

pub(super) async fn check_business_execution(
    tx: &mut dyn Executor,
    context: &UserAppExecutionContext,
) -> Result<(), Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    let app = repo::app(tx, &context.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    if app.lifecycle_id != context.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    let operation = repo::operation(tx, &context.app_id, &context.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if !super::ops::owns(context, &operation)
        || operation.state != UserAppOperationState::Running
        || !domain::slot_matches_operation(&app, &operation)
    {
        return Err(Error::VersionConflict);
    }
    check_business_authority(tx, &operation).await
}

/// A Docker stop may remove the container; the intent remains authoritative
/// for choosing the high-priority physical start path on explicit ensure.
pub(super) async fn desired_stopped(
    tx: &mut dyn Executor,
    app_id: &str,
    lifecycle_id: &str,
    scope: UserAppOperationScope,
) -> Result<bool, Error> {
    if scope == UserAppOperationScope::Application {
        return Err(invalid("Compute intent requires dev or prod scope"));
    }
    let head = models::ComputeIntent::filter_by_app_id_and_scope(app_id, codec::scope_name(scope))
        .first()
        .exec(tx)
        .await
        .map_err(storage)?;
    match head {
        Some(head) if head.lifecycle_id != lifecycle_id => Err(Error::LifecycleConflict),
        Some(head) => Ok(head.desired_state == "stopped"),
        None => Ok(false),
    }
}

/// The explicit-start flag is supplied by chat/workspace control entry points,
/// never by status, proxy, file listing or background discovery.
pub(super) async fn check_access(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    scope: UserAppOperationScope,
    explicit_start: bool,
) -> Result<(), Error> {
    let Some(head) =
        models::ComputeIntent::filter_by_app_id_and_scope(app_id, codec::scope_name(scope))
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
    else {
        return Ok(());
    };
    if explicit_start {
        repo::claim_app(tx, backend, app_id).await?;
        let app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
        domain::validate_active(&app)?;
        if app.lifecycle_id != head.lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
    }
    if let Some(id) = &head.control_operation_id {
        let control = get(tx, app_id, id)
            .await?
            .ok_or_else(|| invalid("Compute intent operation missing"))?;
        if !control.state.is_terminal() {
            return Err(control_conflict(control));
        }
    }
    if head.desired_state == "stopped" {
        if !explicit_start {
            return Err(invalid(
                "Compute is stopped; explicitly start or restart it before accessing this service",
            ));
        }
        let changed = toasty::sql::statement(repo::sql(backend,
            "UPDATE userapp_compute_intents SET desired_state='running',control_operation_id=NULL,revision=revision+1,updated_at_us=$4 WHERE app_id=$1 AND scope=$2 AND revision=$3"))
            .bind(app_id).bind(codec::scope_name(scope)).bind(head.revision).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
        if changed != 1 {
            return Err(Error::VersionConflict);
        }
    }
    Ok(())
}

/// Restore a missing root and stopped intent in one transaction. No operation
/// history or success is invented, and an existing lifecycle remains authoritative.
pub(super) async fn restore_identity(
    tx: &mut dyn Executor,
    backend: Backend,
    found: &UserAppDiscoveredIdentity,
) -> Result<UserAppLifecycleRecord, Error> {
    found.validate().map_err(Error::InvalidOperation)?;
    let existing = repo::app(tx, &found.app_id).await?;
    let (app, created) = if existing.is_some() {
        repo::claim_app(tx, backend, &found.app_id).await?;
        let app = repo::app(tx, &found.app_id).await?.ok_or(Error::NotFound)?;
        domain::validate_active(&app)?;
        if app.lifecycle_id == found.lifecycle_id {
            return Ok(app);
        }
        (
            restore_unprovisioned_root(tx, backend, app, found).await?,
            true,
        )
    } else {
        let mut proposed = domain::identity(&found.app_id)?;
        proposed.lifecycle_id = found.lifecycle_id.clone();
        repo::ensure_with_created(tx, backend, &proposed).await?
    };
    domain::validate_active(&app)?;
    if app.lifecycle_id != found.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    if !created {
        return Ok(app);
    }
    toasty::sql::statement(repo::sql(backend,
        "INSERT INTO userapp_recovery_witnesses (app_id,lifecycle_id,witness_json,created_at_us) VALUES ($1,$2,$3,$4)"))
        .bind(&found.app_id).bind(&found.lifecycle_id)
        .bind(serde_json::to_string(found).map_err(storage)?)
        .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    for (scope, stopped) in [
        (UserAppOperationScope::Dev, found.dev_stopped),
        (UserAppOperationScope::Prod, found.prod_stopped),
    ] {
        if stopped {
            toasty::sql::statement(repo::sql(backend,
                "INSERT INTO userapp_compute_intents (app_id,scope,lifecycle_id,generation,revision,desired_state,updated_at_us) VALUES ($1,$2,$3,1,1,'stopped',$4) ON CONFLICT(app_id,scope) DO NOTHING"))
                .bind(&found.app_id).bind(codec::scope_name(scope)).bind(&found.lifecycle_id)
                .bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
        }
    }
    Ok(app)
}

/// Rebind an unused root or a root whose only attempts were explicitly rejected
/// before runtime mutation. Retain failed operations and their idempotency rows;
/// never reinterpret them as successful creation of the recovered resources.
async fn restore_unprovisioned_root(
    tx: &mut dyn Executor,
    backend: Backend,
    mut app: UserAppLifecycleRecord,
    found: &UserAppDiscoveredIdentity,
) -> Result<UserAppLifecycleRecord, Error> {
    if app.lifecycle_epoch != 1
        || app.metadata_revision != 1
        || app.active_operations != UserAppActiveOperations::default()
    {
        return Err(Error::LifecycleConflict);
    }
    // All writers compete on claim_app before touching these rows. Check in one
    // statement under that token; no runtime I/O or long transaction is needed.
    let rows = toasty::sql::query(repo::sql(backend,
        "SELECT app_id FROM userapp_operations WHERE app_id=$1 AND (lifecycle_id<>$2 OR kind<>'ensure_builder' OR scope<>'dev' OR state<>'failed' OR step<>'creation_result' OR checkpoint_json<>'null') UNION ALL SELECT app_id FROM userapp_requests WHERE app_id=$1 AND (target_kind<>'control' OR lifecycle_id<>$2) UNION ALL SELECT app_id FROM userapp_operation_leases WHERE app_id=$1 UNION ALL SELECT app_id FROM userapp_resource_bindings WHERE app_id=$1 UNION ALL SELECT app_id FROM userapp_activity WHERE app_id=$1 UNION ALL SELECT app_id FROM userapp_runtime_config_versions WHERE app_id=$1 UNION ALL SELECT app_id FROM userapp_runtime_configs WHERE app_id=$1 UNION ALL SELECT app_id FROM userapp_compute_intents WHERE app_id=$1 UNION ALL SELECT app_id FROM userapp_compute_controls WHERE app_id=$1 UNION ALL SELECT app_id FROM userapp_recovery_witnesses WHERE app_id=$1 LIMIT 1"))
        .bind(&app.app_id).bind(&app.lifecycle_id).exec(tx).await.map_err(storage)?;
    if !rows.is_empty() {
        return Err(Error::LifecycleConflict);
    }
    let changed = toasty::sql::statement(repo::sql(backend,
        "DELETE FROM userapp_active_operations WHERE app_id=$1 AND lifecycle_id=$2 AND dev_operation_id IS NULL AND prod_operation_id IS NULL AND application_operation_id IS NULL"))
        .bind(&app.app_id).bind(&app.lifecycle_id).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    let old_lifecycle = app.lifecycle_id.clone();
    let old_revision = app.metadata_revision;
    app.lifecycle_id = found.lifecycle_id.clone();
    app.lifecycle_epoch += 1;
    app.metadata_revision += 1;
    if found.prod_stopped {
        app.runtime_policy.wake_on_traffic = Some(false);
    }
    repo::save_app(tx, backend, &app, &old_lifecycle, old_revision).await?;
    models::ActiveOperations::create()
        .app_id(&app.app_id)
        .lifecycle_id(&app.lifecycle_id)
        .exec(tx)
        .await
        .map_err(storage)?;
    Ok(app)
}
