use chrono::{DateTime, Utc};
use agent_client_protocol::schema::{Error, SessionUpdate, StopReason};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 消息主类型枚举
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub enum SessionMessageType {
    SessionPromptStart, // 用户发送 prompt 开始
    SessionPromptEnd,   // Agent 执行结束
    AgentSessionUpdate, // Agent 执行过程中的更新
    Heartbeat,          // SSE 连接心跳消息
}

/// 统一的会话消息结构
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnifiedSessionMessage {
    /// 会话ID
    pub session_id: String,
    /// 消息主类型
    pub message_type: SessionMessageType,
    /// 消息子类型
    pub sub_type: String,
    /// 具体数据内容
    pub data: serde_json::Value,
    /// 时间戳
    pub timestamp: DateTime<Utc>,
}

/// chat 对话的 prompt 开始
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptStart {
    pub session_id: String,
    /// 可选的请求ID，用于标识对应的用户请求
    pub request_id: Option<String>,
}

/// chat 对话的 prompt 结束
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptEnd {
    pub session_id: String,
    pub stop_reason: StopReason,
    /// 失败消息，用于记录 prompt 发送失败的异常信息
    pub error_message: Option<String>,
    /// 可选的请求ID，用于标识对应的用户请求
    pub request_id: Option<String>,
}
///agent 执行任务报错的消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPromptError {
    pub session_id: String,
    pub error: Error,
    /// 可选的请求ID，用于标识对应的用户请求
    pub request_id: Option<String>,
}

/// agent 的 session 更新
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionUpdate {
    pub session_id: String,
    pub session_update: SessionUpdate,
    /// 可选的请求ID，用于标识对应的用户请求
    pub request_id: Option<String>,
}

/// 需要发给前端的消息通知类型
#[derive(Debug, Clone, Serialize)]
pub enum SessionNotify {
    AgentSessionUpdate(Box<AgentSessionUpdate>),
    SessionPromptStart(SessionPromptStart),
    SessionPromptEnd(SessionPromptEnd),
    SessionPromptError(SessionPromptError),
}

impl SessionNotify {
    /// 转换为统一的前端接收的SSE的json消息
    pub fn to_unified_message(self) -> UnifiedSessionMessage {
        let timestamp = Utc::now();

        match self {
            SessionNotify::SessionPromptStart(start) => {
                let mut data = serde_json::json!({});

                // 如果有 request_id，添加到 data 中
                if let Some(request_id) = &start.request_id {
                    data["request_id"] = serde_json::Value::String(request_id.clone());
                }

                UnifiedSessionMessage {
                    session_id: start.session_id,
                    message_type: SessionMessageType::SessionPromptStart,
                    sub_type: "prompt_start".to_string(),
                    data,
                    timestamp,
                }
            }
            SessionNotify::SessionPromptEnd(end) => {
                let mut data = serde_json::json!({
                    "reason": format!("{:?}", end.stop_reason),
                    "description": stop_reason_to_description(&end.stop_reason)
                });

                // 如果有错误消息，添加到 data 中
                if let Some(error_msg) = &end.error_message {
                    data["error_message"] = serde_json::Value::String(error_msg.clone());
                }

                // 如果有 request_id，添加到 data 中
                if let Some(request_id) = &end.request_id {
                    data["request_id"] = serde_json::Value::String(request_id.clone());
                }

                UnifiedSessionMessage {
                    session_id: end.session_id,
                    message_type: SessionMessageType::SessionPromptEnd,
                    sub_type: stop_reason_to_string(&end.stop_reason),
                    data,
                    timestamp,
                }
            }
            SessionNotify::AgentSessionUpdate(update) => {
                let (sub_type, mut data) = session_update_to_parts(update.session_update);

                // 如果有 request_id，添加到 data 中
                if let Some(request_id) = &update.request_id {
                    data["request_id"] = serde_json::Value::String(request_id.clone());
                }

                UnifiedSessionMessage {
                    session_id: update.session_id,
                    message_type: SessionMessageType::AgentSessionUpdate,
                    sub_type,
                    data,
                    timestamp,
                }
            }
            SessionNotify::SessionPromptError(error) => {
                // 将 Error 直接序列化为 JSON，保持其原有结构（包含 code 和 message）
                let mut data = serde_json::to_value(&error.error).unwrap_or_else(|_| {
                    serde_json::json!({
                        "code": -1,
                        "message": error.error.to_string()
                    })
                });

                // 如果有 request_id，添加到 data 中
                if let Some(request_id) = &error.request_id {
                    data["request_id"] = serde_json::Value::String(request_id.clone());
                }

                UnifiedSessionMessage {
                    session_id: error.session_id,
                    message_type: SessionMessageType::SessionPromptEnd,
                    sub_type: "error".to_string(),
                    data,
                    timestamp,
                }
            }
        }
    }
}

