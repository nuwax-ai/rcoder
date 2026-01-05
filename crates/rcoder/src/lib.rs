//! rcoder 库
//!
//! 提供 ACP 协议集成和 AI 代理管理功能

pub mod cleanup_task;
mod config;
pub mod grpc;
mod handler;
pub mod middleware;
pub mod router;
mod service;
pub mod storage;
mod utils;
pub mod vnc;

// 重新导出主要的类型和函数
pub use storage::{DataBridge, ProjectAdapter};
pub use utils::*;

// 重新导出 shared_types 中的类型
pub use shared_types::{
    AgentSessionUpdate, AgentStatus, AgentStatusResponse, AppError, Attachment, AttachmentError,
    AttachmentSource, AudioAttachment, CancelNotificationRequest, CancelNotificationResponse,
    ChatPrompt, ChatPromptResponse, ChatResponse, DocumentAttachment, HttpResult, ImageAttachment,
    ImageDimensions, ModelProviderConfig, ModelProviderSafeInfo, ProjectAndAgentInfo,
    SessionMessageType, SessionNotify, SessionPromptEnd, SessionPromptStart, TextAttachment,
    UnifiedSessionMessage,
};
