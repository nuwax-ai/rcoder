// Re-export models from shared_types for backwards compatibility
pub use shared_types::{
    AgentStatus, AgentStatusResponse, AgentType, CancelNotificationRequest,
    CancelNotificationResponse, ProjectAndAgentInfo,
    Attachment, AttachmentSource, TextAttachment, ImageAttachment, AudioAttachment, DocumentAttachment,
    ImageDimensions, SessionMessageType, UnifiedSessionMessage, SessionPromptStart, SessionPromptEnd,
    AgentSessionUpdate, SessionNotify, ChatPrompt, ChatPromptResponse,
    AppError, HttpResult,
};

use serde::{Serialize, Deserialize};

/// 聊天响应结构
#[derive(Debug, Serialize, Deserialize, utoipa::ToSchema)]
pub struct ChatResponse {
    /// 项目 ID
    #[schema(example = "test_project")]
    pub project_id: String,
    /// 会话 ID
    #[schema(example = "session456")]
    pub session_id: String,
    /// 可选的错误信息
    pub error: Option<String>,
    /// 请求ID，用于标识和追踪请求
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "req_123456789")]
    pub request_id: Option<String>,
}