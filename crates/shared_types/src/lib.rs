mod container;
mod model;

// Chat Agent 配置模块
mod chat_agent_config;
pub use chat_agent_config::{ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig};

// API Key 验证器模块
pub mod api_key_validator;
pub use api_key_validator::{ApiKeyAuthConfig, ApiKeyAuthError, ApiKeyValidator};

// 新增多镜像配置相关模块
pub mod multi_image_config;
pub mod service_config;
pub mod service_type;

// 常量定义模块
pub mod constants;
pub use constants::*;

// 错误码定义模块
pub mod error_codes;

// gRPC 模块
pub mod grpc {
    // 包含生成的代码，路径相对于当前文件
    include!("grpc/agent.rs");
}

// 导出 URL 脱敏工具函数
pub mod grpc_mask;
pub use grpc_mask::mask_url;

// 导出 gRPC 脱敏包装器
pub mod grpc_wrapper;
pub use grpc_wrapper::MaskedModelConfig;

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
    // 取消相关类型
    CancelNotificationRequestWrapper,
    CancelNotificationResponse,
    CancelResult,
    ChatPrompt,
    ChatPromptResponse,
    ChatResponse,
    ContainerBasicInfo,
    DocumentAttachment,
    HealthResponse,
    HttpResult,
    ImageAttachment,
    ImageDimensions,
    ModelApiProtocol,
    ModelProviderConfig,
    ModelProviderSafeInfo,
    ProjectAndAgentInfo,
    ProjectAndContainerInfo,
    // Session trait
    SessionEntry,
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

// Computer Agent HTTP API 类型
pub mod computer_agent_types;
pub use computer_agent_types::*;
