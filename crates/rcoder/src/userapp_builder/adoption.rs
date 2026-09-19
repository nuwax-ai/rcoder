//! Explicit legacy-resource adoption. SQL binds physical identity; no resource
//! labels are rewritten and no container or volume is recreated.
use crate::app_state::AppState;
use anyhow::{Context as _, Result, anyhow};
use futures::FutureExt as _;
use shared_types::{
    AdoptBuilderRequest, BuilderControlResult, UserAppAdmission, UserAppAdmissionOutcome,
    UserAppExecutionContext, UserAppLifecycleState, UserAppOperationKind, UserAppOperationProgress,
    UserAppOperationRecord, UserAppOperationState, UserAppResourceBinding, UserAppStoreError,
};

fn validate_request(app_id: &str, request: &AdoptBuilderRequest) -> Result<()> {
    for (field, value) in [
        ("app_id", app_id),
        ("lifecycle_id", request.lifecycle_id.as_str()),
        ("request_id", request.request_id.as_str()),
    ] {
        shared_types::validate_identifier(value, field).map_err(|error| anyhow!(error))?;
    }
    if request.expected_container_id.is_empty()
        || request.expected_container_id.len() > 128
        || !request
            .expected_container_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(anyhow!("Expected physical builder ID is required"));
    }
    Ok(())
}

pub(crate) async fn execute(
    state: &AppState,
    app_id: &str,
    request: AdoptBuilderRequest,
) -> Result<BuilderControlResult> {
    validate_request(app_id, &request)?;
    let state = state.clone();
    let app_id = app_id.to_owned();
    tokio::spawn(async move {
        let _local = super::lifecycle::acquire(&app_id).await;
        let input = shared_types::UserAppExecutionInput::new(serde_json::to_string(&request)?);
        let record = match state
            .userapp_store
            .admit_with_input(
                &UserAppAdmission {
                    app_id: app_id.clone(),
                    lifecycle_id: Some(request.lifecycle_id.clone()),
                    request_id: Some(request.request_id.clone()),
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    request_fingerprint: input.digest(),
                    kind: UserAppOperationKind::AdoptBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                },
                Some(&input),
            )
            .await?
        {
            UserAppAdmissionOutcome::Accepted(record) => record,
            UserAppAdmissionOutcome::Existing(record)
                if record.state == UserAppOperationState::Succeeded =>
            {
                return Ok(serde_json::from_value(record.checkpoint)?);
            }
            UserAppAdmissionOutcome::Existing(record)
                if record.state == UserAppOperationState::Pending =>
            {
                record
            }
            UserAppAdmissionOutcome::Existing(record) => {
                return Err(shared_types::BuilderControlError {
                    operation_id: record.operation_id,
                    message: "Builder adoption is not safely retryable in its current state".into(),
                }
                .into());
            }
        };
        run(&state, record).await
    })
    .await
    .context("Observe builder adoption worker")?
}

