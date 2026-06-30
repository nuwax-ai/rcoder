//! 通用 HTTP Handler 层
//!
//! 基于 AgentHttpService trait 的通用 Axum handler 函数
//! RCoder 和 Agent Runner 可以使用这些 handler，通过注入不同的 trait 实现

use std::sync::Arc;

use axum::extract::{Path, State};

use crate::{
    AgentStatusResponse, ChatResponse, HttpResult, I18nJsonOrQuery, RcoderChatRequest,
    agent_http_service::AgentHttpService,
    rcoder_agent_types::{
        RcoderAgentCancelRequest, RcoderAgentCancelResponse, RcoderAgentStopRequest,
        RcoderAgentStopResponse,
    },
};

/// 通用 Chat handler（适用于 POST /chat）
///
/// 使用泛型 S: AgentHttpService，通过 State 注入服务实例
#[utoipa::path(
    post,
    path = "/chat",
    request_body = RcoderChatRequest,
    responses(
        (status = 200, description = "Chat request successful", body = HttpResult<ChatResponse>),
        (status = 400, description = "Bad request - missing prompt"),
        (status = 500, description = "Internal server error")
    ),
    tag = "RCoder Agent"
)]
pub async fn handle_chat<S: AgentHttpService>(
    State(service): State<Arc<S>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<RcoderChatRequest>,
) -> Result<axum::Json<HttpResult<ChatResponse>>, crate::AppError> {
    Ok(axum::Json(service.chat(request).await))
}

/// 通用 Status handler（适用于 GET /agent/status/{project_id}）
#[utoipa::path(
    get,
    path = "/agent/status/{project_id}",
    params(
        ("project_id" = String, Path, description = "项目ID")
    ),
    responses(
        (status = 200, description = "Status query successful", body = HttpResult<AgentStatusResponse>),
        (status = 400, description = "Bad request - missing project_id"),
        (status = 500, description = "Internal server error")
    ),
    tag = "RCoder Agent"
)]
pub async fn handle_status<S: AgentHttpService>(
    State(service): State<Arc<S>>,
    Path(project_id): Path<String>,
) -> axum::Json<HttpResult<AgentStatusResponse>> {
    axum::Json(service.get_status(&project_id).await)
}

/// 通用 Stop handler（适用于 POST /agent/stop）
#[utoipa::path(
    post,
    path = "/agent/stop",
    request_body = RcoderAgentStopRequest,
    responses(
        (status = 200, description = "Stop request successful", body = HttpResult<RcoderAgentStopResponse>),
        (status = 400, description = "Bad request - missing project_id"),
        (status = 500, description = "Internal server error")
    ),
    tag = "RCoder Agent"
)]
pub async fn handle_stop<S: AgentHttpService>(
    State(service): State<Arc<S>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<RcoderAgentStopRequest>,
) -> Result<axum::Json<HttpResult<RcoderAgentStopResponse>>, crate::AppError> {
    Ok(axum::Json(service.stop(request).await))
}

/// 通用 Cancel handler（适用于 POST /agent/session/cancel）
#[utoipa::path(
    post,
    path = "/agent/session/cancel",
    params(
        RcoderAgentCancelRequest
    ),
    request_body = RcoderAgentCancelRequest,
    responses(
        (status = 200, description = "Cancel request successful", body = HttpResult<RcoderAgentCancelResponse>),
        (status = 400, description = "Bad request - missing project_id"),
        (status = 500, description = "Internal server error")
    ),
    tag = "RCoder Agent"
)]
pub async fn handle_cancel<S: AgentHttpService>(
    State(service): State<Arc<S>>,
    I18nJsonOrQuery(request): I18nJsonOrQuery<RcoderAgentCancelRequest>,
) -> Result<axum::Json<HttpResult<RcoderAgentCancelResponse>>, crate::AppError> {
    Ok(axum::Json(service.cancel(request).await))
}
