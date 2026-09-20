//! Explicit deployment password reconciliation under the original operation.
//! Never redeploys and never reissues a password write; the receipt outcome
//! (committed with TCP verification, or cancelled tombstone) is the only proof.
use super::*;

pub(super) fn validate_deploy_pg_snapshot(
    record: &UserAppOperationRecord,
    request: &UserappDeployPgRecoveryRequest,
) -> Result<ExplicitDeploymentPasswordEvidence, AppError> {
    request
        .validate()
        .map_err(|error| AppError::bad_request(&error))?;
    let evidence: ExplicitDeploymentPasswordEvidence =
        serde_json::from_value(record.checkpoint.clone()).map_err(|_| {
            AppError::conflict("Deployment password recovery has no receipt evidence")
        })?;
    evidence
        .validate_operation(record)
        .map_err(|_| AppError::conflict("Deployment password recovery evidence is invalid"))?;
    if evidence.receipt_protocol != Some(1) {
        return Err(AppError::conflict(
            "Legacy deployment password writes cannot be fenced by transaction receipts",
        ));
    }
    if record.app_id != request.app_id
        || record.lifecycle_id != request.lifecycle_id
        || record.operation_id != request.operation_id
        || evidence.username != request.username
    {
        return Err(AppError::conflict(
            "Deployment password recovery identity differs from original admission",
        ));
    }
    if record.state.is_terminal() {
        if record.state != UserAppOperationState::Failed
            || !matches!(
                evidence.stage,
                DatabasePasswordStage::Verified | DatabasePasswordStage::Cancelled
            )
        {
            return Err(AppError::conflict(
                "Deployment password recovery terminal evidence is inconsistent",
            ));
        }
    } else if record.revision != request.expected_revision
        || record.state != UserAppOperationState::RecoveryRequired
        || !matches!(
            evidence.stage,
            DatabasePasswordStage::WriteSubmitted
                | DatabasePasswordStage::Verified
                | DatabasePasswordStage::Cancelled
        )
    {
        return Err(AppError::conflict(
            "Deployment password recovery snapshot changed or cannot be reconciled",
        ));
    }
    Ok(evidence)
}

struct DeployRunner<'a> {
    state: &'a AppState,
    context: &'a UserAppExecutionContext,
    target: &'a RuntimeConfigurationTarget,
    deadline: Instant,
}

