use super::*;
use axum::{extract::Path, response::IntoResponse};

/// 查询计算控制操作状态与恢复阶段
#[utoipa::path(get, path = "/computer/pod/operations/{app_id}/{operation_id}",
    params(("app_id" = String, Path, description = "Application ID"),
           ("operation_id" = String, Path, description = "Compute operation ID")),
    responses((status = 200, description = "Compute state and recovery stage; no private runtime credentials",
        body = HttpResult<crate::userapp_builder::compute_control::ComputeOperationView>)),
    tag = "pod")]
pub async fn pod_compute_operation(
    State(state): State<Arc<AppState>>,
    Path((app_id, operation_id)): Path<(String, String)>,
) -> Result<axum::response::Response, AppError> {
    let record = state
        .userapp_store
        .get_compute_control(&app_id, &operation_id)
        .await
        .map_err(|error| {
            AppError::with_message(
                shared_types::error_codes::ERR_BACKEND_ERROR,
                error.to_string(),
            )
        })?
        .ok_or_else(|| {
            AppError::with_message(
                shared_types::error_codes::ERR_NOT_FOUND,
                "Compute operation not found",
            )
        })?;
    Ok(HttpResult::success(
        crate::userapp_builder::compute_control::ComputeOperationView::from(record),
    )
    .into_response())
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
pub struct ComputeRecoveryRequest {
    /// Revision returned by the operation query; stale requests are rejected.
    pub expected_revision: i64,
}

/// 恢复计算控制操作（续排空或终局确认，不重放）
#[utoipa::path(post, path = "/computer/pod/operations/{app_id}/{operation_id}/recover",
    params(("app_id" = String, Path, description = "Application ID"),
           ("operation_id" = String, Path, description = "Original compute operation ID")),
    request_body = ComputeRecoveryRequest,
    responses((status = 202, description = "Resume original draining or finalize confirmed compute effects without replay",
        body = HttpResult<crate::userapp_builder::compute_control::ComputeOperationView>),
        (status = 409, description = "Stale revision or another control owns the scope"),
        (status = 400, description = "Captured runtime writes require reconciliation before retry")),
    tag = "pod")]
pub async fn pod_compute_recover(
    State(state): State<Arc<AppState>>,
    Path((app_id, operation_id)): Path<(String, String)>,
    axum::Json(request): axum::Json<ComputeRecoveryRequest>,
) -> Result<axum::response::Response, AppError> {
    let view = crate::userapp_builder::compute_control::recover(
        &state,
        &app_id,
        &operation_id,
        request.expected_revision,
    )
    .await
    .map_err(|error| crate::userapp_builder::control_error(&error))?;
    let operation_id = view.operation_id.clone();
    Ok((
        axum::http::StatusCode::ACCEPTED,
        HttpResult::success(view).with_operation_id(operation_id),
    )
        .into_response())
}
