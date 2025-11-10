// 重新导出 shared_types 中的模型，保持向后兼容

pub use shared_types::{
    // Agent model exports
    AgentType, AgentStatus, AgentStatusResponse, ProjectAndAgentInfo, CancelNotificationRequest, CancelNotificationResponse,
    // Session and message exports
    Attachment, AttachmentSource, TextAttachment, ImageAttachment, AudioAttachment, DocumentAttachment,
    ImageDimensions, SessionMessageType, UnifiedSessionMessage, SessionPromptStart, SessionPromptEnd,
    SessionPromptError, AgentSessionUpdate, SessionNotify, ChatPrompt, ChatPromptResponse,
    // Error and HTTP exports
    AppError, HttpResult,
};