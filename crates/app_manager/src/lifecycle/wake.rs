//! Traffic wake uses the same durable admission and physical identity fences as
//! explicit controls. A readiness timeout never authorizes another mutation.

use std::time::Duration;

use shared_types::{UserAppAdmission, UserAppOperationKind, WakeOutcome};
use tokio::time::{Instant, sleep, timeout_at};

use crate::models::{AppOperationError, AppResult};
use crate::service::{AppService, OwnedOperation};
use crate::utils::{map_runtime_error, validate_app_id};

impl AppService {
    pub(crate) async fn wake_app_on_traffic(
        &self,
        app_id: &str,
        budget: Duration,
    ) -> AppResult<WakeOutcome> {
        validate_app_id(app_id)?;
        let deadline = Instant::now() + budget;
        let preflight = async {
            self.metadata
                .store
                .check_compute_access(app_id, shared_types::UserAppOperationScope::Prod, false)
                .await?;
            // An existing operation (including one awaiting recovery) is not a
            // readiness wait. Do not consume the wake budget polling its lease:
            // return the conflict so the caller can observe/recover that operation.
            let guard = self.try_acquire_process_release_lock(app_id).await?;
            let identity = self
                .metadata
                .store
                .get_application(app_id)
                .await?
                .ok_or_else(|| {
                    AppOperationError::NotFound(format!("Application identity not found: {app_id}"))
                })?;
            self.metadata
                .validate_request_lifecycle(app_id, Some(&identity.lifecycle_id))
                .await?;
            let status = self.fetch_runtime_status_or_err(app_id).await?;
            if status.wake_on_traffic == Some(false) {
                return Err(AppOperationError::InvalidState(
                    "Application is intentionally stopped".into(),
                ));
            }
            Ok::<_, AppOperationError>((guard, identity, status))
        };
        let (guard, identity, previous) = match timeout_at(deadline, preflight).await {
            Ok(result) => result?,
            Err(_) => return Ok(WakeOutcome::Timeout),
        };
        // Even an already-running workload must be bound to the current lifecycle.
        // The operation records that observation and prevents a stale cache from
        // bypassing a concurrent durable control operation on another replica.
        use sha2::Digest as _;
        let intent = shared_types::encode_userapp_intent(&serde_json::json!({
            "trigger":"traffic", "lifecycle_id":identity.lifecycle_id,
        }))
        .map_err(|error| AppOperationError::Backend(format!("Encode wake intent: {error}")))?;
        let admission = OwnedOperation::admit(
            self.metadata.store.clone(),
            UserAppAdmission {
                runtime_policy_on_success: None,
                command: Some(shared_types::UserAppControlCommand::Start { traffic: true }),
                app_id: app_id.into(),
                lifecycle_id: Some(identity.lifecycle_id.clone()),
                request_id: None,
                operation_id: uuid::Uuid::new_v4().to_string(),
                kind: UserAppOperationKind::Start,
                request_fingerprint: hex::encode(sha2::Sha256::digest(intent)),
                metadata: None,
            },
        );
        let mut operation = match timeout_at(deadline, admission).await {
            Ok(result) => result?,
            // A cancelled SQL commit may have succeeded. Do not retry admission;
            // persisted Pending/Running records are reconciled by recovery.
            Err(_) => return Ok(WakeOutcome::Timeout),
        };
        let activation = async {
            operation.bind_lease(&guard).await?;
            let context = operation.execution_context();
            let target = self
                .runtime
                .capture_app_mutation_target(&context, previous.resource_version.as_deref())
                .await
                .map_err(|error| map_runtime_error("Capture traffic wake target", error))?;
            operation
                .checkpoint("traffic_wake_target", serde_json::json!({"target":target}))
                .await?;
            // Admission and captured identity are authoritative. A stale local
            // deletion/manual-stop flag cannot override another replica's committed
            // explicit start or policy change. A new stop still serializes on guard.
            operation.authorize_mutation().await?;
            self.activity.prepare_traffic_wake(app_id);
            let outcome = if previous.phase == "Running" {
                WakeOutcome::AlreadyRunning
            } else {
                guard.mark_mutating()?;
                self.runtime
                    .start_app_target(&target)
                    .await
                    .map_err(|error| {
                        map_runtime_error("Start captured traffic wake target", error)
                    })?;
                // Both runtimes have returned from their complete write chain.
                // Persist that boundary before entering read-only observation.
                // A cancelled/failed commit still retains the original lease;
                // recovery must read the durable checkpoint, never a local flag.
                operation
                    .checkpoint(
                        "traffic_wake_observing",
                        serde_json::json!({
                            "target": target,
                            "start_write_acknowledged": true,
                        }),
                    )
                    .await?;
                self.wait_for_captured_wake(&target).await?
            };
            Ok(outcome)
        };
        let observation = timeout_at(deadline, activation).await;
        let timed_out = observation.is_err();
        let result = observation.unwrap_or_else(|_| {
            Err(AppOperationError::Backend(
                "Traffic wake deadline exceeded".into(),
            ))
        });
        match result {
            Ok(outcome) => {
                operation.succeed().await?;
                guard.mark_completed();
                if !self.activity.try_mark_woken(app_id) {
                    guard.finish().await?;
                    return Err(AppOperationError::InvalidState(
                        "Application was stopped during traffic wake completion".into(),
                    ));
                }
                guard.finish().await?;
                Ok(outcome)
            }
            Err(error) => {
                if operation.has_confirmed_wake_write() {
                    // No write future survives this boundary: the entire runtime
                    // start returned before the durable observation checkpoint.
                    // Failure/timeout of later reads does not create an unknown write.
                    operation.fail_confirmed_wake_observation(&error).await?;
                    guard.mark_completed();
                    guard.finish().await?;
                } else if guard.has_unfinished_mutation() {
                    operation.fail(&error).await?;
                } else {
                    operation.reject_without_mutation(&error).await?;
                    guard.finish().await?;
                }
                if timed_out {
                    Ok(WakeOutcome::Timeout)
                } else {
                    Err(error)
                }
            }
        }
    }

