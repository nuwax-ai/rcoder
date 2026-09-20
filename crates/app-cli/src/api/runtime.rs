//! `/v1/runtime/*` — 运行态单一所有者协议端点（阶段二，plan §3.2）。
//!
//! 鉴权与 `/v1/deploy` 同源（`X-Deploy-Token`）；identity/status 不回显
//! secrets。修改端点受 [`ServerState::initializing`] 门控（P1-01 语义沿
//! 用：恢复完成前拒绝写受理）。响应统一 [`super::envelope::HttpResult`]
//! 信封，SSE 事件重放豁免。

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use futures::Stream;
use serde_json::json;

use crate::runtime_kernel::RuntimeKernel;

use super::AppState;

/// 运行操作受理请求（wire = [`shared_types::RuntimeOperationRequest`]）。
#[derive(Debug, serde::Deserialize, utoipa::ToSchema)]
#[schema(as = RuntimeOperationBody)]
pub(super) struct RuntimeOperationBody {
    #[serde(flatten)]
    pub request: shared_types::RuntimeOperationRequest,
}

fn kernel_of(
    state: &AppState,
) -> Result<Arc<RuntimeKernel>, (StatusCode, Json<serde_json::Value>)> {
    state.runtime_kernel().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({
                "success": false,
                "code": "ERR_PROTOCOL_UNSUPPORTED",
                "message": "runtime control kernel is not active in this mode",
            })),
        )
    })
}

fn reject(code: &str, message: &str, status: StatusCode) -> (StatusCode, Json<serde_json::Value>) {
    (
        status,
        Json(json!({ "success": false, "code": code, "message": message })),
    )
}

fn require_token(
    state: &AppState,
    token: Option<&str>,
) -> Result<(), (StatusCode, Json<serde_json::Value>)> {
    let expected = state.server.control_token();
    let Some(expected) = expected else {
        return Err(reject(
            "ERR_FORBIDDEN",
            "runtime endpoints disabled (APP_CLI_DEPLOY_TOKEN not set)",
            StatusCode::FORBIDDEN,
        ));
    };
    match token {
        Some(provided) if constant_time_eq(provided, &expected) => Ok(()),
        _ => Err(reject(
            "ERR_FORBIDDEN",
            "invalid deploy token",
            StatusCode::FORBIDDEN,
        )),
    }
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Read-only recovery evidence; never modifies journal, credentials or admission.
#[utoipa::path(
    get,
    path = "/v1/runtime/recovery",
    params(("X-Deploy-Token" = String, Header, description = "Owner control token")),
    responses(
        (status = 200, description = "Recovery evidence in data; not permission to resume", body = serde_json::Value),
        (status = 403, description = "Invalid owner token"),
        (status = 503, description = "Recovery evidence unavailable")
    ),
    tag = "Runtime Control"
)]
pub(super) async fn recovery(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    require_token(
        &state,
        headers
            .get("x-deploy-token")
            .and_then(|value| value.to_str().ok()),
    )?;
    if state.server.initializing() {
        return Err(reject(
            "ERR_INVALID_STATE",
            "startup recovery has not completed",
            StatusCode::SERVICE_UNAVAILABLE,
        ));
    }
    let view = state.server.recovery_view().await.map_err(|error| {
        reject(
            "ERR_RECOVERY_REQUIRED",
            &format!("read recovery evidence: {error:#}"),
            StatusCode::SERVICE_UNAVAILABLE,
        )
    })?;
    Ok(Json(
        json!({ "success": true, "code": "OK", "data": view, "message": "ok" }),
    ))
}

/// `GET /v1/runtime/identity` — 所有者身份与能力（无 secrets）。
#[utoipa::path(
    get,
    path = "/v1/runtime/identity",
    responses((status = 200, description = "Owner identity, instance and capabilities", body = serde_json::Value)),
    tag = "Runtime Control"
)]
pub(super) async fn identity(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let kernel = kernel_of(&state)?;
    let identity = kernel.identity().clone();
    Ok(Json(
        json!({ "success": true, "code": "OK", "data": identity, "message": "ok" }),
    ))
}

