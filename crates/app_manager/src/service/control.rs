//! Durable admission and execution claims. No SQL transaction spans runtime I/O.
use crate::models::{AppOperationError, AppResult};
use shared_types::{
    UserAppAdmission, UserAppAdmissionOutcome, UserAppLifecycleStore, UserAppOperationProgress,
    UserAppOperationRecord, UserAppOperationState,
};
use std::sync::Arc;

pub(crate) struct OwnedOperation {
    store: Arc<dyn UserAppLifecycleStore>,
    record: UserAppOperationRecord,
    executor: String,
}

impl OwnedOperation {
    /// Claim an existing unexecuted command; never mint a replacement operation.
    /// The caller already holds the application resource operation guard.
    pub(crate) async fn claim_pending(
        store: Arc<dyn UserAppLifecycleStore>,
        record: UserAppOperationRecord,
    ) -> AppResult<Option<Self>> {
        if record.state != UserAppOperationState::Pending
            || record.step != "admitted"
            || record.executor_id.is_some()
            || !record.checkpoint.is_null()
            || record
                .command
                .as_ref()
                .is_none_or(|command| command.kind() != record.kind)
        {
            return Ok(None);
        }
        let executor = uuid::Uuid::new_v4().to_string();
        let progress = UserAppOperationProgress {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            operation_id: record.operation_id.clone(),
            expected_revision: record.revision,
            executor_id: executor.clone(),
            state: UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        };
        let record = match store.advance(&progress).await {
            Ok(record) => record,
            Err(shared_types::UserAppStoreError::VersionConflict) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some(Self {
            store,
            record,
            executor,
        }))
    }

    pub(crate) async fn admit(
        store: Arc<dyn UserAppLifecycleStore>,
        request: UserAppAdmission,
    ) -> AppResult<Self> {
        Self::admit_with_input(store, request, None).await
    }

    pub(crate) async fn admit_with_input(
        store: Arc<dyn UserAppLifecycleStore>,
        request: UserAppAdmission,
        input: Option<&shared_types::UserAppExecutionInput>,
    ) -> AppResult<Self> {
        let record = match store.admit_with_input(&request, input).await? {
            UserAppAdmissionOutcome::Accepted(record) => record,
            UserAppAdmissionOutcome::Existing(record) => {
                return Err(AppOperationError::Conflict(format!(
                    "Application operation {} already exists ({:?})",
                    record.operation_id, record.state
                )));
            }
        };
        let mut owned = Self {
            store,
            record,
            executor: uuid::Uuid::new_v4().to_string(),
        };
        owned
            .advance(
                UserAppOperationState::Running,
                "claimed",
                serde_json::Value::Null,
                None,
            )
            .await?;
        Ok(owned)
    }

    pub(crate) async fn execution_input(
        &self,
        owner: &str,
    ) -> AppResult<shared_types::UserAppExecutionInput> {
        Ok(self
            .store
            .read_execution_input(&self.execution_context(owner))
            .await?)
    }

    pub(crate) async fn bind_lease(
        &self,
        guard: &super::AppOperationGuard,
        owner: &str,
    ) -> AppResult<()> {
        self.store
            .bind_operation_lease(&self.execution_context(owner), &guard.lease_receipt()?)
            .await?;
        Ok(())
    }
    pub(crate) async fn confirm_effects(&mut self) -> AppResult<()> {
        self.checkpoint("control_confirmed", self.record.checkpoint.clone())
            .await
    }
    pub(crate) async fn checkpoint(
        &mut self,
        step: &str,
        evidence: serde_json::Value,
    ) -> AppResult<()> {
        self.advance(UserAppOperationState::Running, step, evidence, None)
            .await
    }
    /// Advance only one confirmed deletion boundary, keeping the original
    /// resource/context evidence unchanged. Publish the local stage after SQL.
    pub(crate) async fn deletion_progress(
        &mut self,
        evidence: &mut shared_types::UserAppDeletionCheckpoint,
        stage: shared_types::UserAppDeletionStage,
    ) -> AppResult<()> {
        use shared_types::UserAppDeletionStage as Stage;
        evidence
            .validate_operation(&self.record)
            .map_err(AppOperationError::Conflict)?;
        let persisted: shared_types::UserAppDeletionCheckpoint =
            serde_json::from_value(self.record.checkpoint.clone()).map_err(|error| {
                AppOperationError::Backend(format!("Read stored deletion checkpoint: {error}"))
            })?;
        if persisted != *evidence
            || !matches!(
                (evidence.stage, stage),
                (Stage::Captured, Stage::ComputeRemoved)
                    | (Stage::ComputeRemoved, Stage::ProductionStorageRemoved)
                    | (Stage::ProductionStorageRemoved, Stage::DevelopmentRemoved)
            )
        {
            return Err(AppOperationError::Conflict(
                "Deletion progress must preserve evidence and confirm the next stage".into(),
            ));
        }
        let mut next = evidence.clone();
        next.stage = stage;
        next.validate().map_err(AppOperationError::Conflict)?;
        let value = serde_json::to_value(&next).map_err(|error| {
            AppOperationError::Backend(format!("Encode deletion progress: {error}"))
        })?;
        self.checkpoint("deletion_progress", value).await?;
        *evidence = next;
        Ok(())
    }

