use std::sync::Arc;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{Response, StatusCode, header};
use futures_util::TryStreamExt;
use serde_json::Value;
use shared_types::AppError;

use crate::models::{AppLogQueryRequest, AppOperationError};

use super::AppManagerState;

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/logs/sources/query",
    params(("app_id" = String, Path)),
    request_body = AppLogQueryRequest,
    responses((status = 200, description = "Declared log sources and matched files")),
    tag = "应用日志"
)]
pub async fn query_app_log_sources(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<AppLogQueryRequest>,
) -> Result<Json<Value>, AppError> {
    forward_json(&state, &app_id, "/v1/logs/sources/query", request).await
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/logs/query",
    params(("app_id" = String, Path)),
    request_body = AppLogQueryRequest,
    responses((status = 200, description = "Multi-service log snapshot and checkpoint cursor")),
    tag = "应用日志"
)]
pub async fn query_app_logs(
    State(state): State<Arc<AppManagerState>>,
    Path(app_id): Path<String>,
    Json(request): Json<AppLogQueryRequest>,
) -> Result<Json<Value>, AppError> {
    forward_json(&state, &app_id, "/v1/logs/query", request).await
}

#[utoipa::path(
    post,
    path = "/api/v1/apps/{app_id}/logs/stream",
    params(("app_id" = String, Path)),
    request_body = AppLogQueryRequest,
    responses((status = 200, description = "SSE log stream")),
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
