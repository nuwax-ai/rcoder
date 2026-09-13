//! Durable compute-only builder controls; HTTP observers do not own execution.
use super::dev_cleanup::BuilderOperation;
use crate::app_state::AppState;
use anyhow::{Context as _, Result, anyhow};
use futures::FutureExt as _;
use sha2::{Digest as _, Sha256};
use shared_types::{
    BuilderControlError, BuilderControlResult, UserAppAdmission, UserAppAdmissionOutcome,
    UserAppControlCommand, UserAppControlRequest, UserAppExecutionContext, UserAppLifecycleState,
    UserAppOperationKind, UserAppOperationProgress, UserAppOperationRecord, UserAppOperationState,
    UserAppStoreError,
};

pub(crate) async fn execute(
    state: &AppState,
    app_id: &str,
    mut request: UserAppControlRequest,
    restart: bool,
) -> Result<BuilderControlResult> {
    shared_types::validate_identifier(app_id, "app_id").map_err(|error| anyhow!(error))?;
    shared_types::validate_identifier(&request.user_id, "user_id")
        .map_err(|error| anyhow!(error))?;
    request
        .request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
    let owned = state.clone();
    let app_id = app_id.to_owned();
    tokio::spawn(async move {
        let _local = super::lifecycle::acquire(&app_id).await;
        let app = owned
            .userapp_store
            .get_application(&app_id)
            .await?
            .ok_or(UserAppStoreError::NotFound)?;
        if app.user_id != request.user_id {
            return Err(UserAppStoreError::OwnershipConflict.into());
        }
        if app.state != UserAppLifecycleState::Active
            || request
                .lifecycle_id
                .as_ref()
                .is_some_and(|expected| expected != &app.lifecycle_id)
        {
            return Err(UserAppStoreError::LifecycleConflict.into());
        }
        let command = if restart {
            UserAppControlCommand::RestartBuilder
        } else {
            UserAppControlCommand::StopBuilder
        };
        let fingerprint = Sha256::digest(shared_types::encode_userapp_intent(&(
            command.clone(),
            &request,
        ))?)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
        let record = match owned
            .userapp_store
            .admit(&UserAppAdmission {
                runtime_policy_on_success: None,
                metadata: None,
                command: Some(command),
                kind: if restart {
                    UserAppOperationKind::RestartBuilder
                } else {
                    UserAppOperationKind::StopBuilder
                },
                app_id: app_id.clone(),
                user_id: request.user_id.clone(),
                lifecycle_id: request.lifecycle_id,
                request_id: request.request_id,
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_fingerprint: fingerprint,
            })
            .await?
        {
            UserAppAdmissionOutcome::Accepted(record) => record,
            UserAppAdmissionOutcome::Existing(record)
                if record.state == UserAppOperationState::Succeeded =>
            {
                return serde_json::from_value(record.checkpoint["result"].clone())
                    .context("Decode completed builder control");
            }
            UserAppAdmissionOutcome::Existing(record)
                if record.state == UserAppOperationState::Pending =>
            {
                record
            }
            UserAppAdmissionOutcome::Existing(record) => {
                return Err(UserAppStoreError::OperationInProgress(record.operation_id).into());
            }
        };
        execute_pending(&owned, record, &request.user_id, restart).await
    })
    .await
    .context("Builder control observer interrupted")?
}

async fn advance(
    state: &AppState,
    record: &mut UserAppOperationRecord,
    executor: &str,
    status: UserAppOperationState,
    step: &str,
    checkpoint: serde_json::Value,
    error: Option<String>,
) -> Result<()> {
    *record = state
        .userapp_store
        .advance(&UserAppOperationProgress {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            operation_id: record.operation_id.clone(),
            expected_revision: record.revision,
            executor_id: executor.into(),
            state: status,
            step: step.into(),
            checkpoint,
            error_code: error
                .as_ref()
                .map(|_| shared_types::error_codes::ERR_BACKEND_ERROR.into()),
            error_message: error,
        })
        .await?;
    Ok(())
}

