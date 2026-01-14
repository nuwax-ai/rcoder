// 重新导出 shared_types 中的模型，保持向后兼容

pub use shared_types::{
    // Agent model exports
    AgentStatus, AgentStatusResponse, ProjectAndAgentInfo,
    // 取消相关类型（新类型优先）
    CancelNotificationRequestWrapper, CancelResult,
    // 旧类型保留兼容性（deprecated）
    CancelNotificationRequest,
    CancelNotificationResponse,
    // Session and message exports
    Attachment, AttachmentSource, TextAttachment, ImageAttachment, AudioAttachment, DocumentAttachment,
    ImageDimensions, SessionMessageType, UnifiedSessionMessage, SessionPromptStart, SessionPromptEnd,
    SessionPromptError, AgentSessionUpdate, SessionNotify, ChatPrompt, ChatPromptResponse,
    // Error and HTTP exports
    AppError, HttpResult,
};