/// `GET /v1/runtime/status` — desired/observed/active operation/恢复保护。
#[utoipa::path(
    get,
    path = "/v1/runtime/status",
    responses((status = 200, description = "Runtime status view", body = serde_json::Value)),
    tag = "Runtime Control"
)]
pub(super) async fn status(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let kernel = kernel_of(&state)?;
    let mut view = kernel.status().await.map_err(|error| {
        reject(
            "ERR_BACKEND_ERROR",
            &format!("{error:#}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        )
    })?;
    // observed 融合 server 相位（管理进程活着 ≠ 业务就绪——spec §4）
    let phase = state.server.phase();
    view.observed = match &phase {
        crate::server::ServerPhase::Running => {
            if state.server.readiness_ok() {
                shared_types::ObservedHealth::Ready
            } else {
                shared_types::ObservedHealth::Degraded
            }
        }
        crate::server::ServerPhase::Idle | crate::server::ServerPhase::Failed(_) => {
            shared_types::ObservedHealth::Stopped
        }
        crate::server::ServerPhase::Deploying | crate::server::ServerPhase::Orchestrating => {
            shared_types::ObservedHealth::Unknown
        }
    };
    if let Some(release) = state.server.release() {
        view.active_target = Some(release.release_id.clone());
    }
    Ok(Json(
        json!({ "success": true, "code": "OK", "data": view, "message": "ok" }),
    ))
}

/// `POST /v1/runtime/operations` — 异步受理（202 + 操作查询地址）。
#[utoipa::path(
    post,
    path = "/v1/runtime/operations",
    request_body = RuntimeOperationBody,
    params(("X-Deploy-Token" = String, Header, description = "Owner control token (APP_CLI_DEPLOY_TOKEN or owner state file)")),
    responses(
        (status = 202, description = "Operation accepted or idempotent replay. Explicit Source start/restart or Artifact deployment with PG credentials can resolve a credentials-only owner hold for the confirmed artifact; uncertain operations remain protected.", body = serde_json::Value),
        (status = 409, description = "Conflict: id/replay/busy/revision/instance/recovery. Owner startup recovery blocks new business operations before persistence; Stop retains kernel safety checks.", body = serde_json::Value),
    ),
    tag = "Runtime Control"
)]
pub(super) async fn submit_operation(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
    body: Result<Json<RuntimeOperationBody>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    require_token(
        &state,
        headers.get("x-deploy-token").and_then(|v| v.to_str().ok()),
    )?;
    if state.server.initializing() {
        return Err(reject(
            "ERR_INVALID_STATE",
            "startup recovery has not completed; retry after initialization",
            StatusCode::CONFLICT,
        ));
    }
    let kernel = kernel_of(&state)?;
    let Json(body) = body.map_err(|error| {
        reject(
            "ERR_VALIDATION",
            &format!("invalid runtime operation body: {error}"),
            StatusCode::BAD_REQUEST,
        )
    })?;
    let credential_mode = match (&body.request.kind, &body.request.profile) {
        (
            shared_types::RuntimeOperationKind::Start | shared_types::RuntimeOperationKind::Restart,
            shared_types::RunProfileInput::Source { .. },
        ) => Some(true),
        (
            shared_types::RuntimeOperationKind::Deploy,
            shared_types::RunProfileInput::Artifact { .. },
        ) => Some(false),
        _ => None,
    };
    let supplying_credentials = if let Some(source_only) = credential_mode {
        state
            .server
            .can_supply_run_credentials(
                body.request
                    .run_config
                    .as_ref()
                    .and_then(|config| config.pg.as_ref()),
                source_only,
            )
            .map_err(|error| {
                reject(
                    "ERR_RECOVERY_REQUIRED",
                    &format!("verify credential recovery: {error:#}"),
                    StatusCode::CONFLICT,
                )
            })?
    } else {
        false
    };
    let admission = if state.server.runtime_recovery_hold_active() && !supplying_credentials {
        kernel.admit_with_owner_hold(body.request, true).await
    } else {
        kernel.admit(body.request).await
    };
    match admission {
        Ok(outcome) => {
            let view = match outcome {
                crate::runtime_kernel::AdmissionOutcome::Accepted(view) => view,
                crate::runtime_kernel::AdmissionOutcome::Replayed(view) => view,
            };
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "success": true,
                    "code": "OK",
                    "message": "accepted",
                    "data": shared_types::RuntimeOperationAccepted {
                        operation_id: view.operation_id.clone(),
                        state: view.state,
                        poll: format!("/v1/runtime/operations/{}", view.operation_id),
                    },
                })),
            ))
        }
        Err(rejection) => {
            let mut payload = json!({
                "success": false,
                "code": rejection.code,
                "message": rejection.message,
            });
            if let Some(active) = rejection.active_operation_id {
                payload["active_operation_id"] = json!(active);
            }
            Ok((StatusCode::CONFLICT, Json(payload)))
        }
    }
}

