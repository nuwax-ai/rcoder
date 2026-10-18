//! 热部署（hot deploy）失败/收敛窗口的恢复协调：只读原 owner 的部署状态
//! 取证，绝不重放部署、写密码或做配置收敛。

use shared_types::{UserAppOperationRecord, UserAppOperationState};
use tokio::time::{Duration, Instant, timeout_at};

use crate::models::{AppOperationError, AppResult};
use crate::service::AppService;
use crate::utils::{map_runtime_error, map_runtime_mutation_error};

impl AppService {
    /// A stopped-renewing coordinator may still have submitted work. Revoke
    /// only its exact SQL snapshot before observing that work; never replay it.
    pub(super) async fn resume_hot_control(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> AppResult<bool> {
        if !snapshot.kind.supports_hot_deployment()
            || snapshot.scope != shared_types::UserAppOperationScope::Prod
        {
            return Ok(false);
        }
        let mut operation = snapshot.clone();
        match operation.state {
            UserAppOperationState::Running => {
                let Some(binding) = self
                    .metadata
                    .store
                    .get_operation_lease(&operation.app_id, &operation.operation_id)
                    .await?
                else {
                    return Ok(false);
                };
                match self.observe_hot_lease(&operation, &binding).await? {
                    container_runtime_api::AppOperationLeaseObservation::ExpiredOriginalIdentity => {}
                    container_runtime_api::AppOperationLeaseObservation::Held
                    | container_runtime_api::AppOperationLeaseObservation::Unconfirmed => return Ok(false),
                }
                operation = self
                    .metadata
                    .store
                    .reserve_interrupted_hot(&operation, &binding)
                    .await?;
            }
            UserAppOperationState::RecoveryRequired => {}
            UserAppOperationState::Pending
            | UserAppOperationState::Succeeded
            | UserAppOperationState::Failed
            | UserAppOperationState::WaitingRetry => return Ok(false),
        }
        match operation.step.as_str() {
            "hot_execution" => Ok(self.reconcile_hot_failure(&operation).await?.is_some()),
            "hot_converging" => Ok(self.reconcile_hot_convergence(&operation).await?.is_some()),
            _ => Ok(false),
        }
    }

    async fn observe_hot_lease(
        &self,
        snapshot: &UserAppOperationRecord,
        binding: &shared_types::UserAppOperationLeaseBinding,
    ) -> AppResult<container_runtime_api::AppOperationLeaseObservation> {
        if binding.context.app_id != snapshot.app_id
            || binding.context.lifecycle_id != snapshot.lifecycle_id
            || binding.context.operation_id != snapshot.operation_id
            || Some(&binding.context.executor_id) != snapshot.executor_id.as_ref()
            || binding.context.request_fingerprint != snapshot.request_fingerprint
            || *binding.receipt.service_type() != shared_types::ServiceType::Userapp
        {
            return Err(AppOperationError::Conflict(
                "Hot recovery lease identity changed".into(),
            ));
        }
        match &binding.receipt {
            shared_types::UserAppOperationLeaseReceipt::Kubernetes { .. } => {
                let observation = self
                    .runtime
                    .observe_app_operation_lease(&binding.context, &binding.receipt)
                    .await
                    .map_err(|error| {
                        map_runtime_error("Observe original hot recovery lease", error)
                    })?;
                match observation {
                    container_runtime_api::AppOperationLeaseObservation::Held
                    | container_runtime_api::AppOperationLeaseObservation::ExpiredOriginalIdentity => Ok(observation),
                    container_runtime_api::AppOperationLeaseObservation::Unconfirmed => {
                        // Pre-Lease ConfigMap receipts have no TTL observation.
                        // Preserve their original exact validation; a native
                        // successor conflict above must never enter this path.
                        let held = self.runtime.validate_app_operation_receipt(
                            &binding.context, &binding.receipt
                        ).await.map_err(|error| map_runtime_error("Validate retained hot recovery receipt", error))?;
                        Ok(if held {
                            container_runtime_api::AppOperationLeaseObservation::Held
                        } else {
                            container_runtime_api::AppOperationLeaseObservation::Unconfirmed
                        })
                    }
                }
            }
            shared_types::UserAppOperationLeaseReceipt::Docker { .. } => {
                // Docker has no expiring Lease. Keep its existing receipt
                // validation; an absent file/unsupported observation grants nothing.
                let held = self
                    .runtime
                    .validate_app_operation_receipt(&binding.context, &binding.receipt)
                    .await
                    .map_err(|error| map_runtime_error("Validate hot recovery lease", error))?;
                Ok(if held {
                    container_runtime_api::AppOperationLeaseObservation::Held
                } else {
                    container_runtime_api::AppOperationLeaseObservation::Unconfirmed
                })
            }
        }
    }

    async fn require_hot_lease_identity(
        &self,
        snapshot: &UserAppOperationRecord,
        binding: &shared_types::UserAppOperationLeaseBinding,
    ) -> AppResult<()> {
        match self.observe_hot_lease(snapshot, binding).await? {
            container_runtime_api::AppOperationLeaseObservation::Held
            | container_runtime_api::AppOperationLeaseObservation::ExpiredOriginalIdentity => {
                Ok(())
            }
            container_runtime_api::AppOperationLeaseObservation::Unconfirmed => {
                Err(AppOperationError::Conflict(
                    "Original hot recovery lease identity is unconfirmed".into(),
                ))
            }
        }
    }

    /// Reconcile a hot failure by querying the exact owner recorded before the
    /// POST. No deployment replay, password write, or configuration convergence.
    pub(super) async fn reconcile_hot_failure(
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
            let evidence = self.observe_hot_owner(snapshot, binding).await?;
            let Some(evidence) = evidence else {
                return Ok(None);
            };
            match evidence.operation.phase {
                shared_types::AppCliDeployPhase::Failed => {
                    evidence
                        .validate(snapshot)
                        .map_err(AppOperationError::Conflict)?;
                    self.metadata
                        .store
                        .finalize_observed_hot_failure(snapshot, &evidence)
                        .await?
                }
                shared_types::AppCliDeployPhase::Running => {
                    // The owner confirmed success for exactly this operation;
                    // a lost platform-side response no longer locks recovery.
                    evidence
                        .validate_success(snapshot)
                        .map_err(AppOperationError::Conflict)?;
                    let encoded = serde_json::to_value(&evidence).map_err(|error| {
                        AppOperationError::Backend(format!("Encode hot success evidence: {error}"))
                    })?;
                    match self.config.access_mode {
                        crate::config::AppAccessMode::Docker => {
                            self.metadata
                                .store
                                .finalize_observed_hot_success(snapshot, &encoded)
                                .await?
                        }
                        crate::config::AppAccessMode::Kubernetes => {
                            if self.hot_environment_matches(snapshot).await? {
                                self.require_hot_lease_identity(snapshot, binding).await?;
                                self.metadata
                                    .store
                                    .finalize_observed_hot_success(snapshot, &encoded)
                                    .await?
                            } else {
                                // The exact submit-stage snapshot was revoked.
                                // The old coordinator must win a newer SQL CAS
                                // before any env write; it can no longer do so.
                                // Owner activation alone is only partial success.
                                self.require_hot_lease_identity(snapshot, binding).await?;
                                self.metadata.store.settle_fenced_operation(snapshot, &encoded,
                                    "original hot owner completed before saved environment convergence; coordinator write authority was revoked; a fresh deployment may retry"
                                ).await?
                            }
                        }
                    }
                }
                shared_types::AppCliDeployPhase::Idle
                | shared_types::AppCliDeployPhase::Deploying
                | shared_types::AppCliDeployPhase::Orchestrating => return Ok(None),
            }
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
                .map_err(|error| {
                    map_runtime_mutation_error(
                        "runtime_lease_release",
                        "Release confirmed hot failure lease",
                        error,
                    )
                })?;
            self.metadata.store.forget_operation_lease(&binding).await?;
        }
        Ok(Some(terminal.into()))
    }