    pub(crate) async fn succeed(mut self) -> AppResult<()> {
        self.advance(
            UserAppOperationState::Succeeded,
            "completed",
            self.record.checkpoint.clone(),
            None,
        )
        .await
    }
    pub(crate) async fn fail(mut self, error: &AppOperationError) -> AppResult<()> {
        let state = if self.record.step == "claimed" && self.record.checkpoint.is_null() {
            UserAppOperationState::Failed
        } else {
            UserAppOperationState::RecoveryRequired
        };
        let step = if state == UserAppOperationState::RecoveryRequired {
            self.record.step.clone()
        } else {
            "failed".into()
        };
        self.advance(state, &step, self.record.checkpoint.clone(), Some(error))
            .await
    }
    /// Only use when the runtime explicitly proves that no application mutation
    /// remains in flight. A timeout or transport failure is not such evidence.
    pub(crate) async fn reject_without_mutation(
        mut self,
        error: &AppOperationError,
    ) -> AppResult<()> {
        if self.record.kind.ends_lifecycle() {
            return Err(AppOperationError::Conflict(
                "Deletion requires checkpoint-specific failure handling".into(),
            ));
        }
        self.advance(
            UserAppOperationState::Failed,
            "rejected_without_mutation",
            self.record.checkpoint.clone(),
            Some(error),
        )
        .await
    }

    pub(crate) fn execution_context(&self, owner: &str) -> shared_types::UserAppExecutionContext {
        shared_types::UserAppExecutionContext {
            app_id: self.record.app_id.clone(),
            user_id: owner.into(),
            lifecycle_id: self.record.lifecycle_id.clone(),
            operation_id: self.record.operation_id.clone(),
            executor_id: self.executor.clone(),
            request_fingerprint: self.record.request_fingerprint.clone(),
        }
    }
    async fn advance(
        &mut self,
        state: UserAppOperationState,
        step: &str,
        checkpoint: serde_json::Value,
        error: Option<&AppOperationError>,
    ) -> AppResult<()> {
        self.record = self
            .store
            .advance(&UserAppOperationProgress {
                app_id: self.record.app_id.clone(),
                operation_id: self.record.operation_id.clone(),
                lifecycle_id: self.record.lifecycle_id.clone(),
                expected_revision: self.record.revision,
                executor_id: self.executor.clone(),
                state,
                step: step.into(),
                checkpoint,
                error_code: error.map(|e| e.code().into()),
                error_message: error.map(ToString::to_string),
            })
            .await?;
        Ok(())
    }
}

