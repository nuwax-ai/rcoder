//! 数据库管理 handler（reset-password / create-database）

use std::sync::Arc;

use axum::{
    Json,
    extract::{Path, State},
};
use tracing::{info, instrument};

use shared_types::{AppError, HttpResult};

use super::state::AppManagerState;
use crate::models::{CreateDatabaseRequest, ResetDbPasswordRequest};

/// 重置 app 容器内 PG 密码（exec 容器内 psql ALTER USER，本地 trust 认证绕过当前密码）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/db/reset-password",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = ResetDbPasswordRequest,
    responses(
        (status = 200, description = "改密成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 400, description = "应用无 PG / 参数错误", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn reset_db_password(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<ResetDbPasswordRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] resetting PG password: {}", app_id);
    state
        .app_service
        .reset_db_password(&app_id, request)
        .await?;
    Ok(Json(HttpResult::success("密码已重置".to_string())))
}

/// 新建 PG 库（exec 容器内 psql CREATE DATABASE）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/db/create-database",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = CreateDatabaseRequest,
    responses(
        (status = 200, description = "建库成功", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 400, description = "应用无 PG / 参数错误", body = HttpResult<String>),
        (status = 409, description = "库已存在", body = HttpResult<String>)
    ),
    tag = "应用管理"
)]
#[instrument(skip(state, request))]
pub async fn create_database(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<CreateDatabaseRequest>,
) -> Result<Json<HttpResult<String>>, AppError> {
    info!("[APP] creating database: {}", app_id);
    state.app_service.create_database(&app_id, request).await?;
    Ok(Json(HttpResult::success("数据库已创建".to_string())))
}
