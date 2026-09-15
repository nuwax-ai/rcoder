//! Pure transitions shared by both SQL backends. No database or runtime I/O.
use shared_types::{
    UserAppAdmission, UserAppAdmissionOutcome, UserAppLifecycleRecord,
    UserAppLifecycleState as AppState, UserAppOperationKind, UserAppOperationProgress,
    UserAppOperationRecord, UserAppOperationState as OpState, UserAppStoreError as Error,
};

pub(super) fn identity(app_id: &str) -> Result<UserAppLifecycleRecord, Error> {
    shared_types::validate_identifier(app_id, "app_id").map_err(Error::InvalidOperation)?;
    Ok(UserAppLifecycleRecord {
        runtime_policy: shared_types::UserAppRuntimePolicy::default(),
        app_id: app_id.into(),
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

pub(super) fn validate_active(app: &UserAppLifecycleRecord) -> Result<(), Error> {
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
    if request.runtime_policy_on_success.is_some()
        && !matches!(
            request.kind,
            UserAppOperationKind::Create
                | UserAppOperationKind::Update
                | UserAppOperationKind::StartDeployment
                | UserAppOperationKind::RestartDeployment
        )
    {
        return Err(Error::InvalidOperation(
            "Configuration policy projection requires a create or update operation".into(),
        ));
    }
    if let Some(shared_types::UserAppControlCommand::SetRecyclePolicy { policy }) = &request.command
        && policy.recycle_enabled.is_none()
        && policy.idle_timeout_seconds.is_none()
        && policy.wake_on_traffic.is_none()
    {
        return Err(Error::InvalidOperation(
            "At least one runtime policy field is required".into(),
        ));
    }
    if request
        .command
        .as_ref()
        .is_some_and(|command| command.kind() != request.kind)
    {
        return Err(Error::InvalidOperation(
            "Control command does not match operation kind".into(),
        ));
    }
    shared_types::validate_identifier(&request.operation_id, "operation_id")
        .map_err(|_| Error::InvalidOperation("invalid operation identity".into()))?;
    if let Some(request_id) = &request.request_id {
        validate_request_id(request_id)?;
    }
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
            || duplicate.command != request.command
            || duplicate.runtime_policy_on_success != request.runtime_policy_on_success
            || duplicate.request_fingerprint != request.request_fingerprint
            || duplicate.admitted_metadata != request.metadata
        {
            return Err(Error::InvalidOperation(
                "request identity was reused with different parameters".into(),
            ));
        }
        return Ok(UserAppAdmissionOutcome::Existing(duplicate));
    }
    validate_active(app)?;
    if let Some(active) = active {
        if !active.state.is_terminal()
            && active.state != OpState::RecoveryRequired
            && active.kind == UserAppOperationKind::EnsureBuilder
            && request.kind == active.kind
            && active.lifecycle_id == app.lifecycle_id
            && active.request_fingerprint == request.request_fingerprint
            && active.command == request.command
            && active.runtime_policy_on_success == request.runtime_policy_on_success
            && active.admitted_metadata == request.metadata
        {
            return Ok(UserAppAdmissionOutcome::Existing(active));
        }
        return Err(Error::OperationInProgress(active.operation_id));
    }
    if let Some(patch) = &request.metadata {
        patch_metadata(app, patch)?;
    }
    let operation = UserAppOperationRecord {
        runtime_policy_on_success: request.runtime_policy_on_success.clone(),
        command: request.command.clone(),
        admitted_metadata: request.metadata.clone(),
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
    shared_types::validate_identifier(&progress.executor_id, "executor_id")
        .map_err(|_| Error::InvalidOperation("invalid executor identity".into()))?;
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
    validate_deletion_progress(app, operation, progress)?;
    validate_storage_destruction_progress(app, operation, progress)?;
    validate_storage_clear_progress(app, operation, progress)?;
    if progress.state.is_terminal() {
        let applied_policy = match &operation.command {
            Some(shared_types::UserAppControlCommand::SetRecyclePolicy { policy }) => {
                Some(policy.clone())
            }
            Some(
                shared_types::UserAppControlCommand::Start { .. }
                | shared_types::UserAppControlCommand::Restart,
            ) => Some(shared_types::UserAppRuntimePolicy {
                wake_on_traffic: Some(true),
                ..Default::default()
            }),
            Some(shared_types::UserAppControlCommand::Stop { wake_on_traffic }) => {
                Some(shared_types::UserAppRuntimePolicy {
                    wake_on_traffic: Some(*wake_on_traffic),
                    ..Default::default()
                })
            }
            Some(
                shared_types::UserAppControlCommand::DeleteResources { .. }
                | shared_types::UserAppControlCommand::DeleteApplication
                | shared_types::UserAppControlCommand::DestroyStorage { .. }
                | shared_types::UserAppControlCommand::ClearStorage { .. }
                | shared_types::UserAppControlCommand::StopBuilder
                | shared_types::UserAppControlCommand::RestartBuilder,
            ) => None,
            None
            | Some(
                shared_types::UserAppControlCommand::Create { .. }
                | shared_types::UserAppControlCommand::Update { .. }
                | shared_types::UserAppControlCommand::Deploy { .. },
            ) => operation.runtime_policy_on_success.clone(),
        };
        if progress.state == OpState::Succeeded
            && let Some(policy) = applied_policy
        {
            let applied = app.runtime_policy.merge(&policy);
            if applied != app.runtime_policy {
                app.metadata_revision = app
                    .metadata_revision
                    .checked_add(1)
                    .ok_or_else(|| Error::InvalidOperation("metadata revision exhausted".into()))?;
                app.runtime_policy = applied;
            }
        }
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

pub(super) fn validate_request_id(request_id: &str) -> Result<(), Error> {
    if request_id.trim().is_empty() || request_id.len() > 128 {
        return Err(Error::InvalidOperation(
            "request identity must contain between 1 and 128 bytes".into(),
        ));
    }
    Ok(())
}

pub(super) fn patch_metadata(
    app: &mut UserAppLifecycleRecord,
    patch: &shared_types::UserAppMetadataPatch,
) -> Result<(), Error> {
    validate_active(app)?;
    if app.app_id != patch.app_id || app.lifecycle_id != patch.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    if app.metadata_revision != patch.expected_revision {
        return Err(Error::VersionConflict);
    }
    let mut updated = app.clone();
    if let Some(name) = &patch.name {
        updated.name = name.clone();
    }
    if let Some(tenant) = &patch.tenant_id {
        updated.tenant_id = tenant.clone();
    }
    if let Some(space) = &patch.space_id {
        updated.space_id = space.clone();
    }
    if updated != *app {
        updated.metadata_revision = app
            .metadata_revision
            .checked_add(1)
            .ok_or_else(|| Error::InvalidOperation("metadata revision exhausted".into()))?;
        *app = updated;
    }
    Ok(())
}

/// SQL callers cannot bypass the coordinator's deletion evidence ordering.
fn validate_deletion_progress(
    app: &UserAppLifecycleRecord,
    operation: &UserAppOperationRecord,
    progress: &UserAppOperationProgress,
) -> Result<(), Error> {
    use shared_types::{UserAppDeletionCheckpoint as Checkpoint, UserAppDeletionStage as Stage};
    if !matches!(
        operation.kind,
        UserAppOperationKind::DeleteCompute
            | UserAppOperationKind::PurgeResources
            | UserAppOperationKind::DeleteApplication
    ) {
        return Ok(());
    }
    if matches!(progress.state, OpState::Failed | OpState::RecoveryRequired) {
        return if progress.checkpoint == operation.checkpoint {
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "Deletion failure must preserve its last recorded evidence".into(),
            ))
        };
    }
    if progress.state == OpState::Running
        && progress.checkpoint.is_null()
        && operation.checkpoint.is_null()
    {
        return Ok(());
    }
    let next: Checkpoint = serde_json::from_value(progress.checkpoint.clone()).map_err(|_| {
        Error::InvalidOperation("Deletion progress requires a complete typed checkpoint".into())
    })?;
    next.validate_operation(operation)
        .map_err(Error::InvalidOperation)?;
    if operation.checkpoint.is_null() {
        if progress.state != OpState::Running || next.stage != Stage::Captured {
            return Err(Error::InvalidOperation(
                "Deletion must first record its captured resources".into(),
            ));
        }
        return Ok(());
    }
    let previous: Checkpoint =
        serde_json::from_value(operation.checkpoint.clone()).map_err(|_| {
            Error::InvalidOperation(
                "Stored deletion evidence requires explicit reconciliation".into(),
            )
        })?;
    let mut same_evidence = next.clone();
    same_evidence.stage = previous.stage;
    if same_evidence != previous {
        return Err(Error::InvalidOperation(
            "Deletion resource evidence cannot change during execution".into(),
        ));
    }
    if progress.state == OpState::Succeeded {
        let required = if operation.kind == UserAppOperationKind::DeleteCompute {
            Stage::ComputeRemoved
        } else {
            Stage::DevelopmentRemoved
        };
        if next.stage != required || previous.stage != required {
            return Err(Error::InvalidOperation(
                "Deletion cannot succeed before its final confirmed stage".into(),
            ));
        }
    } else if progress.state != OpState::Running
        || !matches!(
            (previous.stage, next.stage),
            (Stage::Captured, Stage::ComputeRemoved)
                | (Stage::ComputeRemoved, Stage::ProductionStorageRemoved)
                | (Stage::ProductionStorageRemoved, Stage::DevelopmentRemoved)
        )
    {
        return Err(Error::InvalidOperation(
            "Deletion stages must advance in order".into(),
        ));
    }
    Ok(())
}

fn validate_storage_destruction_progress(
    app: &UserAppLifecycleRecord,
    operation: &UserAppOperationRecord,
    progress: &UserAppOperationProgress,
) -> Result<(), Error> {
    let production = match operation.kind {
        UserAppOperationKind::DestroyProdStorage => true,
        UserAppOperationKind::DestroyDevStorage => false,
        _ => return Ok(()),
    };
    if matches!(progress.state, OpState::Failed | OpState::RecoveryRequired) {
        return if progress.checkpoint == operation.checkpoint {
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "Storage failure must preserve captured identities".into(),
            ))
        };
    }
    if progress.state == OpState::Running
        && progress.checkpoint.is_null()
        && operation.checkpoint.is_null()
    {
        return Ok(());
    }
    let evidence: shared_types::UserAppStorageDestruction =
        serde_json::from_value(progress.checkpoint.clone()).map_err(|_| {
            Error::InvalidOperation("Storage destruction requires typed resource evidence".into())
        })?;
    evidence.validate().map_err(Error::InvalidOperation)?;
    let context = &evidence.context;
    if context.app_id != operation.app_id
        || context.lifecycle_id != operation.lifecycle_id
        || context.operation_id != operation.operation_id
        || Some(context.executor_id.as_str()) != operation.executor_id.as_deref()
        || context.request_fingerprint != operation.request_fingerprint
        || evidence.production.is_some() != production
    {
        return Err(Error::InvalidOperation(
            "Storage evidence does not match operation identity or scope".into(),
        ));
    }
    if operation.checkpoint.is_null() {
        return if progress.state == OpState::Running && progress.step == "storage_captured" {
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "Storage destruction must capture resources before execution".into(),
            ))
        };
    }
    if progress.checkpoint != operation.checkpoint {
        return Err(Error::InvalidOperation(
            "Storage destruction cannot replace captured resources".into(),
        ));
    }
    let ordered = match (operation.step.as_str(), progress.step.as_str()) {
        ("storage_captured", "production_storage_removed") => production,
        ("storage_captured", "development_storage_removed") => !production,
        ("production_storage_removed", "development_storage_removed") => production,
        _ => false,
    };
    if (progress.state == OpState::Running && ordered)
        || (progress.state == OpState::Succeeded && operation.step == "development_storage_removed")
    {
        Ok(())
    } else {
        Err(Error::InvalidOperation(
            "Storage destruction must confirm each stage before success".into(),
        ))
    }
}

