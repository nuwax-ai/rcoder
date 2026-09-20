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
            let guard = self.acquire_process_release_lock(app_id).await?;
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
                if guard.has_unfinished_mutation() {
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
            sleep(Duration::from_secs(1)).await;
        }
    }
}
