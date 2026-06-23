//! SessionNotifier 实现
//!
//! 实现 agent_abstraction 定义的 SessionNotifier trait，
//! 用于推送 SSE 消息到前端。

use agent_abstraction::SessionNotifier;
use async_trait::async_trait;
use shared_types::{
    AgentSessionUpdate, SessionNotify, SessionPromptEnd, SessionPromptError, SessionPromptStart,
};

use super::{SESSION_CACHE, push_session_update_with_project};
use tracing::{debug, info};

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
        // 🧹 新任务开始时清空 ring buffer，防止回放过期的历史消息
        // 这是清空 ring buffer 的最佳时机：
        // 1. cancel/stop 后不能清空，因为需要通过 SSE 发送 SessionPromptEnd 给客户端
        // 2. 在新任务开始时清空，确保不会回放上一次对话的消息
        info!(
            "[SseSessionNotifier] notify_prompt_start called: project_id={}, session_id={}",
            project_id, session_id
        );

        if let Some(sd) = SESSION_CACHE.view(session_id, |_, d| d.clone()) {
            info!(
                "[SseSessionNotifier] SESSION_CACHE found for session_id={}, attempting to clear ring buffer",
                session_id
            );
            let cleared = sd.clear_message_buffer().await;
            if cleared > 0 {
                info!(
                    "[SseSessionNotifier] Cleared {} stale messages from ring buffer at prompt start: session_id={}",
                    cleared, session_id
                );
            } else {
                info!(
                    "[SseSessionNotifier] Ring buffer already empty for session_id={}",
                    session_id
                );
            }
        } else {
            info!(
                "[SseSessionNotifier] SESSION_CACHE not found for session_id={}, skipping clear (new session)",
                session_id
            );
        }

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
        stop_reason: agent_client_protocol::schema::v1::StopReason,
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
        error: agent_client_protocol::schema::v1::Error,
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
        session_update: agent_client_protocol::schema::v1::SessionUpdate,
        request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        debug!(
            "📤 [SseSessionNotifier] Received SessionUpdate from agent: project_id={}, session_id={}, update={:?}",
            project_id, session_id, session_update
        );

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