async fn execute_pending(
    state: &AppState,
    mut record: UserAppOperationRecord,
    owner: &str,
    restart: bool,
) -> Result<BuilderControlResult> {
    let executor = uuid::Uuid::new_v4().to_string();
    advance(
        state,
        &mut record,
        &executor,
        UserAppOperationState::Running,
        "claimed",
        serde_json::Value::Null,
        None,
    )
    .await?;
    let context = UserAppExecutionContext {
        app_id: record.app_id.clone(),
        user_id: owner.into(),
        lifecycle_id: record.lifecycle_id.clone(),
        operation_id: record.operation_id.clone(),
        executor_id: executor.clone(),
        request_fingerprint: record.request_fingerprint.clone(),
    };
    let mut lease = None;
    let mut mutating = false;
    let operation = std::panic::AssertUnwindSafe(async {
        lease = Some(BuilderOperation::new(
            state
                .runtime()
                .acquire_builder_operation(&record.app_id)
                .await?,
        ));
        let receipt = lease
            .as_ref()
            .ok_or_else(|| anyhow!("Builder operation lease missing"))?
            .receipt()
            .map_err(|error| anyhow!(error))?;
        state
            .userapp_store
            .bind_operation_lease(&context, &receipt)
            .await?;
        let target = super::adoption::capture_bound_target(state, &context).await?;
        target.validate().map_err(|error| anyhow!(error))?;
        if target.context != context {
            return Err(anyhow!("Builder control target context mismatch"));
        }
        if restart && target.workload.is_none() {
            return Err(anyhow!("Builder does not exist; restart cannot create it"));
        }
        let target_json = serde_json::to_value(&target)?;
        advance(
            state,
            &mut record,
            &executor,
            UserAppOperationState::Running,
            "compute_captured",
            target_json.clone(),
            None,
        )
        .await?;
        let container = if target.workload.is_some() {
            lease
                .as_mut()
                .ok_or_else(|| anyhow!("Builder operation lease missing"))?
                .begin_external_mutation()
                .map_err(|error| anyhow!(error))?;
            mutating = true;
            state
                .runtime()
                .apply_builder_control(&target, restart)
                .await?
        } else {
            None
        };
        if restart && container.is_none() {
            return Err(anyhow!(
                "Builder restart did not return its physical container"
            ));
        }
        let container = if let Some(info) = container {
            let deadline = tokio::time::Instant::now()
                + std::time::Duration::from_secs(
                    state.config.userapp_storage.ensure_timeout_seconds,
                );
            let info = super::confirm_builder_ready(state, &record.app_id, info, deadline).await?;
            super::register_builder(state, &record.app_id, owner, &info)?;
            Some(info)
        } else {
            None
        };
        let result = BuilderControlResult {
            operation_id: record.operation_id.clone(),
            was_existing: target.workload.is_some(),
            container,
        };
        let checkpoint = serde_json::json!({"target": target_json, "result": result});
        advance(
            state,
            &mut record,
            &executor,
            UserAppOperationState::Running,
            "compute_confirmed",
            checkpoint.clone(),
            None,
        )
        .await?;
        // Registration is an observation, not authority. Do not clear an entire
        // project/session or overwrite a newer registry generation after stopping.
        crate::userapp_forward::invalidate_probe_cache(&record.app_id);
        let guard = lease
            .as_mut()
            .ok_or_else(|| anyhow!("Builder operation lease missing"))?;
        if mutating {
            guard.finish_external_mutation().await
        } else {
            guard.finish_read_only().await
        }
        .map_err(|error| anyhow!(error))?;
        advance(
            state,
            &mut record,
            &executor,
            UserAppOperationState::Succeeded,
            "completed",
            checkpoint,
            None,
        )
        .await?;
        forget_released_lease(
            state,
            &shared_types::UserAppOperationLeaseBinding {
                context: context.clone(),
                receipt,
            },
        )
        .await;
        Ok::<_, anyhow::Error>(result)
    })
    .catch_unwind()
    .await
    .unwrap_or_else(|_| {
        Err(anyhow!(
            "Builder control worker panicked; inspect its durable operation"
        ))
    });
    match operation {
        Ok(result) => Ok(result),
        Err(error) => {
            let rejected = error
                .downcast_ref::<container_runtime_api::ContainerRuntimeError>()
                .is_some_and(|error| {
                    matches!(
                        error,
                        container_runtime_api::ContainerRuntimeError::RequestRejected(_)
                    )
                });
            let mut release_failed = false;
            if mutating
                && rejected
                && let Some(guard) = lease.as_mut()
            {
                match guard.finish_external_mutation().await {
                    Ok(()) => mutating = false,
                    Err(release_error) => {
                        release_failed = true;
                        tracing::error!(operation_id = %record.operation_id, %release_error, "Rejected builder control lease release failed");
                    }
                }
            } else if !mutating
                && let Some(guard) = lease.as_mut()
                && let Err(release_error) = guard.finish_read_only().await
            {
                release_failed = true;
                tracing::error!(operation_id = %record.operation_id, %release_error, "Read-only builder control lease release failed");
            }
            let message = format!("{error:#}");
            let checkpoint = record.checkpoint.clone();
            let status = if mutating || release_failed {
                UserAppOperationState::RecoveryRequired
            } else {
                UserAppOperationState::Failed
            };
            // Keep the last durable evidence boundary for reconciliation. In
            // particular, a terminal SQL failure after compute confirmation must
            // not erase the fact that the runtime operation already completed.
            let failure_step = if status == UserAppOperationState::RecoveryRequired {
                record.step.clone()
            } else {
                "control_failed".to_owned()
            };
            if let Err(storage_error) = advance(
                state,
                &mut record,
                &executor,
                status,
                &failure_step,
                checkpoint,
                Some(message.clone()),
            )
            .await
            {
                tracing::error!(operation_id = %record.operation_id, %storage_error, "Record builder control failure failed");
            }
            Err(BuilderControlError {
                operation_id: record.operation_id,
                message,
            }
            .into())
        }
    }
}

