mod container;
mod model;

// Chat Agent 配置模块
mod chat_agent_config;
pub use chat_agent_config::{ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig};

// 新增多镜像配置相关模块
pub mod multi_image_config;
pub mod service_config;
pub mod service_type;

// 常量定义模块
pub mod constants;
pub use constants::*;

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
    // Error and HTTP exports
    AppError,
    // Session and message exports
    Attachment,
    AttachmentError,
    AttachmentSource,
    AudioAttachment,
    // 旧类型保留兼容性（deprecated）
    CancelNotificationRequest,
    // 取消相关类型（新类型优先）
    CancelNotificationRequestWrapper,
    CancelNotificationResponse,
    CancelResult,
    ChatPrompt,
    ChatPromptResponse,
    ChatResponse,
    ContainerBasicInfo,
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

// 导出多镜像配置相关类型
pub use multi_image_config::{
    GlobalImageDefaults, ImageCacheConfig, ImageSelectionStrategy, MultiImageConfig,
    ProjectImageOverrides, create_default_multi_image_config, create_legacy_multi_image_config,
};
pub use service_config::{
    ServiceImageConfig, ServiceMountConfig, ServiceResourceLimits,
    default_agent_runner_service_config, default_rcoder_service_config,
};
pub use service_type::{
    ServiceType, ServiceTypeError, get_enabled_service_types, get_supported_service_types,
};

// 导出ChatPrompt的Builder
pub use model::chat_prompt::ChatPromptBuilder;