fn validate_storage_clear_progress(
    app: &UserAppLifecycleRecord,
    operation: &UserAppOperationRecord,
    progress: &UserAppOperationProgress,
) -> Result<(), Error> {
    let production = match operation.kind {
        UserAppOperationKind::ClearProdStorage => true,
        UserAppOperationKind::ClearDevStorage => false,
        _ => return Ok(()),
    };
    if matches!(progress.state, OpState::Failed | OpState::RecoveryRequired) {
        return if progress.checkpoint == operation.checkpoint {
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "Storage clear failure must preserve its target evidence".into(),
            ))
        };
    }
    if progress.state == OpState::Running
        && progress.checkpoint.is_null()
        && operation.checkpoint.is_null()
    {
        return Ok(());
    }
    let evidence: shared_types::UserAppStorageClear =
        serde_json::from_value(progress.checkpoint.clone()).map_err(|_| {
            Error::InvalidOperation("Storage clear requires typed target evidence".into())
        })?;
    evidence.validate().map_err(Error::InvalidOperation)?;
    let context = &evidence.context;
    context
        .validate_identity(&operation.app_id)
        .map_err(Error::InvalidOperation)?;
    if context.lifecycle_id != operation.lifecycle_id
        || context.operation_id != operation.operation_id
        || context.request_fingerprint != operation.request_fingerprint
        || Some(context.executor_id.as_str()) != operation.executor_id.as_deref()
    {
        return Err(Error::InvalidOperation(
            "Storage clear evidence does not belong to the operation".into(),
        ));
    }
    let scope_matches = match &evidence.target {
        shared_types::UserAppStorageClearTarget::Production {
            snapshot,
            directories,
        } => {
            production
                && snapshot.app_id == app.app_id
                && directories
                    .iter()
                    .all(|directory| directory.path.is_absolute())
        }
        shared_types::UserAppStorageClearTarget::Development {
            receipt, base_url, ..
        } => !production && receipt.runtime.app_id == app.app_id && !base_url.is_empty(),
    };
    if !scope_matches {
        return Err(Error::InvalidOperation(
            "Storage clear target scope mismatch".into(),
        ));
    }
    if operation.checkpoint.is_null() {
        return if progress.state == OpState::Running && progress.step == "clear_target_captured" {
            Ok(())
        } else {
            Err(Error::InvalidOperation(
                "Storage clear must capture targets before execution".into(),
            ))
        };
    }
    if operation.checkpoint != progress.checkpoint {
        return Err(Error::InvalidOperation(
            "Storage clear cannot change captured targets".into(),
        ));
    }
    if (progress.state == OpState::Running
        && operation.step == "clear_target_captured"
        && progress.step == "storage_contents_cleared")
        || (progress.state == OpState::Succeeded && operation.step == "storage_contents_cleared")
    {
        Ok(())
    } else {
        Err(Error::InvalidOperation(
            "Storage clear cannot succeed before confirmed completion".into(),
        ))
    }
}
