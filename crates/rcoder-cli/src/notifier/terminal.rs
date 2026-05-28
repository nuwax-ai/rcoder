//! 终端会话通知器
//!
//! 实现 SessionNotifier trait，将 agent 的会话事件（文本输出、工具调用等）
//! 渲染到终端。用于 CLI 调试场景。

use agent_abstraction::PromptCompletionSignal;
use agent_abstraction::SessionNotifier;
use agent_client_protocol::schema::{ContentBlock, SessionUpdate, StopReason};
use async_trait::async_trait;
use shared_types::SessionNotify;

use crate::output::OutputFormatter;

/// 终端会话通知器
///
/// 将 ACP 会话事件渲染到 stdout/stderr：
/// - `AgentMessageChunk` → 输出文本内容到 stdout
/// - `ToolCall` / `ToolCallUpdate` → 输出工具调用信息到 stderr
/// - `notify_prompt_end` / `notify_prompt_error` → 触发 completion signal
///
/// # Completion Signal
///
/// 当 `send_prompt_and_wait()` 被调用时，需要 notifier 在 prompt 结束时
/// 触发 `signal.notify.notify_one()`。`TerminalSessionNotifier` 持有
/// `PromptCompletionSignal` 并在 `notify_prompt_end` / `notify_prompt_error` 中触发。
pub struct TerminalSessionNotifier {
    formatter: OutputFormatter,
    completion_signal: Option<PromptCompletionSignal>,
}

impl TerminalSessionNotifier {
    pub fn new(
        formatter: OutputFormatter,
        completion_signal: Option<PromptCompletionSignal>,
    ) -> Self {
        Self {
            formatter,
            completion_signal,
        }
    }

    /// 触发 completion signal（如果已配置）
    fn signal_completion(&self) {
        if let Some(ref signal) = self.completion_signal {
            signal.notify.notify_one();
        }
    }

    /// 渲染 SessionUpdate 事件到终端
    fn render_session_update(&self, update: &SessionUpdate) {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                self.render_content_block(&chunk.content);
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                // Thoughts rendered only in verbose mode
                self.formatter.debug(&format!(
                    "[thought] {}",
                    content_block_to_text(&chunk.content)
                ));
            }
            SessionUpdate::ToolCall(tool_call) => {
                self.formatter.tool_call(&tool_call.title, &format!("{:?}", tool_call.status));
            }
            SessionUpdate::ToolCallUpdate(update) => {
                if let Some(ref title) = update.fields.title {
                    let status_str = update
                        .fields
                        .status
                        .as_ref()
                        .map(|s| format!("{:?}", s))
                        .unwrap_or_else(|| "updating".to_string());
                    self.formatter.tool_call(title, &status_str);
                }
            }
            SessionUpdate::Plan(_plan) => {
                self.formatter.debug("[plan update received]");
            }
            SessionUpdate::CurrentModeUpdate(mode_update) => {
                self.formatter.info(&format!(
                    "Mode changed: {}",
                    mode_update.current_mode_id.0
                ));
            }
            SessionUpdate::ConfigOptionUpdate(_)
            | SessionUpdate::SessionInfoUpdate(_)
            | SessionUpdate::AvailableCommandsUpdate(_) => {
                self.formatter.trace("[config/session update]");
            }
            _ => {
                self.formatter.trace("[unknown session update]");
            }
        }
    }

    /// 渲染 ContentBlock 到 stdout
    fn render_content_block(&self, block: &ContentBlock) {
        match block {
            ContentBlock::Text(text_content) => {
                self.formatter.agent_text(&text_content.text);
            }
            ContentBlock::Image(_) => {
                self.formatter.debug("[image content]");
            }
            ContentBlock::Audio(_) => {
                self.formatter.debug("[audio content]");
            }
            ContentBlock::ResourceLink(link) => {
                self.formatter
                    .debug(&format!("[resource link: {}]", link.name));
            }
            ContentBlock::Resource(_) => {
                self.formatter.debug("[resource content]");
            }
            _ => {
                self.formatter.trace("[unknown content block]");
            }
        }
    }
}

/// 从 ContentBlock 提取纯文本
fn content_block_to_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text_content) => text_content.text.clone(),
        _ => format!("{:?}", block),
    }
}

#[async_trait]
impl SessionNotifier for TerminalSessionNotifier {
    async fn notify_prompt_start(
        &self,
        _project_id: &str,
        session_id: &str,
        _request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.formatter
            .info(&format!("Prompt started (session: {})", session_id));
        Ok(())
    }

    async fn notify_prompt_end(
        &self,
        _project_id: &str,
        session_id: &str,
        stop_reason: StopReason,
        error_message: Option<String>,
        _request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Ensure final newline after agent output
        println!();

        if let Some(ref err) = error_message {
            self.formatter.error(&format!(
                "Prompt ended with error (session: {}): {}",
                session_id, err
            ));
        } else {
            self.formatter
                .info(&format!("Prompt ended (session: {}, reason: {:?})", session_id, stop_reason));
        }

        // Signal completion to unblock send_prompt_and_wait()
        self.signal_completion();

        Ok(())
    }

    async fn notify_prompt_error(
        &self,
        _project_id: &str,
        session_id: &str,
        error: agent_client_protocol::schema::Error,
        _request_id: Option<String>,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.formatter
            .error(&format!("Prompt error (session: {}): {:?}", session_id, error));

        // Signal completion even on error
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
        self.render_session_update(&session_update);
        Ok(())
    }

    async fn notify(
        &self,
        _project_id: &str,
        _session_id: &str,
        notify: SessionNotify,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.formatter
            .debug(&format!("[SessionNotify: {:?}]", notify));
        Ok(())
    }
}
