mod model;

pub use model::{
    ModelApiProtocol, ModelProviderConfig, ModelProviderSafeInfo, AgentType,
    // Agent model exports
    AgentStatus, AgentStatusResponse, ProjectAndAgentInfo, CancelNotificationRequest, CancelNotificationResponse,
    // Session and message exports
    Attachment, AttachmentError, AttachmentSource, TextAttachment, ImageAttachment, AudioAttachment, DocumentAttachment,
    ImageDimensions, SessionMessageType, UnifiedSessionMessage, SessionPromptStart, SessionPromptEnd,
    AgentSessionUpdate, SessionNotify, ChatPrompt, ChatPromptResponse, ChatResponse,
    // Error and HTTP exports
    AppError, HttpResult,
};

// 导出ChatPrompt的Builder
pub use model::chat_prompt::ChatPromptBuilder;
