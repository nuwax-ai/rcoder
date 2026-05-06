//! SessionNotifier 实现
//!
//! 实现 agent_abstraction 定义的 SessionNotifier trait，
//! 用于推送 SSE 消息到前端。

use agent_abstraction::SessionNotifier;
use async_trait::async_trait;
use shared_types::{
    AgentSessionUpdate, SessionNotify, SessionPromptEnd, SessionPromptError, SessionPromptStart,
};

use super::push_session_update_with_project;

/// SSE 消息推送器
///
/// 实现 SessionNotifier trait，将会话消息推送到 SSE 连接。
#[derive(Debug, Clone, Default)]
pub struct SseSessionNotifier;

impl SseSessionNotifier {
    /// 创建新的 SSE 消息推送器
    pub fn new() -> Self {
        Self
    }
}

/// 将 anyhow::Error 转换为 Box<dyn std::error::Error + Send + Sync>
fn convert_error(e: anyhow::Error) -> Box<dyn std::error::Error + Send + Sync> {
    Box::new(std::io::Error::other(e.to_string()))
}

#[async_trait]
impl SessionNotifier for SseSessionNotifier {
    async fn notify_prompt_start(
        &self,
        project_id: &str,
        session_id: &str,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let notify = SessionNotify::SessionPromptStart(SessionPromptStart {
            session_id: session_id.to_string(),
            request_id,
        });

        push_session_update_with_project(project_id, session_id, notify)
            .await
            .map_err(convert_error)
    }

    async fn notify_prompt_end(
        &self,
        project_id: &str,
        session_id: &str,
        stop_reason: agent_client_protocol::schema::StopReason,
        error_message: Option<String>,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
            session_id: session_id.to_string(),
            stop_reason,
            error_message,
            request_id,
        });

        push_session_update_with_project(project_id, session_id, notify)
            .await
            .map_err(convert_error)
    }

    async fn notify_prompt_error(
        &self,
        project_id: &str,
        session_id: &str,
        error: agent_client_protocol::schema::Error,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let notify = SessionNotify::SessionPromptError(SessionPromptError {
            session_id: session_id.to_string(),
            error,
            request_id,
        });

        push_session_update_with_project(project_id, session_id, notify)
            .await
            .map_err(convert_error)
    }

    async fn notify_session_update(
        &self,
        project_id: &str,
        session_id: &str,
        session_update: agent_client_protocol::schema::SessionUpdate,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let notify = SessionNotify::AgentSessionUpdate(Box::new(AgentSessionUpdate {
            session_id: session_id.to_string(),
            session_update,
            request_id,
        }));

        push_session_update_with_project(project_id, session_id, notify)
            .await
            .map_err(convert_error)
    }

    async fn notify(
        &self,
        project_id: &str,
        session_id: &str,
        notify: SessionNotify,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        push_session_update_with_project(project_id, session_id, notify)
            .await
            .map_err(convert_error)
    }
}
