use super::*;

pub(crate) async fn finalize_password_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &DatabasePasswordEvidence,
) -> Result<UserAppOperationRecord, Error> {
    use DatabasePasswordStage as Stage;
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot
        || !matches!(
            op.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
    {
        return Err(Error::VersionConflict);
    }
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    let before: DatabasePasswordEvidence =
        serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
    before
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if before.receipt_protocol != Some(1)
        || evidence.receipt_protocol != Some(1)
        || before.context != evidence.context
        || before.target != evidence.target
        || before.username != evidence.username
        || !matches!(
            before.stage,
            Stage::WriteSubmitted | Stage::Verified | Stage::Cancelled
        )
        || !matches!(evidence.stage, Stage::Verified | Stage::Cancelled)
        || (before.stage != Stage::WriteSubmitted && before.stage != evidence.stage)
    {
        return Err(Error::InvalidOperation(
            "Password recovery evidence changed identity or outcome".into(),
        ));
    }
    let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if lease.context != evidence.context || !owns(&lease.context, &op) {
        return Err(Error::VersionConflict);
    }
    // The same root CAS used by admission/deletion protects this entire short
    // transaction. No remote command occurs within it.
    let before_app = app.clone();
    op.state = UserAppOperationState::Running;
    let mut progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: evidence.context.executor_id.clone(),
        state: UserAppOperationState::Running,
        step: op.step.clone(),
        checkpoint: serde_json::to_value(evidence).map_err(storage)?,
        error_code: None,
        error_message: None,
    };
    domain::advance(&mut app, &mut op, &progress)?;
    progress.expected_revision = op.revision;
    progress.state = if evidence.stage == Stage::Verified {
        UserAppOperationState::Succeeded
    } else {
        UserAppOperationState::Failed
    };
    domain::advance(&mut app, &mut op, &progress)?;
    configuration::validate_terminal(tx, &op, progress.state).await?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    Ok(op)
}

pub(crate) async fn finalize_deploy_pg_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &ExplicitDeploymentPasswordEvidence,
) -> Result<UserAppOperationRecord, Error> {
    use DatabasePasswordStage as Stage;
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot || op.state != UserAppOperationState::RecoveryRequired {
        return Err(Error::VersionConflict);
    }
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    let before: ExplicitDeploymentPasswordEvidence =
        serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
    before
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if before.receipt_protocol != Some(1)
        || evidence.receipt_protocol != Some(1)
        || before.context != evidence.context
        || before.explicit_pg_target != evidence.explicit_pg_target
        || before.username != evidence.username
        || !matches!(
            before.stage,
            Stage::WriteSubmitted | Stage::Verified | Stage::Cancelled
        )
        || !matches!(evidence.stage, Stage::Verified | Stage::Cancelled)
        || (before.stage != Stage::WriteSubmitted && before.stage != evidence.stage)
    {
        return Err(Error::InvalidOperation(
            "Deployment password recovery evidence changed identity or outcome".into(),
        ));
    }
    let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if lease.context != evidence.context || !owns(&lease.context, &op) {
        return Err(Error::VersionConflict);
    }
    // The deployment never durably recorded completion, so both outcomes are
    // terminal Failed; the receipt result stays queryable in the checkpoint.
    // The same root CAS used by admission/deletion protects this transaction.
    let before_app = app.clone();
    let failure_message = if evidence.stage == Stage::Verified {
        "Explicit deployment password committed and verified; deployment completion was not recorded; re-issue the deployment"
    } else {
        "Explicit deployment password write was cancelled; deployment did not complete"
    };
    op.state = UserAppOperationState::Running;
    let mut progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: evidence.context.executor_id.clone(),
        state: UserAppOperationState::Running,
        step: op.step.clone(),
        checkpoint: serde_json::to_value(evidence).map_err(storage)?,
        error_code: None,
        error_message: None,
    };
    domain::advance(&mut app, &mut op, &progress)?;
    progress.expected_revision = op.revision;
    progress.state = UserAppOperationState::Failed;
    progress.step = "deploy_pg_reconciled".into();
    progress.error_code = Some("ERR_BACKEND_ERROR".into());
    progress.error_message = Some(failure_message.into());
    domain::advance(&mut app, &mut op, &progress)?;
    configuration::validate_terminal(tx, &op, progress.state).await?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    Ok(op)
}

