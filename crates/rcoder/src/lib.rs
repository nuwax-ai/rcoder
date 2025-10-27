//! rcoder 库
//!
//! 提供 ACP 协议集成和 codex 代理管理功能

mod config;
mod handler;
mod proxy_agent;
mod router;
mod service;
mod utils;

// 重新导出主要的类型和函数
pub use proxy_agent::*;
pub use utils::*;

// 重新导出 shared_types 中的类型
pub use shared_types::{
    AgentType, AgentStatus, AgentStatusResponse, ProjectAndAgentInfo, CancelNotificationRequest, CancelNotificationResponse,
    Attachment, AttachmentError, AttachmentSource, TextAttachment, ImageAttachment, AudioAttachment, DocumentAttachment,
    ImageDimensions, SessionMessageType, UnifiedSessionMessage, SessionPromptStart, SessionPromptEnd,
    AgentSessionUpdate, SessionNotify, ChatPrompt, ChatPromptResponse,
    AppError, HttpResult, ModelProviderConfig, ModelProviderSafeInfo, ChatResponse,
};
