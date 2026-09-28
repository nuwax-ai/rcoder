use super::*;

pub(crate) async fn current(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    operation_id: &str,
    lifecycle_id: &str,
) -> Result<(UserAppLifecycleRecord, UserAppOperationRecord), Error> {
    repo::claim_app(tx, backend, app_id).await?;
    let app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
    if app.lifecycle_id != lifecycle_id || !domain::operation_owns_any_slot(&app, operation_id) {
        return Err(Error::LifecycleConflict);
    }
    let operation = repo::operation(tx, app_id, operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if !domain::slot_matches_operation(&app, &operation) {
        return Err(Error::LifecycleConflict);
    }
    Ok((app, operation))
}
pub(crate) fn owns(context: &UserAppExecutionContext, op: &UserAppOperationRecord) -> bool {
    op.app_id == context.app_id
        && op.operation_id == context.operation_id
        && op.lifecycle_id == context.lifecycle_id
        && op.executor_id.as_deref() == Some(context.executor_id.as_str())
        && op.request_fingerprint == context.request_fingerprint
}
pub(crate) async fn read_execution_input(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
) -> Result<UserAppExecutionInput, Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    let (_, op) = current(
        tx,
        backend,
        &context.app_id,
        &context.operation_id,
        &context.lifecycle_id,
    )
    .await?;
    if !owns(context, &op) || op.state != UserAppOperationState::Running {
        return Err(Error::VersionConflict);
    }
    crate::userapp_lifecycle::common::compute::check_business_authority(tx, &op).await?;
    let row = models::OperationInput::get_by_operation_id(tx, &context.operation_id)
        .await
        .map_err(storage)?;
    if row.app_id != context.app_id
        || row.lifecycle_id != context.lifecycle_id
        || row.payload_version != 1
    {
        return Err(Error::LifecycleConflict);
    }
    let input = UserAppExecutionInput::new(row.payload);
    if row.payload_digest != input.digest() {
        return Err(Error::InvalidOperation(
            "Stored execution input digest mismatch".into(),
        ));
    }
    validate_input(
        op.kind,
        &op.request_fingerprint,
        op.command.as_ref(),
        Some(&input),
    )?;
    Ok(input)
}
pub(crate) async fn bind_operation_deadline(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    operation_id: &str,
    lifecycle_id: &str,
    deadline_epoch_ms: i64,
) -> Result<i64, Error> {
    if deadline_epoch_ms <= 0 {
        return Err(Error::InvalidOperation(
            "Operation deadline must be positive".into(),
        ));
    }
    let (_, op) = current(tx, backend, app_id, operation_id, lifecycle_id).await?;
    if op.lifecycle_id != lifecycle_id || op.state.is_terminal() {
        return Err(Error::VersionConflict);
    }
    if let Some(row) = models::OperationDeadline::filter_by_operation_id(operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
    {
        if row.app_id != app_id || row.lifecycle_id != lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        return Ok(row.deadline_ms);
    }
    models::OperationDeadline::create()
        .operation_id(operation_id)
        .app_id(app_id)
        .lifecycle_id(lifecycle_id)
        .deadline_ms(deadline_epoch_ms)
        .exec(tx)
        .await
        .map_err(storage)?;
    Ok(deadline_epoch_ms)
}
pub(crate) async fn operation_deadline(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<i64>, Error> {
    Ok(
        models::OperationDeadline::filter_by_operation_id(operation_id)
            .first()
            .exec(tx)
            .await
            .map_err(storage)?
            .filter(|row| row.app_id == app_id)
            .map(|row| row.deadline_ms),
    )
}
pub(crate) async fn get_operation(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<UserAppOperationRecord>, Error> {
    repo::operation(tx, app_id, operation_id).await
}
pub(crate) async fn get_operation_by_request(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    request_id: &str,
) -> Result<Option<UserAppOperationRecord>, Error> {
    domain::validate_request_id(request_id)?;
    match repo::request(tx, app_id, request_id).await? {
        Some(mapping) => repo::request_operation(tx, app_id, &mapping).await,
        None => Ok(None),
    }
}
pub(crate) async fn unfinished_operations(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppOperationRecord>, Error> {
    if limit == 0 {
        return Err(Error::InvalidOperation(
            "scan limit must be positive".into(),
        ));
    }
    let rows = models::Operation::filter(
        models::Operation::fields()
            .operation_id()
            .gt(after.unwrap_or(""))
            .and(models::Operation::fields().terminal_at_us().is_none()),
    )
    .order_by(models::Operation::fields().operation_id().asc())
    .limit(usize::try_from(limit).map_err(storage)?)
    .exec(tx)
    .await
    .map_err(storage)?;
    let _ = backend;
    rows.into_iter()
        .map(codec::operation)
        .map(|result| result.map_err(storage))
        .collect()
}
pub(crate) async fn advance(
    tx: &mut dyn Executor,
    backend: Backend,
    progress: &UserAppOperationProgress,
) -> Result<UserAppOperationRecord, Error> {
    repo::claim_app(tx, backend, &progress.app_id).await?;
    let mut app = repo::app(tx, &progress.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    let before_app = app.clone();
    let mut operation = repo::operation(tx, &progress.app_id, &progress.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    let before = operation.clone();
    // A draining executor must still persist evidence about an already-sent
    // write. Block success, retries and new claims, not evidence checkpoints.
    if matches!(
        progress.state,
        UserAppOperationState::Succeeded | UserAppOperationState::WaitingRetry
    ) || (progress.state == UserAppOperationState::Running
        && matches!(
            operation.state,
            UserAppOperationState::Pending | UserAppOperationState::WaitingRetry
        ))
    {
        crate::userapp_lifecycle::common::compute::check_business_authority(tx, &operation).await?;
    }
    domain::advance(&mut app, &mut operation, progress)?;
    configuration::validate_terminal(tx, &operation, progress.state).await?;
    repo::save_operation(tx, backend, &operation, &before).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    if operation.state.is_terminal() {
        models::OperationInput::delete_by_operation_id(tx, &operation.operation_id)
            .await
            .map_err(storage)?;
    }
    Ok(operation)
}

fn lease_binding(row: models::OperationLease) -> Result<UserAppOperationLeaseBinding, Error> {
    if row.receipt_version != 1 {
        return Err(Error::InvalidOperation(
            "Unsupported lease receipt version".into(),
        ));
    }
    let context = UserAppExecutionContext {
        app_id: row.app_id,
        lifecycle_id: row.lifecycle_id,
        operation_id: row.operation_id,
        executor_id: row.executor_id,
        request_fingerprint: row.request_fingerprint,
    };
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    let receipt: UserAppOperationLeaseReceipt =
        serde_json::from_str(&row.receipt_json).map_err(storage)?;
    receipt.validate().map_err(Error::InvalidOperation)?;
    Ok(UserAppOperationLeaseBinding { context, receipt })
}
pub(crate) async fn get_operation_lease(
    tx: &mut dyn Executor,
    _: Backend,
    app_id: &str,
    operation_id: &str,
) -> Result<Option<UserAppOperationLeaseBinding>, Error> {
    models::OperationLease::filter_by_operation_id(operation_id)
        .first()
        .exec(tx)
        .await
        .map_err(storage)?
        .filter(|row| row.app_id == app_id)
        .map(lease_binding)
        .transpose()
}
pub(crate) async fn bind_operation_lease(
    tx: &mut dyn Executor,
    backend: Backend,
    context: &UserAppExecutionContext,
    receipt: &UserAppOperationLeaseReceipt,
) -> Result<(), Error> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::InvalidOperation)?;
    receipt.validate().map_err(Error::InvalidOperation)?;
    let (_, op) = current(
        tx,
        backend,
        &context.app_id,
        &context.operation_id,
        &context.lifecycle_id,
    )
    .await?;
    if !owns(context, &op)
        || op.state != UserAppOperationState::Running
        || (op.scope == UserAppOperationScope::Dev)
            != (*receipt.service_type() == ServiceType::UserappBuilder)
    {
        return Err(Error::VersionConflict);
    }
    crate::userapp_lifecycle::common::compute::check_business_authority(tx, &op).await?;
    let desired = UserAppOperationLeaseBinding {
        context: context.clone(),
        receipt: receipt.clone(),
    };
    if let Some(previous) =
        get_operation_lease(tx, backend, &context.app_id, &context.operation_id).await?
    {
        if previous != desired {
            return Err(Error::VersionConflict);
        }
        return Ok(());
    }
    models::OperationLease::create()
        .operation_id(&context.operation_id)
        .app_id(&context.app_id)
        .lifecycle_id(&context.lifecycle_id)
        .executor_id(&context.executor_id)
        .request_fingerprint(&context.request_fingerprint)
        .receipt_version(1)
        .receipt_json(serde_json::to_string(receipt).map_err(storage)?)
        .created_at_us(chrono::Utc::now().timestamp_micros())
        .exec(tx)
        .await
        .map_err(storage)?;
    Ok(())
}
pub(crate) async fn terminal_operation_leases(
    tx: &mut dyn Executor,
    backend: Backend,
    after: Option<&str>,
    limit: u32,
) -> Result<Vec<UserAppOperationLeaseBinding>, Error> {
    let ids = repo::strings(toasty::sql::query(repo::sql(backend, "SELECT l.operation_id FROM userapp_operation_leases l JOIN userapp_operations o ON o.operation_id=l.operation_id AND o.app_id=l.app_id AND o.lifecycle_id=l.lifecycle_id WHERE o.terminal_at_us IS NOT NULL AND l.operation_id>$1 ORDER BY l.operation_id LIMIT $2"))
        .bind(after.unwrap_or("")).bind(i64::from(limit.clamp(1,1000))).exec(tx).await.map_err(storage)?)?;
    let mut result = Vec::with_capacity(ids.len());
    for id in ids {
        let binding = lease_binding(
            models::OperationLease::get_by_operation_id(tx, &id)
                .await
                .map_err(storage)?,
        )?;
        let op = repo::operation(tx, &binding.context.app_id, &id)
            .await?
            .ok_or(Error::NotFound)?;
        if !op.state.is_terminal() || !owns(&binding.context, &op) {
            return Err(Error::VersionConflict);
        }
        result.push(binding);
    }
    Ok(result)
}
pub(crate) async fn forget_operation_lease(
    tx: &mut dyn Executor,
    backend: Backend,
    binding: &UserAppOperationLeaseBinding,
) -> Result<(), Error> {
    repo::claim_app(tx, backend, &binding.context.app_id).await?;
    let op = repo::operation(tx, &binding.context.app_id, &binding.context.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if !op.state.is_terminal() || !owns(&binding.context, &op) {
        return Err(Error::VersionConflict);
    }
    if let Some(stored) = get_operation_lease(
        tx,
        backend,
        &binding.context.app_id,
        &binding.context.operation_id,
    )
    .await?
    {
        if stored != *binding {
            return Err(Error::VersionConflict);
        }
        let count = toasty::sql::statement(repo::sql(backend, "DELETE FROM userapp_operation_leases WHERE operation_id=$1 AND app_id=$2 AND lifecycle_id=$3 AND executor_id=$4 AND request_fingerprint=$5 AND receipt_json=$6"))
            .bind(&binding.context.operation_id).bind(&binding.context.app_id).bind(&binding.context.lifecycle_id)
            .bind(&binding.context.executor_id).bind(&binding.context.request_fingerprint)
            .bind(serde_json::to_string(&binding.receipt).map_err(storage)?).exec(tx).await.map_err(storage)?;
        if count != 1 {
            return Err(Error::VersionConflict);
        }
    }
    Ok(())
}
