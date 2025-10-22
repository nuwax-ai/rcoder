mod agent_model;
mod attachment;
mod chat_prompt;

mod agent_session_notify;
mod app_error;
mod http_result;

use serde::{Serialize, Deserialize};

pub use agent_model::{
    AgentStatus, AgentStatusResponse, AgentType, CancelNotificationRequest,
    CancelNotificationResponse, ProjectAndAgentInfo,
};
pub use agent_session_notify::*;
pub use attachment::*;
pub use attachment::AttachmentSource;
pub use chat_prompt::{ChatPrompt, ChatPromptBuilder, ChatPromptResponse};

pub use app_error::AppError;
pub use http_result::*;

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
