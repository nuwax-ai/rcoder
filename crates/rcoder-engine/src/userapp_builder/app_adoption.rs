//! Explicit production-controller adoption. SQL binds physical identity; the
//! K8s backend additionally stamps the current lifecycle identity onto the
//! adopted Deployment. No pod, volume, or controller is recreated.
use crate::app_state::AppState;
use anyhow::{Context as _, Result, anyhow};
use futures::FutureExt as _;
use shared_types::{
    AdoptApplicationRequest, AppAdoptionTarget, BuilderControlResult, UserAppAdmission,
    UserAppAdmissionOutcome, UserAppExecutionContext, UserAppLifecycleState, UserAppOperationKind,
    UserAppOperationProgress, UserAppOperationRecord, UserAppOperationState,
    UserAppResourceBinding, UserAppStoreError,
};

fn validate_request(app_id: &str, request: &AdoptApplicationRequest) -> Result<()> {
    for (field, value) in [
        ("app_id", app_id),
        ("lifecycle_id", request.lifecycle_id.as_str()),
        ("request_id", request.request_id.as_str()),
    ] {
        shared_types::validate_identifier(value, field).map_err(|error| anyhow!(error))?;
    }
    if request.expected_resource_uid.is_empty()
        || request.expected_resource_uid.len() > 128
        || !request
            .expected_resource_uid
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(anyhow!("Expected physical resource UID is required"));
    }
    Ok(())
}

pub(crate) async fn execute(
    state: &AppState,
    app_id: &str,
    request: AdoptApplicationRequest,
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
                    kind: UserAppOperationKind::AdoptApplication,
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
                    message: "Application adoption is not safely retryable in its current state"
                        .into(),
                }
                .into());
            }
        };
        run(&state, record).await
    })
    .await
    .context("Observe application adoption worker")?
}

pub(super) async fn resume_pending(
    state: &AppState,
    pending: &UserAppOperationRecord,
) -> Result<bool> {
    if pending.kind != UserAppOperationKind::AdoptApplication
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
        app_id: claimed.app_id.clone(),
        lifecycle_id: claimed.lifecycle_id.clone(),
        operation_id: claimed.operation_id.clone(),
        executor_id: executor.clone(),
        request_fingerprint: claimed.request_fingerprint.clone(),
    };
    let mut committing = false;
    let outcome = std::panic::AssertUnwindSafe(async {
        let input = state.userapp_store.read_execution_input(&context).await?;
        let request: AdoptApplicationRequest =
            serde_json::from_str(input.encoded()).context("Decode application adoption request")?;
        if request.lifecycle_id != context.lifecycle_id {
            return Err(anyhow!("Stored adoption identity mismatch"));
        }
        // Live verification by name+expected UID; absence or a foreign identity
        // is a clean rejection, never an adoption by name alone.
        let target: AppAdoptionTarget = state
            .runtime()
            .capture_app_adoption(&context, &request.expected_resource_uid)
            .await?
            .ok_or_else(|| anyhow!("Production resource is absent or not adoptable"))?;
        if target.context != context {
            return Err(anyhow!("Adoption capture context mismatch"));
        }
        // Registration first: the K8s stamp is preconditioned on the verified
        // UID+version, so a concurrently replaced controller fails here before
        // any SQL binding exists.
        state.runtime().bind_app_adoption(&target).await?;
        // Post-bind verification: the adopted resource must now validate under
        // the current lifecycle through the ordinary capture path (no expected
        // version — registration itself bumped the resource version on K8s).
        state
            .runtime()
            .capture_app_mutation_target(&context, None)
            .await?;
        let binding = UserAppResourceBinding {
            app_id: context.app_id.clone(),
            lifecycle_id: context.lifecycle_id.clone(),
            service_type: shared_types::ServiceType::Userapp,
            physical_uid: target.resource.uid.clone(),
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
    .map_err(|_| anyhow!("Application adoption worker panicked"))
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
                        state: if committing {
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

/// Binding-aware prod capture for adopted resources whose immutable native
/// identity (Docker labels) predates the current lifecycle: consult the
/// durable binding exactly like the builder's `capture_bound_target`.
pub(crate) async fn capture_bound_app_target(
    state: &AppState,
    context: &UserAppExecutionContext,
) -> Result<shared_types::UserAppMutationTarget> {
    match state
        .runtime()
        .capture_app_mutation_target(context, None)
        .await
    {
        Ok(target) => Ok(target),
        // Only the identity rejection is binding-recoverable; backend and
        // absence failures propagate unchanged.
        Err(error @ container_runtime_api::ContainerRuntimeError::Conflict(_)) => {
            let conflict = anyhow::Error::new(error);
            let uid = match state
                .runtime()
                .adopted_app_physical_uid(context)
                .await
                .map_err(anyhow::Error::new)?
            {
                Some(uid) => uid,
                None => return Err(conflict),
            };
            let binding = state
                .userapp_store
                .get_resource_binding(&shared_types::ServiceType::Userapp, &uid)
                .await?;
            let Some(binding) = binding.filter(|binding| binding.validate(context, &uid).is_ok())
            else {
                return Err(conflict);
            };
            state
                .runtime()
                .capture_bound_app_control(context, &binding)
                .await
                .map_err(anyhow::Error::new)
        }
        Err(error) => Err(anyhow::Error::new(error)),
    }
}

pub fn routes() -> axum::Router<std::sync::Arc<AppState>> {
    axum::Router::new()
        .route(
            "/api/v1/userapp/{app_id}/prod/adopt",
            axum::routing::post(adopt_application),
        )
        .layer(axum::middleware::from_fn(
            shared_types::userapp_http::envelope_errors,
        ))
}

/// Adopt an existing production controller.
///
/// Bind the expected physical resource to the current application lifecycle.
/// Ownership and lifecycle mismatches are rejected before any binding write.
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/prod/adopt",
    params(("app_id" = String, Path, description = "Application identifier")),
    request_body = AdoptApplicationRequest,
    responses((status = 200, description = "HttpResult envelope; data contains operation_id and was_existing. A foreign physical identity is rejected before binding.", body = shared_types::HttpResult<serde_json::Value>)),
    tag = "Userapp · prod · 计算控制",
)]
pub(crate) async fn adopt_application(
    axum::extract::State(state): axum::extract::State<std::sync::Arc<AppState>>,
    axum::extract::Path(app_id): axum::extract::Path<String>,
    body: Result<axum::Json<AdoptApplicationRequest>, axum::extract::rejection::JsonRejection>,
) -> Result<shared_types::HttpResult<serde_json::Value>, shared_types::AppError> {
    let axum::Json(request) = body.map_err(|_| {
        shared_types::AppError::validation_error("Invalid application adoption request")
    })?;
    let result = execute(&state, &app_id, request)
        .await
        .map_err(|error| super::control_error(&error))?;
    let operation_id = result.operation_id.clone();
    let data = serde_json::to_value(result).map_err(|error| {
        shared_types::AppError::internal_server_error(&format!(
            "Encode application adoption result: {error}"
        ))
    })?;
    Ok(shared_types::HttpResult::success(data).with_operation_id(operation_id))
}