    fn hot_environment_precondition(
        snapshot: &UserAppOperationRecord,
    ) -> Option<shared_types::HotDeploymentEnvPrecondition> {
        snapshot.checkpoint.pointer("/hot_execution/converge_precondition")
            .filter(|value| !value.is_null())
            .map(|value| serde_json::from_value::<shared_types::HotDeploymentEnvPrecondition>(value.clone()))
            .and_then(|result| match result {
                Ok(precondition) => match precondition.validate() {
                    Ok(()) => Some(precondition),
                    Err(error) => {
                        tracing::warn!(%error, operation_id = %snapshot.operation_id,
                            "Original hot env write precondition is incomplete; observe without granting write-retirement proof");
                        None
                    }
                },
                Err(error) => {
                    tracing::warn!(%error, operation_id = %snapshot.operation_id,
                        "Original hot env write precondition is unreadable; observe owner and current environment without granting write-retirement proof");
                    None
                }
            })
    }

    async fn hot_environment_matches(&self, snapshot: &UserAppOperationRecord) -> AppResult<bool> {
        let Some(value) = snapshot.checkpoint.pointer("/hot_execution/converge_env") else {
            return Ok(false);
        };
        let expected: std::collections::HashMap<String, String> = match serde_json::from_value(
            value.clone(),
        ) {
            Ok(expected) => expected,
            Err(error) => {
                tracing::warn!(%error, operation_id = %snapshot.operation_id,
                    "Original hot environment target is unreadable; preserve partial failure without replay");
                return Ok(false);
            }
        };
        if expected.get(shared_types::APP_DEPLOY_OPERATION_ID) != Some(&snapshot.operation_id) {
            return Ok(false);
        }
        let current = self
            .runtime
            .app_env_snapshot(&snapshot.app_id)
            .await
            .map_err(|error| {
                map_runtime_error("Read original hot deployment environment", error)
            })?;
        let original = Self::hot_environment_precondition(snapshot);
        Ok(current.env == expected
            && original
                .as_ref()
                .is_none_or(|condition| condition.same_resource(&current)))
    }