/// `GET /v1/runtime/operations/{id}` — 读取指定操作（不能用“最近一次”代替）。
#[utoipa::path(
    get,
    path = "/v1/runtime/operations/{operation_id}",
    params(("operation_id" = String, Path, description = "Runtime operation identifier")),
    responses(
        (status = 200, description = "Operation record", body = serde_json::Value),
        (status = 404, description = "Unknown operation", body = serde_json::Value),
    ),
    tag = "Runtime Control"
)]
pub(super) async fn get_operation(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let kernel = kernel_of(&state)?;
    let view = kernel
        .get(&operation_id)
        .await
        .map_err(|error| {
            reject(
                "ERR_BACKEND_ERROR",
                &format!("{error:#}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?
        .ok_or_else(|| {
            reject(
                "ERR_NOT_FOUND",
                "unknown runtime operation",
                StatusCode::NOT_FOUND,
            )
        })?;
    Ok(Json(
        json!({ "success": true, "code": "OK", "data": view, "message": "ok" }),
    ))
}

#[derive(Debug, serde::Deserialize)]
pub(super) struct EventsQuery {
    pub after_seq: Option<u64>,
}

/// `GET /v1/runtime/operations/{id}/events` — 事件重放（`after_seq` 游标）。
#[utoipa::path(
    get,
    path = "/v1/runtime/operations/{operation_id}/events",
    params(
        ("operation_id" = String, Path, description = "Runtime operation identifier"),
        ("after_seq" = Option<u64>, Query, description = "Replay events with sequence greater than this cursor"),
    ),
    responses((status = 200, description = "Ordered runtime events (JSON array)", body = serde_json::Value)),
    tag = "Runtime Control"
)]
pub(super) async fn operation_events(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let kernel = kernel_of(&state)?;
    let events = kernel
        .store()
        .replay_events(&operation_id, query.after_seq.unwrap_or(0))
        .map_err(|error| {
            reject(
                "ERR_BACKEND_ERROR",
                &format!("{error:#}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;
    Ok(Json(json!({
        "success": true, "code": "OK", "message": "ok",
        "data": { "operation_id": operation_id, "events": events },
    })))
}

/// `POST /v1/runtime/operations/{id}/cancel` — 请求取消（不伪称立即完成）。
#[utoipa::path(
    post,
    path = "/v1/runtime/operations/{operation_id}/cancel",
    params(
        ("operation_id" = String, Path, description = "Runtime operation identifier"),
        ("X-Deploy-Token" = String, Header, description = "Deploy token"),
    ),
    responses((status = 202, description = "Cancel requested", body = serde_json::Value)),
    tag = "Runtime Control"
)]
pub(super) async fn cancel_operation(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
    headers: axum::http::HeaderMap,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    require_token(
        &state,
        headers.get("x-deploy-token").and_then(|v| v.to_str().ok()),
    )?;
    let kernel = kernel_of(&state)?;
    let requested = kernel
        .request_cancel(&operation_id)
        .await
        .map_err(|error| {
            reject(
                "ERR_BACKEND_ERROR",
                &format!("{error:#}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;
    let message = if requested {
        "cancel requested; poll the operation for the terminal state"
    } else {
        "operation is not active; nothing to cancel"
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "success": true, "code": "OK",
            "message": message,
        })),
    ))
}

/// SSE 事件流（断线重连按 after_seq 续传；首版为游标重放快照流）。
pub(super) async fn operation_events_sse(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
    Query(query): Query<EventsQuery>,
) -> Result<
    Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>,
    (StatusCode, Json<serde_json::Value>),
> {
    let kernel = kernel_of(&state)?;
    let events = kernel
        .store()
        .replay_events(&operation_id, query.after_seq.unwrap_or(0))
        .map_err(|error| {
            reject(
                "ERR_BACKEND_ERROR",
                &format!("{error:#}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            )
        })?;
    let stream = futures::stream::iter(events.into_iter().map(|event| {
        Ok(Event::default()
            .event(
                event
                    .event_name
                    .clone()
                    .unwrap_or_else(|| "runtime_event".into()),
            )
            .id(event.sequence.to_string())
            .json_data(&event)
            .unwrap_or_default())
    }));
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// 共享：runtime_kernel 槽位访问（AppState 转发）。
impl AppState {
    pub(super) fn runtime_kernel(&self) -> Option<Arc<RuntimeKernel>> {
        self.server.runtime_kernel()
    }
}
