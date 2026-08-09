//! 应用查询 handler（logs / health / stats / events / file-logs）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{HealthInfo, ResourceStats};

/// 获取应用健康状态（由运行时状态派生）
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/health",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<HealthInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_health(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<HealthInfo>>, AppError> {
    info!("[APP] getting app health: {}", app_id);
    let runtime = state.app_service.get_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime.health)))
}

/// 获取应用资源使用（best-effort：restart_count 来自运行时；CPU/内存需 metrics-server）
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/stats",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<ResourceStats>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_stats(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<ResourceStats>>, AppError> {
    info!("[APP] getting app stats: {}", app_id);
    let stats = state.app_service.get_app_stats(&app_id).await?;
    Ok(Json(HttpResult::success(stats)))
}

/// 获取应用事件（best-effort：当前返回空，TODO 接 K8s events）
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/events",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<Vec<String>>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_events(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<Vec<container_runtime_api::AppEventInfo>>>, AppError> {
    info!("[APP] getting app events: {}", app_id);
    let events = state.app_service.get_app_events(&app_id).await?;
    Ok(Json(HttpResult::success(events)))
}
