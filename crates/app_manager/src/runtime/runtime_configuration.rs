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
    pub(crate) async fn apply_explicit_deployment_credentials(
        &self,
        operation: &mut crate::service::OwnedOperation,
        guard: &crate::service::AppOperationGuard,
        pg: &StartPgCredential,
    ) -> AppResult<()> {
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
            let evidence = serde_json::json!({"explicit_pg_target":target});
            guard.mark_mutating()?;
            operation
                .checkpoint("explicit_pg_applying", evidence.clone())
                .await?;
            let runner = BoundManagementRunner {
                service: self,
                context: &context,
                target: &target,
            };
            shared_types::align_pg_credentials_with_admin(
                &runner,
                &admin,
                &pg.username,
                &pg.password,
            )
            .await
            .map_err(|error| AppOperationError::credential_failure(error, &pg.password))?;
            operation
                .checkpoint("explicit_pg_applied", evidence)
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
            ("", 1),
            ("t", 0),
            ("", 0),
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
}
