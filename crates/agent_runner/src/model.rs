// 重新导出 shared_types 中的模型，保持向后兼容


pub use shared_types::{
    // Agent model exports
    AgentStatus,
    // Session and message exports
    Attachment,
    AttachmentSource,
    AudioAttachment,
    // 取消相关类型
    CancelNotificationRequestWrapper,
    CancelResult,
    ChatPromptResponse,
    DocumentAttachment,
    ImageAttachment,
    ProjectAndAgentInfo,
    SessionMessageType,
    SessionNotify,
    TextAttachment,
    UnifiedSessionMessage,
};

// 重新导出 ACP 类型，供客户端构造取消请求使用
