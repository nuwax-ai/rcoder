//! DevComputer 调试接口处理器
//!
//! `/devcomputer/*` 路由的薄包装处理器，委托给对应的 `/computer/*` 处理器。
//! 核心差异：`handle_devcomputer_chat` 注入 auto_reload 默认配置（Phase 3 实现）。
//!
//! ## 设计原则
//!
//! - **共享容器**：`/devcomputer/chat` 和 `/computer/chat` 使用同一个容器（按 `user_id` 标识）
//! - **零逻辑分歧**：devcomputer handler 严格委托 computer handler，不重复业务逻辑
//! - **差异通过配置注入**：auto_reload 等调试配置通过修改请求参数注入

use axum::{
    Json,
    extract::State,
    http::HeaderMap,
    response::sse::{Event, Sse},
};
use futures_util::stream::Stream;
use shared_types::{
    AgentCancelResponse, AutoReloadConfig, ChatAgentConfig, ChatResponse,
    ComputerAgentCancelRequest, ComputerAgentStatusRequest, ComputerAgentStatusResponse,
    ComputerAgentStopRequest, ComputerAgentStopResponse, ComputerChatRequest,
    ResolvePermissionHttpRequest, ResolvePermissionResponseDto,
};
use std::convert::Infallible;
use std::sync::Arc;
use tracing::{info, instrument};

use crate::handler::utils::{I18nJsonOrQuery, I18nPath};
use crate::handler::{
    SessionNotificationParams, computer_agent_progress_notification, computer_agent_session_cancel,
    computer_agent_status, computer_agent_stop,
    computer_chat_handler::handle_computer_chat_internal, computer_notify_resolved,
};
use crate::{AppError, HttpResult, router::AppState};

/// 处理 DevComputer 聊天请求
///
/// 委托给 `handle_computer_chat`，自动注入 auto_reload 默认配置（默认启用）。
#[utoipa::path(
    post,
    path = "/devcomputer/chat",
    request_body(
        content = ComputerChatRequest,
        description = "DevComputer 聊天请求（自动注入 auto_reload 默认配置）",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功处理聊天请求",
            body = HttpResult<ChatResponse>
        ),
        (
            status = 400,
            description = "请求参数错误（如 user_id 为空）",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 500,
            description = "服务器内部错误",
            body = HttpResult<String>
        )
    ),
    tag = "devcomputer",
    operation_id = "handle_devcomputer_chat",
    summary = "发送 DevComputer 聊天消息",
    description = "与 /computer/chat 功能相同，自动注入 auto_reload 默认配置（默认启用热重载），适用于开发调试场景"
)]
#[instrument(skip(state, request), fields(user_id = %request.user_id, project_id = ?request.project_id))]
pub async fn handle_devcomputer_chat(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerChatRequest>,
) -> Result<HttpResult<ChatResponse>, AppError> {
    // 注入 auto_reload 默认配置（默认启用热重载）
    let mut request = request;
    if let Some(ref mut agent_config) = request.agent_config {
        agent_config
            .auto_reload
            .get_or_insert(AutoReloadConfig::default_enabled());
    } else {
        request.agent_config = Some(ChatAgentConfig {
            auto_reload: Some(AutoReloadConfig::default_enabled()),
            ..Default::default()
        });
    }

    // 打印完整入参日志（devcomputer 调试接口专用）
    info!(
        "[DEVCOMPUTER] Received DevComputer Chat request: user_id={}, project_id={:?}, session_id={:?}, request_id={:?}, prompt_len={}, prompt={}, attachments={:?}, data_source_attachments={:?}, model_provider={:#?}, agent_config={:#?}, system_prompt_len={}, user_prompt_len={}, pod_id={:?}, tenant_id={:?}, space_id={:?}, isolation_type={:?}",
        request.user_id,
        request.project_id,
        request.session_id,
        request.request_id,
        request.prompt.len(),
        request.prompt,
        request.attachments,
        request.data_source_attachments,
        request.model_provider,
        request.agent_config,
        request.system_prompt.as_ref().map(|s| s.len()).unwrap_or(0),
        request.user_prompt.as_ref().map(|s| s.len()).unwrap_or(0),
        request.pod_id,
        request.tenant_id,
        request.space_id,
        request.isolation_type,
    );

    // 委托给 computer handler，设置 is_devcomputer=true
    handle_computer_chat_internal(state, headers, I18nJsonOrQuery(request), true).await
}

