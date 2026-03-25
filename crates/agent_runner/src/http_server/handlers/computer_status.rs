//! Computer Agent Status Handler
//!
//! 处理 POST /computer/agent/status 请求

use axum::{Json, extract::State, http::StatusCode};
use std::sync::Arc;
use tracing::{info, warn};

use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{ComputerAgentStatusRequest, ComputerAgentStatusResponse, HttpResult};

/// 查询 Computer Agent 状态
///
/// 直接使用 AGENT_REGISTRY 查询,无需 gRPC 调用
#[utoipa::path(
    post,
    path = "/computer/agent/status",
    request_body = ComputerAgentStatusRequest,
    responses(
        (status = 200, description = "Status query successful", body = HttpResult<ComputerAgentStatusResponse>),
        (status = 400, description = "Bad request - missing fields"),
        (status = 500, description = "Internal server error")
    ),
    tag = "Computer Agent"
)]
pub async fn handle_computer_status(
    State(_state): State<Arc<AppState>>,
    Json(request): Json<ComputerAgentStatusRequest>,
) -> Result<Json<HttpResult<ComputerAgentStatusResponse>>, (StatusCode, Json<HttpResult<String>>)> {
    info!(
        "🔍 [HTTP] Computer Agent 状态查询: user_id={}, project_id={}",
        request.user_id, request.project_id
    );

    // 1. 验证必填字段
    if request.user_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(HttpResult::error("VALIDATION_ERROR", "user_id is required")),
        ));
    }

    if request.project_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(HttpResult::error(
                "VALIDATION_ERROR",
                "project_id is required",
            )),
        ));
    }

    // 2. 从 AGENT_REGISTRY 查询 Agent 状态
    let agent_info = AGENT_REGISTRY.get_agent_info(&request.project_id);

    let response = match agent_info {
        Some(info) => {
            // Agent 存在且活跃
            info!(
                "✅ [HTTP] Agent 状态: project_id={}, is_alive=true, session_id={:?}",
                request.project_id, info.session_id
            );

            ComputerAgentStatusResponse {
                user_id: request.user_id.clone(),
                project_id: request.project_id.clone(),
                is_alive: true,
                session_id: Some(info.session_id.to_string()),
                status: Some(format!("{:?}", info.status)),
                last_activity: Some(info.last_activity),
                created_at: Some(info.created_at),
            }
        }
        None => {
            // Agent 不存在
            warn!(" [HTTP] Agent 不存在: project_id={}", request.project_id);

            ComputerAgentStatusResponse {
                user_id: request.user_id.clone(),
                project_id: request.project_id.clone(),
                is_alive: false,
                session_id: None,
                status: None,
                last_activity: None,
                created_at: None,
            }
        }
    };

    Ok(Json(HttpResult::success(response)))
}
