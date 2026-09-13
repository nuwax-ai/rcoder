//! Pure transitions shared by both SQL backends. No database or runtime I/O.
use shared_types::{
    UserAppAdmission, UserAppAdmissionOutcome, UserAppLifecycleRecord,
    UserAppLifecycleState as AppState, UserAppOperationKind, UserAppOperationProgress,
    UserAppOperationRecord, UserAppOperationState as OpState, UserAppStoreError as Error,
};

pub(super) fn identity(app_id: &str, user_id: &str) -> Result<UserAppLifecycleRecord, Error> {
    shared_types::validate_identifier(app_id, "app_id").map_err(Error::InvalidOperation)?;
    shared_types::validate_identifier(user_id, "user_id").map_err(Error::InvalidOperation)?;
    Ok(UserAppLifecycleRecord {
        app_id: app_id.into(),
        user_id: user_id.into(),
        lifecycle_id: uuid::Uuid::new_v4().to_string(),
        lifecycle_epoch: 1,
        metadata_revision: 1,
        state: AppState::Active,
        name: None,
        tenant_id: None,
        space_id: None,
        created_at: chrono::Utc::now(),
        current_operation_id: None,
    })
}

pub(super) fn validate_owner(app: &UserAppLifecycleRecord, owner: &str) -> Result<(), Error> {
    if app.user_id != owner {
        return Err(Error::OwnershipConflict);
    }
    Ok(())
}

pub(super) fn validate_active(app: &UserAppLifecycleRecord, owner: &str) -> Result<(), Error> {
    validate_owner(app, owner)?;
    if app.state != AppState::Active {
        return Err(Error::LifecycleConflict);
    }
    Ok(())
}

pub(super) fn admission(
    app: &mut UserAppLifecycleRecord,
    request: &UserAppAdmission,
    duplicate: Option<UserAppOperationRecord>,
    active: Option<UserAppOperationRecord>,
) -> Result<UserAppAdmissionOutcome, Error> {
    validate_owner(app, &request.user_id)?;
    if request.operation_id.is_empty()
        || request.request_fingerprint.is_empty()
        || request.request_id.as_deref().is_some_and(str::is_empty)
    {
        return Err(Error::InvalidOperation(
            "operation identity and fingerprint must be nonempty".into(),
        ));
    }
    if request
        .lifecycle_id
        .as_ref()
        .is_some_and(|id| id != &app.lifecycle_id)
        || (app.lifecycle_epoch > 1 && request.lifecycle_id.is_none())
    {
        return Err(Error::LifecycleConflict);
    }
    if let Some(duplicate) = duplicate {
        if duplicate.lifecycle_id != app.lifecycle_id {
            return Err(Error::LifecycleConflict);
        }
        if duplicate.kind != request.kind
            || duplicate.request_fingerprint != request.request_fingerprint
        {
            return Err(Error::InvalidOperation(
                "request identity was reused with different parameters".into(),
            ));
        }
        return Ok(UserAppAdmissionOutcome::Existing(duplicate));
    }
    validate_active(app, &request.user_id)?;
    if let Some(active) = active {
        if !active.state.is_terminal()
            && active.state != OpState::RecoveryRequired
            && active.kind == UserAppOperationKind::EnsureBuilder
            && request.kind == active.kind
            && active.lifecycle_id == app.lifecycle_id
            && active.request_fingerprint == request.request_fingerprint
        {
            return Ok(UserAppAdmissionOutcome::Existing(active));
        }
        return Err(Error::OperationInProgress(active.operation_id));
    }
    let operation = UserAppOperationRecord {
        operation_id: request.operation_id.clone(),
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        request_id: request.request_id.clone(),
        request_fingerprint: request.request_fingerprint.clone(),
        kind: request.kind,
        state: OpState::Pending,
        revision: 1,
        executor_id: None,
        step: "admitted".into(),
        checkpoint: serde_json::Value::Null,
        error_code: None,
        error_message: None,
        created_at: chrono::Utc::now(),
    };
    app.current_operation_id = Some(operation.operation_id.clone());
    if operation.kind.ends_lifecycle() {
        app.state = AppState::Deleting;
    }
    Ok(UserAppAdmissionOutcome::Accepted(operation))
}

pub(super) fn advance(
    app: &mut UserAppLifecycleRecord,
    operation: &mut UserAppOperationRecord,
    progress: &UserAppOperationProgress,
) -> Result<(), Error> {
    if app.lifecycle_id != progress.lifecycle_id || operation.lifecycle_id != progress.lifecycle_id
    {
        return Err(Error::LifecycleConflict);
    }
    if app.current_operation_id.as_deref() != Some(&operation.operation_id)
        || operation.revision != progress.expected_revision
    {
        return Err(Error::VersionConflict);
    }
    if progress.executor_id.is_empty() {
        return Err(Error::InvalidOperation(
            "executor identity must be nonempty".into(),
        ));
    }
    match operation.state {
        OpState::Pending | OpState::WaitingRetry if progress.state == OpState::Running => {
            operation.executor_id = Some(progress.executor_id.clone());
        }
        OpState::Running if operation.executor_id.as_deref() == Some(&progress.executor_id) => {}
        _ => {
            return Err(Error::InvalidOperation(
                "operation requires an exclusive execution claim".into(),
            ));
        }
    }
    if operation.state.is_terminal()
        || progress.state == OpState::Pending
        || progress.step.is_empty()
    {
        return Err(Error::InvalidOperation(
            "invalid operation state transition".into(),
        ));
    }
    if progress.state == OpState::Failed
        && operation.kind.ends_lifecycle()
        && (operation.step != "claimed" || !operation.checkpoint.is_null())
    {
        return Err(Error::InvalidOperation(
            "incomplete deletion requires recovery, not terminal failure".into(),
        ));
    }
    if progress.state.is_terminal() {
        app.current_operation_id = None;
        if operation.kind.ends_lifecycle() {
            app.state = if progress.state == OpState::Succeeded {
                AppState::Deleted
            } else {
                AppState::Active
            };
        }
    }
    operation.revision = operation
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("operation revision exhausted".into()))?;
    operation.state = progress.state;
    operation.step = progress.step.clone();
    operation.checkpoint = progress.checkpoint.clone();
    operation.error_code = progress.error_code.clone();
    operation.error_message = progress.error_message.clone();
    Ok(())
}