/// 处理 DevComputer Agent 停止请求
#[utoipa::path(
    post,
    path = "/devcomputer/agent/stop",
    request_body(
        content = ComputerAgentStopRequest,
        description = "停止特定 project_id 的 Agent 请求",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功停止 Agent",
            body = HttpResult<ComputerAgentStopResponse>
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "找不到指定的容器或 Agent",
            body = HttpResult<String>
        )
    ),
    tag = "devcomputer",
    operation_id = "devcomputer_agent_stop",
    summary = "停止 DevComputer Agent",
    description = "与 /computer/agent/stop 功能相同，停止特定 user_id 下的特定 project_id 的 Agent（不销毁容器）"
)]
#[instrument(skip(state, request))]
pub async fn devcomputer_agent_stop(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStopRequest>,
) -> Result<HttpResult<ComputerAgentStopResponse>, AppError> {
    computer_agent_stop(state, headers, I18nJsonOrQuery(request)).await
}

/// 处理 DevComputer Agent 状态查询
#[utoipa::path(
    post,
    path = "/devcomputer/agent/status",
    request_body(
        content = ComputerAgentStatusRequest,
        description = "DevComputer Agent 状态查询请求",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功获取 Agent 状态",
            body = HttpResult<ComputerAgentStatusResponse>
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        )
    ),
    tag = "devcomputer",
    operation_id = "devcomputer_agent_status",
    summary = "查询 DevComputer Agent 状态",
    description = "与 /computer/agent/status 功能相同，查询指定 Agent 的运行状态"
)]
#[instrument(skip(state, request))]
pub async fn devcomputer_agent_status(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStatusRequest>,
) -> Result<HttpResult<ComputerAgentStatusResponse>, AppError> {
    computer_agent_status(state, headers, I18nJsonOrQuery(request)).await
}

/// 处理 DevComputer Agent 会话取消
#[utoipa::path(
    post,
    path = "/devcomputer/agent/session/cancel",
    request_body(
        content = ComputerAgentCancelRequest,
        description = "取消 DevComputer Agent 会话请求",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "成功转发取消请求到容器",
            body = HttpResult<AgentCancelResponse>
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "未找到对应的用户容器或会话",
            body = HttpResult<String>
        )
    ),
    tag = "devcomputer",
    operation_id = "devcomputer_agent_session_cancel",
    summary = "取消 DevComputer Agent 会话",
    description = "与 /computer/agent/session/cancel 功能相同，转发取消请求到容器内的 agent_runner 服务"
)]
#[instrument(skip(state, request))]
pub async fn devcomputer_agent_session_cancel(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentCancelRequest>,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    computer_agent_session_cancel(state, headers, I18nJsonOrQuery(request)).await
}

/// 处理 DevComputer 权限审批回调
#[utoipa::path(
    post,
    path = "/devcomputer/notify-resolved",
    request_body(
        content_type = "application/json",
        description = "权限审批结果，包含 session_id、tool_call_id 等"
    ),
    responses(
        (
            status = 200,
            description = "权限审批处理完成",
            body = HttpResult<ResolvePermissionResponseDto>
        ),
        (
            status = 400,
            description = "请求参数错误",
            body = HttpResult<String>
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        )
    ),
    tag = "devcomputer",
    operation_id = "devcomputer_notify_resolved",
    summary = "DevComputer 权限审批回调",
    description = "与 /computer/notify-resolved 功能相同，处理 Agent 工具调用的权限审批结果"
)]
#[instrument(skip(state, input))]
pub async fn devcomputer_notify_resolved(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(input): I18nJsonOrQuery<ResolvePermissionHttpRequest>,
) -> Result<Json<HttpResult<ResolvePermissionResponseDto>>, AppError> {
    computer_notify_resolved(state, headers, I18nJsonOrQuery(input)).await
}

/// 处理 DevComputer Agent 进度通知 SSE 流
#[utoipa::path(
    get,
    path = "/devcomputer/progress/{session_id}",
    params(
        ("session_id" = String, Path, description = "会话 ID")
    ),
    responses(
        (
            status = 200,
            description = "SSE 进度事件流",
            content_type = "text/event-stream"
        ),
        (
            status = 401,
            description = "API Key 鉴权失败",
            body = HttpResult<String>
        ),
        (
            status = 404,
            description = "会话不存在或已完成",
            body = HttpResult<String>
        )
    ),
    tag = "devcomputer",
    operation_id = "devcomputer_agent_progress_notification",
    summary = "DevComputer 进度 SSE 流",
    description = "与 /computer/progress/{session_id} 功能相同，建立 SSE 连接实时接收执行进度和状态更新"
)]
pub async fn devcomputer_agent_progress_notification(
    params: I18nPath<SessionNotificationParams>,
    state: State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, AppError> {
    computer_agent_progress_notification(params, state, headers).await
}
