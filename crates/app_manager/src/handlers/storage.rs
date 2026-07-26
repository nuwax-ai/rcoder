//! 持久存储管理 handler（v2 §5.4）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{DestroyStorageRequest, PaginatedResponse, QueryStorageRequest, StorageInfo};

/// 查询应用持久存储状态
#[utoipa::path(
    get,
    path = "/api/v1/apps/{app_id}/storage",
    params(("app_id" = String, Path, description = "应用 ID")),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<StorageInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn get_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<StorageInfo>>, AppError> {
    info!("[APP] getting app storage: {}", app_id);
    let info = state.app_service.get_app_storage(&app_id).await?;
    Ok(Json(HttpResult::success(info)))
}

/// 清空应用持久存储内容（留 PVC，可恢复；仅当 app 已 delete 时允许，否则 409 INVALID_STATE）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/storage/clear",
    params(("app_id" = String, Path, description = "应用 ID")),
    responses(
        (status = 200, description = "清空成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用仍存在，需先 delete", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state))]
pub async fn clear_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] clearing app storage: {}", app_id);
    state.app_service.clear_app_storage(&app_id).await?;
    Ok(Json(HttpResult::success("存储已清空".to_string())))
}

/// 销毁应用持久存储 PVC（高危·不可逆·释放配额；需 body `confirm=app_id`，仅 app 已 delete 后允许）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/storage/destroy",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = DestroyStorageRequest,
    responses(
        (status = 200, description = "PVC 已销毁", body = HttpResult<String>),
        (status = 400, description = "confirm 缺失/不匹配 app_id", body = HttpResult<String>),
        (status = 409, description = "应用仍存在，需先 delete", body = HttpResult<String>),
        (status = 500, description = "PVC 卡 Terminating，需运维介入（pvc-protection finalizer 未移除）", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, req))]
pub async fn destroy_app_storage(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(req): Json<DestroyStorageRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] destroying app PVC: {}", app_id);
    state
        .app_service
        .destroy_app_storage(&app_id, &req.confirm)
        .await?;
    Ok(Json(HttpResult::success(
        "PVC 已销毁，配额已释放".to_string(),
    )))
}

/// 分页查询持久存储（强制分页，无全量模式）
#[utoipa::path(
    post,
    path = "/api/v1/apps/storage/query",
    request_body = QueryStorageRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<StorageInfo>>),
        (status = 400, description = "分页参数错误", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn query_storage(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<QueryStorageRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<StorageInfo>>>, AppError> {
    info!(
        "[APP] querying storage list: page={} page_size={}",
        request.page, request.page_size
    );
    let resp = state.app_service.query_storage(request).await?;
    Ok(Json(HttpResult::success(resp)))
}
