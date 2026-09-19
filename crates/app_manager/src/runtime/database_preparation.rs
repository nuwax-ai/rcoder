//! A durable management-only wake. It neither observes business Ready nor applies
//! pending credentials. The enclosing detached coordinator owns its deadline.
use crate::{
    models::*,
    service::{AppOperationGuard, AppService, OwnedOperation},
    utils::map_runtime_error,
};
use container_runtime_api::ContainerRuntimeError;
use shared_types::*;
use std::time::Duration;
use tokio::time::{Instant, sleep_until, timeout_at};

fn expired() -> AppOperationError {
    AppOperationError::Backend(
        "Database management preparation deadline exceeded; inspect original operation".into(),
    )
}

impl AppService {
    /// request_id is a deterministic child identity of the caller's original
    /// request. The caller must not allocate a new identity to retry uncertainty.
    pub async fn prepare_prod_database(
        &self,
        app_id: &str,
        lifecycle_id: &str,
        request_id: &str,
        fingerprint: &str,
        deadline: Instant,
    ) -> AppResult<()> {
        for (value, name) in [
            (app_id, "app_id"),
            (lifecycle_id, "lifecycle_id"),
            (request_id, "request_id"),
        ] {
            validate_identifier(value, name).map_err(AppOperationError::Validation)?;
        }
        let previous = timeout_at(deadline, async {
            self.metadata
                .validate_request_lifecycle(app_id, Some(lifecycle_id))
                .await?;
            self.metadata
                .store
                .get_operation_by_request(app_id, request_id)
                .await
                .map_err(AppOperationError::from)
        })
        .await
        .map_err(|_| expired())??;
        if let Some(previous) = previous {
            if previous.lifecycle_id != lifecycle_id
                || previous.kind != UserAppOperationKind::PrepareProdDatabase
                || previous.request_fingerprint != fingerprint
            {
                return Err(AppOperationError::Conflict(
                    "Database preparation request identity differs".into(),
                ));
            }
            return match previous.state {
                UserAppOperationState::Succeeded
                    if userapp_operation_has_final_evidence(&previous) =>
                {
                    Ok(())
                }
                UserAppOperationState::Failed => Err(AppOperationError::Backend(
                    "Original database preparation failed".into(),
                )),
                _ => Err(AppOperationError::ConflictBlocked {
                    message: "Inspect the original database preparation before retrying".into(),
                    blocker: previous.blocker(),
                }),
            };
        }
        let guard = timeout_at(deadline, self.try_acquire_process_release_lock(app_id))
            .await
            .map_err(|_| expired())??;
        // A cancelled admission observer may leave its transaction committing.
        // Fence the lease before admission; release only after a known outcome.
        guard.mark_mutating()?;
        let admitted = timeout_at(
            deadline,
            self.metadata.store.admit(&UserAppAdmission {
                app_id: app_id.into(),
                lifecycle_id: Some(lifecycle_id.into()),
                operation_id: request_id.into(),
                request_id: Some(request_id.into()),
                request_fingerprint: fingerprint.into(),
                kind: UserAppOperationKind::PrepareProdDatabase,
                command: Some(UserAppControlCommand::PrepareProdDatabase),
                metadata: None,
                runtime_policy_on_success: None,
            }),
        )
        .await
        .map_err(|_| expired())?;
        let record = match admitted {
            Ok(UserAppAdmissionOutcome::Accepted(record)) => record,
            Ok(UserAppAdmissionOutcome::Existing(record)) => {
                // Another replica may have admitted between the preflight read
                // and lease acquisition. Do not claim or replay its execution.
                let error = AppOperationError::ConflictBlocked {
                    message: "Database preparation was already admitted".into(),
                    blocker: record.blocker(),
                };
                guard.mark_completed();
                timeout_at(deadline, guard.finish())
                    .await
                    .map_err(|_| expired())??;
                return Err(error);
            }
            Err(error) => {
                // Only a definitive admission rejection permits release. Once
                // admitted, a failed claim is handled separately and stays fenced.
                if !matches!(error, UserAppStoreError::Storage(_)) {
                    guard.mark_completed();
                    timeout_at(deadline, guard.finish())
                        .await
                        .map_err(|_| expired())??;
                }
                return Err(error.into());
            }
        };
        let operation = timeout_at(
            deadline,
            OwnedOperation::claim_pending(self.metadata.store.clone(), record),
        )
        .await
        .map_err(|_| expired())??
        .ok_or_else(|| AppOperationError::Conflict("Database preparation claim changed".into()))?;
        self.execute_database_preparation(operation, guard, deadline)
            .await
    }

