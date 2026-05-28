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
    response::{
        Response,
        sse::{Event, Sse},
    },
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
use tracing::instrument;

use crate::handler::utils::{I18nJsonOrQuery, I18nPath};
use crate::handler::{
    SessionNotificationParams, computer_agent_progress_notification, computer_agent_session_cancel,
    computer_agent_status, computer_agent_stop, computer_notify_resolved,
    handle_computer_chat,
};
use crate::{AppError, HttpResult, router::AppState};

/// 处理 DevComputer 聊天请求
///
/// 委托给 `handle_computer_chat`，自动注入 auto_reload 默认配置（默认启用）。
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

    // 直接委托给 computer handler
    handle_computer_chat(state, headers, I18nJsonOrQuery(request)).await
}

/// 处理 DevComputer Agent 停止请求
#[instrument(skip(state, request))]
pub async fn devcomputer_agent_stop(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStopRequest>,
) -> Result<HttpResult<ComputerAgentStopResponse>, AppError> {
    computer_agent_stop(state, headers, I18nJsonOrQuery(request)).await
}

/// 处理 DevComputer Agent 状态查询
#[instrument(skip(state, request))]
pub async fn devcomputer_agent_status(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentStatusRequest>,
) -> Result<HttpResult<ComputerAgentStatusResponse>, AppError> {
    computer_agent_status(state, headers, I18nJsonOrQuery(request)).await
}

/// 处理 DevComputer Agent 会话取消
#[instrument(skip(state, request))]
pub async fn devcomputer_agent_session_cancel(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(request): I18nJsonOrQuery<ComputerAgentCancelRequest>,
) -> Result<HttpResult<AgentCancelResponse>, AppError> {
    computer_agent_session_cancel(state, headers, I18nJsonOrQuery(request)).await
}

/// 处理 DevComputer 权限审批回调
#[instrument(skip(state, input))]
pub async fn devcomputer_notify_resolved(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(input): I18nJsonOrQuery<ResolvePermissionHttpRequest>,
) -> Result<Json<HttpResult<ResolvePermissionResponseDto>>, AppError> {
    computer_notify_resolved(state, headers, I18nJsonOrQuery(input)).await
}

/// 处理 DevComputer Agent 进度通知 SSE 流
pub async fn devcomputer_agent_progress_notification(
    params: I18nPath<SessionNotificationParams>,
    state: State<Arc<AppState>>,
) -> Result<Sse<impl Stream<Item = Result<Event, Infallible>>>, Response> {
    computer_agent_progress_notification(params, state).await
}
