//! Row access only; transitions and admission remain in the shared domain module.
use super::codec;
use crate::{
    db::{models, schema::Backend},
    userapp_lifecycle::storage,
};
use shared_types::{
    UserAppActiveOperationRecords, UserAppLifecycleRecord, UserAppOperationRecord,
    UserAppOperationScope, UserAppStoreError as Error,
};
use toasty::Executor;
use toasty_core::schema::db::Type;

pub(super) fn sql(backend: Backend, source: &str) -> String {
    match backend {
        Backend::Postgres => source.to_owned(),
        Backend::Turso => source.replace('$', "?"),
    }
}
pub(super) fn strings(rows: Vec<toasty_core::stmt::Value>) -> Result<Vec<String>, Error> {
    use toasty_core::stmt::Value;
    rows.into_iter()
        .map(|row| match row {
            Value::Record(mut record) if record.fields.len() == 1 => {
                match record.fields.remove(0) {
                    Value::String(value) => Ok(value),
                    _ => Err(Error::InvalidOperation(
                        "Invalid identity column type".into(),
                    )),
                }
            }
            _ => Err(Error::InvalidOperation(
                "Invalid identity query shape".into(),
            )),
        })
        .collect()
}
/// Every lifecycle writer competes on the same root token before reading child
/// rows. The token is independent from metadata and never resets on recreation.
/// A losing transaction must roll back and re-read; an UPDATE is still subject
/// to the database lock timeout. No runtime I/O may occur in this transaction.
pub(super) async fn claim_app(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
) -> Result<(), Error> {
    use toasty_core::stmt::Value;
    let rows = toasty::sql::query(sql(
        backend,
        "SELECT lifecycle_id,lifecycle_state,control_revision FROM userapps WHERE app_id=$1",
    ))
    .bind(app_id)
    .exec(tx)
    .await
    .map_err(storage)?;
    let Some(Value::Record(row)) = rows.first() else {
        if rows.is_empty() {
            return Ok(());
        }
        return Err(Error::InvalidOperation("Invalid control token row".into()));
    };
    let [
        Value::String(lifecycle),
        Value::String(state),
        Value::I64(revision),
    ] = row.fields.as_slice()
    else {
        return Err(Error::InvalidOperation(
            "Invalid control token fields".into(),
        ));
    };
    let next = revision
        .checked_add(1)
        .filter(|_| *revision > 0)
        .ok_or_else(|| Error::InvalidOperation("Application control revision exhausted".into()))?;
    let changed = toasty::sql::statement(sql(backend,
        "UPDATE userapps SET control_revision=$2 WHERE app_id=$1 AND control_revision=$3 AND lifecycle_id=$4 AND lifecycle_state=$5"))
        .bind(app_id).bind(next).bind(*revision).bind(lifecycle).bind(state)
        .exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}