    pub(crate) async fn wait_for_captured_wake(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> AppResult<WakeOutcome> {
        let context = &target.context;
        let app_id = &context.app_id;
        loop {
            if self.activity.is_wake_blocked(app_id) {
                return Err(AppOperationError::InvalidState(
                    "Application was stopped during traffic wake".into(),
                ));
            }
            let current = self
                .runtime
                .capture_app_mutation_target(context, None)
                .await
                .map_err(|error| map_runtime_error("Verify traffic wake identity", error))?;
            if current.resource.uid != target.resource.uid
                || current.resource.name != target.resource.name
            {
                return Err(AppOperationError::Conflict(
                    "Traffic wake target was replaced".into(),
                ));
            }
            let status = self.fetch_runtime_status_or_err(app_id).await?;
            if status.wake_on_traffic == Some(false) {
                return Err(AppOperationError::InvalidState(
                    "Application was stopped during traffic wake".into(),
                ));
            }
            if status.phase == "Running" {
                // Bind the status observation on both sides to the captured UID.
                let after = self
                    .runtime
                    .capture_app_mutation_target(context, None)
                    .await
                    .map_err(|error| map_runtime_error("Confirm traffic wake identity", error))?;
                if after.resource.uid != target.resource.uid
                    || after.resource.name != target.resource.name
                {
                    return Err(AppOperationError::Conflict(
                        "Traffic wake target was replaced".into(),
                    ));
                }
                return Ok(WakeOutcome::Ready);
            }
            if status.phase == "Error" {
                return Err(AppOperationError::Backend(status.message.unwrap_or_else(
                    || "Application failed during traffic wake".into(),
                )));
            }
            if let Some(reason) = self.observe_wake_startup_failure(target).await? {
                return Err(AppOperationError::Backend(reason));
            }
            sleep(Duration::from_secs(1)).await;
        }
    }
    /// Diagnostic observation only: a failed app-cli never authorizes lease release.
    /// Exec binds the physical Pod/container and generation, so NotReady Services
    /// and stale IP routing cannot hide or substitute the observed owner.
    async fn observe_wake_startup_failure(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> AppResult<Option<String>> {
        let observation = async {
            let snapshot = self
                .runtime
                .app_env_snapshot(&target.context.app_id)
                .await
                .ok()?;
            let generation = snapshot.env.get(shared_types::APP_DEPLOY_GENERATION_ID)?;
            let physical = self
                .runtime
                .capture_app_configuration_target(&target.context, generation)
                .await
                .ok()?;
            let output = self.runtime.exec_app_configuration_target(
                &target.context, &physical,
                vec!["sh".into(), "-c".into(),
                    "curl --silent --show-error --noproxy '*' --connect-timeout 1 --max-time 2 http://127.0.0.1:3010/v1/deploy/status".into()],
            ).await.ok()?;
            if output.exit_code != 0 {
                return None;
            }
            let body: serde_json::Value = serde_json::from_str(&output.stdout).ok()?;
            let data = body.get("data").unwrap_or(&body);
            let phase: shared_types::AppCliDeployPhase =
                serde_json::from_value(data.get("phase")?.clone()).ok()?;
            if phase != shared_types::AppCliDeployPhase::Failed {
                return None;
            }
            // Failure detail belongs to the serving deployment, not this newly
            // admitted wake. Never require its operation ID to equal the wake ID.
            let detail = if let Some(value) = data.get("operation").filter(|v| !v.is_null()) {
                let operation: shared_types::AppDeploymentOperation =
                    serde_json::from_value(value.clone()).ok()?;
                if operation.deployment_generation_id != *generation {
                    return None;
                }
                data.get("error")
                    .and_then(serde_json::Value::as_str)
                    .or(operation.error.as_deref())
                    .map(|error| error.chars().take(2048).collect::<String>())
            } else {
                None
            };
            let after = self
                .runtime
                .capture_app_configuration_target(&target.context, generation)
                .await
                .ok()?;
            if physical != after {
                return None;
            }
            Some((generation.clone(), detail))
        };
        let Some((generation, detail)) = tokio::time::timeout(Duration::from_secs(4), observation)
            .await
            .ok()
            .flatten()
        else {
            return Ok(None);
        };
        let current = self
            .runtime
            .capture_app_mutation_target(&target.context, None)
            .await
            .map_err(|error| map_runtime_error("Confirm failed wake owner", error))?;
        if current.resource.uid != target.resource.uid
            || current.resource.name != target.resource.name
        {
            return Err(AppOperationError::Conflict(
                "Traffic wake target was replaced during startup diagnosis".into(),
            ));
        }
        let mut reason = format!(
            "Application startup failed (app={}, generation={}, wake operation={})",
            target.context.app_id, generation, target.context.operation_id
        );
        if let Some(detail) = detail.filter(|value| !value.trim().is_empty()) {
            reason.push_str(": ");
            reason.push_str(&detail);
        } else {
            reason.push_str("; inspect app-cli service logs for the failure details");
        }
        Ok(Some(reason))
    }
}
