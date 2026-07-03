//! TUI 会话通知器
//!
//! 实现 SessionNotifier trait，将 agent 事件通过 mpsc channel 转发到 TUI 事件循环。
//! 关键：必须在 notify_prompt_end / notify_prompt_error 中触发 completion_signal。

use agent_abstraction::{PromptCompletionSignal, SessionNotifier};
use agent_client_protocol::schema::v1::{ContentBlock, SessionUpdate, StopReason};
use async_trait::async_trait;
use shared_types::SessionNotify;
use tokio::sync::mpsc;

use crate::tui::event::AppEvent;

/// TUI 会话通知器
pub struct TuiSessionNotifier {
    tx: mpsc::Sender<AppEvent>,
    completion_signal: Option<PromptCompletionSignal>,
}

impl TuiSessionNotifier {
    pub fn new(
        tx: mpsc::Sender<AppEvent>,
        completion_signal: Option<PromptCompletionSignal>,
    ) -> Self {
        Self {
            tx,
            completion_signal,
        }
    }

    fn signal_completion(&self) {
        if let Some(ref signal) = self.completion_signal {
            signal.notify.notify_one();
        }
    }
}

#[async_trait]
impl SessionNotifier for TuiSessionNotifier {
    async fn notify_prompt_start(
        &self,
        _project_id: &str,
        session_id: &str,
        _request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // 使用 try_send 提供背压保护，避免无限缓冲
        let _ = self.tx.try_send(AppEvent::PromptStarted {
            session_id: session_id.to_string(),
        });
        Ok(())
    }

    async fn notify_prompt_end(
        &self,
        _project_id: &str,
        session_id: &str,
        _stop_reason: StopReason,
        error_message: Option<String>,
        _request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = self.tx.try_send(AppEvent::PromptEnded {
            session_id: session_id.to_string(),
            error: error_message,
        });
        self.signal_completion();
        Ok(())
    }

    async fn notify_prompt_error(
        &self,
        _project_id: &str,
        session_id: &str,
        error: agent_client_protocol::schema::v1::Error,
        _request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let _ = self.tx.try_send(AppEvent::PromptEnded {
            session_id: session_id.to_string(),
            error: Some(format!("{:?}", error)),
        });
        self.signal_completion();
        Ok(())
    }

    async fn notify_session_update(
        &self,
        _project_id: &str,
        _session_id: &str,
        session_update: SessionUpdate,
        _request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        match session_update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                if let ContentBlock::Text(text) = &chunk.content {
                    let _ = self.tx.try_send(AppEvent::AgentText(text.text.clone()));
                }
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                if let ContentBlock::Text(text) = &chunk.content {
                    let _ = self.tx.try_send(AppEvent::AgentThought(text.text.clone()));
                }
            }
            SessionUpdate::ToolCall(tool_call) => {
                let _ = self.tx.try_send(AppEvent::ToolCall {
                    title: tool_call.title.clone(),
                    status: format!("{:?}", tool_call.status),
                });
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(ref title) = update.fields.title {
                    let status_str = update
                        .fields
                        .status
                        .as_ref()
                        .map(|s| format!("{:?}", s))
                        .unwrap_or_else(|| "updating".to_string());
                    let _ = self.tx.try_send(AppEvent::ToolCall {
                        title: title.clone(),
                        status: status_str,
                    });
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn notify(
        &self,
        _project_id: &str,
        _session_id: &str,
        _notify: SessionNotify,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}
