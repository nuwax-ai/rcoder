//! 应用日志 handler（sources/query/stream，转发到 app 容器内 app-cli :3010）

use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Response, StatusCode, header};
use futures_util::TryStreamExt;
use serde_json::Value;
use shared_types::{AppError, HttpResult};

use crate::models::{AppLogQueryRequest, AppOperationError};

use super::AppManagerState;

/// 查询应用声明的日志源与匹配到的日志文件（转发 app-cli /v1/logs/sources/query）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/logs/sources/query",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = AppLogQueryRequest,
    responses(
        (status = 200, description = "声明的日志源与匹配文件列表"),
        (status = 400, description = "app-cli 拒绝请求（参数错误）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用无就绪实例 IP（未运行/未就绪），无法访问日志", body = HttpResult<String>),
        (status = 500, description = "连接 app-cli / 响应解析失败", body = HttpResult<String>)
    ),
    tag = "应用日志"
)]
pub async fn query_app_log_sources(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<AppLogQueryRequest>,
) -> Result<Json<Value>, AppError> {
    forward_json(&state, &app_id, "/v1/logs/sources/query", request).await
}

/// 查询应用多服务日志快照（带 checkpoint 游标，支持增量拉取；转发 app-cli /v1/logs/query）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/logs/query",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = AppLogQueryRequest,
    responses(
        (status = 200, description = "多服务日志快照与 checkpoint 游标"),
        (status = 400, description = "app-cli 拒绝请求（参数错误）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用无就绪实例 IP（未运行/未就绪），无法访问日志", body = HttpResult<String>),
        (status = 500, description = "连接 app-cli / 响应解析失败", body = HttpResult<String>)
    ),
    tag = "应用日志"
)]
pub async fn query_app_logs(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<AppLogQueryRequest>,
) -> Result<Json<Value>, AppError> {
    forward_json(&state, &app_id, "/v1/logs/query", request).await
}

/// 实时日志 SSE 流（转发 app-cli /v1/logs/stream，Content-Type: text/event-stream）
#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/logs/stream",
    params(("app_id" = String, Path, description = "应用 ID")),
    request_body = AppLogQueryRequest,
    responses(
        (status = 200, description = "SSE 日志流（text/event-stream）"),
        (status = 400, description = "app-cli 拒绝请求（参数错误）", body = HttpResult<String>),
        (status = 404, description = "应用不存在", body = HttpResult<String>),
        (status = 409, description = "应用无就绪实例 IP（未运行/未就绪），无法访问日志", body = HttpResult<String>),
        (status = 500, description = "连接 app-cli / 建流失败", body = HttpResult<String>)
    ),
    tag = "应用日志"
)]
pub async fn stream_app_logs_v1(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<AppLogQueryRequest>,
) -> Result<Response<Body>, AppError> {
    let base = runtime_api_base(&state, &app_id).await?;
    let response = state
        .http_client
        .post(format!("{base}/v1/logs/stream"))
        .json(&request)
        .send()
        .await
        .map_err(|error| backend(format!("connect to app-cli log stream: {error}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let message = response.text().await.unwrap_or_default();
        if status.is_client_error() {
            return Err(AppOperationError::Validation(format!(
                "app-cli rejected log stream ({status}): {message}"
            ))
            .into());
        }
        return Err(backend(format!("app-cli log stream failed ({status}): {message}")).into());
    }
    let stream = response.bytes_stream().map_err(std::io::Error::other);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|error| backend(format!("build SSE response: {error}")).into())
}

async fn forward_json(
    state: &Arc<AppManagerState>,
    app_id: &str,
    path: &str,
    request: AppLogQueryRequest,
) -> Result<Json<Value>, AppError> {
    let base = runtime_api_base(state, app_id).await?;
    let response = state
        .http_client
        .post(format!("{base}{path}"))
        .json(&request)
        .send()
        .await
        .map_err(|error| backend(format!("connect to app-cli logs API: {error}")))?;
    let status = response.status();
    let value: Value = response
        .json()
        .await
        .map_err(|error| backend(format!("parse app-cli logs response: {error}")))?;
    if status.is_client_error() {
        return Err(AppOperationError::Validation(format!(
            "app-cli rejected log query ({status}): {value}"
        ))
        .into());
    }
    if !status.is_success() {
        return Err(backend(format!("app-cli log query failed ({status}): {value}")).into());
    }
    Ok(Json(value))
}

async fn runtime_api_base(state: &Arc<AppManagerState>, app_id: &str) -> Result<String, AppError> {
    let runtime = state.app_service.get_app(app_id).await?;
    let ip = runtime
        .health
        .instance
        .map(|instance| instance.ip)
        .filter(|ip| !ip.is_empty())
        .ok_or_else(|| {
            AppOperationError::InvalidState(format!(
                "app {app_id} has no ready runtime IP for log access"
            ))
        })?;
    Ok(format!("http://{ip}:3010"))
}

fn backend(message: String) -> AppOperationError {
    AppOperationError::Backend(message)
}