    pub(crate) async fn execute_database_preparation(
        &self,
        mut operation: OwnedOperation,
        guard: AppOperationGuard,
        deadline: Instant,
    ) -> AppResult<()> {
        guard.mark_mutating()?;
        let context = operation.execution_context();
        let app_id = context.app_id.as_str();
        let mut submitted = false;
        let result = timeout_at(deadline, async {
            operation.bind_lease(&guard).await?;
            let spec = self
                .runtime
                .get_app_container_spec(app_id)
                .await
                .map_err(|error| map_runtime_error("Read management generation", error))?;
            let generation = spec
                .env
                .as_ref()
                .and_then(|env| env.get(APP_DEPLOY_GENERATION_ID))
                .filter(|value| !value.is_empty())
                .cloned()
                .ok_or_else(|| {
                    AppOperationError::InvalidState("Management generation is missing".into())
                })?;
            let target = self
                .runtime
                .capture_app_mutation_target(&context, None)
                .await
                .map_err(|error| map_runtime_error("Capture management workload", error))?;
            let mut evidence = DatabasePreparationEvidence {
                target,
                deployment_generation: generation,
                stage: DatabasePreparationStage::Captured,
                management: None,
            };
            operation
                .checkpoint(
                    "database_management",
                    serde_json::to_value(&evidence).map_err(|_| {
                        AppOperationError::Backend("Encode management evidence".into())
                    })?,
                )
                .await?;
            submitted = true; // before durable intent: its commit can become unknown
            evidence.stage = DatabasePreparationStage::StartSubmitted;
            operation
                .checkpoint(
                    "database_management",
                    serde_json::to_value(&evidence).map_err(|_| {
                        AppOperationError::Backend("Encode management intent".into())
                    })?,
                )
                .await?;
            self.runtime
                .start_app_management_target(&evidence.target)
                .await
                .map_err(|error| map_runtime_error("Start captured management workload", error))?;
            let management = loop {
                if Instant::now() >= deadline {
                    return Err(expired());
                }
                match self
                    .runtime
                    .capture_app_configuration_target(&context, &evidence.deployment_generation)
                    .await
                {
                    Ok(target) => break target,
                    Err(ContainerRuntimeError::ManagementNotRunning) => {
                        sleep_until(std::cmp::min(
                            deadline,
                            Instant::now() + Duration::from_millis(500),
                        ))
                        .await;
                    }
                    Err(error) => {
                        return Err(map_runtime_error(
                            "Capture running management target",
                            error,
                        ));
                    }
                }
            };
            // This channel is physical UID/generation bound and does not pass
            // through a Service that removes NotReady Pods from its endpoints.
            loop {
                if Instant::now() >= deadline {
                    return Err(expired());
                }
                let marker = self
                    .runtime
                    .exec_app_configuration_target(
                        &context,
                        &management,
                        vec![
                            "sh".into(),
                            "-c".into(),
                            "test -n \"${PGDATA:-}\" && cat \"$PGDATA/.rcoder-admin-user\"".into(),
                        ],
                    )
                    .await
                    .map_err(|error| map_runtime_error("Read management administrator", error))?;
                if marker.exit_code == 0 {
                    let admin = pg_utils::PgAdministrationTarget::new(
                        marker.stdout.trim().into(),
                        "/var/run/postgresql".into(),
                    )
                    .map_err(AppOperationError::Validation)?;
                    let ready = self
                        .runtime
                        .exec_app_configuration_target(
                            &context,
                            &management,
                            vec!["sh".into(), "-c".into(), admin.ready_command()],
                        )
                        .await
                        .map_err(|error| {
                            map_runtime_error("Observe management PostgreSQL", error)
                        })?;
                    if ready.exit_code == 0 {
                        break;
                    }
                }
                sleep_until(std::cmp::min(
                    deadline,
                    Instant::now() + Duration::from_millis(500),
                ))
                .await;
            }
            evidence.stage = DatabasePreparationStage::ManagementReady;
            evidence.management = Some(management);
            operation
                .checkpoint(
                    "database_management",
                    serde_json::to_value(&evidence).map_err(|_| {
                        AppOperationError::Backend("Encode management readiness".into())
                    })?,
                )
                .await
        })
        .await
        .unwrap_or_else(|_| Err(expired()));
        let settlement = Instant::now() + Duration::from_secs(5);
        match result {
            Ok(()) => {
                timeout_at(settlement, operation.succeed())
                    .await
                    .map_err(|_| expired())??;
                guard.mark_completed();
                timeout_at(settlement, guard.finish())
                    .await
                    .map_err(|_| expired())?
            }
            Err(error) => {
                if submitted {
                    timeout_at(settlement, operation.fail(&error))
                        .await
                        .map_err(|_| expired())??;
                    // Uncertain remote startup keeps its original lease/identity.
                } else {
                    timeout_at(settlement, operation.reject_without_mutation(&error))
                        .await
                        .map_err(|_| expired())??;
                    guard.mark_completed();
                    timeout_at(settlement, guard.finish())
                        .await
                        .map_err(|_| expired())??;
                }
                Err(error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockRuntime, test_service};
    use std::sync::{Arc, atomic::Ordering};

    async fn fixture() -> (
        tempfile::TempDir,
        AppService,
        Arc<MockRuntime>,
        UserAppLifecycleRecord,
    ) {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let mut service = test_service(directory.path(), runtime.clone()).await;
        service.config.access_mode = crate::config::AppAccessMode::Kubernetes;
        let app = service
            .metadata
            .store
            .ensure_identity("managementapp")
            .await
            .unwrap();
        runtime.deployments.insert(
            app.app_id.clone(),
            container_runtime_api::DeploymentStatus {
                app_id: app.app_id.clone(),
                phase: "Stopped".into(),
                resource_version: Some("versionone".into()),
                ..Default::default()
            },
        );
        runtime.specs.insert(
            app.app_id.clone(),
            container_runtime_api::ContainerSpecSnapshot {
                env: Some(
                    [(APP_DEPLOY_GENERATION_ID.into(), "generationone".into())]
                        .into_iter()
                        .collect(),
                ),
                ..Default::default()
            },
        );
        (directory, service, runtime, app)
    }

    #[tokio::test]
    async fn database_preparation_pending_recovery_keeps_original_identity() {
        let (_directory, service, runtime, app) = fixture().await;
        let pending = match service
            .metadata
            .store
            .admit(&UserAppAdmission {
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: "pendingmanagement".into(),
                request_id: Some("pendingrequest".into()),
                request_fingerprint: "d".repeat(64),
                kind: UserAppOperationKind::PrepareProdDatabase,
                command: Some(UserAppControlCommand::PrepareProdDatabase),
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap()
        {
            UserAppAdmissionOutcome::Accepted(operation) => operation,
            _ => panic!("new admission required"),
        };
        runtime.configuration_replies.lock().unwrap().extend([
            container_runtime_api::ExecResult {
                exit_code: 0,
                stdout: "originaladmin\n".into(),
                stderr: String::new(),
            },
            container_runtime_api::ExecResult {
                exit_code: 0,
                stdout: "1\n".into(),
                stderr: String::new(),
            },
        ]);
        assert!(service.resume_pending_control(&pending).await.unwrap());
        let finished = service
            .metadata
            .store
            .get_operation_by_request(&app.app_id, "pendingrequest")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(finished.operation_id, "pendingmanagement");
        assert_eq!(finished.state, UserAppOperationState::Succeeded);
        assert!(userapp_operation_has_final_evidence(&finished));
        assert!(!runtime.lease_held.load(Ordering::SeqCst));
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 1);
        assert!(!service.resume_pending_control(&pending).await.unwrap());
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn database_preparation_completes_with_failed_business_and_replays_without_start() {
        let (_directory, service, runtime, app) = fixture().await;
        runtime.crash_on_start.store(true, Ordering::SeqCst);
        service
            .runtime_configuration
            .save_runtime_configuration(
                &app.app_id,
                UserAppOperationScope::Prod,
                &SaveRuntimeConfigurationRequest {
                    lifecycle_id: app.lifecycle_id.clone(),
                    request_id: "pendingconfig".into(),
                    expected_revision: 0,
                    pg: StartPgCredential {
                        username: "nextbusiness".into(),
                        password: "pendingsecret".into(),
                    },
                },
            )
            .await
            .unwrap();
        runtime.configuration_replies.lock().unwrap().extend([
            container_runtime_api::ExecResult {
                exit_code: 0,
                stdout: "originaladmin\n".into(),
                stderr: String::new(),
            },
            container_runtime_api::ExecResult {
                exit_code: 0,
                stdout: "1\n".into(),
                stderr: String::new(),
            },
        ]);
        let fingerprint = "b".repeat(64);
        service
            .prepare_prod_database(
                &app.app_id,
                &app.lifecycle_id,
                "managementrequest",
                &fingerprint,
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
        let operation = service
            .metadata
            .store
            .get_operation_by_request(&app.app_id, "managementrequest")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.state, UserAppOperationState::Succeeded);
        assert!(userapp_operation_has_final_evidence(&operation));
        assert_eq!(runtime.deployments.get(&app.app_id).unwrap().phase, "Error");
        assert!(!runtime.lease_held.load(Ordering::SeqCst));
        let current = service
            .metadata
            .store
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.runtime_policy, app.runtime_policy);
        let config = service
            .runtime_configuration
            .runtime_configuration_status(
                &app.app_id,
                &app.lifecycle_id,
                UserAppOperationScope::Prod,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(config.applied_version, None);
        assert_eq!(config.saved_version, 1);
        service
            .prepare_prod_database(
                &app.app_id,
                &app.lifecycle_id,
                "managementrequest",
                &fingerprint,
                Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap();
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.configuration_commands.lock().unwrap().len(), 2);
        assert!(
            service
                .prepare_prod_database(
                    &app.app_id,
                    &app.lifecycle_id,
                    "managementrequest",
                    &"c".repeat(64),
                    Instant::now() + Duration::from_secs(5)
                )
                .await
                .is_err()
        );
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn database_preparation_failure_after_start_keeps_operation_and_lease() {
        let (_directory, service, runtime, app) = fixture().await;
        // No scripted management reply: physical start succeeds, subsequent
        // bound observation fails. That is not proof that startup was undone.
        let result = service
            .prepare_prod_database(
                &app.app_id,
                &app.lifecycle_id,
                "uncertainrequest",
                &"a".repeat(64),
                Instant::now() + Duration::from_secs(5),
            )
            .await;
        assert!(result.is_err());
        let operation = service
            .metadata
            .store
            .get_operation_by_request(&app.app_id, "uncertainrequest")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(operation.state, UserAppOperationState::RecoveryRequired);
        assert!(runtime.lease_held.load(Ordering::SeqCst));
        let current = service
            .metadata
            .store
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            current.active_operations.prod.as_deref(),
            Some("uncertainrequest")
        );
        let result = service
            .prepare_prod_database(
                &app.app_id,
                &app.lifecycle_id,
                "uncertainrequest",
                &"a".repeat(64),
                Instant::now() + Duration::from_secs(5),
            )
            .await;
        assert!(
            matches!(result, Err(AppOperationError::ConflictBlocked { blocker, .. })
            if blocker.operation_id == "uncertainrequest")
        );
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.management_start_calls.load(Ordering::SeqCst), 1);
    }
}