#[async_trait::async_trait]
impl PgCommandRunner for DeployRunner<'_> {
    async fn run(&self, command: &str) -> Result<CommandOutcome, String> {
        let args = vec!["sh".into(), "-c".into(), command.into()];
        let result = timeout_at(self.deadline, async {
            self.state
                .runtime()
                .exec_app_configuration_target(self.context, self.target, args)
                .await
        })
        .await
        .map_err(|_| "Deployment database command deadline exceeded".to_string())?
        .map_err(|_| "Identity-bound deployment database command failed".to_string())?;
        Ok(CommandOutcome {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }
}

pub(crate) async fn recover_deploy_pg(
    state: Arc<AppState>,
    request: UserappDeployPgRecoveryRequest,
) -> Result<UserappDeployPgRecoveryResponse, AppError> {
    request
        .validate()
        .map_err(|error| AppError::bad_request(&error))?;
    let flight = state
        .userapp_op_flight
        .guard()
        .map_err(|_| backend("Runtime is shutting down"))?;
    let operation_id = request.operation_id.clone();
    tokio::spawn(async move {
        let _flight = flight;
        recover_deploy_pg_coordinated(&state, request).await
    })
    .await
    .map_err(|_| {
        backend("Deployment password recovery interrupted; query original operation")
            .with_operation_id(operation_id.clone())
    })?
    .map_err(|error| error.with_operation_id(operation_id))
}

async fn recover_deploy_pg_coordinated(
    state: &AppState,
    request: UserappDeployPgRecoveryRequest,
) -> Result<UserappDeployPgRecoveryResponse, AppError> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut record = read_before(
        deadline,
        state
            .userapp_store
            .get_operation(&request.app_id, &request.operation_id),
    )
    .await?
    .ok_or_else(|| AppError::not_found("Original deployment operation not found"))?;
    let mut evidence = validate_deploy_pg_snapshot(&record, &request)?;
    let app = read_before(
        deadline,
        state.userapp_store.get_application(&record.app_id),
    )
    .await?
    .ok_or_else(|| AppError::with_message(ERR_APP_NOT_FOUND, "Application identity not found"))?;
    if app.lifecycle_id != record.lifecycle_id {
        return Err(AppError::conflict(
            "Application lifecycle changed before deployment password recovery",
        ));
    }
    let binding = read_before(
        deadline,
        state
            .userapp_store
            .get_operation_lease(&record.app_id, &record.operation_id),
    )
    .await?;
    if let Some(binding) = &binding
        && binding.context != evidence.context
    {
        return Err(AppError::conflict(
            "Deployment password recovery lease execution identity changed",
        ));
    }
    if !record.state.is_terminal() {
        if binding.is_none()
            || app.active_operations.slot(record.scope) != Some(&record.operation_id)
        {
            return Err(AppError::conflict(
                "Deployment password recovery lost its original lease or scope slot",
            ));
        }
        // Re-capture the current database target; recovery only proceeds on the
        // original physical identity, never on a replaced container.
        let spec = timeout_at(
            deadline,
            state.runtime().get_app_container_spec(&record.app_id),
        )
        .await
        .map_err(|_| backend("Deployment database observation deadline exceeded"))?
        .map_err(|_| backend("Read deployment database generation failed"))?;
        let generation = spec
            .env
            .as_ref()
            .and_then(|env| env.get(APP_DEPLOY_GENERATION_ID))
            .filter(|generation| !generation.is_empty())
            .ok_or_else(|| backend("Deployment database has no generation"))?;
        let target = timeout_at(
            deadline,
            state
                .runtime()
                .capture_app_configuration_target(&evidence.context, generation),
        )
        .await
        .map_err(|_| backend("Deployment database observation deadline exceeded"))?
        .map_err(|_| backend("Capture deployment database target failed"))?;
        if target != evidence.explicit_pg_target {
            return Err(AppError::conflict(
                "Original deployment database target was replaced; manual reconciliation required",
            ));
        }
        let runner = DeployRunner {
            state,
            context: &evidence.context,
            target: &target,
            deadline,
        };
        let marker = runner
            .run("test -n \"${PGDATA:-}\" && cat \"$PGDATA/.rcoder-admin-user\"")
            .await
            .map_err(|_| backend("Read original database administrator failed"))?;
        if marker.exit_code != 0 {
            return Err(backend("Original database administrator is unavailable"));
        }
        let admin = pg_utils::PgAdministrationTarget::new(
            marker.stdout.trim().into(),
            "/var/run/postgresql".into(),
        )
        .map_err(|_| backend("Original database administrator is invalid"))?;
        evidence.stage = confirm_deploy_outcome(
            &runner,
            &admin,
            &evidence.context,
            &evidence.username,
            &request.password,
        )
        .await?;
        // Storage owns the transaction to completion. Do not cancel its future
        // and mistake an observation timeout for a rolled-back terminal commit.
        record = state
            .userapp_store
            .finalize_deploy_pg_recovery(&record, &evidence)
            .await
            .map_err(store_error)?;
    }
    let mut lease_cleanup_pending = binding.is_some();
    if let Some(binding) = binding {
        let cleanup_deadline = Instant::now() + Duration::from_secs(5);
        if matches!(
            timeout_at(
                cleanup_deadline,
                state
                    .runtime()
                    .release_app_operation_receipt(&binding.context, &binding.receipt)
            )
            .await,
            Ok(Ok(()))
        ) {
            // Failure to forget is harmless: the original receipt remains for
            // idempotent terminal scanning; never remove another operation's lock.
            lease_cleanup_pending = state
                .userapp_store
                .forget_operation_lease(&binding)
                .await
                .is_err();
        }
    }
    Ok(UserappDeployPgRecoveryResponse {
        operation_id: record.operation_id,
        lifecycle_id: record.lifecycle_id,
        revision: record.revision,
        state: record.state,
        stage: evidence.stage,
        lease_cleanup_pending,
    })
}

