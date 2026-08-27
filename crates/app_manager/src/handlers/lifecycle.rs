//! 应用生命周期 handler（create / query / get / update / delete）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{
    AppRuntimeInfo, DeleteAppRequest, PaginatedResponse, QueryAppsRequest, UpdateAppRequest,
};

// create REST 面已删除（统一走 POST /{app_id}/start：不存在则由发布链/ url 部署自动创建）。

/// 查询应用列表
///
/// 实时查集群 + 过滤/分页；仅 status/app_ids 过滤生效。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/query",
    request_body = QueryAppsRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<PaginatedResponse<AppRuntimeInfo>>),
        (status = 500, description = "集群查询失败", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state, request))]
pub async fn query_apps(
    State(state): State<Arc<AppManagerState>>,
    Json(request): Json<QueryAppsRequest>,
) -> Result<Json<HttpResult<PaginatedResponse<AppRuntimeInfo>>>, AppError> {
    info!("[APP] querying apps");
    let response = state.app_service.query_apps(request).await?;
    Ok(Json(HttpResult::success(response)))
}

/// 对账接口：列出集群中所有 rcoder 托管的应用运行时状态
///
/// 供 Java 在 rcoder/自身重启后对账（rcoder 不持久化 app 元数据）。
#[utoipa::path(
    get,
    path = "/api/v1/userapp/runtime",
    responses(
        (status = 200, description = "对账成功", body = HttpResult<Vec<AppRuntimeInfo>>),
        (status = 500, description = "集群查询失败", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state))]
pub async fn list_app_runtimes(
    State(state): State<Arc<AppManagerState>>,
) -> Result<Json<HttpResult<Vec<AppRuntimeInfo>>>, AppError> {
    info!("[APP] reconcile: listing all app runtimes");
    let runtimes = state.app_service.list_app_runtimes().await?;
    Ok(Json(HttpResult::success(runtimes)))
}

/// 获取应用运行时详情（实时查集群）
#[utoipa::path(
    get,
    path = "/api/v1/userapp/{app_id}",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    responses(
        (status = 200, description = "查询成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state))]
pub async fn get_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] getting app runtime: {}", app_id);
    let runtime = state.app_service.get_app(&app_id).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 更新应用（全量替换 desired state）
///
/// rcoder 无状态：调用方需发送完整新状态（`image` 必填）。K8s SSA re-apply 幂等，
/// Docker 重建容器；工作空间目录保留。详见设计文档 §5.2。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/update",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = UpdateAppRequest,
    responses(
        (status = 200, description = "更新成功", body = HttpResult<AppRuntimeInfo>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state, request))]
pub async fn update_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<UpdateAppRequest>,
) -> Result<Json<HttpResult<AppRuntimeInfo>>, AppError> {
    info!("[APP] updating app: {}", app_id);
    let runtime = state.app_service.update_app(&app_id, request).await?;
    Ok(Json(HttpResult::success(runtime)))
}

/// 删除应用
///
/// 默认保留持久存储；body `{"purge": true}` 一键连数据面一起清空。
#[utoipa::path(
    post,
    path = "/api/v1/userapp/{app_id}/delete",
    params(
        ("app_id" = String, Path, description = "应用 ID")
    ),
    request_body = DeleteAppRequest,
    responses(
        (status = 200, description = "删除成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>)
    ),
    tag = "UserApp · 生命周期"
)]
#[instrument(skip(state, body))]
pub async fn delete_app(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    body: Option<Json<DeleteAppRequest>>,
) -> Result<Json<HttpResult<String>>, AppError> {
    let (purge, expected_rv) = body
        .map(|Json(r)| (r.purge.unwrap_or(false), r.expected_resource_version))
        .unwrap_or((false, None));
    info!("[APP] deleting app: {} (purge={})", app_id, purge);
    state
        .app_service
        .delete_app(&app_id, purge, expected_rv.as_deref())
        .await?;
    Ok(Json(HttpResult::success("删除成功".to_string())))
}
