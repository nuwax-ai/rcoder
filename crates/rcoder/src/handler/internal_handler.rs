//! K8s 网关内部 API
//!
//! 仅供 rcoder-gateway 调用的内部端点，不走 API Key 鉴权。
//! - `POST /internal/pod/ensure` — 按 identifier 创建/查找 Pod（仅 ComputerAgentRunner）
//! - `GET  /internal/session/{session_id}/resolve` — 解析 session → identifier

use axum::{
    Json,
    extract::{Path, State},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, info, warn};

use crate::router::AppState;
use shared_types::{AppError, HttpResult, ServiceType};

// ─── Request / Response ───────────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct InternalEnsurePodRequest {
    pub identifier: String,
    /// "ComputerAgentRunner" | "RCoder"
    pub service_type: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InternalEnsurePodResponse {
    pub container_name: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionResolveData {
    pub identifier: String,
    pub service_type: String,
}

// ─── Handlers ─────────────────────────────────────────────────

/// `POST /internal/pod/ensure`
///
/// 按 identifier + service_type 确保容器存在。
///
/// - **ComputerAgentRunner**: 容器不存在时通过 `pod_ensure` 创建
/// - **RCoder**: 容器不存在时返回 `not_found`，网关会回退到控制面透传，
///   由 rcoder 的 `chat_handler` 在首次 `/chat` 时正常创建容器。
pub async fn internal_pod_ensure(
    State(state): State<Arc<AppState>>,
    Json(request): Json<InternalEnsurePodRequest>,
) -> Result<Json<HttpResult<InternalEnsurePodResponse>>, AppError> {
    let service_type = parse_service_type(&request.service_type);
    let identifier = request.identifier.trim().to_string();

    if identifier.is_empty() {
        return Ok(Json(HttpResult::error(
            shared_types::error_codes::ERR_VALIDATION,
            "identifier is required",
        )));
    }

    info!(
        "[INTERNAL] pod ensure: identifier={}, service_type={:?}",
        identifier, service_type
    );

    // 1. 先查找已有容器（所有 service_type 都走此路径）
    if let Ok(Some(info)) = state
        .runtime()
        .get_container_info_by_identifier(&identifier, &service_type)
        .await
    {
        debug!(
            "[INTERNAL] container already exists: name={}",
            info.container_name
        );
        return Ok(Json(HttpResult::success(InternalEnsurePodResponse {
            container_name: info.container_name,
        })));
    }

    // 2. 容器不存在 → 按 service_type 分支处理
    match service_type {
        ServiceType::ComputerAgentRunner => {
            // 委托给 pod_handler::pod_ensure（ComputerContainerManager）
            let ensure_request = super::pod_handler::EnsurePodRequest {
                user_id: identifier.clone(),
                project_id: identifier.clone(),
                resource_limits: None,
                pod_id: None,
                tenant_id: None,
                space_id: None,
                isolation_type: None,
            };

            let state_clone = state.clone();
            let result = super::pod_handler::pod_ensure(
                State(state),
                super::utils::I18nJsonOrQuery(ensure_request),
            )
            .await?;

            if result.success {
                if let Ok(Some(info)) = state_clone
                    .runtime()
                    .get_container_info_by_identifier(&identifier, &service_type)
                    .await
                {
                    info!("[INTERNAL] pod created: name={}", info.container_name);
                    return Ok(Json(HttpResult::success(InternalEnsurePodResponse {
                        container_name: info.container_name,
                    })));
                }
                return Ok(Json(HttpResult::success(InternalEnsurePodResponse {
                    container_name: identifier,
                })));
            }

            warn!("[INTERNAL] pod ensure failed: {:?}", result.message);
            Ok(Json(HttpResult::error(
                shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                &result.message,
            )))
        }
        ServiceType::RCoder => {
            // RCoder 容器由 chat_handler 在首次 /chat 时创建，
            // 此处返回 not_found 让网关回退到控制面透传。
            debug!(
                "[INTERNAL] RCoder container not found for {}, falling back to control plane",
                identifier
            );
            Ok(Json(HttpResult::error(
                "not_found",
                "RCoder container not found, route through control plane",
            )))
        }
    }
}

/// `GET /internal/session/{session_id}/resolve`
///
/// 解析 session_id → (identifier, service_type)。
/// 供 rcoder-gateway 在 SSE progress 路由中查找目标 Pod。
pub async fn internal_session_resolve(
    State(state): State<Arc<AppState>>,
    Path(session_id): Path<String>,
) -> Result<Json<HttpResult<SessionResolveData>>, AppError> {
    debug!("[INTERNAL] session resolve: session_id={}", session_id);

    let project_info = state
        .get_by_session(&session_id)
        .ok_or_else(|| AppError::generic("session not found"))?;

    let identifier = project_info.container_key().to_string();
    let service_type = project_info
        .service_type()
        .map(|st| st.to_string())
        .unwrap_or_else(|| "rcoder".to_string());

    debug!(
        "[INTERNAL] session resolved: {} → {} ({})",
        session_id, identifier, service_type
    );

    Ok(Json(HttpResult::success(SessionResolveData {
        identifier,
        service_type,
    })))
}

// ─── Helpers ──────────────────────────────────────────────────

fn parse_service_type(s: &str) -> ServiceType {
    match s {
        "ComputerAgentRunner" => ServiceType::ComputerAgentRunner,
        "RCoder" | "rcoder" => ServiceType::RCoder,
        other => {
            warn!(
                "[INTERNAL] unknown service_type '{}', defaulting to RCoder",
                other
            );
            ServiceType::RCoder
        }
    }
}
