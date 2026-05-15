// 在 crate 根级别初始化 i18n
// fallback 设置为默认语言 en-US，保持一致性
rust_i18n::i18n!("locales", fallback = "en-US");

mod container;
mod model;

// 灵活的字符串反序列化器（支持 JSON 字符串和数字）
pub mod flexible_string;

// i18n 国际化模块
pub mod i18n;
pub use i18n::{
    DEFAULT_LOCALE, SUPPORTED_LOCALES, get_locale, parse_accept_language, set_locale, t, t_default,
};
pub mod request_locale;
pub use request_locale::{current_request_locale, scope_request_locale};

// HTTP 请求提取器模块（支持 JSON body 和 Query string 两种参数方式）
pub mod i18n_extractors;
pub use i18n_extractors::I18nJsonOrQuery;

// Chat Agent 配置模块
mod chat_agent_config;
pub use chat_agent_config::{
    ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig, ModelEnvBinding,
    ModelEnvBindingSource,
};

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
pub use error_codes::{get_error_message, get_i18n_message, get_i18n_message_default};

// Validation 模块
pub mod validation;
pub use validation::garde_err_to_app_error;

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
    PodCountByServiceType,
    PodCountResponse,
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

// 隔离类型模块
pub mod isolation_type;
pub use isolation_type::{IsolationType, IsolationTypeError};

// 导出ChatPrompt的Builder
pub use model::chat_prompt::ChatPromptBuilder;

// Agent HTTP API 类型（rcoder 和 agent_runner 共用）
pub mod agent_types;
pub use agent_types::*;

// Computer Agent HTTP API 类型
pub mod computer_agent_types;
pub use computer_agent_types::*;

// RCoder Agent HTTP Service trait
pub mod agent_http_service;
pub use agent_http_service::AgentHttpService;

// RCoder Agent HTTP API 类型
pub mod rcoder_agent_types;
pub use rcoder_agent_types::*;

// 通用 HTTP Handlers（基于 trait）
pub mod http_handlers;
