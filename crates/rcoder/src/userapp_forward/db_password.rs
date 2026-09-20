//! Detached, durable password administration. No database transaction spans exec.
use crate::{AppError, router::AppState};
use sha2::{Digest as _, Sha256};
use shared_types::*;
use std::{sync::Arc, time::Duration};
use tokio::time::{Instant, timeout_at};

mod deploy_recovery;
mod recovery;
pub(crate) use deploy_recovery::recover_deploy_pg;
pub(super) use recovery::recover;
#[cfg(test)]
use recovery::validate_recovery_snapshot;

fn backend(message: &str) -> AppError {
    AppError::with_message(ERR_BACKEND_ERROR, message)
}
fn request_fingerprint(
    stage: UserappStage,
    body: &UserappDbResetPasswordRequest,
) -> Result<String, AppError> {
    Ok(Sha256::digest(
        serde_json::to_vec(&(stage.as_str(), body))
            .map_err(|_| backend("Encode database operation identity"))?,
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect())
}

fn replay_result(
    record: &UserAppOperationRecord,
    lifecycle: &str,
    stage: UserappStage,
    fingerprint: &str,
) -> Result<String, AppError> {
    let expected_kind = match stage {
        UserappStage::Dev => UserAppOperationKind::ResetDevDatabasePassword,
        UserappStage::Prod => UserAppOperationKind::ResetProdDatabasePassword,
    };
    if record.lifecycle_id != lifecycle
        || record.kind != expected_kind
        || record.request_fingerprint != fingerprint
    {
        return Err(AppError::conflict(
            "Database request identity was reused with different input or lifecycle",
        )
        .with_operation_id(record.operation_id.clone()));
    }
    if record.state == UserAppOperationState::Succeeded {
        if !userapp_operation_has_final_evidence(record) {
            return Err(
                backend("Completed database operation has invalid verification evidence")
                    .with_operation_id(record.operation_id.clone()),
            );
        }
        return Ok("密码已设置".into());
    }
    if record.state == UserAppOperationState::Failed {
        return Err(backend(
            "Original database operation failed before a confirmed password write",
        )
        .with_operation_id(record.operation_id.clone()));
    }
    Err(
        AppError::conflict("Database operation already exists; inspect its original result")
            .with_operation_id(record.operation_id.clone())
            .with_blocker(record.blocker()),
    )
}

fn store_error(error: UserAppStoreError) -> AppError {
    match error {
        UserAppStoreError::NotFound => AppError::not_found("Application was not found"),
        UserAppStoreError::LifecycleConflict | UserAppStoreError::OwnershipConflict => {
            AppError::conflict(
                "Application lifecycle changed; refresh its identity before changing the password",
            )
        }
        UserAppStoreError::VersionConflict => AppError::conflict(
            "Application operation revision changed; reload the original operation",
        ),
        UserAppStoreError::OperationInProgress(blocker) => {
            AppError::conflict("A conflicting application operation is in progress")
                .with_operation_id(blocker.operation_id.clone())
                .with_blocker(blocker)
        }
        UserAppStoreError::InvalidOperation(message) => AppError::bad_request(&message),
        // Driver diagnostics can include a failed row. Never return or log private
        // credential-table values; original request identity permits safe replay.
        UserAppStoreError::Storage(_) => AppError::with_message(
            ERR_BACKEND_ERROR,
            "Application operation storage is unavailable; retry using the original request identity",
        ),
    }
}

fn classify_lease_error(error: &container_runtime_api::ContainerRuntimeError) -> AppError {
    use container_runtime_api::ContainerRuntimeError as E;
    match error {
        E::Conflict(_) | E::OperationInProgress(_) => AppError::conflict(
            "Database runtime operation lease is occupied or has a resource conflict",
        ),
        E::ConfigurationError(_) => {
            backend("Database operation lease configuration is invalid or unsupported")
        }
        E::ConnectionError(_) => backend("Database operation lease connection failed"),
        E::Timeout(_) => backend(
            "Database operation lease acquisition timed out; its outcome requires verification",
        ),
        _ => backend("Database runtime operation lease is unavailable"),
    }
}

/// Cancellation of a read does not authorize cancelling a store transaction.
async fn read_before<T>(
    deadline: Instant,
    read: impl Future<Output = Result<T, UserAppStoreError>>,
) -> Result<T, AppError> {
    timeout_at(deadline, read)
        .await
        .map_err(|_| backend("Database operation observation deadline exceeded"))?
        .map_err(store_error)
}

async fn lease_error(
    state: &AppState,
    app_id: &str,
    lifecycle: &str,
    stage: UserappStage,
    error: container_runtime_api::ContainerRuntimeError,
) -> AppError {
    let mapped = classify_lease_error(&error);
    if !matches!(
        error,
        container_runtime_api::ContainerRuntimeError::Conflict(_)
            | container_runtime_api::ContainerRuntimeError::OperationInProgress(_)
    ) {
        return mapped;
    }
    let scope = match stage {
        UserappStage::Dev => UserAppOperationScope::Dev,
        UserappStage::Prod => UserAppOperationScope::Prod,
    };
    let Ok(Some(app)) = state.userapp_store.get_application(app_id).await else {
        return mapped;
    };
    if app.lifecycle_id != lifecycle {
        return mapped;
    }
    for (expected_scope, id) in [
        (
            UserAppOperationScope::Application,
            app.active_operations.application.as_deref(),
        ),
        (scope, app.active_operations.slot(scope).map(String::as_str)),
    ] {
        if let Some(id) = id
            && let Ok(Some(operation)) = state.userapp_store.get_operation(app_id, id).await
            && operation.app_id == app_id
            && operation.operation_id == id
            && operation.lifecycle_id == lifecycle
            && operation.scope == expected_scope
            && !operation.state.is_terminal()
        {
            return mapped
                .with_operation_id(operation.operation_id.clone())
                .with_blocker(operation.blocker());
        }
    }
    // A runtime lock may predate durable admission. Do not invent a blocker or
    // use a different environment's operation merely because app_id matches.
    mapped
}

fn management_available<T>(
    result: Result<T, container_runtime_api::ContainerRuntimeError>,
) -> Result<bool, AppError> {
    match result {
        Ok(_) => Ok(true),
        Err(container_runtime_api::ContainerRuntimeError::ManagementNotRunning) => Ok(false),
        Err(container_runtime_api::ContainerRuntimeError::Conflict(_)) => Err(AppError::conflict(
            "Database management target identity changed",
        )),
        Err(_) => Err(backend("Database management target observation failed")),
    }
}

async fn probe_prod_management(
    state: &AppState,
    context: &UserAppExecutionContext,
) -> Result<bool, AppError> {
    let spec = state
        .runtime()
        .get_app_container_spec(&context.app_id)
        .await
        .map_err(|_| backend("Read database container generation failed"))?;
    let generation = spec
        .env
        .as_ref()
        .and_then(|env| env.get(APP_DEPLOY_GENERATION_ID))
        .filter(|generation| !generation.is_empty())
        .ok_or_else(|| backend("Database container has no deployment generation"))?;
    management_available(
        state
            .runtime()
            .capture_app_configuration_target(context, generation)
            .await,
    )
}

struct Runner<'a> {
    state: &'a AppState,
    context: &'a UserAppExecutionContext,
    target: &'a DatabasePasswordTarget,
    deadline: Instant,
}
#[async_trait::async_trait]
impl PgCommandRunner for Runner<'_> {
    async fn run(&self, command: &str) -> Result<CommandOutcome, String> {
        let args = vec!["sh".into(), "-c".into(), command.into()];
        let result = timeout_at(self.deadline, async {
            match self.target {
                DatabasePasswordTarget::Dev(target) => {
                    self.state
                        .runtime()
                        .exec_builder_control_target(target, args)
                        .await
                }
                DatabasePasswordTarget::Prod(target) => {
                    self.state
                        .runtime()
                        .exec_app_configuration_target(self.context, target, args)
                        .await
                }
            }
        })
        .await
        .map_err(|_| "Database command deadline exceeded".to_string())?
        .map_err(|_| "Identity-bound database command failed".to_string())?;
        Ok(CommandOutcome {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }
}

async fn advance(
    state: &AppState,
    record: &mut UserAppOperationRecord,
    executor: &str,
    status: UserAppOperationState,
    checkpoint: serde_json::Value,
) -> Result<(), AppError> {
    *record = state
        .userapp_store
        .advance(&UserAppOperationProgress {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            operation_id: record.operation_id.clone(),
            expected_revision: record.revision,
            executor_id: executor.into(),
            state: status,
            step: "claimed".into(),
            checkpoint,
            error_code: (status == UserAppOperationState::RecoveryRequired)
                .then(|| ERR_BACKEND_ERROR.into()),
            error_message: (status == UserAppOperationState::RecoveryRequired)
                .then(|| "Database password outcome requires verification".into()),
        })
        .await
        .map_err(store_error)?;
    Ok(())
}

pub(super) async fn execute(
    state: Arc<AppState>,
    stage: UserappStage,
    mut body: UserappDbResetPasswordRequest,
) -> Result<String, AppError> {
    body.validate().map_err(|e| AppError::bad_request(&e))?;
    body.request_id
        .get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
    let flight = state
        .userapp_op_flight
        .guard()
        .map_err(|_| backend("Runtime is shutting down"))?;
    tokio::spawn(async move {
        let _flight = flight;
        coordinated(&state, stage, body).await
    }).await.map_err(|_| backend("Password operation coordinator interrupted; inspect original operation before retrying"))?
}

async fn coordinated(
    state: &AppState,
    stage: UserappStage,
    body: UserappDbResetPasswordRequest,
) -> Result<String, AppError> {
    let deadline = Instant::now() + Duration::from_secs(180);
    if let Some(expected) = &body.lifecycle_id {
        let current =
            read_before(deadline, state.userapp_store.get_application(&body.app_id)).await?;
        if current
            .as_ref()
            .is_none_or(|app| &app.lifecycle_id != expected)
        {
            return Err(AppError::conflict(
                "Application lifecycle changed before database preparation",
            ));
        }
    }
    let fingerprint = request_fingerprint(stage, &body)?;
    if let Some(request_id) = &body.request_id
        && let Some(existing) = read_before(
            deadline,
            state
                .userapp_store
                .get_operation_by_request(&body.app_id, request_id),
        )
        .await?
    {
        let current = read_before(deadline, state.userapp_store.get_application(&body.app_id))
            .await?
            .ok_or_else(|| {
                AppError::with_message(ERR_APP_NOT_FOUND, "Application identity not found")
            })?;
        // Replays resolve durable results before wake/capture/lease/SQL. In
        // particular, a retained lease cannot hide the original operation ID.
        return replay_result(&existing, &current.lifecycle_id, stage, &fingerprint);
    }
    // Dev retains its existing lazy builder creation. Prod first inspects the
    // physical management channel below, without consulting business readiness.
    if stage == UserappStage::Dev {
        timeout_at(
            deadline,
            crate::userapp_builder::ensure_userapp_builder_probed(state, &body.app_id),
        )
        .await
        .map_err(|_| backend("Database target preparation timed out"))?
        .map_err(|_| backend("Ensure database builder management channel failed"))?;
    }
    let app = read_before(deadline, state.userapp_store.get_application(&body.app_id))
        .await?
        .ok_or_else(|| {
            AppError::with_message(ERR_APP_NOT_FOUND, "Application identity not found")
        })?;
    if body
        .lifecycle_id
        .as_ref()
        .is_some_and(|id| id != &app.lifecycle_id)
    {
        return Err(AppError::conflict("Application lifecycle changed"));
    }
    let context = UserAppExecutionContext {
        app_id: body.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: uuid::Uuid::new_v4().to_string(),
        executor_id: uuid::Uuid::new_v4().to_string(),
        request_fingerprint: fingerprint,
    };
    if stage == UserappStage::Prod {
        let available = timeout_at(deadline, probe_prod_management(state, &context))
            .await
            .map_err(|_| backend("Database management observation timed out"))??;
        if !available {
            return Err(AppError::conflict(
                "Production database is not running; start the container explicitly before changing its password",
            ));
        }
    }

    let lease_result = timeout_at(deadline, async {
        match stage {
            UserappStage::Dev => state
                .runtime()
                .acquire_builder_operation(&body.app_id)
                .await
                .map(Some),
            UserappStage::Prod => state.runtime().acquire_app_operation(&body.app_id).await,
        }
    })
    .await
    .map_err(|_| backend("Database operation lease deadline exceeded"))?;
    let lease = match lease_result {
        Ok(lease) => {
            lease.ok_or_else(|| backend("Runtime has no durable database operation lease"))?
        }
        Err(error) => {
            let fallback = classify_lease_error(&error);
            return Err(timeout_at(
                deadline,
                lease_error(state, &body.app_id, &app.lifecycle_id, stage, error),
            )
            .await
            .unwrap_or(fallback));
        }
    };
    let mut record: Option<UserAppOperationRecord> = None;
    let mut uncertain = false;
    // Set before polling admission: a cancelled store observer can leave an
    // owned transaction committing. Its prospective operation ID remains ours.
    let mut admission_uncertain = false;
    let outcome = timeout_at(deadline, async {
        let target = match stage {
            UserappStage::Dev => DatabasePasswordTarget::Dev(Box::new(
                crate::userapp_builder::adoption::capture_bound_target(state, &context)
                    .await
                    .map_err(|_| backend("Capture database builder identity failed"))?,
            )),
            UserappStage::Prod => {
                let spec = state
                    .runtime()
                    .get_app_container_spec(&body.app_id)
                    .await
                    .map_err(|_| backend("Read database container generation failed"))?;
                let generation = spec
                    .env
                    .as_ref()
                    .and_then(|env| env.get(APP_DEPLOY_GENERATION_ID))
                    .ok_or_else(|| backend("Database container has no deployment generation"))?;
                DatabasePasswordTarget::Prod(
                    state
                        .runtime()
                        .capture_app_configuration_target(&context, generation)
                        .await
                        .map_err(|_| backend("Capture database container identity failed"))?,
                )
            }
        };
        let runner = Runner {
            state,
            context: &context,
            target: &target,
            deadline,
        };
        let marker = loop {
            let marker = runner
                .run("test -n \"${PGDATA:-}\" && cat \"$PGDATA/.rcoder-admin-user\"")
                .await
                .map_err(|_| backend("Read database administrator identity failed"))?;
            if marker.exit_code == 0 {
                break marker;
            }
            // initdb publishes the marker asynchronously. This is management
            // initialization, unrelated to the business service's Ready state.
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                Instant::now() + Duration::from_millis(500),
            ))
            .await;
            if Instant::now() >= deadline {
                return Err(backend("Database administrator identity deadline exceeded"));
            }
        };
        let admin_name = marker.stdout.trim();
        let admin =
            pg_utils::PgAdministrationTarget::new(admin_name.into(), "/var/run/postgresql".into())
                .map_err(|_| backend("Database administrator identity is invalid"))?;
        let username = body.username.as_deref().unwrap_or(admin_name).to_string();
        let command = UserAppControlCommand::ResetDatabasePassword {
            production: stage == UserappStage::Prod,
            username: username.clone(),
        };
        admission_uncertain = true;
        let admitted = state
            .userapp_store
            .admit(&UserAppAdmission {
                app_id: body.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: context.operation_id.clone(),
                request_id: body.request_id.clone(),
                request_fingerprint: context.request_fingerprint.clone(),
                kind: command.kind(),
                command: Some(command),
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await;
        // Domain rejections are confirmed transaction outcomes; storage errors
        // may have lost the commit acknowledgement and keep the lease fenced.
        admission_uncertain = matches!(&admitted, Err(UserAppStoreError::Storage(_)));
        let admitted = admitted.map_err(store_error)?;
        let op = match admitted {
            UserAppAdmissionOutcome::Accepted(op) => op,
            UserAppAdmissionOutcome::Existing(op) => {
                return replay_result(&op, &app.lifecycle_id, stage, &context.request_fingerprint);
            }
        };
        record = Some(op);
        let op = record
            .as_mut()
            .ok_or_else(|| backend("Database operation record unavailable"))?;
        advance(
            state,
            op,
            &context.executor_id,
            UserAppOperationState::Running,
            serde_json::Value::Null,
        )
        .await?;
        let receipt = lease
            .receipt()
            .ok_or_else(|| backend("Database operation lease identity unavailable"))?;
        state
            .userapp_store
            .bind_operation_lease(&context, &receipt)
            .await
            .map_err(store_error)?;
        let mut evidence = DatabasePasswordEvidence {
            receipt_protocol: Some(1),
            context: context.clone(),
            username: username.clone(),
            target: target.clone(),
            stage: DatabasePasswordStage::Captured,
        };
        advance(
            state,
            op,
            &context.executor_id,
            UserAppOperationState::Running,
            serde_json::to_value(&evidence)
                .map_err(|_| backend("Encode database target failed"))?,
        )
        .await?;
        loop {
            let ready = runner
                .run(&admin.ready_command())
                .await
                .map_err(|_| backend("Database administrator readiness could not be observed"))?;
            if ready.exit_code == 0 {
                break;
            }
            tokio::time::sleep_until(std::cmp::min(
                deadline,
                Instant::now() + Duration::from_millis(500),
            ))
            .await;
            if Instant::now() >= deadline {
                return Err(backend(
                    "Database administrator readiness deadline exceeded",
                ));
            }
        }
        let exists = runner
            .run(
                &admin
                    .role_exists_command(&username)
                    .map_err(|e| AppError::bad_request(&e))?,
            )
            .await
            .map_err(|_| backend("Database account preflight failed"))?;
        if exists.exit_code != 0 {
            return Err(backend("Database account preflight failed"));
        }
        let create = match exists.stdout.trim() {
            "1" => false,
            "" => true,
            _ => return Err(backend("Database account preflight returned invalid data")),
        };
        let sql = admin
            .password_operation_command(&context, op.scope, &username, &body.password, create)
            .map_err(|e| AppError::bad_request(&e))?;
        // Retain lease even if the durable intent commit itself becomes uncertain.
        uncertain = true;
        evidence.stage = DatabasePasswordStage::WriteSubmitted;
        advance(
            state,
            op,
            &context.executor_id,
            UserAppOperationState::Running,
            serde_json::to_value(&evidence)
                .map_err(|_| backend("Encode database intent failed"))?,
        )
        .await?;
        let applied = runner
            .run(&sql)
            .await
            .map_err(|_| backend("Password command outcome is unknown"))?;
        if applied.exit_code != 0 {
            return Err(backend("Password command did not confirm success"));
        }
        let receipt = runner
            .run(
                &admin
                    .password_operation_receipt_command(&context, op.scope, &username)
                    .map_err(|e| AppError::bad_request(&e))?,
            )
            .await
            .map_err(|_| backend("Password transaction receipt is unavailable"))?;
        if receipt.exit_code != 0 || receipt.stdout.trim() != "1" {
            return Err(backend(
                "Password transaction commit could not be confirmed",
            ));
        }
        let verified = runner
            .run(&pg_utils::pg_verify_credentials_cmd(
                &username,
                &body.password,
            ))
            .await
            .map_err(|_| backend("Password changed but TCP verification is unavailable"))?;
        if verified.exit_code != 0 {
            return Err(backend("Password changed but TCP verification failed"));
        }
        evidence.stage = DatabasePasswordStage::Verified;
        advance(
            state,
            op,
            &context.executor_id,
            UserAppOperationState::Running,
            serde_json::to_value(evidence)
                .map_err(|_| backend("Encode password verification failed"))?,
        )
        .await?;
        advance(
            state,
            op,
            &context.executor_id,
            UserAppOperationState::Succeeded,
            op.checkpoint.clone(),
        )
        .await?;
        uncertain = false;
        Ok("密码已设置".into())
    })
    .await;
    let result = match outcome {
        Ok(result) => result,
        Err(_) => {
            // Never infer rollback from a timeout: even the last Succeeded
            // commit can still finish after its observer is cancelled.
            uncertain |= record.is_some() || admission_uncertain;
            Err(backend(
                "Database password operation deadline exceeded; inspect original operation",
            ))
        }
    };
    uncertain |= admission_uncertain;
    // A separate, shared five-second settlement allowance bounds both durable
    // error recording and lease release. No runtime SQL is issued here.
    let settlement_deadline = Instant::now() + Duration::from_secs(5);
    if result.is_err()
        && let Some(op) = record.as_mut()
    {
        let status = if uncertain {
            UserAppOperationState::RecoveryRequired
        } else {
            UserAppOperationState::Failed
        };
        let checkpoint = op.checkpoint.clone();
        if !matches!(
            timeout_at(
                settlement_deadline,
                advance(state, op, &context.executor_id, status, checkpoint),
            )
            .await,
            Ok(Ok(()))
        ) {
            uncertain = true;
        }
    }
    if !uncertain
        && !matches!(
            timeout_at(settlement_deadline, lease.release()).await,
            Ok(Ok(()))
        )
    {
        return Err(
            backend("Database operation completed but lease release requires recovery")
                .with_operation_id(context.operation_id.clone()),
        );
    }
    result.map_err(|error| match &record {
        Some(record) => error.with_operation_id(record.operation_id.clone()),
        None if admission_uncertain => error.with_operation_id(context.operation_id.clone()),
        None => error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn password_observation_deadline_does_not_hide_store_errors() {
        let error = read_before(Instant::now() + Duration::from_secs(1), async {
            Err::<(), _>(UserAppStoreError::LifecycleConflict)
        })
        .await
        .unwrap_err();
        let AppError::Structured(error) = error else {
            panic!("structured error required")
        };
        assert_eq!(error.code, ERR_CONFLICT);
        let error = read_before(
            Instant::now() + Duration::from_millis(10),
            std::future::pending::<Result<(), UserAppStoreError>>(),
        )
        .await
        .unwrap_err();
        let AppError::Structured(error) = error else {
            panic!("structured error required")
        };
        assert_eq!(error.code, ERR_BACKEND_ERROR);
    }

    #[test]
    fn password_management_faults_cannot_trigger_wake() {
        use container_runtime_api::ContainerRuntimeError as E;
        assert!(management_available(Ok(())).unwrap());
        assert!(!management_available::<()>(Err(E::ManagementNotRunning)).unwrap());
        for error in [
            E::Conflict("different lifecycle".into()),
            E::ConnectionError("unreachable".into()),
            E::ContainerNotFound("missing workload".into()),
            E::ConfigurationError("unsupported".into()),
            E::Timeout("observation".into()),
        ] {
            assert!(management_available::<()>(Err(error)).is_err());
        }
    }

    fn request() -> UserappDbResetPasswordRequest {
        UserappDbResetPasswordRequest {
            request_id: Some("requestone".into()),
            lifecycle_id: Some("lifeone".into()),
            app_id: "appone".into(),
            username: Some("independent".into()),
            password: "private_marker".into(),
        }
    }
    pub(super) fn completed() -> UserAppOperationRecord {
        let body = request();
        let fingerprint = request_fingerprint(UserappStage::Prod, &body).unwrap();
        let context = UserAppExecutionContext {
            app_id: "appone".into(),
            lifecycle_id: "lifeone".into(),
            operation_id: "originaloperation".into(),
            executor_id: "originalexecutor".into(),
            request_fingerprint: fingerprint.clone(),
        };
        let evidence = DatabasePasswordEvidence {
            receipt_protocol: Some(1),
            context,
            username: "independent".into(),
            stage: DatabasePasswordStage::Verified,
            target: DatabasePasswordTarget::Prod(RuntimeConfigurationTarget {
                physical_uid: "originalphysical".into(),
                deployment_generation: "originalgeneration".into(),
            }),
        };
        UserAppOperationRecord {
            runtime_policy_on_success: None,
            admitted_metadata: None,
            command: Some(UserAppControlCommand::ResetDatabasePassword {
                production: true,
                username: "independent".into(),
            }),
            operation_id: "originaloperation".into(),
            app_id: "appone".into(),
            lifecycle_id: "lifeone".into(),
            request_id: body.request_id,
            request_fingerprint: fingerprint,
            kind: UserAppOperationKind::ResetProdDatabasePassword,
            scope: UserAppOperationScope::Prod,
            state: UserAppOperationState::Succeeded,
            revision: 5,
            executor_id: Some("originalexecutor".into()),
            step: "claimed".into(),
            checkpoint: serde_json::to_value(evidence).unwrap(),
            error_code: None,
            error_message: None,
            created_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn password_replay_requires_matching_request_lifecycle_and_verification() {
        let mut op = completed();
        let fingerprint = op.request_fingerprint.clone();
        assert!(replay_result(&op, "lifeone", UserappStage::Prod, &fingerprint).is_ok());
        assert!(replay_result(&op, "lifetwo", UserappStage::Prod, &fingerprint).is_err());
        assert!(replay_result(&op, "lifeone", UserappStage::Dev, &fingerprint).is_err());
        let mut changed = request();
        changed.password = "different_password".into();
        assert!(
            replay_result(
                &op,
                "lifeone",
                UserappStage::Prod,
                &request_fingerprint(UserappStage::Prod, &changed).unwrap()
            )
            .is_err()
        );
        op.checkpoint["stage"] = serde_json::json!("write_submitted");
        assert!(replay_result(&op, "lifeone", UserappStage::Prod, &fingerprint).is_err());
    }

    #[test]
    fn password_uncertain_replay_preserves_original_blocker() {
        let mut op = completed();
        op.state = UserAppOperationState::RecoveryRequired;
        op.checkpoint["stage"] = serde_json::json!("write_submitted");
        let AppError::Structured(error) =
            replay_result(&op, "lifeone", UserappStage::Prod, &op.request_fingerprint).unwrap_err()
        else {
            panic!("structured conflict required");
        };
        assert_eq!(error.operation_id.as_deref(), Some("originaloperation"));
        let blocker = error.blocker.unwrap();
        assert_eq!(blocker.operation_id, "originaloperation");
        assert_eq!(blocker.state, UserAppOperationState::RecoveryRequired);
        assert_eq!(blocker.scope, UserAppOperationScope::Prod);
        assert!(!format!("{op:?}").contains("private_marker"));
    }
    #[test]
    fn password_recovery_rejects_stale_foreign_and_legacy_evidence() {
        let mut op = completed();
        op.state = UserAppOperationState::RecoveryRequired;
        op.checkpoint["stage"] = serde_json::json!("write_submitted");
        let request = UserappDbPasswordRecoveryRequest {
            lifecycle_id: op.lifecycle_id.clone(),
            operation_id: op.operation_id.clone(),
            expected_revision: op.revision,
            original: request(),
        };
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &request).is_ok());
        assert!(validate_recovery_snapshot(&op, UserappStage::Dev, &request).is_err());
        let mut wrong = request.clone();
        wrong.expected_revision -= 1;
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &wrong).is_err());
        wrong = request.clone();
        wrong.original.password = "different_private_input".into();
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &wrong).is_err());
        wrong = request.clone();
        wrong.operation_id = "replacement".into();
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &wrong).is_err());
        let mut legacy = op.clone();
        legacy
            .checkpoint
            .as_object_mut()
            .unwrap()
            .remove("receipt_protocol");
        assert!(validate_recovery_snapshot(&legacy, UserappStage::Prod, &request).is_err());
        let mut foreign = op.clone();
        foreign.checkpoint["context"]["executor_id"] = serde_json::json!("otherworker");
        assert!(validate_recovery_snapshot(&foreign, UserappStage::Prod, &request).is_err());
        assert!(!format!("{request:?}").contains("private_marker"));
    }

    #[test]
    fn password_recovery_terminal_replay_distinguishes_cancelled_from_verified() {
        let mut op = completed();
        let request = UserappDbPasswordRecoveryRequest {
            lifecycle_id: op.lifecycle_id.clone(),
            operation_id: op.operation_id.clone(),
            expected_revision: 0,
            original: request(),
        };
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &request).is_ok());
        op.checkpoint["stage"] = serde_json::json!("cancelled");
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &request).is_err());
        op.state = UserAppOperationState::Failed;
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &request).is_ok());
        op.checkpoint["stage"] = serde_json::json!("verified");
        assert!(validate_recovery_snapshot(&op, UserappStage::Prod, &request).is_err());
    }

    #[test]
    fn password_lease_faults_are_not_mislabeled_as_contention() {
        use container_runtime_api::ContainerRuntimeError as E;
        for (error, code) in [
            (E::Conflict("private-detail".into()), ERR_CONFLICT),
            (
                E::ConfigurationError("private-detail".into()),
                ERR_BACKEND_ERROR,
            ),
            (
                E::ConnectionError("private-detail".into()),
                ERR_BACKEND_ERROR,
            ),
            (E::Timeout("private-detail".into()), ERR_BACKEND_ERROR),
            (E::K8sError("private-detail".into()), ERR_BACKEND_ERROR),
        ] {
            let AppError::Structured(result) = classify_lease_error(&error) else {
                panic!("structured error required");
            };
            assert_eq!(result.code, code);
            assert!(result.blocker.is_none());
            assert!(!format!("{result:?}").contains("private-detail"));
        }
    }
}