    /// Read the original hot owner's deployment status through the captured
    /// physical target. Returns validated evidence for a terminal owner
    /// outcome (Failed or Running); None while the owner is still deploying.
    async fn observe_hot_owner(
        &self,
        snapshot: &UserAppOperationRecord,
        binding: &shared_types::UserAppOperationLeaseBinding,
    ) -> AppResult<Option<shared_types::HotDeploymentFailureEvidence>> {
        self.require_hot_lease_identity(snapshot, binding).await?;
        let hot = snapshot
            .checkpoint
            .get("hot_execution")
            .ok_or_else(|| AppOperationError::Conflict("Hot recovery checkpoint missing".into()))?;
        let target: shared_types::RuntimeConfigurationTarget =
            serde_json::from_value(hot.get("target").cloned().ok_or_else(|| {
                AppOperationError::Conflict(
                    "Legacy hot operation has no physical recovery target".into(),
                )
            })?)
            .map_err(|_| AppOperationError::Conflict("Invalid hot recovery target".into()))?;
        let observed = timeout_at(
            Instant::now() + Duration::from_secs(15),
            self.runtime.exec_app_configuration_target(
                &binding.context,
                &target,
                vec![
                    "sh".into(),
                    "-c".into(),
                    "curl --silent --show-error --fail --noproxy '*' --connect-timeout 2 --max-time 5 http://127.0.0.1:3010/v1/deploy/status"
                        .into(),
                ],
            ),
        )
        .await
        .map_err(|_| AppOperationError::Backend("Hot recovery observation timed out".into()))?
        .map_err(|error| map_runtime_error("Observe original hot owner", error))?;
        // A successor arriving during the remote read cannot lend its mutex
        // to the old result's final CAS or conditional lease cleanup.
        self.require_hot_lease_identity(snapshot, binding).await?;
        if observed.exit_code != 0 {
            return Err(AppOperationError::Backend(
                "Original hot owner status is unavailable".into(),
            ));
        }
        let body: serde_json::Value = serde_json::from_str(&observed.stdout)
            .map_err(|_| AppOperationError::Backend("Invalid hot owner status response".into()))?;
        let data = body.get("data").ok_or_else(|| {
            AppOperationError::Backend("Hot owner response envelope missing".into())
        })?;
        let operation: shared_types::AppDeploymentOperation =
            serde_json::from_value(data.get("operation").cloned().ok_or_else(|| {
                AppOperationError::Conflict("Original hot operation status missing".into())
            })?)
            .map_err(|_| AppOperationError::Conflict("Invalid hot operation status".into()))?;
        match operation.phase {
            shared_types::AppCliDeployPhase::Failed | shared_types::AppCliDeployPhase::Running => {}
            shared_types::AppCliDeployPhase::Idle
            | shared_types::AppCliDeployPhase::Deploying
            | shared_types::AppCliDeployPhase::Orchestrating => return Ok(None),
        }
        let evidence =
            shared_types::HotDeploymentFailureEvidence {
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
        Ok(Some(evidence))
    }

    /// Recover a crash between the hot convergence CAS and the env write.
    /// Evidence order: the converged environment matches the persisted target
    /// (K8s ConfigMap readback), else the original owner's terminal Running
    /// outcome through its captured physical target (Docker's only source —
    /// its environment is immutable). Anything else keeps the protection.
    pub(super) async fn reconcile_hot_convergence(
        &self,
        snapshot: &UserAppOperationRecord,
    ) -> AppResult<Option<shared_types::UserAppOperationView>> {
        let binding = self
            .metadata
            .store
            .get_operation_lease(&snapshot.app_id, &snapshot.operation_id)
            .await?;
        let mut evidence = None;
        let mut current_env = None;
        let mut configuration_conflict = false;
        let precondition = Self::hot_environment_precondition(snapshot);
        if let Some(binding) = &binding {
            self.require_hot_lease_identity(snapshot, binding).await?;
        }
        let expected = snapshot.checkpoint.pointer("/hot_execution/converge_env")
            .and_then(|value| match serde_json::from_value::<std::collections::HashMap<String, String>>(value.clone()) {
                Ok(expected) => Some(expected),
                Err(error) => {
                    tracing::warn!(%error, operation_id = %snapshot.operation_id,
                        "Original hot environment target is unreadable; observe native write retirement and owner without granting success");
                    None
                }
            });
        let observe_environment = match self.config.access_mode {
            crate::config::AppAccessMode::Kubernetes => {
                expected.is_some() || precondition.is_some()
            }
            crate::config::AppAccessMode::Docker => expected.is_some(),
        };
        if observe_environment {
            let current = self
                .runtime
                .app_env_snapshot(&snapshot.app_id)
                .await
                .map_err(|error| map_runtime_error("Read converged environment", error))?;
            if precondition
                .as_ref()
                .is_some_and(|original| !original.same_resource(&current))
            {
                return Err(AppOperationError::Conflict(
                    "Original hot environment resource was replaced; recovery will observe again"
                        .into(),
                ));
            }
            if let Some(expected) = &expected
                && current.env == *expected
                && expected.get(shared_types::APP_DEPLOY_OPERATION_ID)
                    == Some(&snapshot.operation_id)
            {
                evidence = Some(serde_json::to_value(expected).map_err(|error| {
                    AppOperationError::Backend(format!("Encode convergence evidence: {error}"))
                })?);
            }
            current_env = Some(current);
        }
        if evidence.is_none() {
            let Some(binding) = binding.as_ref() else {
                return Err(AppOperationError::Conflict(
                    "Hot convergence recovery has no original lease receipt".into(),
                ));
            };
            if let Some(owner) = self.observe_hot_owner(snapshot, binding).await? {
                match owner.operation.phase {
                    shared_types::AppCliDeployPhase::Running => {
                        owner
                            .validate_success(snapshot)
                            .map_err(AppOperationError::Conflict)?;
                        match self.config.access_mode {
                            crate::config::AppAccessMode::Docker => {}
                            crate::config::AppAccessMode::Kubernetes => {
                                if !precondition.as_ref().zip(current_env.as_ref()).is_some_and(
                                    |(original, observed)| original.write_retired_by(observed),
                                ) {
                                    return Err(AppOperationError::Conflict(
                                        "Hot owner completed, but the original conditional environment write is still unconfirmed; automatic observation will retry".into(),
                                    ));
                                }
                                // The owner completed, and the original PATCH's
                                // exact revision cannot write over this newer
                                // ConfigMap. Preserve its newer values, report
                                // partial failure, and permit a fresh request.
                                configuration_conflict = true;
                            }
                        }
                        evidence = Some(serde_json::to_value(&owner).map_err(|error| {
                            AppOperationError::Backend(format!(
                                "Encode hot success evidence: {error}"
                            ))
                        })?);
                    }
                    shared_types::AppCliDeployPhase::Failed
                    | shared_types::AppCliDeployPhase::Idle
                    | shared_types::AppCliDeployPhase::Deploying
                    | shared_types::AppCliDeployPhase::Orchestrating => {
                        return Err(AppOperationError::Conflict(
                            "Original hot owner no longer confirms activation after convergence began; automatic observation will retry"
                                .into(),
                        ));
                    }
                }
            }
        }
        let Some(evidence) = evidence else {
            // Convergence target not reached and owner outcome still unknown:
            // the original protection stays.
            return Ok(None);
        };
        if let Some(binding) = &binding {
            self.require_hot_lease_identity(snapshot, binding).await?;
        }
        let terminal = if configuration_conflict {
            self.metadata.store.settle_fenced_operation(snapshot, &evidence,
                "original hot owner completed; its conditional environment write was retired by a newer configuration; newer configuration retained"
            ).await?
        } else {
            self.metadata
                .store
                .finalize_observed_hot_success(snapshot, &evidence)
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
                .map_err(|error| {
                    map_runtime_mutation_error(
                        "runtime_lease_release",
                        "Release confirmed hot success lease",
                        error,
                    )
                })?;
            self.metadata.store.forget_operation_lease(&binding).await?;
        }
        Ok(Some(terminal.into()))
    }
}
