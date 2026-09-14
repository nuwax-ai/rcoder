//! 跨 Pod 内部执行端点（`/api/v1/preview-internal/*`，rcoder 主 API 合并挂载）。
//!
//! 鉴权：`x-preview-internal-token` 必须与本进程令牌一致；缺失/不符返回 404
//! （不向外部探测者暴露端点存在性）。目标身份来自请求体并经协调器与权威库
//! 双重校验——外部伪造身份无法触发执行。
use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;

use crate::service::PreviewCoordinator;
use shared_types::PreviewCoordination as _;

pub const INTERNAL_TOKEN_HEADER: &str = "x-preview-internal-token";

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InternalStopRequest {
    pub preview_key: String,
    pub instance_id: String,
    pub operation_id: String,
    pub revision: i64,
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct InternalVerifyRequest {
    pub preview_key: String,
    pub instance_id: String,
}

#[derive(Deserialize, utoipa::IntoParams)]
#[serde(rename_all = "camelCase")]
pub struct InternalLogQuery {
    pub log_key: String,
    pub log_type: String,
    pub start_index: usize,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct StopResponse {
    outcome: &'static str,
}

#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
struct VerifyResponse {
    identity_match: bool,
    alive: bool,
    pid: Option<i64>,
    port: Option<u16>,
}

/// 内部端点 Router（挂在协调器所在 rcoder 主 API 上）。
pub fn internal_router(coordinator: Arc<PreviewCoordinator>) -> Router {
    let token = dispatch_token();
    Router::new()
        .route("/api/v1/preview-internal/stop", post(stop_endpoint))
        .route("/api/v1/preview-internal/verify", post(verify_endpoint))
        .route("/api/v1/preview-internal/log", get(log_endpoint))
        .layer(axum::middleware::from_fn_with_state(
            (token, coordinator.clone()),
            token_guard,
        ))
        .with_state(coordinator)
}

fn dispatch_token() -> String {
    // 与 dispatch 客户端同源的令牌获取方式（装配方构造前已 fail-fast 校验非空）。
    crate::token::internal_token_from_env().unwrap_or_default()
}

async fn token_guard(
    State((expected, _coordinator)): State<(String, Arc<PreviewCoordinator>)>,
    headers: HeaderMap,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let provided = headers
        .get(INTERNAL_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if expected.is_empty() || provided != expected {
        // 令牌缺失/不符：404（不暴露端点存在性；恒定响应不给时序侧信道）
        return (StatusCode::NOT_FOUND, "Not Found").into_response();
    }
    next.run(request).await
}

#[utoipa::path(
    post,
    path = "/api/v1/preview-internal/stop",
    request_body = InternalStopRequest,
    responses((status = 200, description = "宿主侧执行结果", body = StopResponse), (status = 404, description = "令牌缺失/不符")),
    tag = "PreviewInternal",
    description = "协调器跨 Pod 停止派发的宿主侧落地。仅校验通过的协调请求可执行；操作身份（operation/revision/state）与权威库不匹配时返回 identity_mismatch 不执行。"
)]
async fn stop_endpoint(
    State(coordinator): State<Arc<PreviewCoordinator>>,
    Json(req): Json<InternalStopRequest>,
) -> Result<Json<StopResponse>, StatusCode> {
    let outcome = coordinator
        .internal_stop(
            &req.preview_key,
            &req.instance_id,
            &req.operation_id,
            req.revision,
        )
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let outcome = match outcome {
        shared_types::ExecutorStopOutcome::Stopped => "stopped",
        shared_types::ExecutorStopOutcome::NotRegistered => "not_registered",
        shared_types::ExecutorStopOutcome::IdentityMismatch => "identity_mismatch",
    };
    Ok(Json(StopResponse { outcome }))
}

#[utoipa::path(
    post,
    path = "/api/v1/preview-internal/verify",
    request_body = InternalVerifyRequest,
    responses((status = 200, description = "宿主侧登记/探活校验", body = VerifyResponse), (status = 404, description = "令牌缺失/不符")),
    tag = "PreviewInternal",
    description = "协调器跨 Pod verify 派发的宿主侧落地（keep-alive 存活判定）。"
)]
async fn verify_endpoint(
    State(coordinator): State<Arc<PreviewCoordinator>>,
    Json(req): Json<InternalVerifyRequest>,
) -> Result<Json<VerifyResponse>, StatusCode> {
    let report = coordinator
        .internal_verify(&req.preview_key, &req.instance_id)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(VerifyResponse {
        identity_match: report.identity_match,
        alive: report.alive,
        pid: report.pid,
        port: report.port,
    }))
}

#[utoipa::path(
    get,
    path = "/api/v1/preview-internal/log",
    params(InternalLogQuery),
    responses((status = 200, description = "宿主侧日志读取", body = shared_types::ExecutorLogChunk), (status = 404, description = "令牌缺失/不符")),
    tag = "PreviewInternal",
    description = "协调器跨 Pod get-dev-log 转发的宿主侧落地。"
)]
async fn log_endpoint(
    State(coordinator): State<Arc<PreviewCoordinator>>,
    Query(q): Query<InternalLogQuery>,
) -> Result<Json<shared_types::ExecutorLogChunk>, StatusCode> {
    let chunk = coordinator
        .read_dev_log(&q.log_key, &q.log_type, q.start_index)
        .await
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(chunk))
}
