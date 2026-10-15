//! 控制操作恢复：重试入口（retry_control_operation）、终态对账
//! （reconcile_completed_control）与未认领 Pending 操作的续跑
//! （resume_pending_control）。

use shared_types::{
    UserAppControlCommand as Command, UserAppOperationRecord, UserAppOperationState,
};
use tokio::time::{Duration, Instant, timeout_at};

use crate::models::{AppOperationError, AppResult};
use crate::service::{AppService, OwnedOperation};
use crate::utils::{map_runtime_error, map_runtime_mutation_error};

impl AppService {
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
            .ok_or_else(|| {
                AppOperationError::OperationNotFound("Application operation not found".into())
            })?;
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
        if matches!(
            operation.kind,
            shared_types::UserAppOperationKind::DeleteCompute
                | shared_types::UserAppOperationKind::PurgeResources
                | shared_types::UserAppOperationKind::DeleteApplication
        ) && matches!(
            operation.state,
            UserAppOperationState::Running | UserAppOperationState::RecoveryRequired
        ) && !shared_types::userapp_operation_has_final_evidence(&operation)
        {
            self.reconcile_interrupted_deletion(&operation).await?;
            return self
                .get_control_operation(app_id, Some(operation_id))
                .await?
                .ok_or_else(|| {
                    AppOperationError::OperationNotFound("Deletion operation disappeared".into())
                        .with_operation_id(operation.operation_id.clone())
                });
        }
        if ((operation.state == UserAppOperationState::RecoveryRequired
            && operation.step == "hot_execution")
            || (operation.state == UserAppOperationState::Failed
                && operation.step == "hot_execution_failed"))
            && let Some(view) = self.reconcile_hot_failure(&operation).await?
        {
            return Ok(view);
        }
        if operation.state == UserAppOperationState::RecoveryRequired
            && operation.step == "hot_converging"
            && let Some(view) = self.reconcile_hot_convergence(&operation).await?
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
                if operation.checkpoint.get("start_write_acknowledged")
                    == Some(&serde_json::Value::Bool(true))
                {
                    self.metadata
                        .store
                        .finalize_observed_wake(&operation)
                        .await?
                } else {
                    // Legacy record predating the acknowledged-write flag. The
                    // independent durable evidence is the operation-bound
                    // compute start receipt: a confirmed receipt observes the
                    // same terminal boundary; anything else keeps protection
                    // and leaves an explicit Stop as the recovery path.
                    let target: shared_types::UserAppMutationTarget = serde_json::from_value(
                        operation
                            .checkpoint
                            .get("target")
                            .ok_or_else(|| {
                                AppOperationError::Conflict(
                                    "Legacy wake checkpoint lost its captured target".into(),
                                )
                            })?
                            .clone(),
                    )
                    .map_err(|_| {
                        AppOperationError::Conflict(
                            "Legacy wake checkpoint target is invalid".into(),
                        )
                    })?;
                    let confirmed = self
                        .runtime
                        .reconcile_app_compute_start(&target)
                        .await
                        .map_err(|error| {
                            map_runtime_error("Verify legacy wake start receipt", error)
                        })?;
                    if !confirmed {
                        return Err(AppOperationError::Conflict(
                            "Legacy wake start write is not confirmed by its runtime receipt; \
                             verify the deployment manually or stop the application explicitly"
                                .into(),
                        ));
                    }
                    self.metadata
                        .store
                        .finalize_legacy_observed_wake(
                            &operation,
                            &serde_json::json!({
                                "runtime_start_receipt": target.context,
                            }),
                        )
                        .await?
                }
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
                    .map_err(|error| {
                        map_runtime_mutation_error(
                            "runtime_lease_release",
                            "Release confirmed wake lease",
                            error,
                        )
                    })?;
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
                    AppOperationError::OperationNotFound("Management operation disappeared".into())
                        .with_operation_id(operation.operation_id.clone())
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
                AppOperationError::OperationNotFound(
                    "Application operation not found after retry".into(),
                )
                .with_operation_id(operation.operation_id.clone())
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
        if snapshot.state == UserAppOperationState::RecoveryRequired
            && snapshot.step == "hot_converging"
        {
            return Ok(self.reconcile_hot_convergence(snapshot).await?.is_some());
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
                    AppOperationError::OperationNotFound(
                        "Recovery operation no longer exists".into(),
                    )
                    .with_operation_id(snapshot.operation_id.clone())
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
                let input = super::super::deploy_control::DeployInput::decode(&input, *restart)?;
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
                let (params, previous) = super::super::config_input::decode(&input)?;
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
            let context = operation.execution_context();
            let target = self
                .capture_bound_app_target(&context, previous.resource_version.as_deref())
                .await?;
            operation
                .checkpoint(
                    "recovery_control_target",
                    serde_json::json!({"target":target,"command":command}),
                )
                .await?;
            if matches!(command, Command::Start { traffic: true }) {
                self.restore_activity_state(&snapshot.app_id, &previous);
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
                        map_runtime_mutation_error(
                            "runtime_policy_apply",
                            "Recover captured runtime policy",
                            error,
                        )
                    })?;
                }
                Command::Stop { wake_on_traffic } => {
                    self.apply_scale_zero(&target, *wake_on_traffic, &previous, &guard)
                        .await?;
                }
                Command::Restart => {
                    guard.mark_mutating()?;
                    let restart_image = crate::runtime::params::platform_restart_image(
                        &std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").ok(),
                    );
                    self.runtime
                        .restart_app_target(&target, restart_image.as_deref())
                        .await
                        .map_err(|error| {
                            map_runtime_mutation_error(
                                "container_restart",
                                "Restart captured recovery target",
                                error,
                            )
                        })?;
                    self.refresh_pingora_after_restart(&snapshot.app_id).await;
                }
                Command::Start { traffic } => {
                    if !*traffic || previous.phase != "Running" {
                        guard.mark_mutating()?;
                        let image = if *traffic {
                            None
                        } else {
                            crate::runtime::params::platform_restart_image(
                                &std::env::var("RCODER_RUNTIME_IMAGE_DIGEST").ok(),
                            )
                        };
                        self.runtime
                            .start_app_target_with_image(&target, image.as_deref())
                            .await
                            .map_err(|error| {
                                map_runtime_mutation_error(
                                    "container_start",
                                    "Start captured recovery target",
                                    error,
                                )
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
                    Command::SetRecyclePolicy { .. } => {
                        if previous.replicas == 0 {
                            self.restore_activity_state(&snapshot.app_id, &previous);
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
