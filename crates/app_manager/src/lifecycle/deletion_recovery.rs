//! Retire interrupted deletion writes, never replay an unknown purge.
use crate::models::{AppOperationError, AppResult};
use crate::service::AppService;
use crate::utils::map_runtime_error;
use shared_types::{
    DeletionInspection, UserAppDeletionCheckpoint, UserAppOperationKind, UserAppOperationRecord,
    UserAppOperationState, UserAppStoreError,
};

impl AppService {
    /// Shared by the scanner and explicit recover. The lease probe only avoids
    /// interrupting a live deletion; loss of authority is not completion evidence.
    /// Every captured resource writer is inspected after the executor's CAS revoke.
    pub async fn reconcile_interrupted_deletion(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> AppResult<bool> {
        if !matches!(
            snapshot.kind,
            UserAppOperationKind::DeleteCompute
                | UserAppOperationKind::PurgeResources
                | UserAppOperationKind::DeleteApplication
        ) || !matches!(
            snapshot.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        ) || shared_types::userapp_operation_has_final_evidence(snapshot)
        {
            return Ok(false);
        }
        let checkpoint: UserAppDeletionCheckpoint =
            serde_json::from_value(snapshot.checkpoint.clone()).map_err(|error| {
                AppOperationError::Conflict(format!(
                    "Original deletion targets are unavailable: {error}"
                ))
            })?;
        checkpoint
            .validate_operation(snapshot)
            .map_err(AppOperationError::Conflict)?;
        let binding = self
            .metadata
            .store
            .get_operation_lease(&snapshot.app_id, &snapshot.operation_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::Conflict(
                    "Interrupted deletion has no original physical lease receipt".into(),
                )
            })?;
        if binding.context != checkpoint.context {
            return Err(AppOperationError::Conflict(
                "Deletion lease does not belong to the captured executor".into(),
            ));
        }
        if snapshot.state == UserAppOperationState::Running
            && !self
                .runtime
                .app_operation_receipt_holder_dead(&binding.context, &binding.receipt)
                .await
                .map_err(|error| map_runtime_error("Inspect active deletion holder", error))?
        {
            return Ok(false);
        }
        let reserved = match self
            .metadata
            .store
            .reserve_interrupted_deletion(snapshot)
            .await
        {
            Ok(operation) => operation,
            Err(UserAppStoreError::VersionConflict | UserAppStoreError::LifecycleConflict) => {
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        let inspection = self
            .runtime
            .inspect_app_deletion(
                &binding.context,
                Some(&binding.receipt),
                &checkpoint.production,
            )
            .await;
        let production_remaining = match inspection {
            Ok(DeletionInspection::ConfirmedQuiescent {
                remaining_resources,
            }) => remaining_resources,
            Ok(other) => {
                return self.record_deletion_inspection(&reserved, other).await;
            }
            Err(error) => {
                return self
                    .record_deletion_inspection(
                        &reserved,
                        DeletionInspection::Unknown(format!(
                            "Production deletion observation failed: {error}"
                        )),
                    )
                    .await;
            }
        };
        let mut development_remaining = Vec::new();
        if let Some(receipt) = &checkpoint.development {
            let cleanup = self
                .dev_cleanup
                .read()
                .map_err(|_| {
                    AppOperationError::Backend("Development cleanup registry poisoned".into())
                })?
                .clone();
            let Some(cleanup) = cleanup else {
                return self
                    .record_deletion_inspection(
                        &reserved,
                        DeletionInspection::Unknown(
                            "Captured development deletion inspection is not configured".into(),
                        ),
                    )
                    .await;
            };
            match cleanup.inspect_captured(&binding.context, receipt).await {
                Ok(DeletionInspection::ConfirmedQuiescent {
                    remaining_resources,
                }) => {
                    development_remaining = remaining_resources;
                }
                Ok(other) => return self.record_deletion_inspection(&reserved, other).await,
                Err(error) => {
                    return self
                        .record_deletion_inspection(
                            &reserved,
                            DeletionInspection::Unknown(format!(
                                "Development deletion observation failed: {error}"
                            )),
                        )
                        .await;
                }
            }
        }
        // Retiring writes does not cancel a lifecycle deletion intent. In
        // particular, releasing an Application slot must never revive its tombstone.
        if snapshot.kind == UserAppOperationKind::DeleteApplication {
            return self
                .record_deletion_inspection(
                    &reserved,
                    DeletionInspection::Unknown(
                        "Original deletion writes are retired; lifecycle deletion intent remains protected and requires explicit completion".into(),
                    ),
                )
                .await;
        }
        if let Err(error) = self
            .runtime
            .release_app_operation_receipt(&binding.context, &binding.receipt)
            .await
        {
            return self
                .record_deletion_inspection(
                    &reserved,
                    DeletionInspection::Unknown(format!(
                        "Original deletion lease cleanup is unconfirmed: {error}"
                    )),
                )
                .await;
        }
        let evidence = serde_json::json!({
            "execution_quiescent": true,
            "context": binding.context,
            "captured_checkpoint": reserved.checkpoint,
            "production_remaining": production_remaining,
            "development_remaining": development_remaining,
        });
        match self
            .metadata
            .store
            .finalize_interrupted_deletion(&reserved, &evidence)
            .await
        {
            Ok(_) => {
                self.metadata.store.forget_operation_lease(&binding).await?;
                self.invalidate_deploy_cache().await;
                Ok(true)
            }
            Err(UserAppStoreError::VersionConflict | UserAppStoreError::LifecycleConflict) => {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn record_deletion_inspection(
        &self,
        snapshot: &UserAppOperationRecord,
        inspection: DeletionInspection,
    ) -> AppResult<bool> {
        let message = match inspection {
            DeletionInspection::StillHeld(reason) => {
                format!("Deletion execution still held: {reason}")
            }
            DeletionInspection::ForeignIdentity(reason) => {
                format!("Deletion identity changed: {reason}")
            }
            DeletionInspection::Unknown(reason) => {
                format!("Deletion inspection required: {reason}")
            }
            DeletionInspection::ConfirmedQuiescent { .. } => {
                return Err(AppOperationError::Backend(
                    "Completed inspection is not a recovery problem".into(),
                ));
            }
        };
        match self
            .metadata
            .store
            .record_deletion_recovery_problem(snapshot, &message)
            .await
        {
            Ok(_) => Ok(true),
            Err(UserAppStoreError::VersionConflict | UserAppStoreError::LifecycleConflict) => {
                Ok(false)
            }
            Err(error) => Err(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockRuntime, test_service};
    use container_runtime_api::UserAppDeploymentRuntime as _;
    use shared_types::{UserAppAdmission, UserAppAdmissionOutcome};
    use std::sync::{Arc, atomic::Ordering};

    #[tokio::test]
    async fn interrupted_deletion_requires_all_evidence_then_releases_without_replaying_purge() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(directory.path(), runtime.clone()).await;
        let store = &service.metadata.store;
        let admission = UserAppAdmission {
            app_id: "interrupteddelete".into(),
            lifecycle_id: None,
            operation_id: "interrupted-delete-operation".into(),
            request_id: Some("original-request".into()),
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::DeleteCompute,
            command: Some(shared_types::UserAppControlCommand::DeleteResources {
                purge: false,
                expected_resource_version: None,
            }),
            metadata: None,
            runtime_policy_on_success: None,
        };
        let pending = match store.admit(&admission).await.unwrap() {
            UserAppAdmissionOutcome::Accepted(operation) => operation,
            UserAppAdmissionOutcome::Existing(_) => panic!("new operation expected"),
        };
        let running = store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: pending.app_id.clone(),
                lifecycle_id: pending.lifecycle_id.clone(),
                operation_id: pending.operation_id.clone(),
                expected_revision: pending.revision,
                executor_id: "original-executor".into(),
                state: UserAppOperationState::Running,
                step: "claimed".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        let context = shared_types::UserAppExecutionContext {
            app_id: running.app_id.clone(),
            lifecycle_id: running.lifecycle_id.clone(),
            operation_id: running.operation_id.clone(),
            executor_id: running.executor_id.clone().unwrap(),
            request_fingerprint: running.request_fingerprint.clone(),
        };
        let lease = runtime
            .acquire_app_operation(&running.app_id)
            .await
            .unwrap()
            .unwrap();
        store
            .bind_operation_lease(&context, &lease.receipt().unwrap())
            .await
            .unwrap();
        let checkpoint = UserAppDeletionCheckpoint {
            stage: shared_types::UserAppDeletionStage::Captured,
            schema_version: 1,
            kind: running.kind,
            context: context.clone(),
            production: shared_types::AppDeletionSnapshot {
                app_id: running.app_id.clone(),
                operation_id: "captured-runtime-operation".into(),
                resources: vec![],
                directories: None,
            },
            development: None,
        };
        let captured = store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: running.app_id.clone(),
                lifecycle_id: running.lifecycle_id.clone(),
                operation_id: running.operation_id.clone(),
                expected_revision: running.revision,
                executor_id: context.executor_id.clone(),
                state: UserAppOperationState::Running,
                step: "deleting_resources".into(),
                checkpoint: serde_json::to_value(checkpoint).unwrap(),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        assert!(
            !service
                .reconcile_interrupted_deletion(&captured)
                .await
                .unwrap()
        );
        assert_eq!(
            store
                .get_operation(&captured.app_id, &captured.operation_id)
                .await
                .unwrap(),
            Some(captured.clone()),
            "an active user's deletion must not be interrupted by the scanner"
        );
        assert_eq!(runtime.deletion_inspection_calls.load(Ordering::SeqCst), 0);
        // A dead mutex holder alone does not prove its submitted DELETE ended.
        runtime.lease_held.store(false, Ordering::SeqCst);
        *runtime.deletion_inspection.lock().unwrap() = Some(DeletionInspection::Unknown(
            "DELETE reply lost; resource is terminating".into(),
        ));
        assert!(
            service
                .reconcile_interrupted_deletion(&captured)
                .await
                .unwrap()
        );
        let recovery = store
            .get_operation(&captured.app_id, &captured.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(recovery.state, UserAppOperationState::RecoveryRequired);
        assert_eq!(recovery.checkpoint, captured.checkpoint);
        assert!(
            recovery
                .error_message
                .as_deref()
                .unwrap()
                .contains("terminating")
        );
        assert!(store.check_business_execution(&context).await.is_err());
        let mut successor = admission.clone();
        successor.kind = UserAppOperationKind::Start;
        successor.command = None;
        successor.operation_id = "successor-start".into();
        successor.request_id = Some("successor-request".into());
        assert!(store.admit(&successor).await.is_err());
        *runtime.deletion_inspection.lock().unwrap() =
            Some(DeletionInspection::ConfirmedQuiescent {
                remaining_resources: vec![],
            });
        assert!(
            service
                .reconcile_interrupted_deletion(&recovery)
                .await
                .unwrap()
        );
        let terminal = store
            .get_operation(&captured.app_id, &captured.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(terminal.state, UserAppOperationState::Failed);
        assert_eq!(terminal.checkpoint["stage"], captured.checkpoint["stage"]);
        assert!(
            store
                .get_operation_lease(&captured.app_id, &captured.operation_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.admit(&successor).await.is_ok());
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime.destroy_pvc_calls.load(Ordering::SeqCst),
            0,
            "recovery may retire writes but cannot execute the unknown purge"
        );
        drop(lease);
    }
}
