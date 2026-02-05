// 重新导出 shared_types 中的模型，保持向后兼容

pub use agent_abstraction::PromptMessage;

pub use shared_types::{
    // Agent model exports
    AgentStatus, ProjectAndAgentInfo,
    // 取消相关类型
    CancelNotificationRequestWrapper, CancelResult,
    // Session and message exports
    Attachment, AttachmentSource, TextAttachment, ImageAttachment, AudioAttachment, DocumentAttachment, SessionMessageType, UnifiedSessionMessage, SessionNotify, ChatPromptResponse,
};