//! Policy changes use durable admission; Docker stores policy in SQL and K8s
//! projects it to a captured Deployment without changing its pod template.
use super::ops::validate_recycle_policy_fields;
use crate::models::{AppResult, AppRuntimeInfo, RecyclePolicyRequest};
use crate::service::{AppService, OwnedOperation};
use crate::utils::{map_runtime_error, validate_app_id};

impl AppService {
    pub async fn set_recycle_policy(
        &self,
        app_id: &str,
        request: RecyclePolicyRequest,
    ) -> AppResult<AppRuntimeInfo> {
        validate_app_id(app_id)?;
        validate_recycle_policy_fields(
            request.recycle_enabled,
            request.idle_timeout_seconds,
            request.wake_on_traffic,
        )?;
        let guard = self.acquire_process_release_lock(app_id).await?;
        let result = async {
            self.metadata
                .validate_request_lifecycle(app_id, request.lifecycle_id.as_deref())
                .await?;
            use sha2::Digest as _;
            let fingerprint = hex::encode(sha2::Sha256::digest(
                shared_types::encode_userapp_intent(&request).map_err(|error| {
                    crate::models::AppOperationError::Backend(format!(
                        "Encode policy intent: {error}"
                    ))
                })?,
            ));
            let control = shared_types::UserAppControlRequest {
                lifecycle_id: request.lifecycle_id.clone(),
                request_id: request.request_id.clone(),
            };
            if self
                .replay_control(
                    app_id,
                    &control,
                    shared_types::UserAppOperationKind::SetRecyclePolicy,
                    &fingerprint,
                )
                .await?
                .is_some()
            {
                return self.get_app(app_id).await;
            }
            let previous = self.fetch_runtime_status_or_err(app_id).await?;
            let policy = shared_types::UserAppRuntimePolicy {
                recycle_enabled: request.recycle_enabled,
                idle_timeout_seconds: request.idle_timeout_seconds,
                wake_on_traffic: request.wake_on_traffic,
            };
            let mut operation = OwnedOperation::admit(
                self.metadata.store.clone(),
                shared_types::UserAppAdmission {
                    runtime_policy_on_success: None,
                    app_id: app_id.into(),
                    lifecycle_id: request.lifecycle_id.clone(),
                    request_id: request.request_id.clone(),
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    kind: shared_types::UserAppOperationKind::SetRecyclePolicy,
                    request_fingerprint: fingerprint,
                    command: Some(shared_types::UserAppControlCommand::SetRecyclePolicy {
                        policy: policy.clone(),
                    }),
                    metadata: None,
                },
            )
            .await?;
            let mutation = async {
                operation.bind_lease(&guard).await?;
                let context = operation.execution_context();
                let target = self
                    .runtime
                    .capture_app_mutation_target(&context, previous.resource_version.as_deref())
                    .await
                    .map_err(|error| map_runtime_error("Capture policy target", error))?;
                operation
                    .checkpoint(
                        "applying_runtime_policy",
                        serde_json::json!({"target":target,"policy":policy}),
                    )
                    .await?;
                operation.authorize_mutation().await?;
                if self.config.access_mode == crate::config::AppAccessMode::Kubernetes {
                    guard.mark_mutating()?;
                }
                let result = self.runtime.patch_app_policy_target(&target, &policy).await;
                if matches!(
                    &result,
                    Err(container_runtime_api::ContainerRuntimeError::RequestRejected(_))
                ) {
                    guard.mark_rejected_before_mutation();
                }
                result.map_err(|error| map_runtime_error("Apply captured runtime policy", error))
            }
            .await;
            match mutation {
                Ok(()) => {
                    operation.confirm_effects().await?;
                    operation.succeed().await?;
                    guard.mark_completed();
                }
                Err(error) => {
                    if guard.has_unfinished_mutation() {
                        operation.fail(&error).await?;
                    } else {
                        operation.reject_without_mutation(&error).await?;
                    }
                    return Err(error);
                }
            }
            if previous.replicas == 0
                && let Some(wake) = policy.wake_on_traffic
            {
                self.restore_activity_state(app_id, &previous, wake);
            }
            self.invalidate_deploy_cache().await;
            self.get_app(app_id).await
        }
        .await;
        if result.is_ok() || !guard.has_unfinished_mutation() {
            guard.finish().await?;
        }
        result
    }
}
