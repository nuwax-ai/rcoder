//! Computer Agent Status Handler
//!
//! 处理 POST /computer/agent/status 请求

use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;
use tracing::{info, warn};

use crate::http_server::router::AppState;
use crate::service::AGENT_REGISTRY;
use shared_types::{
    ComputerAgentStatusRequest, ComputerAgentStatusResponse, HttpResult, I18nJsonOrQuery,
    error_codes::ERR_VALIDATION, get_i18n_message,
};

use super::locale_from_headers;

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
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStatusRequest>,
) -> Result<Json<HttpResult<ComputerAgentStatusResponse>>, shared_types::AppError> {
    let locale = locale_from_headers(&headers);

    // 使用 garde 进行字段校验
    let I18nJsonOrQuery(request) = I18nJsonOrQuery(request).validate_into_app_error()?;
    let project_id = request
        .project_id
        .as_ref()
        .expect("validated: project_id is required and non-empty");

    // 验证 user_id 或 project_id 至少有一个
    let user_id_empty = request.user_id.as_ref().is_none_or(|s| s.is_empty());
    if user_id_empty && project_id.is_empty() {
        return Err(shared_types::AppError::with_i18n_key(
            ERR_VALIDATION,
            &get_i18n_message("error.user_id_or_project_id_required", locale),
        ));
    }

    info!(
        "🔍 [HTTP] Computer Agent 状态查询: user_id={:?}, project_id={}, pod_id={:?}, tenant_id={:?}, space_id={:?}, isolation_type={:?}",
        request.user_id,
        project_id,
        request.pod_id,
        request.tenant_id,
        request.space_id,
        request.isolation_type
    );

    // 从 AGENT_REGISTRY 查询 Agent 状态
    let agent_info = AGENT_REGISTRY.get_agent_info(project_id);

    let response = match agent_info {
        Some(info) => {
            // Agent 存在且活跃
            info!(
                "✅ [HTTP] Agent status: project_id={}, is_alive=true, session_id={:?}",
                project_id, info.session_id
            );

            ComputerAgentStatusResponse {
                user_id: request.user_id.clone(),
                project_id: project_id.to_string(),
                is_alive: true,
                session_id: Some(info.session_id.to_string()),
                status: Some(format!("{:?}", info.status)),
                last_activity: Some(info.last_activity),
                created_at: Some(info.created_at),
            }
        }
        None => {
            // Agent 不存在
            warn!(" [HTTP] Agent not found: project_id={}", project_id);

            ComputerAgentStatusResponse {
                user_id: request.user_id.clone(),
                project_id: project_id.to_string(),
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
