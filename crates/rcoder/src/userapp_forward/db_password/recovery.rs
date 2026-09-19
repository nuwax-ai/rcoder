//! Explicit original-operation reconciliation; never replays password writes.
use super::*;

pub(super) fn validate_recovery_snapshot(
    record: &UserAppOperationRecord,
    stage: UserappStage,
    request: &UserappDbPasswordRecoveryRequest,
) -> Result<DatabasePasswordEvidence, AppError> {
    request
        .validate()
        .map_err(|error| AppError::bad_request(&error))?;
    let fingerprint = request_fingerprint(stage, &request.original)?;
    let expected_kind = if stage == UserappStage::Prod {
        UserAppOperationKind::ResetProdDatabasePassword
    } else {
        UserAppOperationKind::ResetDevDatabasePassword
    };
    if record.app_id != request.original.app_id
        || record.lifecycle_id != request.lifecycle_id
        || record.operation_id != request.operation_id
        || record.request_id != request.original.request_id
        || record.request_fingerprint != fingerprint
        || record.kind != expected_kind
    {
        return Err(AppError::conflict(
            "Password recovery identity differs from original admission",
        ));
    }
    let evidence: DatabasePasswordEvidence = serde_json::from_value(record.checkpoint.clone())
        .map_err(|_| AppError::conflict("Password recovery has no physical write evidence"))?;
    evidence
        .validate_operation(record)
        .map_err(|_| AppError::conflict("Password recovery physical evidence is invalid"))?;
    if evidence.receipt_protocol != Some(1) {
        return Err(AppError::conflict(
            "Legacy password writes cannot be fenced by transaction receipts",
        ));
    }
    if record.state.is_terminal() {
        if !matches!(
            (&record.state, &evidence.stage),
            (
                UserAppOperationState::Succeeded,
                DatabasePasswordStage::Verified
            ) | (
                UserAppOperationState::Failed,
                DatabasePasswordStage::Cancelled
            )
        ) {
            return Err(AppError::conflict(
                "Password recovery terminal evidence is inconsistent",
            ));
        }
    } else if record.revision != request.expected_revision
        || !matches!(
            record.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        )
        || !matches!(
            evidence.stage,
            DatabasePasswordStage::WriteSubmitted
                | DatabasePasswordStage::Verified
                | DatabasePasswordStage::Cancelled
        )
    {
        return Err(AppError::conflict(
            "Password recovery snapshot changed or cannot be reconciled",
        ));
    }
    Ok(evidence)
}

/// Explicit recovery never starts/replaces a container or reissues a password
/// write. The original physical execution channel and original request survive.
pub(crate) async fn recover(
    state: Arc<AppState>,
    stage: UserappStage,
    request: UserappDbPasswordRecoveryRequest,
) -> Result<UserappDbPasswordRecoveryResponse, AppError> {
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
        recover_coordinated(&state, stage, request).await
    })
    .await
    .map_err(|_| {
        backend("Password recovery interrupted; query original operation")
            .with_operation_id(operation_id.clone())
    })?
    .map_err(|error| error.with_operation_id(operation_id))
}

