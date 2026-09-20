//! Replay only unclaimed controls with their original durable inputs. A retained
//! runtime marker/lease blocks this entry; elapsed time never grants ownership.

use shared_types::{
    UserAppControlCommand as Command, UserAppOperationRecord, UserAppOperationState,
};
use tokio::time::{Duration, Instant, timeout_at};

use crate::models::{AppOperationError, AppResult};
use crate::service::{AppService, OwnedOperation};
use crate::utils::map_runtime_error;

impl AppService {
    pub async fn verify_recovered_storage(
        &self,
        app_id: &str,
        scope: shared_types::UserAppOperationScope,
    ) -> AppResult<()> {
        let Some(app) = self.metadata.store.get_application(app_id).await? else {
            return Ok(());
        };
        if app.state != shared_types::UserAppLifecycleState::Active {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        let Some(witness) = self
            .metadata
            .store
            .get_recovery_witness(app_id, &app.lifecycle_id)
            .await?
        else {
            return Ok(());
        };
        let volumes = match scope {
            shared_types::UserAppOperationScope::Dev => &witness.dev_volumes,
            shared_types::UserAppOperationScope::Prod => &witness.prod_volumes,
            shared_types::UserAppOperationScope::Application => {
                return Err(AppOperationError::Validation(
                    "Storage verification requires an explicit scope".into(),
                ));
            }
        };
        let context = shared_types::UserAppExecutionContext {
            app_id: app_id.into(),
            lifecycle_id: app.lifecycle_id,
            operation_id: "recovery-storage-observation".into(),
            executor_id: "reader".into(),
            request_fingerprint: "0".repeat(64),
        };
        self.runtime
            .verify_recovered_volumes(&context, scope, volumes)
            .await
            .map_err(|error| map_runtime_error("Verify recovered storage", error))
    }
    /// Reconstruct a missing control root from consistent managed resource
    /// identities. No runtime writes, deployment, or historical operation replay.
    pub async fn discover_missing_identity(
        &self,
        app_id: &str,
    ) -> AppResult<Option<shared_types::UserAppLifecycleRecord>> {
        crate::utils::validate_app_id(app_id)?;
        let existing = self.metadata.store.get_application(app_id).await?;
        if let Some(existing) = &existing {
            if existing.state != shared_types::UserAppLifecycleState::Active {
                return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
            }
            if existing.lifecycle_epoch != 1
                || existing.metadata_revision != 1
                || existing.active_operations != shared_types::UserAppActiveOperations::default()
            {
                return Ok(Some(existing.clone()));
            }
        }
        let found = self
            .runtime
            .discover_application_identity(app_id)
            .await
            .map_err(|e| map_runtime_error("Discover existing lifecycle", e))?;
        let Some(found) = found else {
            return Ok(existing);
        };
        if existing
            .as_ref()
            .is_some_and(|app| app.lifecycle_id == found.lifecycle_id)
        {
            return Ok(existing);
        }
        found.validate().map_err(AppOperationError::Validation)?;
        let confirmed = self
            .runtime
            .discover_application_identity(app_id)
            .await
            .map_err(|e| map_runtime_error("Confirm existing lifecycle", e))?;
        if confirmed.as_ref() != Some(&found) {
            return Err(AppOperationError::Conflict(
                "Managed resource inventory changed during registration recovery".into(),
            ));
        }
        Ok(Some(
            self.metadata
                .store
                .restore_discovered_identity(&found)
                .await?,
        ))
    }

    /// Reconcile a hot failure by querying the exact owner recorded before the
    /// POST. No deployment replay, password write, or configuration convergence.
    async fn reconcile_hot_failure(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> AppResult<Option<shared_types::UserAppOperationView>> {
        let binding = self
            .metadata
            .store
            .get_operation_lease(&snapshot.app_id, &snapshot.operation_id)
            .await?;
        let terminal = if snapshot.state == UserAppOperationState::Failed
            && snapshot.step == "hot_execution_failed"
        {
            snapshot.clone()
        } else {
            let binding = binding.as_ref().ok_or_else(|| {
                AppOperationError::Conflict(
                    "Hot recovery has no original physical lease receipt".into(),
                )
            })?;
            if binding.context.app_id != snapshot.app_id
                || binding.context.lifecycle_id != snapshot.lifecycle_id
                || binding.context.operation_id != snapshot.operation_id
                || Some(&binding.context.executor_id) != snapshot.executor_id.as_ref()
                || binding.context.request_fingerprint != snapshot.request_fingerprint
            {
                return Err(AppOperationError::Conflict(
                    "Hot recovery lease identity changed".into(),
                ));
            }
            if !self
                .runtime
                .validate_app_operation_receipt(&binding.context, &binding.receipt)
                .await
                .map_err(|error| map_runtime_error("Validate hot recovery lease", error))?
            {
                return Err(AppOperationError::Conflict(
                    "Hot recovery lease is no longer held".into(),
                ));
            }
            let hot = snapshot.checkpoint.get("hot_execution").ok_or_else(|| {
                AppOperationError::Conflict("Hot recovery checkpoint missing".into())
            })?;
            let target: shared_types::RuntimeConfigurationTarget =
                serde_json::from_value(hot.get("target").cloned().ok_or_else(|| {
                    AppOperationError::Conflict(
                        "Legacy hot operation has no physical recovery target".into(),
                    )
                })?)
                .map_err(|_| AppOperationError::Conflict("Invalid hot recovery target".into()))?;
            let observed = timeout_at(Instant::now() + Duration::from_secs(15),
                self.runtime.exec_app_configuration_target(&binding.context, &target, vec![
                    "sh".into(), "-c".into(),
                    "curl --silent --show-error --fail --noproxy '*' --connect-timeout 2 --max-time 5 http://127.0.0.1:3010/v1/deploy/status".into(),
                ])).await.map_err(|_| AppOperationError::Backend("Hot recovery observation timed out".into()))?
                .map_err(|error| map_runtime_error("Observe original hot owner", error))?;
            if observed.exit_code != 0 {
                return Err(AppOperationError::Backend(
                    "Original hot owner status is unavailable".into(),
                ));
            }
            let body: serde_json::Value = serde_json::from_str(&observed.stdout).map_err(|_| {
                AppOperationError::Backend("Invalid hot owner status response".into())
            })?;
            let data = body.get("data").ok_or_else(|| {
                AppOperationError::Backend("Hot owner response envelope missing".into())
            })?;
            let operation: shared_types::AppDeploymentOperation =
                serde_json::from_value(data.get("operation").cloned().ok_or_else(|| {
                    AppOperationError::Conflict("Original hot operation status missing".into())
                })?)
                .map_err(|_| AppOperationError::Conflict("Invalid hot operation status".into()))?;
            if operation.phase != shared_types::AppCliDeployPhase::Failed {
                return Ok(None);
            }
            let evidence = shared_types::HotDeploymentFailureEvidence {
                target,
                protocol_version: data
                    .get("protocol_version")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|version| u32::try_from(version).ok())
                    .ok_or_else(|| {
                        AppOperationError::Conflict("Hot recovery protocol missing".into())
                    })?,
                server_phase: serde_json::from_value(data.get("phase").cloned().ok_or_else(
                    || AppOperationError::Conflict("Hot owner phase missing".into()),
                )?)
                .map_err(|_| AppOperationError::Conflict("Invalid hot owner phase".into()))?,
                operation,
            };
            evidence
                .validate(snapshot)
                .map_err(AppOperationError::Conflict)?;
            self.metadata
                .store
                .finalize_observed_hot_failure(snapshot, &evidence)
                .await?
        };
        if let Some(binding) = binding {
            if binding.context.app_id != terminal.app_id
                || binding.context.lifecycle_id != terminal.lifecycle_id
                || binding.context.operation_id != terminal.operation_id
                || Some(&binding.context.executor_id) != terminal.executor_id.as_ref()
                || binding.context.request_fingerprint != terminal.request_fingerprint
            {
                return Err(AppOperationError::Conflict(
                    "Hot terminal lease identity changed".into(),
                ));
            }
            self.runtime
                .release_app_operation_receipt(&binding.context, &binding.receipt)
                .await
                .map_err(|error| map_runtime_error("Release confirmed hot failure lease", error))?;
            self.metadata.store.forget_operation_lease(&binding).await?;
        }
        Ok(Some(terminal.into()))
    }

    pub fn set_builder_recovery(
        &self,
        recovery: std::sync::Arc<dyn shared_types::UserAppBuilderRecovery>,
    ) -> AppResult<()> {
        *self.builder_recovery.write().map_err(|_| {
            AppOperationError::Backend("Builder recovery registry lock poisoned".into())
        })? = Some(recovery);
        Ok(())
    }

    /// Resume the same unclaimed operation; a retry never clears a runtime lease
    /// or invents missing command inputs from current deployment configuration.
    /// 只读观察围栏应用的运行时实况（诊断用；观察结果不构成任何裁决依据）。
    async fn observe_uncertain_runtime(&self, app_id: &str) -> String {
        match self.runtime.get_deployment_status(app_id).await {
            Ok(Some(status)) => format!(
                "observed runtime deployment present (phase={}, ready={})",
                status.phase, status.ready_replicas
            ),
            Ok(None) => "observed runtime deployment absent at observation time (not proof of \
                 absence; a late write may still land)"
                .to_string(),
            Err(error) => format!("runtime observation unavailable: {error}"),
        }
    }

    pub async fn retry_control_operation(
        &self,
        app_id: &str,
        operation_id: &str,
        request: shared_types::UserAppRetryRequest,
    ) -> AppResult<shared_types::UserAppOperationView> {
        use garde::Validate as _;
        request
            .validate()
            .map_err(|error| AppOperationError::Validation(error.to_string()))?;
        shared_types::validate_identifier(operation_id, "operation_id")
            .map_err(AppOperationError::Validation)?;
        let identity = self.get_lifecycle(app_id).await?;
        if identity.lifecycle_id != request.lifecycle_id {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        let operation = self
            .metadata
            .store
            .get_operation(app_id, operation_id)
            .await?
            .ok_or_else(|| AppOperationError::NotFound("Application operation not found".into()))?;
        if operation.lifecycle_id != request.lifecycle_id {
            return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
        }
        // A completed successful request is safe to observe repeatedly. Never
        // re-execute it, even if the caller still holds its admission revision.
        if operation.state == UserAppOperationState::Succeeded {
            return Ok(operation.into());
        }
        if operation.revision != request.expected_revision {
            return Err(shared_types::UserAppStoreError::VersionConflict.into());
        }
        if ((operation.state == UserAppOperationState::RecoveryRequired
            && operation.step == "hot_execution")
            || (operation.state == UserAppOperationState::Failed
                && operation.step == "hot_execution_failed"))
            && let Some(view) = self.reconcile_hot_failure(&operation).await?
        {
            return Ok(view);
        }
        if operation.kind == shared_types::UserAppOperationKind::Start
            && ((operation.state == UserAppOperationState::RecoveryRequired
                && operation.step == "traffic_wake_observing")
                || (operation.state == UserAppOperationState::Failed
                    && operation.step == "traffic_wake_observation_failed"))
        {
            // Reconcile the original observation, never issue another start.
            // Full-record CAS in the store validates acknowledged write evidence.
            let terminal = if operation.state == UserAppOperationState::RecoveryRequired {
                self.metadata
                    .store
                    .finalize_observed_wake(&operation)
                    .await?
            } else {
                operation
            };
            if let Some(binding) = self
                .metadata
                .store
                .get_operation_lease(app_id, operation_id)
                .await?
            {
                if binding.context.app_id != terminal.app_id
                    || binding.context.lifecycle_id != terminal.lifecycle_id
                    || binding.context.operation_id != terminal.operation_id
                    || Some(&binding.context.executor_id) != terminal.executor_id.as_ref()
                    || binding.context.request_fingerprint != terminal.request_fingerprint
                {
                    return Err(AppOperationError::Conflict(
                        "Wake terminal lease does not match the original executor".into(),
                    ));
                }
                self.runtime
                    .release_app_operation_receipt(&binding.context, &binding.receipt)
                    .await
                    .map_err(|error| map_runtime_error("Release confirmed wake lease", error))?;
                self.metadata.store.forget_operation_lease(&binding).await?;
            }
            return Ok(terminal.into());
        }
        if operation.kind == shared_types::UserAppOperationKind::PrepareProdDatabase
            && matches!(
                operation.state,
                UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
            )
            && !shared_types::userapp_operation_has_final_evidence(&operation)
        {
            self.reconcile_database_preparation(&operation).await?;
            return self
                .get_control_operation(app_id, Some(operation_id))
                .await?
                .ok_or_else(|| {
                    AppOperationError::NotFound("Management operation disappeared".into())
                });
        }
        let recoverable_final = matches!(
            operation.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        ) && shared_types::userapp_operation_has_final_evidence(&operation);
        let pending_builder = operation.state == UserAppOperationState::Pending
            && matches!(
                operation.kind,
                shared_types::UserAppOperationKind::EnsureBuilder
                    | shared_types::UserAppOperationKind::StopBuilder
                    | shared_types::UserAppOperationKind::RestartBuilder
                    | shared_types::UserAppOperationKind::AdoptBuilder
            );
        if !recoverable_final
            && !pending_builder
            && (operation.state != UserAppOperationState::Pending || operation.command.is_none())
        {
            // B′：未知结果的围栏操作不自动重放；拒绝响应附只读观察证据
            // （纯 GET，不改任何状态、不释放任何锁）供人工裁决。观察不
            // 构成裁决——"查无"不能证明迟到写不会落盘（R02）。
            let observed = self.observe_uncertain_runtime(app_id).await;
            return Err(AppOperationError::InvalidState(format!(
                "Operation cannot be replayed automatically ({observed}; last step: {}; \
                 manual reconciliation required — the fence stays until the outcome \
                 is verified by an operator)",
                operation.step
            )));
        }
        let completed_builder = recoverable_final
            && matches!(
                operation.kind,
                shared_types::UserAppOperationKind::EnsureBuilder
                    | shared_types::UserAppOperationKind::StopBuilder
                    | shared_types::UserAppOperationKind::RestartBuilder
            );
        let resumed = if pending_builder || completed_builder {
            let recovery = self
                .builder_recovery
                .read()
                .map_err(|_| {
                    AppOperationError::Backend("Builder recovery registry lock poisoned".into())
                })?
                .clone()
                .ok_or_else(|| {
                    AppOperationError::Backend("Builder recovery is not configured".into())
                })?;
            if pending_builder {
                recovery
                    .resume_pending(&operation)
                    .await
                    .map_err(AppOperationError::Backend)?
            } else {
                recovery
                    .reconcile_completed(&operation)
                    .await
                    .map_err(AppOperationError::Backend)?
            }
        } else {
            self.resume_pending_control(&operation).await?
        };
        if !resumed {
            return Err(AppOperationError::Conflict(
                "Operation changed or cannot be safely claimed; query its current state".into(),
            ));
        }
        self.get_control_operation(app_id, Some(operation_id))
            .await?
            .ok_or_else(|| {
                AppOperationError::NotFound("Application operation not found after retry".into())
            })
    }

    /// Reconcile a durable final checkpoint without replaying any resource write.
    pub(crate) async fn reconcile_completed_control(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> AppResult<bool> {
        if !matches!(
            snapshot.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        ) || !shared_types::userapp_operation_has_final_evidence(snapshot)
        {
            return Ok(false);
        }
        let binding = self
            .metadata
            .store
            .get_operation_lease(&snapshot.app_id, &snapshot.operation_id)
            .await?
            .ok_or_else(|| {
                AppOperationError::Conflict(
                    "Completed operation has no physical lease receipt".into(),
                )
            })?;
        let reserved = self
            .metadata
            .store
            .reserve_completed_operation(snapshot)
            .await?;
        let release = self
            .runtime
            .release_app_operation_receipt(&binding.context, &binding.receipt)
            .await;
        let (state, error_message) = match release {
            Ok(()) => (UserAppOperationState::Succeeded, None),
            Err(error) => (
                UserAppOperationState::RecoveryRequired,
                Some(format!(
                    "Conditional operation lease cleanup failed: {error}"
                )),
            ),
        };
        self.metadata
            .store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: reserved.app_id.clone(),
                lifecycle_id: reserved.lifecycle_id.clone(),
                operation_id: reserved.operation_id.clone(),
                expected_revision: reserved.revision,
                executor_id: binding.context.executor_id.clone(),
                state,
                step: reserved.step,
                checkpoint: reserved.checkpoint,
                error_code: error_message
                    .as_ref()
                    .map(|_| shared_types::error_codes::ERR_BACKEND_ERROR.into()),
                error_message,
            })
            .await?;
        if state == UserAppOperationState::Succeeded {
            self.metadata.store.forget_operation_lease(&binding).await?;
        }
        Ok(true)
    }

    /// Compute the remaining deploy budget for a recovery replay.
    /// Reads the bound deadline (epoch ms) and converts to monotonic remaining
    /// duration. For old-version operations without a bound deadline, derives
    /// from created_at + absolute_budget and bind-once persists the result.
    async fn recovery_deploy_budget(
        &self,
        operation: &OwnedOperation,
        snapshot: &UserAppOperationRecord,
    ) -> AppResult<Duration> {
        let context = operation.execution_context();
        let absolute_secs = self.config.deploy_budget.absolute_budget_secs as i64;
        let absolute_ms = absolute_secs * 1000;

        let deadline_ms = match self
            .metadata
            .store
            .operation_deadline(&context.app_id, &context.operation_id)
            .await
        {
            Ok(Some(ms)) => ms,
            Ok(None) => {
                // Old version: derive from created_at and bind-once persist.
                let derived = snapshot
                    .created_at
                    .timestamp_millis()
                    .saturating_add(absolute_ms);
                self.metadata
                    .store
                    .bind_operation_deadline(
                        &context.app_id,
                        &context.operation_id,
                        &context.lifecycle_id,
                        derived,
                    )
                    .await
                    .map_err(AppOperationError::from)?
            }
            Err(e) => return Err(e.into()),
        };

        let now_ms = chrono::Utc::now().timestamp_millis();
        let remaining_ms = deadline_ms.saturating_sub(now_ms);
        if remaining_ms <= 0 {
            return Err(AppOperationError::Backend(
                "Deployment recovery deadline already exceeded".into(),
            ));
        }
        Ok(Duration::from_millis(remaining_ms as u64))
    }

    pub async fn resume_pending_control(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> AppResult<bool> {
        if snapshot.state == UserAppOperationState::RecoveryRequired
            && snapshot.step == "hot_execution"
        {
            return Ok(self.reconcile_hot_failure(snapshot).await?.is_some());
        }
        if self.reconcile_completed_control(snapshot).await? {
            return Ok(true);
        }
        if snapshot.state != UserAppOperationState::Pending
            || snapshot.command.is_none()
            || matches!(
                snapshot.command,
                Some(Command::StopBuilder | Command::RestartBuilder)
            )
        {
            return Ok(false);
        }
        let deadline = Instant::now() + Duration::from_secs(90);
        let preflight = async {
            let guard = self
                .try_acquire_process_release_lock(&snapshot.app_id)
                .await?;
            let current = self
                .metadata
                .store
                .get_operation(&snapshot.app_id, &snapshot.operation_id)
                .await?
                .ok_or_else(|| {
                    AppOperationError::NotFound("Recovery operation no longer exists".into())
                })?;
            if current.state != UserAppOperationState::Pending
                || current.revision != snapshot.revision
            {
                guard.finish().await?;
                return Ok(None);
            }
            let identity = self
                .metadata
                .store
                .get_application(&current.app_id)
                .await?
                .ok_or_else(|| {
                    AppOperationError::NotFound("Recovery application identity is missing".into())
                })?;
            // Full deletion enters Deleting in its admission transaction. Only
            // that same lifecycle's linked deletion may continue in this state.
            let expected_state = if current.kind.ends_lifecycle() {
                shared_types::UserAppLifecycleState::Deleting
            } else {
                shared_types::UserAppLifecycleState::Active
            };
            if identity.lifecycle_id != current.lifecycle_id || identity.state != expected_state {
                return Err(shared_types::UserAppStoreError::LifecycleConflict.into());
            }
            if identity
                .active_operations
                .slot(current.scope)
                .map(String::as_str)
                != Some(current.operation_id.as_str())
            {
                return Err(AppOperationError::Conflict(
                    "Recovery operation no longer owns the lifecycle".into(),
                ));
            }
            Ok::<_, AppOperationError>(Some((guard, identity, current)))
        };
        let Some((mut guard, _identity, current)) =
            timeout_at(deadline, preflight).await.map_err(|_| {
                AppOperationError::Backend("Control recovery preflight deadline exceeded".into())
            })??
        else {
            return Ok(false);
        };
        let Some(command) = current.command.clone() else {
            guard.finish().await?;
            return Ok(false);
        };
        // Database password writes require the private original request and
        // physical-target verification; generic lifecycle recovery cannot replay them.
        if matches!(command, Command::ResetDatabasePassword { .. }) {
            guard.finish().await?;
            return Ok(false);
        }
        let Some(mut operation) =
            OwnedOperation::claim_pending(self.metadata.store.clone(), current).await?
        else {
            guard.finish().await?;
            return Ok(false);
        };
        if command == Command::PrepareProdDatabase {
            self.execute_database_preparation(operation, guard, deadline)
                .await?;
            return Ok(true);
        }
        if let Command::Deploy { restart, .. } = &command {
            let guard = std::sync::Arc::new(guard);
            // Read the bound deadline (if present) and convert wall-clock epoch ms
            // to a monotonic remaining duration. For old-version operations without
            // a bound deadline, derive from created_at + absolute_budget and
            // bind-once persist for consistency.
            let recovery_budget = self
                .recovery_deploy_budget(&operation, snapshot)
                .await
                .unwrap_or(Duration::from_secs(420));
            let execute = async {
                let input = operation.execution_input().await?;
                let input = super::deploy_control::DeployInput::decode(&input, *restart)?;
                self.execute_deploy_input(&snapshot.app_id, input, &mut operation, guard.clone())
                    .await
            };
            let result = tokio::time::timeout(recovery_budget, execute)
                .await
                .unwrap_or_else(|_| {
                    Err(AppOperationError::Backend(
                        "Deployment recovery deadline exceeded".into(),
                    ))
                });
            let result = match result {
                Ok(_) => {
                    operation.succeed().await?;
                    guard.mark_completed();
                    self.activity.mark_running(&snapshot.app_id);
                    Ok(true)
                }
                Err(error) => {
                    if guard.has_unfinished_mutation() {
                        operation.fail(&error).await?;
                    } else {
                        operation.reject_without_mutation(&error).await?;
                    }
                    Err(error)
                }
            };
            let guard = std::sync::Arc::try_unwrap(guard).map_err(|_| {
                AppOperationError::Conflict("Recovery deployment still owns resource lease".into())
            })?;
            if result.is_ok() || !guard.has_unfinished_mutation() {
                guard.finish().await?;
            }
            return result;
        }
        let mut clear_leases = crate::ops::StorageClearLeases::default();
        let execute = async {
            if matches!(&command, Command::Create { .. } | Command::Update { .. }) {
                let input = operation.execution_input().await?;
                let (params, previous) = super::config_input::decode(&input)?;
                if matches!(&command, Command::Create { .. }) {
                    if previous.is_some() {
                        return Err(AppOperationError::InvalidState(
                            "Creation input contains update state".into(),
                        ));
                    }
                    self.execute_creation(&snapshot.app_id, params, &mut operation, &guard)
                        .await?;
                    return Ok(container_runtime_api::DeploymentStatus::default());
                }
                let previous = previous.ok_or_else(|| {
                    AppOperationError::InvalidState("Update input is missing prior state".into())
                })?;
                self.execute_update(
                    &snapshot.app_id,
                    params,
                    previous.clone(),
                    &mut operation,
                    &guard,
                )
                .await?;
                return Ok(previous);
            }
            if let Command::ClearStorage { production } = &command {
                self.execute_storage_clear(
                    &snapshot.app_id,
                    *production,
                    &mut operation,
                    &guard,
                    &mut clear_leases,
                )
                .await?;
                return Ok(container_runtime_api::DeploymentStatus::default());
            }
            if let Command::DestroyStorage { production } = &command {
                self.execute_storage_destruction(
                    &snapshot.app_id,
                    *production,
                    &mut operation,
                    &mut guard,
                )
                .await?;
                return Ok(container_runtime_api::DeploymentStatus::default());
            }
            if matches!(&command, Command::DeleteApplication) {
                self.purge_app_resources(&snapshot.app_id, &mut operation, &guard)
                    .await?;
                return Ok(container_runtime_api::DeploymentStatus::default());
            }
            if let Command::DeleteResources {
                purge,
                expected_resource_version,
            } = &command
            {
                self.execute_resource_deletion(
                    &snapshot.app_id,
                    *purge,
                    expected_resource_version.as_deref(),
                    &mut operation,
                    &guard,
                )
                .await?;
                // Deletion has no prior runtime policy to restore on completion.
                return Ok(container_runtime_api::DeploymentStatus::default());
            }
            operation.bind_lease(&guard).await?;
            let previous = self.fetch_runtime_status_or_err(&snapshot.app_id).await?;
            if matches!(command, Command::Start { traffic: true })
                && previous.wake_on_traffic == Some(false)
            {
                return Err(AppOperationError::InvalidState(
                    "Traffic recovery cannot override an intentional stop".into(),
                ));
            }
            let context = operation.execution_context();
            let target = self
                .runtime
                .capture_app_mutation_target(&context, previous.resource_version.as_deref())
                .await
                .map_err(|error| map_runtime_error("Capture recovery control target", error))?;
            operation
                .checkpoint(
                    "recovery_control_target",
                    serde_json::json!({"target":target,"command":command}),
                )
                .await?;
            if matches!(command, Command::Start { traffic: true }) {
                self.restore_activity_state(&snapshot.app_id, &previous, true);
            }
            operation.authorize_mutation().await?;
            match &command {
                Command::Deploy { .. }
                | Command::Create { .. }
                | Command::Update { .. }
                | Command::StopBuilder
                | Command::RestartBuilder
                | Command::DeleteResources { .. }
                | Command::DeleteApplication
                | Command::DestroyStorage { .. }
                | Command::ClearStorage { .. }
                | Command::ResetDatabasePassword { .. }
                | Command::PrepareProdDatabase => {
                    return Err(AppOperationError::InvalidState(
                        "Deletion must use its captured-resource executor".into(),
                    ));
                }
                Command::SetRecyclePolicy { policy } => {
                    if self.config.access_mode == crate::config::AppAccessMode::Kubernetes {
                        guard.mark_mutating()?;
                    }
                    let projected = self.runtime.patch_app_policy_target(&target, policy).await;
                    if matches!(
                        &projected,
                        Err(container_runtime_api::ContainerRuntimeError::RequestRejected(_))
                    ) {
                        guard.mark_rejected_before_mutation();
                    }
                    projected.map_err(|error| {
                        map_runtime_error("Recover captured runtime policy", error)
                    })?;
                }
                Command::Stop { wake_on_traffic } => {
                    self.apply_scale_zero(&target, *wake_on_traffic, &previous, &guard)
                        .await?;
                }
                Command::Restart => {
                    guard.mark_mutating()?;
                    self.runtime
                        .restart_app_target(&target)
                        .await
                        .map_err(|error| {
                            map_runtime_error("Restart captured recovery target", error)
                        })?;
                }
                Command::Start { traffic } => {
                    if !*traffic || previous.phase != "Running" {
                        guard.mark_mutating()?;
                        self.runtime
                            .start_app_target(&target)
                            .await
                            .map_err(|error| {
                                map_runtime_error("Start captured recovery target", error)
                            })?;
                    }
                    if *traffic {
                        self.wait_for_captured_wake(&target).await?;
                    }
                }
            }
            Ok::<_, AppOperationError>(previous)
        };
        let result = timeout_at(deadline, execute).await.unwrap_or_else(|_| {
            Err(AppOperationError::Backend(
                "Control recovery execution deadline exceeded".into(),
            ))
        });
        match result {
            Ok(previous) => {
                if matches!(
                    command,
                    Command::Start { .. }
                        | Command::Restart
                        | Command::Stop { .. }
                        | Command::SetRecyclePolicy { .. }
                ) {
                    operation.confirm_effects().await?;
                }
                operation.succeed().await?;
                guard.mark_completed();
                match command {
                    Command::DestroyStorage { .. }
                    | Command::ClearStorage { .. }
                    | Command::Update { .. }
                    | Command::StopBuilder
                    | Command::RestartBuilder
                    | Command::ResetDatabasePassword { .. }
                    | Command::PrepareProdDatabase => {}
                    Command::Create { .. } | Command::Deploy { .. } => {
                        self.activity.mark_running(&snapshot.app_id)
                    }
                    Command::DeleteResources { .. } | Command::DeleteApplication => {
                        self.activity
                            .forget_lifecycle(&snapshot.app_id, &snapshot.lifecycle_id);
                    }
                    Command::Start { traffic: true } => {
                        if !self.activity.try_mark_woken(&snapshot.app_id) {
                            guard.finish().await?;
                            return Err(AppOperationError::Conflict(
                                "Application was stopped during recovery completion".into(),
                            ));
                        }
                    }
                    Command::Start { traffic: false } | Command::Restart => {
                        self.activity.mark_running(&snapshot.app_id)
                    }
                    Command::Stop { .. } => {}
                    Command::SetRecyclePolicy { policy } => {
                        if previous.replicas == 0
                            && let Some(wake) = policy.wake_on_traffic
                        {
                            self.restore_activity_state(&snapshot.app_id, &previous, wake);
                        }
                    }
                }
                guard.finish().await?;
                self.invalidate_deploy_cache().await;
                Ok(true)
            }
            Err(error) => {
                if guard.has_unfinished_mutation() || snapshot.kind.ends_lifecycle() {
                    operation.fail(&error).await?;
                    if !guard.has_unfinished_mutation() {
                        guard.finish().await?;
                    }
                } else {
                    operation.reject_without_mutation(&error).await?;
                    guard.finish().await?;
                }
                Err(error)
            }
        }
    }
}