/// Mirrors the password recovery outcome machine: the only remote mutation is
/// inserting an immutable cancellation receipt, which fences delayed writers.
async fn confirm_deploy_outcome(
    runner: &dyn PgCommandRunner,
    admin: &pg_utils::PgAdministrationTarget,
    context: &UserAppExecutionContext,
    username: &str,
    password: &str,
) -> Result<DatabasePasswordStage, AppError> {
    let command = admin
        .password_operation_cancel_command(context, UserAppOperationScope::Prod, username)
        .map_err(|_| backend("Build deployment password recovery command failed"))?;
    let result = runner
        .run(&command)
        .await
        .map_err(|_| backend("Deployment password recovery receipt outcome is unknown"))?;
    if result.exit_code != 0 {
        return Err(backend(
            "Deployment password recovery receipt transaction did not confirm commit",
        ));
    }
    let stage = match result.stdout.trim() {
        "cancelled" => DatabasePasswordStage::Cancelled,
        "committed" => {
            let verified = runner
                .run(&pg_utils::pg_verify_credentials_cmd(username, password))
                .await
                .map_err(|_| backend("Committed password TCP verification is unavailable"))?;
            if verified.exit_code != 0 || verified.stdout.trim() != "1" {
                return Err(backend("Committed password TCP verification failed"));
            }
            DatabasePasswordStage::Verified
        }
        _ => {
            return Err(AppError::conflict(
                "Deployment password recovery receipt identity is unconfirmed",
            ));
        }
    };
    Ok(stage)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deploy_record(
        stage: DatabasePasswordStage,
        state: UserAppOperationState,
    ) -> UserAppOperationRecord {
        UserAppOperationRecord {
            runtime_policy_on_success: None,
            admitted_metadata: None,
            command: Some(UserAppControlCommand::Deploy {
                restart: false,
                input_digest: "b".repeat(64),
            }),
            operation_id: "deployoperation".into(),
            app_id: "deployapp".into(),
            lifecycle_id: "deploylife".into(),
            request_id: Some("deployrequest".into()),
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::StartDeployment,
            scope: UserAppOperationScope::Prod,
            state,
            revision: 3,
            executor_id: Some("deployworker".into()),
            step: "explicit_pg_applying".into(),
            checkpoint: serde_json::to_value(ExplicitDeploymentPasswordEvidence {
                receipt_protocol: Some(1),
                context: UserAppExecutionContext {
                    app_id: "deployapp".into(),
                    lifecycle_id: "deploylife".into(),
                    operation_id: "deployoperation".into(),
                    executor_id: "deployworker".into(),
                    request_fingerprint: "a".repeat(64),
                },
                username: "business".into(),
                explicit_pg_target: RuntimeConfigurationTarget {
                    physical_uid: "physicalone".into(),
                    deployment_generation: "generationone".into(),
                },
                stage,
            })
            .unwrap(),
            error_code: None,
            error_message: None,
            created_at: chrono::Utc::now(),
        }
    }

    fn request() -> UserappDeployPgRecoveryRequest {
        UserappDeployPgRecoveryRequest {
            app_id: "deployapp".into(),
            lifecycle_id: "deploylife".into(),
            operation_id: "deployoperation".into(),
            expected_revision: 3,
            username: "business".into(),
            password: "private_marker".into(),
        }
    }

    #[test]
    fn deploy_pg_snapshot_accepts_uncertain_and_terminal_failed_outcomes() {
        let request = request();
        assert!(
            validate_deploy_pg_snapshot(
                &deploy_record(
                    DatabasePasswordStage::WriteSubmitted,
                    UserAppOperationState::RecoveryRequired
                ),
                &request
            )
            .is_ok()
        );
        assert!(
            validate_deploy_pg_snapshot(
                &deploy_record(
                    DatabasePasswordStage::WriteSubmitted,
                    UserAppOperationState::Running
                ),
                &request,
            )
            .is_err(),
            "an active deployment may still execute policy or SQL after password verification"
        );
        // Terminal replay of an already reconciled outcome is idempotent.
        for stage in [
            DatabasePasswordStage::Verified,
            DatabasePasswordStage::Cancelled,
        ] {
            assert!(
                validate_deploy_pg_snapshot(
                    &deploy_record(stage, UserAppOperationState::Failed),
                    &request
                )
                .is_ok()
            );
        }
        // A completed deployment never parses as write evidence.
        assert!(
            validate_deploy_pg_snapshot(
                &deploy_record(
                    DatabasePasswordStage::Verified,
                    UserAppOperationState::Succeeded
                ),
                &request
            )
            .is_err()
        );
    }

    #[test]
    fn deploy_pg_snapshot_rejects_stale_foreign_and_legacy_evidence() {
        let record = deploy_record(
            DatabasePasswordStage::WriteSubmitted,
            UserAppOperationState::RecoveryRequired,
        );
        let mut wrong = request();
        wrong.expected_revision -= 1;
        assert!(validate_deploy_pg_snapshot(&record, &wrong).is_err());
        wrong = request();
        wrong.operation_id = "replacement".into();
        assert!(validate_deploy_pg_snapshot(&record, &wrong).is_err());
        wrong = request();
        wrong.username = "otheraccount".into();
        assert!(validate_deploy_pg_snapshot(&record, &wrong).is_err());
        wrong = request();
        wrong.password = "different_private_input".into();
        assert!(
            validate_deploy_pg_snapshot(&record, &wrong).is_ok(),
            "password itself is verified remotely, not by fingerprint"
        );
        let mut legacy = record.clone();
        legacy
            .checkpoint
            .as_object_mut()
            .unwrap()
            .remove("receipt_protocol");
        assert!(validate_deploy_pg_snapshot(&legacy, &request()).is_err());
        let mut foreign = record.clone();
        foreign.checkpoint["context"]["executor_id"] = serde_json::json!("otherworker");
        assert!(validate_deploy_pg_snapshot(&foreign, &request()).is_err());
        let mut reset_kind = record.clone();
        reset_kind.kind = UserAppOperationKind::ResetProdDatabasePassword;
        reset_kind.command = Some(UserAppControlCommand::ResetDatabasePassword {
            production: true,
            username: "business".into(),
        });
        assert!(validate_deploy_pg_snapshot(&reset_kind, &request()).is_err());
        assert!(!format!("{:?}", request()).contains("private_marker"));
        assert!(!record.checkpoint.to_string().contains("private_marker"));
    }
}