async fn recover_coordinated(
    state: &AppState,
    stage: UserappStage,
    request: UserappDbPasswordRecoveryRequest,
) -> Result<UserappDbPasswordRecoveryResponse, AppError> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut record = read_before(
        deadline,
        state
            .userapp_store
            .get_operation(&request.original.app_id, &request.operation_id),
    )
    .await?
    .ok_or_else(|| AppError::not_found("Original password operation not found"))?;
    let mut evidence = validate_recovery_snapshot(&record, stage, &request)?;
    let app = read_before(
        deadline,
        state.userapp_store.get_application(&record.app_id),
    )
    .await?
    .ok_or_else(|| AppError::with_message(ERR_APP_NOT_FOUND, "Application identity not found"))?;
    if app.lifecycle_id != record.lifecycle_id {
        return Err(AppError::conflict(
            "Application lifecycle changed before password recovery",
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
            "Password recovery lease execution identity changed",
        ));
    }
    if !record.state.is_terminal() {
        if binding.is_none()
            || app.active_operations.slot(record.scope) != Some(&record.operation_id)
        {
            return Err(AppError::conflict(
                "Password recovery lost its original lease or scope slot",
            ));
        }
        let runner = Runner {
            state,
            context: &evidence.context,
            target: &evidence.target,
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
        evidence.stage = confirm_remote_outcome(
            &runner,
            &admin,
            &evidence,
            record.scope,
            &request.original.password,
        )
        .await?;
        // Storage owns the transaction to completion. Do not cancel its future
        // and mistake an observation timeout for a rolled-back terminal commit.
        record = state
            .userapp_store
            .finalize_password_recovery(&record, &evidence)
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
    Ok(UserappDbPasswordRecoveryResponse {
        operation_id: record.operation_id,
        lifecycle_id: record.lifecycle_id,
        revision: record.revision,
        state: record.state,
        lease_cleanup_pending,
    })
}

async fn confirm_remote_outcome(
    runner: &dyn PgCommandRunner,
    admin: &pg_utils::PgAdministrationTarget,
    evidence: &DatabasePasswordEvidence,
    scope: UserAppOperationScope,
    password: &str,
) -> Result<DatabasePasswordStage, AppError> {
    // This is the sole remote mutation: inserting an immutable cancellation
    // receipt. It fences even a writer that has not reached PostgreSQL yet.
    let command = admin
        .password_operation_cancel_command(&evidence.context, scope, &evidence.username)
        .map_err(|_| backend("Build password recovery command failed"))?;
    let result = runner
        .run(&command)
        .await
        .map_err(|_| backend("Password recovery receipt outcome is unknown"))?;
    if result.exit_code != 0 {
        return Err(backend(
            "Password recovery receipt transaction did not confirm commit",
        ));
    }
    let stage = match result.stdout.trim() {
        "cancelled" => DatabasePasswordStage::Cancelled,
        "committed" => {
            let verified = runner
                .run(&pg_utils::pg_verify_credentials_cmd(
                    &evidence.username,
                    password,
                ))
                .await
                .map_err(|_| backend("Committed password TCP verification is unavailable"))?;
            if verified.exit_code != 0 || verified.stdout.trim() != "1" {
                return Err(backend("Committed password TCP verification failed"));
            }
            DatabasePasswordStage::Verified
        }
        _ => {
            return Err(AppError::conflict(
                "Password recovery receipt identity is unconfirmed",
            ));
        }
    };
    Ok(stage)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::VecDeque, sync::Mutex};

    struct Scripted {
        outcomes: Mutex<VecDeque<Result<CommandOutcome, String>>>,
        commands: Mutex<Vec<String>>,
    }
    #[async_trait::async_trait]
    impl PgCommandRunner for Scripted {
        async fn run(&self, command: &str) -> Result<CommandOutcome, String> {
            self.commands.lock().unwrap().push(command.into());
            self.outcomes
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected command")
        }
    }
    fn outcome(code: i64, text: &str) -> Result<CommandOutcome, String> {
        Ok(CommandOutcome {
            exit_code: code,
            stdout: text.into(),
            stderr: String::new(),
        })
    }
    fn evidence() -> DatabasePasswordEvidence {
        let op = super::super::tests::completed();
        serde_json::from_value(op.checkpoint).unwrap()
    }

    #[tokio::test]
    async fn recovery_receipt_requires_commit_and_tcp_and_never_replays_password_sql() {
        let admin =
            pg_utils::PgAdministrationTarget::new("postgres".into(), "/var/run/postgresql".into())
                .unwrap();
        for (outputs, expected, count) in [
            (
                vec![outcome(0, "cancelled")],
                Some(DatabasePasswordStage::Cancelled),
                1,
            ),
            (
                vec![outcome(0, "committed"), outcome(0, "1")],
                Some(DatabasePasswordStage::Verified),
                2,
            ),
            (vec![outcome(1, "committed")], None, 1),
            (vec![outcome(0, "")], None, 1),
            (vec![outcome(0, "committed\ncancelled")], None, 1),
            (vec![Err("sensitive transport diagnostics".into())], None, 1),
            (vec![outcome(0, "committed"), outcome(1, "")], None, 2),
            (vec![outcome(0, "committed"), outcome(0, "")], None, 2),
            (
                vec![outcome(0, "committed"), Err("private SQL".into())],
                None,
                2,
            ),
        ] {
            let runner = Scripted {
                outcomes: Mutex::new(outputs.into()),
                commands: Mutex::new(Vec::new()),
            };
            let result = confirm_remote_outcome(
                &runner,
                &admin,
                &evidence(),
                UserAppOperationScope::Prod,
                "private_password",
            )
            .await;
            match expected {
                Some(stage) => assert_eq!(result.unwrap(), stage),
                None => {
                    let error = format!("{:?}", result.unwrap_err());
                    assert!(!error.contains("sensitive transport diagnostics"));
                    assert!(!error.contains("private SQL"));
                    assert!(!error.contains("private_password"));
                }
            }
            let commands = runner.commands.lock().unwrap();
            assert_eq!(commands.len(), count);
            assert!(
                commands
                    .iter()
                    .all(|cmd| !cmd.contains("ALTER ROLE") && !cmd.contains("CREATE ROLE"))
            );
            assert!(commands[0].contains("COMMIT"));
            if count == 2 {
                assert!(commands[1].contains("-h 127.0.0.1"));
            }
        }
    }
}
