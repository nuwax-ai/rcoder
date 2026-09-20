//! Explicit deployment credentials update only the identity-bound current database.
//! No database transaction remains open while the runtime executes commands.
use async_trait::async_trait;
use shared_types::CredentialMutationEvidence;

use crate::{models::*, service::AppService};

struct BoundManagementRunner<'a> {
    service: &'a AppService,
    context: &'a shared_types::UserAppExecutionContext,
    target: &'a shared_types::RuntimeConfigurationTarget,
}

#[async_trait]
impl shared_types::PgCommandRunner for BoundManagementRunner<'_> {
    async fn run(&self, command: &str) -> Result<shared_types::CommandOutcome, String> {
        let result = self
            .service
            .runtime
            .exec_app_configuration_target(
                self.context,
                self.target,
                vec!["sh".into(), "-c".into(), command.into()],
            )
            .await
            .map_err(|error| format!("Bound PostgreSQL command failed: {error}"))?;
        Ok(shared_types::CommandOutcome {
            exit_code: result.exit_code,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }
}

impl AppService {
    /// The caller supplies its overall operation deadline; retries cannot extend it.
    pub(crate) async fn wait_for_configuration_postgres(
        &self,
        context: &shared_types::UserAppExecutionContext,
        target: &shared_types::RuntimeConfigurationTarget,
        deadline: tokio::time::Instant,
    ) -> AppResult<shared_types::pg_utils::PgAdministrationTarget> {
        use shared_types::PgCommandRunner as _;
        let runner = BoundManagementRunner {
            service: self,
            context,
            target,
        };
        loop {
            let attempt = async {
                // The image writes this marker from initdb/verified legacy PGDATA.
                // Never guess the administrator from a new business credential.
                let Ok(marker) = runner
                    .run("test -n \"${PGDATA:-}\" && cat \"$PGDATA/.rcoder-admin-user\"")
                    .await
                else {
                    return Ok::<_, AppOperationError>(None);
                };
                if marker.exit_code != 0 {
                    return Ok(None);
                }
                let admin = shared_types::pg_utils::PgAdministrationTarget::new(
                    marker.stdout.trim().into(),
                    "/var/run/postgresql".into(),
                )
                .map_err(|_| AppOperationError::InvalidState("PGDATA administrator identity is invalid; explicit reconciliation is required".into()))?;
                let Ok(ready) = runner.run(&admin.business_database_ready_command()).await else {
                    return Ok(None);
                };
                Ok((ready.exit_code == 0 && ready.stdout.trim() == "1").then_some(admin))
            };
            match tokio::time::timeout_at(deadline, attempt).await {
                Ok(Ok(Some(admin))) => return Ok(admin),
                Ok(Err(error)) => return Err(error),
                Ok(Ok(None)) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep_until(std::cmp::min(deadline, tokio::time::Instant::now() + std::time::Duration::from_millis(500))).await;
                }
                _ => return Err(AppOperationError::CredentialApplication {
                    message: "PostgreSQL management readiness was not confirmed within the operation budget".into(),
                    mutation: CredentialMutationEvidence::NotAttempted,
                }),
            }
        }
    }

    /// Explicit deployment PG input updates the current database only. It is
    /// never saved as desired startup configuration or injected into business env.
    ///
    /// The write uses the receipt transaction: the password change and its
    /// immutable receipt commit together, so an unknown outcome is later
    /// provable under the original operation identity.
    pub(crate) async fn apply_explicit_deployment_credentials(
        &self,
        operation: &mut crate::service::OwnedOperation,
        guard: &crate::service::AppOperationGuard,
        pg: &StartPgCredential,
    ) -> AppResult<()> {
        use shared_types::PgCommandRunner as _;

        let context = operation.execution_context();
        let deadline_ms = self
            .metadata
            .store
            .operation_deadline(&context.app_id, &context.operation_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::InvalidState(
                    "Explicit deployment credentials require the original deadline".into(),
                )
            })?;
        let remaining = deadline_ms
            .saturating_sub(chrono::Utc::now().timestamp_millis())
            .max(0) as u64;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(remaining);
        let effect = async {
            let spec = self
                .runtime
                .get_app_container_spec(&context.app_id)
                .await
                .map_err(|error| {
                    crate::utils::map_runtime_error(
                        "Read explicit database target generation",
                        error,
                    )
                })?;
            let generation = spec
                .env
                .as_ref()
                .and_then(|env| env.get(shared_types::APP_DEPLOY_GENERATION_ID))
                .filter(|value| !value.is_empty())
                .ok_or_else(|| {
                    AppOperationError::InvalidState("Database target generation is missing".into())
                })?;
            let target = self
                .runtime
                .capture_app_configuration_target(&context, generation)
                .await
                .map_err(|error| {
                    crate::utils::map_runtime_error("Capture explicit database target", error)
                })?;
            let admin = self
                .wait_for_configuration_postgres(&context, &target, deadline)
                .await?;
            let runner = BoundManagementRunner {
                service: self,
                context: &context,
                target: &target,
            };
            let exists = runner
                .run(
                    &admin
                        .role_exists_command(&pg.username)
                        .map_err(AppOperationError::Validation)?,
                )
                .await
                .map_err(|_| AppOperationError::CredentialApplication {
                    message: "Explicit database account preflight transport failed".into(),
                    mutation: CredentialMutationEvidence::NotAttempted,
                })?;
            if exists.exit_code != 0 {
                return Err(AppOperationError::CredentialApplication {
                    message: "Explicit database account preflight failed".into(),
                    mutation: CredentialMutationEvidence::NotAttempted,
                });
            }
            let create = match exists.stdout.trim() {
                "1" => false,
                "" => true,
                _ => {
                    return Err(AppOperationError::CredentialApplication {
                        message: "Explicit database account preflight returned invalid data".into(),
                        mutation: CredentialMutationEvidence::NotAttempted,
                    });
                }
            };
            let sql = admin
                .password_operation_command(
                    &context,
                    shared_types::UserAppOperationScope::Prod,
                    &pg.username,
                    &pg.password,
                    create,
                )
                .map_err(AppOperationError::Validation)?;
            let mut evidence = shared_types::ExplicitDeploymentPasswordEvidence {
                receipt_protocol: Some(1),
                context: context.clone(),
                username: pg.username.clone(),
                explicit_pg_target: target.clone(),
                stage: shared_types::DatabasePasswordStage::WriteSubmitted,
            };
            guard.mark_mutating()?;
            operation
                .checkpoint(
                    "explicit_pg_applying",
                    serde_json::to_value(&evidence).map_err(|_| {
                        AppOperationError::Backend("Encode explicit database intent".into())
                    })?,
                )
                .await?;
            let applied =
                runner
                    .run(&sql)
                    .await
                    .map_err(|_| AppOperationError::CredentialApplication {
                        message: "Explicit database write outcome is unknown".into(),
                        mutation: CredentialMutationEvidence::Unknown,
                    })?;
            if applied.exit_code != 0 {
                return Err(AppOperationError::CredentialApplication {
                    message: "Explicit database write did not confirm success".into(),
                    mutation: CredentialMutationEvidence::Unknown,
                });
            }
            let receipt = runner
                .run(
                    &admin
                        .password_operation_receipt_command(
                            &context,
                            shared_types::UserAppOperationScope::Prod,
                            &pg.username,
                        )
                        .map_err(AppOperationError::Validation)?,
                )
                .await
                .map_err(|_| AppOperationError::CredentialApplication {
                    message: "Explicit database transaction receipt is unavailable".into(),
                    mutation: CredentialMutationEvidence::Unknown,
                })?;
            if receipt.exit_code != 0 || receipt.stdout.trim() != "1" {
                return Err(AppOperationError::CredentialApplication {
                    message: "Explicit database transaction commit could not be confirmed".into(),
                    mutation: CredentialMutationEvidence::Unknown,
                });
            }
            let verified = runner
                .run(&shared_types::pg_utils::pg_verify_credentials_cmd(
                    &pg.username,
                    &pg.password,
                ))
                .await
                .map_err(|_| AppOperationError::CredentialApplication {
                    message:
                        "Explicit database password changed but TCP verification is unavailable"
                            .into(),
                    mutation: CredentialMutationEvidence::AppliedButUnverified,
                })?;
            if verified.exit_code != 0 {
                return Err(AppOperationError::CredentialApplication {
                    message: "Explicit database password changed but TCP verification failed"
                        .into(),
                    mutation: CredentialMutationEvidence::AppliedButUnverified,
                });
            }
            evidence.stage = shared_types::DatabasePasswordStage::Verified;
            operation
                .checkpoint(
                    "explicit_pg_applied",
                    serde_json::to_value(&evidence).map_err(|_| {
                        AppOperationError::Backend("Encode explicit database completion".into())
                    })?,
                )
                .await?;
            Ok(())
        };
        tokio::time::timeout_at(deadline,effect).await.map_err(|_|AppOperationError::CredentialApplication {
            message:"Explicit database write outcome requires reconciliation under the original operation".into(),
            mutation:CredentialMutationEvidence::Unknown,
        })?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::UserAppRuntimeConfigurationStore as _;
    use std::sync::{Arc, atomic::Ordering};
    #[tokio::test]
    async fn explicit_deployment_password_changes_only_current_database() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (mut service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
        let app = "explicitpassword";
        let identity = service.metadata.store.ensure_identity(app).await.unwrap();
        runtime.specs.insert(
            app.into(),
            container_runtime_api::ContainerSpecSnapshot {
                env: Some(
                    [(
                        shared_types::APP_DEPLOY_GENERATION_ID.into(),
                        "existinggeneration".into(),
                    )]
                    .into(),
                ),
                ..Default::default()
            },
        );
        let guard = service.try_acquire_process_release_lock(app).await.unwrap();
        let mut operation = crate::service::OwnedOperation::admit(
            service.metadata.store.clone(),
            shared_types::UserAppAdmission {
                app_id: app.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                operation_id: "explicitoperation".into(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::StartDeployment,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            },
        )
        .await
        .unwrap();
        operation.bind_lease(&guard).await.unwrap();
        let context = operation.execution_context();
        service
            .metadata
            .store
            .bind_operation_deadline(
                app,
                &context.operation_id,
                &context.lifecycle_id,
                chrono::Utc::now().timestamp_millis() + 3000,
            )
            .await
            .unwrap();
        for (stdout, exit_code) in [
            ("administrator", 0),
            ("1", 0),
            ("1", 0),
            ("", 0),
            ("1", 0),
            ("1", 0),
        ] {
            runtime.configuration_replies.lock().unwrap().push_back(
                container_runtime_api::ExecResult {
                    stdout: stdout.into(),
                    exit_code,
                    stderr: String::new(),
                },
            );
        }
        service
            .apply_explicit_deployment_credentials(
                &mut operation,
                &guard,
                &StartPgCredential {
                    username: "business".into(),
                    password: "newprivate".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 0);
        let commands = runtime.configuration_commands.lock().unwrap().clone();
        assert_eq!(commands.len(), 6);
        assert!(commands.iter().all(
            |command| !command.join(" ").contains("pg_terminate_backend")
                && !command.join(" ").contains("/configuration/")
                && !command.join(" ").contains("restart")
        ));
        // The write itself is the receipt transaction: password change and its
        // immutable receipt commit together under the captured administrator.
        let receipt_command = commands[3].join(" ");
        assert!(receipt_command.contains("BEGIN ISOLATION LEVEL READ COMMITTED;"));
        assert!(receipt_command.contains("COMMIT;"));
        assert!(receipt_command.contains("-U 'administrator'"));
        let targets = runtime.configuration_targets.lock().unwrap().clone();
        assert!(
            targets
                .iter()
                .all(|target| target.deployment_generation == "existinggeneration")
        );
        let record = service
            .metadata
            .store
            .get_operation(app, &context.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.checkpoint["explicit_pg_target"]["deployment_generation"],
            "existinggeneration"
        );
        assert_eq!(record.checkpoint["stage"], "verified");
        assert_eq!(record.checkpoint["receipt_protocol"], 1);
        assert_eq!(record.step, "explicit_pg_applied");
        assert!(!record.checkpoint.to_string().contains("newprivate"));
        assert!(
            store
                .runtime_configuration_status(
                    app,
                    &identity.lifecycle_id,
                    shared_types::UserAppOperationScope::Prod
                )
                .await
                .unwrap()
                .is_none()
        );
    }

    /// 反例：写命令失败（断连/超时同类的未知结果）时，证据停留在
    /// write_submitted 且错误按 Unknown 分类，操作进 RecoveryRequired。
    #[tokio::test]
    async fn explicit_deployment_write_failure_keeps_write_submitted_evidence() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (mut service, _store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
        let app = "explicitunknown";
        let identity = service.metadata.store.ensure_identity(app).await.unwrap();
        runtime.specs.insert(
            app.into(),
            container_runtime_api::ContainerSpecSnapshot {
                env: Some(
                    [(
                        shared_types::APP_DEPLOY_GENERATION_ID.into(),
                        "existinggeneration".into(),
                    )]
                    .into(),
                ),
                ..Default::default()
            },
        );
        let guard = service.try_acquire_process_release_lock(app).await.unwrap();
        let input = shared_types::UserAppExecutionInput::new("{}".into());
        let mut operation = crate::service::OwnedOperation::admit_with_input(
            service.metadata.store.clone(),
            shared_types::UserAppAdmission {
                app_id: app.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                operation_id: "explicitunknownop".into(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::StartDeployment,
                command: Some(shared_types::UserAppControlCommand::Deploy {
                    restart: false,
                    input_digest: input.digest(),
                }),
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await
        .unwrap();
        operation.bind_lease(&guard).await.unwrap();
        let context = operation.execution_context();
        service
            .metadata
            .store
            .bind_operation_deadline(
                app,
                &context.operation_id,
                &context.lifecycle_id,
                chrono::Utc::now().timestamp_millis() + 3000,
            )
            .await
            .unwrap();
        for (stdout, exit_code) in [("administrator", 0), ("1", 0), ("1", 0), ("", 1)] {
            runtime.configuration_replies.lock().unwrap().push_back(
                container_runtime_api::ExecResult {
                    stdout: stdout.into(),
                    exit_code,
                    stderr: String::new(),
                },
            );
        }
        let error = service
            .apply_explicit_deployment_credentials(
                &mut operation,
                &guard,
                &StartPgCredential {
                    username: "business".into(),
                    password: "newprivate".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                AppOperationError::CredentialApplication {
                    mutation: CredentialMutationEvidence::Unknown,
                    ..
                }
            ),
            "got: {error:?}"
        );
        assert!(error.requires_recovery());
        let record = service
            .metadata
            .store
            .get_operation(app, &context.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.checkpoint["stage"], "write_submitted");
        assert!(record.checkpoint["explicit_pg_target"].is_object());
        assert!(!record.checkpoint.to_string().contains("newprivate"));
    }

    /// 反例：回执查询缺失（exit 0 但无 committed 行）不能当成功，仍属 Unknown。
    #[tokio::test]
    async fn explicit_deployment_missing_receipt_stays_unknown() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (mut service, _store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
        let app = "explicitreceipt";
        let identity = service.metadata.store.ensure_identity(app).await.unwrap();
        runtime.specs.insert(
            app.into(),
            container_runtime_api::ContainerSpecSnapshot {
                env: Some(
                    [(
                        shared_types::APP_DEPLOY_GENERATION_ID.into(),
                        "existinggeneration".into(),
                    )]
                    .into(),
                ),
                ..Default::default()
            },
        );
        let guard = service.try_acquire_process_release_lock(app).await.unwrap();
        let input = shared_types::UserAppExecutionInput::new("{}".into());
        let mut operation = crate::service::OwnedOperation::admit_with_input(
            service.metadata.store.clone(),
            shared_types::UserAppAdmission {
                app_id: app.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                operation_id: "explicitreceiptop".into(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::StartDeployment,
                command: Some(shared_types::UserAppControlCommand::Deploy {
                    restart: false,
                    input_digest: input.digest(),
                }),
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await
        .unwrap();
        operation.bind_lease(&guard).await.unwrap();
        let context = operation.execution_context();
        service
            .metadata
            .store
            .bind_operation_deadline(
                app,
                &context.operation_id,
                &context.lifecycle_id,
                chrono::Utc::now().timestamp_millis() + 3000,
            )
            .await
            .unwrap();
        for (stdout, exit_code) in [("administrator", 0), ("1", 0), ("1", 0), ("", 0), ("", 0)] {
            runtime.configuration_replies.lock().unwrap().push_back(
                container_runtime_api::ExecResult {
                    stdout: stdout.into(),
                    exit_code,
                    stderr: String::new(),
                },
            );
        }
        let error = service
            .apply_explicit_deployment_credentials(
                &mut operation,
                &guard,
                &StartPgCredential {
                    username: "business".into(),
                    password: "newprivate".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                AppOperationError::CredentialApplication {
                    mutation: CredentialMutationEvidence::Unknown,
                    ..
                }
            ),
            "got: {error:?}"
        );
        let record = service
            .metadata
            .store
            .get_operation(app, &context.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.checkpoint["stage"], "write_submitted");
    }

    /// 反例：回执 committed 但 TCP 复验失败 → AppliedButUnverified（仍需恢复）。
    #[tokio::test]
    async fn explicit_deployment_tcp_failure_is_applied_but_unverified() {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (mut service, _store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
        let app = "explicitunverified";
        let identity = service.metadata.store.ensure_identity(app).await.unwrap();
        runtime.specs.insert(
            app.into(),
            container_runtime_api::ContainerSpecSnapshot {
                env: Some(
                    [(
                        shared_types::APP_DEPLOY_GENERATION_ID.into(),
                        "existinggeneration".into(),
                    )]
                    .into(),
                ),
                ..Default::default()
            },
        );
        let guard = service.try_acquire_process_release_lock(app).await.unwrap();
        let input = shared_types::UserAppExecutionInput::new("{}".into());
        let mut operation = crate::service::OwnedOperation::admit_with_input(
            service.metadata.store.clone(),
            shared_types::UserAppAdmission {
                app_id: app.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                operation_id: "explicitunverifiedop".into(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::StartDeployment,
                command: Some(shared_types::UserAppControlCommand::Deploy {
                    restart: false,
                    input_digest: input.digest(),
                }),
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await
        .unwrap();
        operation.bind_lease(&guard).await.unwrap();
        let context = operation.execution_context();
        service
            .metadata
            .store
            .bind_operation_deadline(
                app,
                &context.operation_id,
                &context.lifecycle_id,
                chrono::Utc::now().timestamp_millis() + 3000,
            )
            .await
            .unwrap();
        for (stdout, exit_code) in [
            ("administrator", 0),
            ("1", 0),
            ("1", 0),
            ("", 0),
            ("1", 0),
            ("", 2),
        ] {
            runtime.configuration_replies.lock().unwrap().push_back(
                container_runtime_api::ExecResult {
                    stdout: stdout.into(),
                    exit_code,
                    stderr: String::new(),
                },
            );
        }
        let error = service
            .apply_explicit_deployment_credentials(
                &mut operation,
                &guard,
                &StartPgCredential {
                    username: "business".into(),
                    password: "newprivate".into(),
                },
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                &error,
                AppOperationError::CredentialApplication {
                    mutation: CredentialMutationEvidence::AppliedButUnverified,
                    ..
                }
            ),
            "got: {error:?}"
        );
        assert!(error.requires_recovery());
        let record = service
            .metadata
            .store
            .get_operation(app, &context.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(record.checkpoint["stage"], "write_submitted");
    }
}
