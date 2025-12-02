mod container;
mod model;

// gRPC 模块
pub mod grpc {
    // 包含生成的代码，路径相对于当前文件
    include!("grpc/agent.rs");
}

pub use model::{
    AgentLifecycle,
    AgentLifecycleGuard,
    AgentSessionUpdate,
    // Agent model exports
    AgentStatus,
    AgentStatusResponse,
    AgentStopHandle,
    AgentType,
    // Error and HTTP exports
    AppError,
    // Session and message exports
    Attachment,
    AttachmentError,
    AttachmentSource,
    AudioAttachment,
    CancelNotificationRequest,
    CancelNotificationResponse,
    ChatPrompt,
    ChatPromptResponse,
    ChatResponse,
    DocumentAttachment,
    HttpResult,
    ImageAttachment,
    ImageDimensions,
    ModelApiProtocol,
    ModelProviderConfig,
    ModelProviderSafeInfo,
    ProjectAndAgentInfo,
    ProjectAndContainerInfo,
    SessionMessageType,
    SessionNotify,
    SessionPromptEnd,
    SessionPromptError,
    SessionPromptStart,
    TextAttachment,
    UnifiedSessionMessage,
};

// 导出ChatPrompt的Builder
pub use model::chat_prompt::ChatPromptBuilder;