impl UnifiedSessionMessage {
    /// 创建心跳消息
    pub fn heartbeat(session_id: String) -> Self {
        Self {
            session_id,
            message_type: SessionMessageType::Heartbeat,
            sub_type: "ping".to_string(),
            data: serde_json::json!({
                "type": "heartbeat",
                "message": "keep-alive",
                "timestamp": Utc::now().to_rfc3339()
            }),
            timestamp: Utc::now(),
        }
    }
}

/// 将 StopReason 转换为字符串
fn stop_reason_to_string(reason: &StopReason) -> String {
    match reason {
        StopReason::EndTurn => "end_turn".to_string(),
        StopReason::MaxTokens => "max_tokens".to_string(),
        StopReason::MaxTurnRequests => "max_turn_requests".to_string(),
        StopReason::Refusal => "refusal".to_string(),
        StopReason::Cancelled => "cancelled".to_string(),
        // 处理未来可能添加的新停止原因
        _ => "unknown".to_string(),
    }
}

/// 获取 StopReason 的描述
fn stop_reason_to_description(reason: &StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "Normal end",
        StopReason::MaxTokens => "Max token limit reached",
        StopReason::MaxTurnRequests => "Max turn request limit reached",
        StopReason::Refusal => "Agent refused to continue",
        StopReason::Cancelled => "User cancelled",
        // 处理未来可能添加的新停止原因
        _ => "unknown",
    }
}

