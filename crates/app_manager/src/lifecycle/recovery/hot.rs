//! 热部署（hot deploy）失败/收敛窗口的恢复协调：只读原 owner 的部署状态
//! 取证，绝不重放部署、写密码或做配置收敛。

use shared_types::{UserAppOperationRecord, UserAppOperationState};
use tokio::time::{Duration, Instant, timeout_at};

use crate::models::{AppOperationError, AppResult};
use crate::service::AppService;
use crate::utils::{map_runtime_error, map_runtime_mutation_error};

impl AppService {
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
                    self.metadata
                        .store
                        .finalize_observed_hot_success(snapshot, &encoded)
                        .await?
                }
                _ => return Ok(None),
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

    /// Read the original hot owner's deployment status through the captured
    /// physical target. Returns validated evidence for a terminal owner
    /// outcome (Failed or Running); None while the owner is still deploying.
    async fn observe_hot_owner(
        &self,
        snapshot: &UserAppOperationRecord,
        binding: &shared_types::UserAppOperationLeaseBinding,
    ) -> AppResult<Option<shared_types::HotDeploymentFailureEvidence>> {
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
        if !matches!(
            operation.phase,
            shared_types::AppCliDeployPhase::Failed | shared_types::AppCliDeployPhase::Running
        ) {
            return Ok(None);
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
        if let Some(expected) = snapshot
            .checkpoint
            .pointer("/hot_execution/converge_env")
            .cloned()
        {
            let expected: std::collections::HashMap<String, String> =
                serde_json::from_value(expected).map_err(|error| {
                    AppOperationError::Conflict(format!("Invalid hot convergence target: {error}"))
                })?;
            let current = self
                .runtime
                .app_env_snapshot(&snapshot.app_id)
                .await
                .map_err(|error| map_runtime_error("Read converged environment", error))?;
            if current.env == expected {
                evidence = Some(serde_json::to_value(&expected).map_err(|error| {
                    AppOperationError::Backend(format!("Encode convergence evidence: {error}"))
                })?);
            }
        }
        if evidence.is_none() {
            let Some(binding) = binding.as_ref() else {
                return Err(AppOperationError::Conflict(
                    "Hot convergence recovery has no original lease receipt".into(),
                ));
            };
            if let Some(owner) = self.observe_hot_owner(snapshot, binding).await? {
                if owner.operation.phase == shared_types::AppCliDeployPhase::Running {
                    owner
                        .validate_success(snapshot)
                        .map_err(AppOperationError::Conflict)?;
                    evidence = Some(serde_json::to_value(&owner).map_err(|error| {
                        AppOperationError::Backend(format!("Encode hot success evidence: {error}"))
                    })?);
                } else {
                    return Err(AppOperationError::Conflict(
                        "Hot owner reports failure after convergence began; manual reconciliation required"
                            .into(),
                    ));
                }
            }
        }
        let Some(evidence) = evidence else {
            // Convergence target not reached and owner outcome still unknown:
            // the original protection stays.
            return Ok(None);
        };
        let terminal = self
            .metadata
            .store
            .finalize_observed_hot_success(snapshot, &evidence)
            .await?;
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
