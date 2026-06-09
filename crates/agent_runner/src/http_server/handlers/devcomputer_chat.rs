//! DevComputer Chat Handler
//!
//! 处理 POST /devcomputer/chat 请求
//! 注入 auto_reload 默认配置后委托给 computer_chat

use axum::{Json, extract::State, http::HeaderMap};
use std::sync::Arc;

use crate::http_server::router::AppState;
use shared_types::{
    AppError, AutoReloadConfig, ChatAgentConfig, ChatResponse, ComputerChatRequest, HttpResult,
    I18nJsonOrQuery,
};

use super::computer_chat::handle_computer_chat;

/// 处理 DevComputer Chat 请求
///
/// 与 /computer/chat 共享同一个容器，自动注入 auto_reload 默认配置
#[utoipa::path(
    post,
    path = "/devcomputer/chat",
    request_body = ComputerChatRequest,
    responses(
        (status = 200, description = "Chat request successful", body = HttpResult<ChatResponse>),
        (status = 400, description = "Bad request - missing user_id"),
        (status = 500, description = "Internal server error")
    ),
    tag = "DevComputer"
)]
pub async fn handle_devcomputer_chat(
    state: State<Arc<AppState>>,
    headers: HeaderMap,
    I18nJsonOrQuery(mut request): I18nJsonOrQuery<ComputerChatRequest>,
) -> Result<Json<HttpResult<ChatResponse>>, AppError> {
    // 注入 auto_reload 默认配置（启用热重载）
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

    handle_computer_chat(state, headers, I18nJsonOrQuery(request)).await
}