async fn forget_released_lease(
    state: &AppState,
    binding: &shared_types::UserAppOperationLeaseBinding,
) {
    // Runtime release and terminal commit are already confirmed. A failure to
    // retire this cleanup record is retried by discovery, not by repeating I/O.
    if let Err(error) = state.userapp_store.forget_operation_lease(binding).await {
        tracing::warn!(%error, operation_id = %binding.context.operation_id,
            "Completed builder lease cleanup record remains pending");
    }
}

/// Recovery only finalizes a confirmed effect; it never starts/stops the
/// container again and never derives a lease owner from the application name.
pub(super) async fn reconcile_completed(
    state: &AppState,
    snapshot: &UserAppOperationRecord,
) -> Result<bool> {
    if !matches!(
        snapshot.kind,
        UserAppOperationKind::StopBuilder | UserAppOperationKind::RestartBuilder
    ) || !shared_types::userapp_operation_has_final_evidence(snapshot)
    {
        return Ok(false);
    }
    let Some(_local) = super::lifecycle::try_acquire(&snapshot.app_id).await else {
        return Ok(false);
    };
    let binding = state
        .userapp_store
        .get_operation_lease(&snapshot.app_id, &snapshot.operation_id)
        .await?
        .ok_or_else(|| anyhow!("Builder recovery lease receipt missing"))?;
    if binding.receipt.service_type() != &shared_types::ServiceType::UserappBuilder {
        return Err(anyhow!("Builder recovery lease family mismatch"));
    }
    let mut reserved = state
        .userapp_store
        .reserve_completed_operation(snapshot)
        .await?;
    let result = state
        .runtime()
        .release_app_operation_receipt(&binding.context, &binding.receipt)
        .await;
    let checkpoint = reserved.checkpoint.clone();
    let step = reserved.step.clone();
    let (status, message) = match &result {
        Ok(()) => (UserAppOperationState::Succeeded, None),
        Err(error) => (
            UserAppOperationState::RecoveryRequired,
            Some(error.to_string()),
        ),
    };
    advance(
        state,
        &mut reserved,
        &binding.context.executor_id,
        status,
        &step,
        checkpoint,
        message,
    )
    .await?;
    result?;
    forget_released_lease(state, &binding).await;
    crate::userapp_forward::invalidate_probe_cache(&snapshot.app_id);
    Ok(true)
}

pub(super) async fn resume_pending(
    state: &AppState,
    record: &UserAppOperationRecord,
) -> Result<bool> {
    let restart = match record.command.as_ref() {
        Some(UserAppControlCommand::StopBuilder) => false,
        Some(UserAppControlCommand::RestartBuilder) => true,
        _ => return Ok(false),
    };
    let Some(_local) = super::lifecycle::try_acquire(&record.app_id).await else {
        return Ok(false);
    };
    let current = state
        .userapp_store
        .get_operation(&record.app_id, &record.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if current.state != UserAppOperationState::Pending || current.revision != record.revision {
        return Ok(false);
    }
    let app = state
        .userapp_store
        .get_application(&record.app_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if app.lifecycle_id != record.lifecycle_id || app.state != UserAppLifecycleState::Active {
        return Err(UserAppStoreError::LifecycleConflict.into());
    }
    execute_pending(state, current, &app.user_id, restart).await?;
    Ok(true)
}
