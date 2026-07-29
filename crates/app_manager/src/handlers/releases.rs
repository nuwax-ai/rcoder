use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use shared_types::AppError;

use crate::models::{
    ActivateReleaseRequest, ConfirmReleaseRequest, PrepareReleaseRequest, ReleaseInfo,
    ReleaseListResponse,
};

use super::AppManagerState;

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/prepare",
    params(("app_id" = String, Path)),
    request_body = PrepareReleaseRequest,
    responses((status = 200, body = ReleaseInfo)),
    tag = "应用发布"
)]
pub async fn prepare_release(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<PrepareReleaseRequest>,
) -> Result<Json<ReleaseInfo>, AppError> {
    Ok(Json(
        state.app_service.prepare_release(&app_id, request).await?,
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/activate",
    params(("app_id" = String, Path), ("release_id" = String, Path)),
    request_body = ActivateReleaseRequest,
    responses((status = 200, body = ReleaseInfo)),
    tag = "应用发布"
)]
pub async fn activate_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
    Json(_request): Json<ActivateReleaseRequest>,
) -> Result<Json<ReleaseInfo>, AppError> {
    Ok(Json(
        state
            .app_service
            .activate_release(&app_id, &release_id)
            .await?,
    ))
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/confirm",
    params(("app_id" = String, Path), ("release_id" = String, Path)),
    request_body = ConfirmReleaseRequest,
    responses((status = 200, body = ReleaseInfo)),
    tag = "应用发布"
)]
pub async fn confirm_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
    Json(request): Json<ConfirmReleaseRequest>,
) -> Result<Json<ReleaseInfo>, AppError> {
    Ok(Json(
        state
            .app_service
            .confirm_release(&app_id, &release_id, request.healthy, request.message)
            .await?,
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/releases",
    params(("app_id" = String, Path)),
    responses((status = 200, body = ReleaseListResponse)),
    tag = "应用发布"
)]
pub async fn list_releases(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<ReleaseListResponse>, AppError> {
    Ok(Json(state.app_service.list_releases(&app_id).await?))
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/releases/{release_id}/delete",
    params(("app_id" = String, Path), ("release_id" = String, Path)),
    responses((status = 200, description = "Release deleted")),
    tag = "应用发布"
)]
pub async fn delete_release(
    State(state): State<Arc<AppManagerState>>,
    Path((app_id, release_id)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, AppError> {
    state
        .app_service
        .delete_release(&app_id, &release_id)
        .await?;
    Ok(Json(serde_json::json!({"success": true})))
}
