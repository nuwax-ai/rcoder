// Re-export models from shared_types for backwards compatibility
pub use shared_types::{
    AgentStatus, AgentStatusResponse, AgentType, CancelNotificationRequest,
    CancelNotificationResponse, ProjectAndAgentInfo,
    Attachment, AttachmentSource, TextAttachment, ImageAttachment, AudioAttachment, DocumentAttachment,
    ImageDimensions, SessionMessageType, UnifiedSessionMessage, SessionPromptStart, SessionPromptEnd,
    AgentSessionUpdate, SessionNotify, ChatPrompt, ChatPromptResponse, ChatResponse,
    AppError, HttpResult,
};