pub(super) async fn resume_pending(
    state: &AppState,
    pending: &UserAppOperationRecord,
) -> Result<bool> {
    if pending.kind != UserAppOperationKind::AdoptBuilder
        || pending.state != UserAppOperationState::Pending
    {
        return Ok(false);
    }
    let Some(_local) = super::lifecycle::try_acquire(&pending.app_id).await else {
        return Ok(false);
    };
    let current = state
        .userapp_store
        .get_operation(&pending.app_id, &pending.operation_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if current.state != UserAppOperationState::Pending || current.revision != pending.revision {
        return Ok(false);
    }
    let app = state
        .userapp_store
        .get_application(&pending.app_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if app.state != UserAppLifecycleState::Active || app.lifecycle_id != current.lifecycle_id {
        return Err(UserAppStoreError::LifecycleConflict.into());
    }
    run(state, current).await?;
    Ok(true)
}

async fn run(state: &AppState, record: UserAppOperationRecord) -> Result<BuilderControlResult> {
    let executor = uuid::Uuid::new_v4().to_string();
    // 应用共享：builder identifier == 纯 app_id（受理与物理资源定位同一键）
    let instance = record.app_id.clone();
    let claimed = state
        .userapp_store
        .advance(&UserAppOperationProgress {
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
        })
        .await?;
    let context = UserAppExecutionContext {
        app_id: instance,
        lifecycle_id: claimed.lifecycle_id.clone(),
        operation_id: claimed.operation_id.clone(),
        executor_id: executor.clone(),
        request_fingerprint: claimed.request_fingerprint.clone(),
    };
    let mut committing = false;
    let mut lease_uncertain = false;
    let outcome = std::panic::AssertUnwindSafe(async {
        let input = state.userapp_store.read_execution_input(&context).await?;
        let request: AdoptBuilderRequest =
            serde_json::from_str(input.encoded()).context("Decode builder adoption request")?;
        if request.lifecycle_id != context.lifecycle_id {
            return Err(anyhow!("Stored adoption identity mismatch"));
        }
        let mut lease = super::dev_cleanup::BuilderOperation::new(
            state
                .runtime()
                .acquire_builder_operation(&context.app_id)
                .await?,
        );
        let inspected = state
            .runtime()
            .capture_builder_adoption(&context, &request.expected_container_id)
            .await;
        // Inspection has no remote writes. Release the runtime read fence before
        // atomic SQL binding; SQL admission still excludes concurrent controls.
        lease_uncertain = true;
        lease
            .finish_read_only()
            .await
            .map_err(|error| anyhow!(error))?;
        lease_uncertain = false;

        let target = inspected?;
        target.validate().map_err(|error| anyhow!(error))?;
        if target.context != context {
            return Err(anyhow!("Adoption capture context mismatch"));
        }
        let workload = target
            .workload
            .ok_or_else(|| anyhow!("Builder disappeared before adoption"))?;
        let binding = UserAppResourceBinding {
            app_id: context.app_id.clone(),
            lifecycle_id: context.lifecycle_id.clone(),
            service_type: shared_types::ServiceType::UserappBuilder,
            physical_uid: workload.uid,
            adopted_by_operation: context.operation_id.clone(),
        };
        let result = BuilderControlResult {
            operation_id: context.operation_id.clone(),
            was_existing: true,
            container: None,
        };
        committing = true;
        state
            .userapp_store
            .commit_resource_binding(
                &binding,
                &UserAppOperationProgress {
                    app_id: context.app_id.clone(),
                    lifecycle_id: context.lifecycle_id.clone(),
                    operation_id: context.operation_id.clone(),
                    expected_revision: claimed.revision,
                    executor_id: executor.clone(),
                    state: UserAppOperationState::Succeeded,
                    step: "physical_resource_adopted".into(),
                    checkpoint: serde_json::to_value(&result)?,
                    error_code: None,
                    error_message: None,
                },
            )
            .await?;
        Ok::<_, anyhow::Error>(result)
    })
    .catch_unwind()
    .await
    .map_err(|_| anyhow!("Builder adoption worker panicked"))
    .and_then(|result| result);
    match outcome {
        Ok(result) => Ok(result),
        Err(error) => {
            // A failed SQL commit may have an uncertain result. Never overwrite
            // a terminal outcome or manufacture a fresh operation for this UID.
            let message = format!("{error:#}");
            if let Some(current) = state
                .userapp_store
                .get_operation(&context.app_id, &context.operation_id)
                .await?
                && current.state == UserAppOperationState::Running
                && current.revision == claimed.revision
            {
                state
                    .userapp_store
                    .advance(&UserAppOperationProgress {
                        app_id: context.app_id.clone(),
                        lifecycle_id: context.lifecycle_id.clone(),
                        operation_id: context.operation_id.clone(),
                        expected_revision: current.revision,
                        executor_id: executor,
                        state: if committing || lease_uncertain {
                            UserAppOperationState::RecoveryRequired
                        } else {
                            UserAppOperationState::Failed
                        },
                        step: "adoption_failed".into(),
                        checkpoint: current.checkpoint,
                        error_code: Some("ERR_BACKEND_ERROR".into()),
                        error_message: Some(message.clone()),
                    })
                    .await?;
            }
            Err(shared_types::BuilderControlError {
                operation_id: context.operation_id,
                message,
            }
            .into())
        }
    }
}

pub(crate) async fn capture_bound_target(
    state: &AppState,
    context: &UserAppExecutionContext,
) -> Result<shared_types::BuilderControlTarget> {
    // Query the compute resource independently of Pod existence: a stopped
    // StatefulSet still has the UID used by its durable lifecycle binding.
    let raw = state.runtime().inspect_builder_candidate(context).await?;
    let Some(workload) = raw.workload.as_ref() else {
        return Ok(state.runtime().capture_builder_control(context).await?);
    };
    let uid = workload.uid.as_str();
    let binding = state
        .userapp_store
        .get_resource_binding(&shared_types::ServiceType::UserappBuilder, uid)
        .await?;
    Ok(state
        .runtime()
        .capture_bound_builder_control(context, binding.as_ref())
        .await?)
}

/// 只读活体校验：以 lifecycle 构造 capture context（builder identifier =
/// 纯 app_id），校验物理负载未被替换。应用共享，无归属注解可比对。
pub(super) async fn verify_live_builder(
    state: &AppState,
    app_id: &str,
    instance: &str,
    physical_id: &str,
) -> Result<()> {
    let app = state
        .userapp_store
        .get_application(app_id)
        .await?
        .ok_or(UserAppStoreError::NotFound)?;
    if app.state != UserAppLifecycleState::Active {
        return Err(UserAppStoreError::LifecycleConflict.into());
    }
    let context = UserAppExecutionContext {
        app_id: instance.into(),
        lifecycle_id: app.lifecycle_id,
        operation_id: "read-only-verification".into(),
        executor_id: "reader".into(),
        request_fingerprint: "0".repeat(64),
    };
    let target = capture_bound_target(state, &context).await?;
    let actual = target.pod.as_ref().map(|pod| pod.uid.as_str()).or_else(|| {
        target
            .workload
            .as_ref()
            .map(|workload| workload.uid.as_str())
    });
    if actual != Some(physical_id) {
        return Err(anyhow!(
            "Builder physical identity changed during verification"
        ));
    }
    Ok(())
}

pub(crate) fn routes() -> axum::Router<std::sync::Arc<AppState>> {
    axum::Router::new()
        .route(
            "/api/v1/userapp/{app_id}/builder/adopt",
            axum::routing::post(adopt_builder),
        )
        .layer(axum::middleware::from_fn(
            shared_types::userapp_http::envelope_errors,
        ))
}

/// Adopt an existing development builder.
///
/// Bind the expected physical resource to the current application lifecycle.
/// Ownership and lifecycle mismatches are rejected before any binding is written.
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/builder/adopt",
    params(("app_id" = String, Path, description = "Application identifier")),
    request_body = AdoptBuilderRequest,
    responses((status = 200, description = "HttpResult envelope; data contains operation_id and was_existing. A conflicting physical identity or lifecycle is rejected before binding.", body = shared_types::HttpResult<serde_json::Value>)),
    tag = "Userapp · dev · 工作区与工具链",
)]
pub(crate) async fn adopt_builder(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    axum::extract::Path(app_id): axum::extract::Path<String>,
    body: Result<axum::Json<AdoptBuilderRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<shared_types::HttpResult<serde_json::Value>, shared_types::AppError> {
    let axum::Json(request) = body.map_err(|_| {
        shared_types::AppError::validation_error("Invalid builder adoption request")
    })?;
    let result = execute(&state, &app_id, request)
        .await
        .map_err(|error| super::control_error(&error))?;
    let operation_id = result.operation_id.clone();
    let data = serde_json::to_value(result).map_err(|error| {
        shared_types::AppError::internal_server_error(&format!(
            "Encode builder adoption result: {error}"
        ))
    })?;
    Ok(shared_types::HttpResult::success(data).with_operation_id(operation_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn adoption_requires_explicit_lifecycle_request_and_physical_identity() {
        let request = AdoptBuilderRequest {
            lifecycle_id: "life".into(),
            request_id: "request".into(),
            expected_container_id: "physical-uid".into(),
        };
        validate_request("app", &request).expect("valid");
        for field in ["lifecycle_id", "request_id", "expected_container_id"] {
            let mut encoded = serde_json::to_value(&request).expect("encode");
            encoded[field] = serde_json::json!("");
            let invalid = serde_json::from_value(encoded).expect("request shape");
            assert!(validate_request("app", &invalid).is_err(), "{field}");
        }
        let mut invalid = request;
        invalid.expected_container_id = "../replacement".into();
        assert!(validate_request("app", &invalid).is_err());
    }
}