pub(crate) async fn confirm_database_preparation_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &DatabasePreparationEvidence,
) -> Result<UserAppOperationRecord, Error> {
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot
        || !matches!(
            op.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
    {
        return Err(Error::VersionConflict);
    }
    domain::validate_active(&app)?;
    let before: DatabasePreparationEvidence =
        serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
    before
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if before.stage != DatabasePreparationStage::StartSubmitted
        || evidence.stage != DatabasePreparationStage::ManagementReady
        || before.target != evidence.target
        || before.deployment_generation != evidence.deployment_generation
    {
        return Err(Error::InvalidOperation(
            "Management recovery evidence changed identity or stage".into(),
        ));
    }
    let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if lease.context != evidence.target.context || !owns(&lease.context, &op) {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    op.state = UserAppOperationState::Running;
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: evidence.target.context.executor_id.clone(),
        state: UserAppOperationState::Running,
        step: "database_management".into(),
        checkpoint: serde_json::to_value(evidence).map_err(storage)?,
        error_code: None,
        error_message: None,
    };
    domain::advance(&mut app, &mut op, &progress)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    Ok(op)
}

pub(crate) async fn finalize_builder_creation_rejection(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    rejection: Option<&RuntimeRequestRejection>,
) -> Result<UserAppOperationRecord, Error> {
    let (mut app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    if op != *snapshot
        || op.kind != UserAppOperationKind::EnsureBuilder
        || if rejection.is_some() {
            op.state != UserAppOperationState::RecoveryRequired
                || op.step != "creation_confirmation_timed_out"
        } else {
            !userapp_builder_creation_needs_runtime_receipt(&op)
        }
        || !op.checkpoint.is_null()
        || rejection.is_some_and(|rejection| {
            RuntimeRequestRejection::from_status(rejection.status, String::new()).is_none()
        })
    {
        return Err(Error::VersionConflict);
    }
    let before_app = app.clone();
    // Internal, transactional transition only; this never grants runtime work.
    op.state = UserAppOperationState::Running;
    let progress = UserAppOperationProgress {
        app_id: op.app_id.clone(),
        lifecycle_id: op.lifecycle_id.clone(),
        operation_id: op.operation_id.clone(),
        expected_revision: op.revision,
        executor_id: op.executor_id.clone().ok_or(Error::VersionConflict)?,
        state: UserAppOperationState::Failed,
        step: "creation_result".into(),
        checkpoint: if rejection.is_none() {
            serde_json::json!({"creation_cancelled": true})
        } else {
            serde_json::Value::Null
        },
        error_code: Some("ERR_BACKEND_ERROR".into()),
        error_message: Some(rejection.map_or_else(
            || "Builder creation cancelled after acknowledged writes".into(),
            ToString::to_string,
        )),
    };
    domain::advance(&mut app, &mut op, &progress)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    repo::save_app(
        tx,
        backend,
        &app,
        &before_app.lifecycle_id,
        before_app.metadata_revision,
    )
    .await?;
    repo::save_slots(tx, backend, &app, &before_app).await?;
    models::OperationInput::delete_by_operation_id(tx, &op.operation_id)
        .await
        .map_err(storage)?;
    Ok(op)
}

pub(crate) async fn confirm_builder_creation_recovery(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
    evidence: &BuilderCreationEvidence,
    management_ready: bool,
) -> Result<UserAppOperationRecord, Error> {
    let (app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    domain::validate_active(&app)?;
    if op != *snapshot {
        return Err(Error::VersionConflict);
    }
    evidence
        .validate_operation(&op)
        .map_err(Error::InvalidOperation)?;
    if management_ready {
        if op.state != UserAppOperationState::RecoveryRequired
            || op.step != "builder_created_observed"
        {
            return Err(Error::VersionConflict);
        }
        let before: BuilderCreationEvidence =
            serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
        before
            .validate_operation(&op)
            .map_err(Error::InvalidOperation)?;
        if before.target != evidence.target {
            return Err(Error::VersionConflict);
        }
    } else if !userapp_builder_creation_needs_runtime_receipt(&op) {
        return Err(Error::VersionConflict);
    }
    // Evidence is permitted while Stop drains this executor. It grants no
    // business authority; reserve_completed_operation checks that separately.
    op.revision = op
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    op.state = UserAppOperationState::RecoveryRequired;
    op.step = if management_ready {
        "builder_ready_confirmed"
    } else {
        "builder_created_observed"
    }
    .into();
    op.checkpoint = serde_json::to_value(evidence).map_err(storage)?;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    Ok(op)
}

pub(crate) async fn reserve_completed_operation(
    tx: &mut dyn Executor,
    backend: Backend,
    snapshot: &UserAppOperationRecord,
) -> Result<UserAppOperationRecord, Error> {
    let (app, mut op) = current(
        tx,
        backend,
        &snapshot.app_id,
        &snapshot.operation_id,
        &snapshot.lifecycle_id,
    )
    .await?;
    crate::userapp_lifecycle::common::compute::check_business_authority(tx, &op).await?;
    if op != *snapshot
        || !matches!(
            op.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
        || !userapp_operation_has_final_evidence(&op)
    {
        return Err(Error::VersionConflict);
    }
    if op.kind == UserAppOperationKind::EnsureBuilder {
        let _evidence: BuilderCreationEvidence =
            serde_json::from_value(op.checkpoint.clone()).map_err(storage)?;
        domain::validate_active(&app)?;
    } else {
        let lease = get_operation_lease(tx, backend, &op.app_id, &op.operation_id)
            .await?
            .ok_or(Error::NotFound)?;
        if !owns(&lease.context, &op) {
            return Err(Error::VersionConflict);
        }
    }
    op.revision = op
        .revision
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("Operation revision exhausted".into()))?;
    op.state = UserAppOperationState::Running;
    repo::save_operation(tx, backend, &op, snapshot).await?;
    Ok(op)
}
pub(crate) async fn get_resource_binding(
    tx: &mut dyn Executor,
    _: Backend,
    service_type: &ServiceType,
    physical_uid: &str,
) -> Result<Option<UserAppResourceBinding>, Error> {
    let row = models::ResourceBinding::filter_by_service_type_and_physical_uid(
        service_type.to_string(),
        physical_uid,
    )
    .first()
    .exec(tx)
    .await
    .map_err(storage)?;
    row.map(|row| {
        if row.service_type != ServiceType::UserappBuilder.to_string() {
            return Err(Error::InvalidOperation(
                "Unsupported physical binding family".into(),
            ));
        }
        Ok(UserAppResourceBinding {
            app_id: row.app_id,
            lifecycle_id: row.lifecycle_id,
            service_type: ServiceType::UserappBuilder,
            physical_uid: row.physical_uid,
            adopted_by_operation: row.adopted_by_operation,
        })
    })
    .transpose()
}
pub(crate) async fn commit_resource_binding(
    tx: &mut dyn Executor,
    backend: Backend,
    binding: &UserAppResourceBinding,
    progress: &UserAppOperationProgress,
) -> Result<UserAppOperationRecord, Error> {
    if progress.app_id != binding.app_id
        || progress.lifecycle_id != binding.lifecycle_id
        || progress.state != UserAppOperationState::Succeeded
        || binding.adopted_by_operation != progress.operation_id
    {
        return Err(Error::InvalidOperation(
            "Binding requires its successful adoption operation".into(),
        ));
    }
    repo::claim_app(tx, backend, &binding.app_id).await?;
    let app = repo::app(tx, &binding.app_id)
        .await?
        .ok_or(Error::NotFound)?;
    domain::validate_active(&app)?;
    if app.lifecycle_id != binding.lifecycle_id {
        return Err(Error::LifecycleConflict);
    }
    let op = repo::operation(tx, &binding.app_id, &progress.operation_id)
        .await?
        .ok_or(Error::NotFound)?;
    if op.kind != UserAppOperationKind::AdoptBuilder {
        return Err(Error::InvalidOperation(
            "Binding requires explicit adoption admission".into(),
        ));
    }
    let context = UserAppExecutionContext {
        app_id: app.app_id,
        lifecycle_id: app.lifecycle_id,
        operation_id: op.operation_id,
        executor_id: progress.executor_id.clone(),
        request_fingerprint: op.request_fingerprint,
    };
    binding
        .validate(&context, &binding.physical_uid)
        .map_err(Error::InvalidOperation)?;
    // Physical UID is global to the deployed runtime. A concurrent other app
    // may race this insertion even though each app holds its own root lock.
    toasty::sql::statement(repo::sql(backend, "INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,lifecycle_id,adopted_by_operation,created_at_us) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(service_type,physical_uid) DO NOTHING"))
        .bind(binding.service_type.to_string()).bind(&binding.physical_uid).bind(&binding.app_id).bind(&binding.lifecycle_id)
        .bind(&binding.adopted_by_operation).bind(chrono::Utc::now().timestamp_micros()).exec(tx).await.map_err(storage)?;
    let previous = get_resource_binding(tx, backend, &binding.service_type, &binding.physical_uid)
        .await?
        .ok_or(Error::NotFound)?;
    if previous != *binding {
        return Err(Error::LifecycleConflict);
    }
    advance(tx, backend, progress).await
}
pub(crate) async fn recreate(
    tx: &mut dyn Executor,
    backend: Backend,
    app_id: &str,
    expected_lifecycle_id: &str,
    request_id: &str,
) -> Result<UserAppLifecycleRecord, Error> {
    domain::validate_request_id(request_id)?;
    repo::claim_app(tx, backend, app_id).await?;
    let mut app = repo::app(tx, app_id).await?.ok_or(Error::NotFound)?;
    if let Some(previous) = repo::request(tx, app_id, request_id).await? {
        if previous.target_kind != "recreate" {
            return Err(Error::InvalidOperation(
                "request identity was already used for a control operation".into(),
            ));
        }
        if previous.previous_lifecycle_id.as_deref() != Some(expected_lifecycle_id)
            || previous.new_lifecycle_id.as_deref() != Some(app.lifecycle_id.as_str())
        {
            return Err(Error::LifecycleConflict);
        }
        return Ok(app);
    }
    if app.lifecycle_id != expected_lifecycle_id
        || app.state != UserAppLifecycleState::Deleted
        || !app.active_operations.is_empty()
    {
        return Err(Error::LifecycleConflict);
    }
    let old_revision = app.metadata_revision;
    app.lifecycle_id = uuid::Uuid::new_v4().to_string();
    app.lifecycle_epoch = app
        .lifecycle_epoch
        .checked_add(1)
        .ok_or_else(|| Error::InvalidOperation("lifecycle epoch exhausted".into()))?;
    app.metadata_revision = 1;
    app.state = UserAppLifecycleState::Active;
    app.created_at = chrono::Utc::now().trunc_subsecs(6);
    app.name = None;
    app.tenant_id = None;
    app.space_id = None;
    app.runtime_policy = UserAppRuntimePolicy::default();
    // Immediate FK ordering: retire only empty current slots/activity first.
    // Historical operations, configuration versions, leases and bindings remain.
    models::ActiveOperations::delete_by_app_id(tx, app_id)
        .await
        .map_err(storage)?;

    #[cfg(test)]
    crate::userapp_lifecycle::common::transaction_fault_tests::check("recreate_deleted_slots")?;
    toasty::sql::statement(repo::sql(
        backend,
        "DELETE FROM userapp_activity WHERE app_id=$1 AND lifecycle_id=$2",
    ))
    .bind(app_id)
    .bind(expected_lifecycle_id)
    .exec(tx)
    .await
    .map_err(storage)?;
    repo::save_app(tx, backend, &app, expected_lifecycle_id, old_revision).await?;

    #[cfg(test)]
    crate::userapp_lifecycle::common::transaction_fault_tests::check("recreate_application")?;
    models::ActiveOperations::create()
        .app_id(app_id)
        .lifecycle_id(&app.lifecycle_id)
        .exec(tx)
        .await
        .map_err(storage)?;

    #[cfg(test)]
    crate::userapp_lifecycle::common::transaction_fault_tests::check("recreate_slots")?;
    models::Request::create()
        .app_id(app_id)
        .request_id(request_id)
        .target_kind("recreate")
        .previous_lifecycle_id(Some(expected_lifecycle_id.to_owned()))
        .new_lifecycle_id(Some(app.lifecycle_id.clone()))
        .created_at_us(chrono::Utc::now().timestamp_micros())
        .exec(tx)
        .await
        .map_err(storage)?;

    #[cfg(test)]
    crate::userapp_lifecycle::common::transaction_fault_tests::check("recreate_request")?;
    Ok(app)
}

#[cfg(feature = "userapp-turso")]
pub(crate) async fn quarantine_local_restart(
    tx: &mut dyn Executor,
    backend: Backend,
) -> Result<(), Error> {
    let mut after = String::new();
    loop {
        let ids = repo::strings(toasty::sql::query(repo::sql(backend, "SELECT operation_id FROM userapp_operations WHERE state IN ('running','waiting_retry') AND operation_id>$1 ORDER BY operation_id LIMIT 256"))
            .bind(&after).exec(tx).await.map_err(storage)?)?;
        if ids.is_empty() {
            break;
        }
        for id in ids {
            let mut op = codec::operation(
                models::Operation::get_by_operation_id(tx, &id)
                    .await
                    .map_err(storage)?,
            )
            .map_err(storage)?;
            let before = op.clone();
            let app = repo::app(tx, &op.app_id).await?.ok_or(Error::NotFound)?;
            if app.lifecycle_id != op.lifecycle_id || !domain::slot_matches_operation(&app, &op) {
                return Err(Error::LifecycleConflict);
            }
            op.revision = op.revision.checked_add(1).ok_or_else(|| {
                Error::InvalidOperation("Operation revision exhausted during restart".into())
            })?;
            op.state = UserAppOperationState::RecoveryRequired;
            op.error_code = Some(ERR_BACKEND_ERROR.into());
            op.error_message = Some(
                "Previous local executor stopped; remote outcome requires verification".into(),
            );
            repo::save_operation(tx, backend, &op, &before).await?;
            after = id;
        }
    }
    Ok(())
}