pub(super) async fn app(
    tx: &mut dyn Executor,
    app_id: &str,
) -> Result<Option<UserAppLifecycleRecord>, Error> {
    let Some(row) = models::Application::filter_by_app_id(app_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
    else {
        return Ok(None);
    };
    let slots = models::ActiveOperations::filter_by_app_id(app_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            Error::InvalidOperation("Application active-operation slots are missing".into())
        })?;
    codec::application(row, slots).map(Some).map_err(storage)
}
pub(super) async fn operation(
    tx: &mut dyn Executor,
    app_id: &str,
    id: &str,
) -> Result<Option<UserAppOperationRecord>, Error> {
    let row = models::Operation::filter_by_operation_id(id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?;
    row.filter(|row| row.app_id == app_id)
        .map(codec::operation)
        .transpose()
        .map_err(storage)
}
pub(super) async fn active(
    tx: &mut dyn Executor,
    app: &UserAppLifecycleRecord,
) -> Result<UserAppActiveOperationRecords, Error> {
    let mut records = UserAppActiveOperationRecords::default();
    for scope in UserAppOperationScope::ALL {
        let Some(id) = app.active_operations.slot(scope) else {
            continue;
        };
        let op = operation(tx, &app.app_id, id)
            .await?
            .ok_or_else(|| Error::InvalidOperation("Active operation record is missing".into()))?;
        if op.lifecycle_id != app.lifecycle_id || op.scope != scope || op.state.is_terminal() {
            return Err(Error::InvalidOperation(
                "Active operation identity/scope/state is inconsistent".into(),
            ));
        }
        records.set(scope, Some(op));
    }
    Ok(records)
}
pub(super) async fn request(
    tx: &mut dyn Executor,
    app_id: &str,
    request_id: &str,
) -> Result<Option<models::Request>, Error> {
    models::Request::filter_by_app_id_and_request_id(app_id, request_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)
}
pub(super) async fn request_operation(
    tx: &mut dyn Executor,
    app_id: &str,
    request: &models::Request,
) -> Result<Option<UserAppOperationRecord>, Error> {
    if request.target_kind == "recreate" {
        return Ok(None);
    }
    if request.target_kind != "control" || request.app_id != app_id {
        return Err(Error::InvalidOperation("Invalid request mapping".into()));
    }
    let id = request
        .operation_id
        .as_deref()
        .ok_or_else(|| Error::InvalidOperation("Control request has no operation".into()))?;
    let op = operation(tx, app_id, id)
        .await?
        .ok_or_else(|| Error::InvalidOperation("Request operation is missing".into()))?;
    if request.lifecycle_id.as_deref() != Some(op.lifecycle_id.as_str()) {
        return Err(Error::LifecycleConflict);
    }
    Ok(Some(op))
}
pub(super) async fn ensure(
    tx: &mut dyn Executor,
    backend: Backend,
    proposed: &UserAppLifecycleRecord,
) -> Result<UserAppLifecycleRecord, Error> {
    Ok(ensure_with_created(tx, backend, proposed).await?.0)
}

pub(super) async fn ensure_with_created(
    tx: &mut dyn Executor,
    backend: Backend,
    proposed: &UserAppLifecycleRecord,
) -> Result<(UserAppLifecycleRecord, bool), Error> {
    let row =
        codec::application_row(proposed, chrono::Utc::now().timestamp_micros()).map_err(storage)?;
    let inserted = toasty::sql::statement(sql(backend, "INSERT INTO userapps(app_id,lifecycle_id,lifecycle_epoch,lifecycle_state,metadata_revision,name,tenant_id,space_id,recycle_enabled,wake_on_traffic,idle_timeout_seconds,created_at_us,updated_at_us) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT(app_id) DO NOTHING"))
        .bind(&row.app_id).bind(&row.lifecycle_id).bind(row.lifecycle_epoch).bind(&row.lifecycle_state)
        .bind(row.metadata_revision).bind_typed(row.name, Type::Text).bind_typed(row.tenant_id, Type::Text).bind_typed(row.space_id, Type::Text)
        .bind_typed(row.recycle_enabled, Type::Boolean).bind_typed(row.wake_on_traffic, Type::Boolean).bind_typed(row.idle_timeout_seconds, Type::Integer(8))
        .bind(row.created_at_us).bind(row.updated_at_us).exec(tx).await.map_err(storage)?;
    claim_app(tx, backend, &row.app_id).await?;
    if inserted == 1 {
        models::ActiveOperations::create()
            .app_id(&row.app_id)
            .lifecycle_id(&row.lifecycle_id)
            .exec(tx)
            .await
            .map_err(storage)?;
    }
    Ok((
        app(tx, &row.app_id).await?.ok_or(Error::NotFound)?,
        inserted == 1,
    ))
}
pub(super) async fn save_app(
    tx: &mut dyn Executor,
    backend: Backend,
    record: &UserAppLifecycleRecord,
    old_lifecycle: &str,
    old_revision: i64,
) -> Result<(), Error> {
    let row =
        codec::application_row(record, chrono::Utc::now().timestamp_micros()).map_err(storage)?;
    let changed = toasty::sql::statement(sql(backend, "UPDATE userapps SET lifecycle_id=$2,lifecycle_epoch=$3,lifecycle_state=$4,metadata_revision=$5,name=$6,tenant_id=$7,space_id=$8,recycle_enabled=$9,wake_on_traffic=$10,idle_timeout_seconds=$11,created_at_us=$12,updated_at_us=$13 WHERE app_id=$1 AND lifecycle_id=$14 AND metadata_revision=$15"))
        .bind(&row.app_id).bind(row.lifecycle_id).bind(row.lifecycle_epoch).bind(row.lifecycle_state)
        .bind(row.metadata_revision).bind_typed(row.name, Type::Text).bind_typed(row.tenant_id, Type::Text).bind_typed(row.space_id, Type::Text)
        .bind_typed(row.recycle_enabled, Type::Boolean).bind_typed(row.wake_on_traffic, Type::Boolean).bind_typed(row.idle_timeout_seconds, Type::Integer(8))
        .bind(row.created_at_us).bind(row.updated_at_us).bind(old_lifecycle).bind(old_revision).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}
pub(super) async fn save_slots(
    tx: &mut dyn Executor,
    backend: Backend,
    app: &UserAppLifecycleRecord,
    previous: &UserAppLifecycleRecord,
) -> Result<(), Error> {
    let changed = toasty::sql::statement(sql(backend, "UPDATE userapp_active_operations SET dev_operation_id=$3,prod_operation_id=$4,application_operation_id=$5 WHERE app_id=$1 AND lifecycle_id=$2 AND (dev_operation_id=$6 OR (dev_operation_id IS NULL AND $6 IS NULL)) AND (prod_operation_id=$7 OR (prod_operation_id IS NULL AND $7 IS NULL)) AND (application_operation_id=$8 OR (application_operation_id IS NULL AND $8 IS NULL))"))
        .bind(&app.app_id).bind(&app.lifecycle_id).bind_typed(app.active_operations.dev.clone(), Type::Text)
        .bind_typed(app.active_operations.prod.clone(), Type::Text).bind_typed(app.active_operations.application.clone(), Type::Text)
        .bind_typed(previous.active_operations.dev.clone(), Type::Text)
        .bind_typed(previous.active_operations.prod.clone(), Type::Text)
        .bind_typed(previous.active_operations.application.clone(), Type::Text).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}
pub(super) async fn insert_operation(
    tx: &mut dyn Executor,
    record: &UserAppOperationRecord,
) -> Result<(), Error> {
    let row = codec::operation_row(record, chrono::Utc::now().timestamp_micros(), None)
        .map_err(storage)?;
    models::Operation::create()
        .operation_id(row.operation_id)
        .app_id(row.app_id)
        .lifecycle_id(row.lifecycle_id)
        .kind(row.kind)
        .scope(row.scope)
        .state(row.state)
        .revision(row.revision)
        .origin_request_id(row.origin_request_id)
        .request_fingerprint(row.request_fingerprint)
        .executor_id(row.executor_id)
        .step(row.step)
        .error_code(row.error_code)
        .error_message(row.error_message)
        .payload_version(row.payload_version)
        .command_json(row.command_json)
        .admitted_metadata_json(row.admitted_metadata_json)
        .runtime_policy_on_success_json(row.runtime_policy_on_success_json)
        .checkpoint_json(row.checkpoint_json)
        .created_at_us(row.created_at_us)
        .updated_at_us(row.updated_at_us)
        .terminal_at_us(row.terminal_at_us)
        .exec(tx)
        .await
        .map_err(storage)?;
    Ok(())
}
pub(super) async fn save_operation(
    tx: &mut dyn Executor,
    backend: Backend,
    record: &UserAppOperationRecord,
    previous: &UserAppOperationRecord,
) -> Result<(), Error> {
    // Immutable identity/command fields never participate in a progress update.
    let terminal = models::Operation::get_by_operation_id(tx, &record.operation_id)
        .await
        .map_err(storage)?
        .terminal_at_us;
    let row = codec::operation_row(record, chrono::Utc::now().timestamp_micros(), terminal)
        .map_err(storage)?;
    let changed = toasty::sql::statement(sql(backend, "UPDATE userapp_operations SET state=$4,revision=$5,executor_id=$6,step=$7,error_code=$8,error_message=$9,checkpoint_json=$10,updated_at_us=$11,terminal_at_us=$12 WHERE operation_id=$1 AND app_id=$2 AND lifecycle_id=$3 AND revision=$13 AND state=$14 AND (executor_id=$15 OR (executor_id IS NULL AND $15 IS NULL))"))
        .bind(&record.operation_id).bind(&record.app_id).bind(&record.lifecycle_id)
        .bind(row.state).bind(row.revision).bind_typed(row.executor_id, Type::Text).bind(row.step).bind_typed(row.error_code, Type::Text).bind_typed(row.error_message, Type::Text)
        .bind(row.checkpoint_json).bind(row.updated_at_us).bind_typed(row.terminal_at_us, Type::Integer(8))
        .bind(previous.revision).bind(codec::state_name(previous.state)).bind_typed(previous.executor_id.clone(), Type::Text).exec(tx).await.map_err(storage)?;
    if changed != 1 {
        return Err(Error::VersionConflict);
    }
    Ok(())
}