impl super::AppService {
    pub(crate) async fn replay_control(
        &self,
        app_id: &str,
        request: &shared_types::UserAppControlRequest,
        kind: shared_types::UserAppOperationKind,
        fingerprint: &str,
    ) -> AppResult<Option<UserAppOperationRecord>> {
        let Some(request_id) = request.request_id.as_deref() else {
            return Ok(None);
        };
        let app = self.get_lifecycle(app_id, &request.user_id).await?;
        let Some(operation) = self
            .metadata
            .store
            .get_operation_by_request(app_id, request_id)
            .await?
        else {
            return Ok(None);
        };
        if operation.lifecycle_id != app.lifecycle_id {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        if operation.kind != kind || operation.request_fingerprint != fingerprint {
            return Err(AppOperationError::Conflict(
                "Request identity was reused with different parameters".into(),
            ));
        }
        match operation.state {
            UserAppOperationState::Succeeded => Ok(Some(operation)),
            UserAppOperationState::Failed => Err(AppOperationError::Backend(format!(
                "Application operation {} failed: {}",
                operation.operation_id,
                operation
                    .error_message
                    .as_deref()
                    .unwrap_or("No failure details recorded")
            ))),
            _ => Err(AppOperationError::Conflict(format!(
                "Application operation {} is not complete ({:?})",
                operation.operation_id, operation.state
            ))),
        }
    }
    pub async fn get_lifecycle(
        &self,
        app_id: &str,
        user_id: &str,
    ) -> AppResult<shared_types::UserAppLifecycleRecord> {
        crate::utils::validate_app_id(app_id)?;
        let app = self
            .metadata
            .store
            .get_application(app_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
            })?;
        if app.user_id != user_id {
            return Err(shared_types::UserAppStoreError::OwnershipConflict.into());
        }
        Ok(app)
    }
    pub async fn get_control_operation(
        &self,
        app_id: &str,
        user_id: &str,
        operation_id: Option<&str>,
    ) -> AppResult<Option<shared_types::UserAppOperationView>> {
        let app = self.get_lifecycle(app_id, user_id).await?;
        let Some(id) = operation_id.or(app.current_operation_id.as_deref()) else {
            return Ok(None);
        };
        let operation = self
            .metadata
            .store
            .get_operation(app_id, id)
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("Application operation not found: {id}"))
            })?;
        if operation.lifecycle_id != app.lifecycle_id {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        Ok(Some(operation.into()))
    }
    pub async fn get_control_operation_by_request(
        &self,
        app_id: &str,
        user_id: &str,
        request_id: &str,
    ) -> AppResult<Option<shared_types::UserAppOperationView>> {
        let app = self.get_lifecycle(app_id, user_id).await?;
        let operation = self
            .metadata
            .store
            .get_operation_by_request(app_id, request_id)
            .await?;
        if operation
            .as_ref()
            .is_some_and(|operation| operation.lifecycle_id != app.lifecycle_id)
        {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        Ok(operation.map(Into::into))
    }

    pub async fn recreate_identity(
        &self,
        app_id: &str,
        request: shared_types::UserAppRecreateRequest,
    ) -> AppResult<shared_types::UserAppLifecycleRecord> {
        use garde::Validate as _;
        request
            .validate()
            .map_err(|error| AppOperationError::Validation(error.to_string()))?;
        let lease = self.acquire_process_release_lock(app_id).await?;
        let current = self.get_lifecycle(app_id, &request.user_id).await?;
        if current.state != shared_types::UserAppLifecycleState::Deleted {
            // On an active lifecycle the store can only return an exact prior
            // recreation result; it cannot create another generation here.
            let existing = self
                .metadata
                .store
                .recreate(
                    app_id,
                    &request.user_id,
                    &request.expected_lifecycle_id,
                    &request.request_id,
                )
                .await?;
            lease.finish().await?;
            return Ok(existing);
        }
        if self
            .runtime
            .get_deployment_status(app_id)
            .await
            .map_err(|error| crate::utils::map_runtime_error("verify recreation resources", error))?
            .is_some()
        {
            return Err(AppOperationError::Conflict(
                "Application compute resources remain; recovery is required".into(),
            ));
        }
        let locator = self
            .dev_locator
            .read()
            .map_err(|_| AppOperationError::Backend("Dev locator lock poisoned".into()))?
            .clone()
            .ok_or_else(|| AppOperationError::Backend("Dev locator is not configured".into()))?;
        if locator
            .dev_container_alive(app_id, None)
            .await
            .map_err(AppOperationError::Backend)?
        {
            return Err(AppOperationError::Conflict(
                "Application builder resources remain; recovery is required".into(),
            ));
        }
        let app = self
            .metadata
            .store
            .recreate(
                app_id,
                &request.user_id,
                &request.expected_lifecycle_id,
                &request.request_id,
            )
            .await?;
        lease.finish().await?;
        Ok(app)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{MockRuntime, test_service};
    struct NoBuilder;
    #[async_trait::async_trait]
    impl shared_types::UserappDevLocator for NoBuilder {
        async fn dev_file_server_addr(&self, _: &str, _: Option<&str>) -> Result<String, String> {
            Err("No builder".into())
        }
        async fn dev_container_alive(&self, _: &str, _: Option<&str>) -> Result<bool, String> {
            Ok(false)
        }
    }
    #[tokio::test]
    async fn operation_queries_validate_owner_and_hide_internal_checkpoints() {
        let directory = tempfile::tempdir().expect("directory");
        let service = test_service(directory.path(), Arc::new(MockRuntime::default())).await;
        let identity = service
            .metadata
            .store
            .ensure_identity("query", "owner")
            .await
            .expect("identity");
        let mut operation = OwnedOperation::admit(
            service.metadata.store.clone(),
            UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: "query".into(),
                user_id: "owner".into(),
                lifecycle_id: Some(identity.lifecycle_id),
                operation_id: "operation-query".into(),
                request_id: Some("request".into()),
                request_fingerprint: "fingerprint".into(),
                kind: shared_types::UserAppOperationKind::Start,
            },
        )
        .await
        .expect("admit");
        operation
            .checkpoint(
                "prepared",
                serde_json::json!({"private_reference":"not-public"}),
            )
            .await
            .expect("checkpoint");
        assert!(
            service
                .get_control_operation("query", "foreign", None)
                .await
                .is_err()
        );
        let public = service
            .get_control_operation("query", "owner", None)
            .await
            .expect("query")
            .expect("current");
        let wire = serde_json::to_value(public).expect("JSON");
        assert_eq!(wire["operation_id"], "operation-query");
        for internal in ["executor_id", "checkpoint", "request_fingerprint"] {
            assert!(wire.get(internal).is_none());
        }
        operation.succeed().await.expect("complete");
        assert!(
            service
                .get_control_operation("query", "owner", None)
                .await
                .expect("current")
                .is_none()
        );
        assert_eq!(
            service
                .get_control_operation("query", "owner", Some("operation-query"))
                .await
                .expect("history")
                .expect("record")
                .state,
            UserAppOperationState::Succeeded
        );
    }
    #[tokio::test]
    async fn recreation_requires_old_lifecycle_and_duplicate_returns_same_new_identity() {
        let directory = tempfile::tempdir().expect("directory");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(directory.path(), runtime.clone()).await;
        service
            .set_dev_locator(Arc::new(NoBuilder))
            .expect("locator");
        let old = service
            .metadata
            .store
            .ensure_identity("recreate", "owner")
            .await
            .expect("identity");
        let deletion = OwnedOperation::admit(
            service.metadata.store.clone(),
            UserAppAdmission {
                runtime_policy_on_success: None,
                command: None,
                metadata: None,
                app_id: "recreate".into(),
                user_id: "owner".into(),
                lifecycle_id: Some(old.lifecycle_id.clone()),
                operation_id: "delete".into(),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: shared_types::UserAppOperationKind::DeleteApplication,
            },
        )
        .await
        .expect("admit");
        crate::test_support::complete_empty_deletion_fixture(deletion, "owner").await;
        let request = shared_types::UserAppRecreateRequest {
            user_id: "owner".into(),
            expected_lifecycle_id: old.lifecycle_id.clone(),
            request_id: "recreate-request".into(),
        };
        let next = service
            .recreate_identity("recreate", request.clone())
            .await
            .expect("recreate");
        assert_ne!(next.lifecycle_id, old.lifecycle_id);
        assert_eq!(next.lifecycle_epoch, 2);
        for lifecycle_id in [None, Some(old.lifecycle_id.clone())] {
            let late = crate::models::StartAppRequest {
                user_id: "owner".into(),
                lifecycle_id,
                ..Default::default()
            };
            assert!(matches!(
                service.start_app_enhanced("recreate", late.clone()).await,
                Err(AppOperationError::Conflict(_))
            ));
            assert!(matches!(
                service.restart_app_enhanced("recreate", late).await,
                Err(AppOperationError::Conflict(_))
            ));
        }
        assert_eq!(
            runtime
                .create_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(
            runtime
                .delete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        service
            .metadata
            .validate_request_lifecycle("recreate", "owner", Some(&next.lifecycle_id))
            .await
            .expect("current lifecycle is accepted");
        assert!(
            service
                .metadata
                .validate_request_lifecycle("recreate", "foreign", Some(&next.lifecycle_id))
                .await
                .is_err()
        );
        runtime.deployments.insert(
            "recreate".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "recreate".into(),
                ..Default::default()
            },
        );
        let duplicate = service
            .recreate_identity("recreate", request)
            .await
            .expect("duplicate despite new runtime");
        assert_eq!(next, duplicate);
        assert!(
            service
                .get_control_operation("recreate", "owner", Some("delete"))
                .await
                .is_err()
        );
        assert!(
            service
                .recreate_identity(
                    "recreate",
                    shared_types::UserAppRecreateRequest {
                        user_id: "owner".into(),
                        expected_lifecycle_id: old.lifecycle_id,
                        request_id: "different-request".into()
                    }
                )
                .await
                .is_err()
        );
    }
}