/// 将 SessionUpdate 转换为 (sub_type, data) 元组
fn session_update_to_parts(update: SessionUpdate) -> (String, serde_json::Value) {
    match update {
        SessionUpdate::UserMessageChunk(content) => {
            ("user_message_chunk".to_string(), serde_json::json!(content))
        }
        SessionUpdate::AgentMessageChunk(content) => (
            "agent_message_chunk".to_string(),
            serde_json::json!(content),
        ),
        SessionUpdate::AgentThoughtChunk(content) => (
            "agent_thought_chunk".to_string(),
            serde_json::json!(content),
        ),
        SessionUpdate::ToolCall(tool_call) => {
            ("tool_call".to_string(), serde_json::json!(tool_call))
        }
        SessionUpdate::ToolCallUpdate(tool_call_update) => (
            "tool_call_update".to_string(),
            serde_json::json!(tool_call_update),
        ),
        SessionUpdate::Plan(plan) => ("plan".to_string(), serde_json::json!(plan)),
        SessionUpdate::AvailableCommandsUpdate(available_commands) => (
            "available_commands_update".to_string(),
            serde_json::json!({
                "available_commands": available_commands
            }),
        ),
        SessionUpdate::CurrentModeUpdate(current_mode_id) => (
            "current_mode_update".to_string(),
            serde_json::json!({
                "current_mode_id": current_mode_id
            }),
        ),
        // 处理未来可能添加的新更新类型
        _ => ("unknown_update".to_string(), serde_json::json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::ContentChunk;

    #[test]
    fn test_session_prompt_start_to_unified() {
        let notify = SessionNotify::SessionPromptStart(SessionPromptStart {
            session_id: "test_session".to_string(),
            request_id: None,
        });

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::SessionPromptStart)
        );
        assert_eq!(unified.sub_type, "prompt_start");
        assert_eq!(unified.data, serde_json::json!({}));
    }

    #[test]
    fn test_session_prompt_start_with_request_id_to_unified() {
        let notify = SessionNotify::SessionPromptStart(SessionPromptStart {
            session_id: "test_session".to_string(),
            request_id: Some("req_123456789".to_string()),
        });

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::SessionPromptStart)
        );
        assert_eq!(unified.sub_type, "prompt_start");
        assert_eq!(unified.data["request_id"], "req_123456789");
    }

    #[test]
    fn test_session_prompt_end_to_unified() {
        let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
            session_id: "test_session".to_string(),
            stop_reason: StopReason::EndTurn,
            error_message: None,
            request_id: None,
        });

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::SessionPromptEnd)
        );
        assert_eq!(unified.sub_type, "end_turn");
        assert_eq!(unified.data["reason"], "EndTurn");
        assert_eq!(unified.data["description"], "正常结束");
        assert!(
            !unified
                .data
                .as_object()
                .unwrap()
                .contains_key("error_message")
        );
        assert!(!unified.data.as_object().unwrap().contains_key("request_id"));
    }

    #[test]
    fn test_session_prompt_end_with_error_to_unified() {
        let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
            session_id: "test_session".to_string(),
            stop_reason: StopReason::Cancelled,
            error_message: Some("Connection timeout".to_string()),
            request_id: None,
        });

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::SessionPromptEnd)
        );
        assert_eq!(unified.sub_type, "cancelled");
        assert_eq!(unified.data["reason"], "Cancelled");
        assert_eq!(unified.data["description"], "用户取消");
        assert_eq!(unified.data["error_message"], "Connection timeout");
        assert!(!unified.data.as_object().unwrap().contains_key("request_id"));
    }

    #[test]
    fn test_session_prompt_end_with_request_id_to_unified() {
        let notify = SessionNotify::SessionPromptEnd(SessionPromptEnd {
            session_id: "test_session".to_string(),
            stop_reason: StopReason::Cancelled,
            error_message: Some("Connection timeout".to_string()),
            request_id: Some("req_123456789".to_string()),
        });

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::SessionPromptEnd)
        );
        assert_eq!(unified.sub_type, "cancelled");
        assert_eq!(unified.data["reason"], "Cancelled");
        assert_eq!(unified.data["description"], "用户取消");
        assert_eq!(unified.data["error_message"], "Connection timeout");
        assert_eq!(unified.data["request_id"], "req_123456789");
    }

    #[test]
    fn test_agent_session_update_to_unified() {
        let content = ContentChunk::new("Hello, World!".into());

        let update = SessionUpdate::AgentMessageChunk(content);
        let notify = SessionNotify::AgentSessionUpdate(Box::new(AgentSessionUpdate {
            session_id: "test_session".to_string(),
            session_update: update,
            request_id: None,
        }));

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::AgentSessionUpdate)
        );
        assert_eq!(unified.sub_type, "agent_message_chunk");

        // ACP 0.8 中 ContentChunk 的格式：{"content": {"text": "...", "type": "text"}}
        assert_eq!(unified.data["content"]["text"], "Hello, World!");
        assert_eq!(unified.data["content"]["type"], "text");

        assert!(!unified.data.as_object().unwrap().contains_key("request_id"));
    }

    #[test]
    fn test_agent_session_update_with_request_id_to_unified() {
        let content = ContentChunk::new("Hello, World!".into());

        let update = SessionUpdate::AgentMessageChunk(content);
        let notify = SessionNotify::AgentSessionUpdate(Box::new(AgentSessionUpdate {
            session_id: "test_session".to_string(),
            session_update: update,
            request_id: Some("req_123456789".to_string()),
        }));

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::AgentSessionUpdate)
        );
        assert_eq!(unified.sub_type, "agent_message_chunk");
        // ACP 0.8 中 ContentChunk 的格式：{"content": {"text": "...", "type": "text"}}
        assert_eq!(unified.data["content"]["text"], "Hello, World!");
        assert_eq!(unified.data["content"]["type"], "text");
        assert_eq!(unified.data["request_id"], "req_123456789");
    }

    #[test]
    fn test_session_prompt_error_to_unified() {
        let notify = SessionNotify::SessionPromptError(SessionPromptError {
            session_id: "test_session".to_string(),
            error: Error::internal_error(),
            request_id: None,
        });

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::SessionPromptEnd)
        );
        assert_eq!(unified.sub_type, "error");

        // 验证 data 直接包含 code 和 message 字段
        let data_obj = unified.data.as_object().unwrap();
        assert!(data_obj.contains_key("code"));
        assert!(data_obj.contains_key("message"));
        assert!(!data_obj.contains_key("request_id"));
    }

    #[test]
    fn test_session_prompt_error_with_request_id_to_unified() {
        let notify = SessionNotify::SessionPromptError(SessionPromptError {
            session_id: "test_session".to_string(),
            error: Error::method_not_found(),
            request_id: Some("req_123456789".to_string()),
        });

        let unified = notify.to_unified_message();

        assert_eq!(unified.session_id, "test_session");
        assert!(
            matches!(unified.message_type, SessionMessageType::SessionPromptEnd)
        );
        assert_eq!(unified.sub_type, "error");

        // 验证 data 直接包含 code 和 message 字段
        let data_obj = unified.data.as_object().unwrap();
        assert!(data_obj.contains_key("code"));
        assert!(data_obj.contains_key("message"));
        assert_eq!(unified.data["request_id"], "req_123456789");
    }
}
