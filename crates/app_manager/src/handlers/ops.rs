//! 应用操作 handler（start / stop / restart）

use std::sync::Arc;

use axum::extract::{Path, State};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::AppRuntimeInfo;

/// 启动应用（scale replicas = 1）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/start",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "启动成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn start_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<axum::Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] starting app: {}", app_id);
    let runtime = state.app_service.start_app(&app_id).await?;
    Ok(axum::Json(HttpResult::success(runtime)))
}

/// 停止应用（scale replicas = 0）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/stop",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "停止成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn stop_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<axum::Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] stopping app: {}", app_id);
    let runtime = state.app_service.stop_app(&app_id).await?;
    Ok(axum::Json(HttpResult::success(runtime)))
}

/// 重启应用（rollout restart）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/restart",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "重启成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn restart_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<axum::Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] restarting app: {}", app_id);
    let runtime = state.app_service.restart_app(&app_id).await?;
    Ok(axum::Json(HttpResult::success(runtime)))
}
