mod container;
mod model;

// UserApp build 进度事件 —— file-server(发送)与 rcoder(接收)共享的类型化 DTO
pub mod build_event;
pub use build_event::BuildProgressEvent;

// 清理请求模块
pub mod cleanup_request;
pub use cleanup_request::CleanupRequest;

// 存储类型模块
pub mod storage_types;
pub use storage_types::{IdleContainerInfo, StorageStats};

// 容器查找接口模块
pub mod container_lookup;
pub use container_lookup::{ContainerLookup, ProjectScope};

// UserApp 活动追踪 + 流量唤醒接口模块（闲置自动回收 / wake-on-traffic）
pub mod app_activity;
pub use app_activity::{AppAccessTracker, AppWakeControl, WakeOutcome};

// 容器条目模块（refcount + 活跃时间跟踪）
pub mod container_entry;
pub use container_entry::ContainerEntry;

// 灵活的字符串反序列化器（支持 JSON 字符串和数字）
pub mod flexible_string;
pub mod version_util;

// i18n 国际化模块 — 重导出自 shared_types_i18n（过渡期兼容）
pub use shared_types_i18n::{
    DEFAULT_LOCALE, SUPPORTED_LOCALES, get_locale, i18n, parse_accept_language, set_locale, t,
    t_default,
};

pub mod request_locale;
pub use request_locale::{current_request_locale, scope_request_locale};

// HTTP 请求提取器模块（支持 JSON body 和 Query string 两种参数方式）
pub mod i18n_extractors;
pub use i18n_extractors::I18nJsonOrQuery;

// Chat Agent 配置模块
mod chat_agent_config;
pub use chat_agent_config::{
    AgentMode, AutoReloadConfig, ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig,
    ModelEnvBinding, ModelEnvBindingSource, ToolApprovalAction, ToolApprovalRule, VALID_TOOL_KINDS,
};

// API Key 验证器模块
pub mod api_key_validator;
pub use api_key_validator::{ApiKeyAuthConfig, ApiKeyAuthError, ApiKeyValidator};

// 新增多镜像配置相关模块
pub mod multi_image_config;
pub mod permission_types;
pub mod pg_utils;
pub mod service_config;
pub mod service_type;
// K8s 运行时专用配置(与 docker_config 分家)
pub mod k8s_config;
pub use permission_types::{
    PermissionResolveRequest, ResolvePermissionHttpRequest, ResolvePermissionRequestDto,
    ResolvePermissionResponseDto,
};

// 常量定义模块
pub mod constants;
// 工作区路径常量 (单一事实源, 所有 crate 共用: rcoder/docker_manager/agent_runner)
pub mod paths;
pub use constants::*;

// 错误码定义模块 — 重导出自 shared_types_i18n（过渡期兼容）
pub use shared_types_i18n::{
    ERR_AGENT_BUSY, ERR_AGENT_ERROR, ERR_AGENT_MGMT_ALREADY_INSTALLED, ERR_AGENT_MGMT_ARCHIVE_BOMB,
    ERR_AGENT_MGMT_BINARY_TOO_LARGE, ERR_AGENT_MGMT_BUILTIN_PROTECTED, ERR_AGENT_MGMT_CHECK_FAILED,
    ERR_AGENT_MGMT_CHECKSUM_MISMATCH, ERR_AGENT_MGMT_COMMAND_TIMEOUT, ERR_AGENT_MGMT_DISK_FULL,
    ERR_AGENT_MGMT_INSTALL_CANCELLED, ERR_AGENT_MGMT_INSTALL_FAILED, ERR_AGENT_MGMT_INVALID_CHUNK,
    ERR_AGENT_MGMT_INVALID_MANIFEST, ERR_AGENT_MGMT_INVALID_VERSION, ERR_AGENT_MGMT_NOT_FOUND,
    ERR_AGENT_MGMT_PATH_TRAVERSAL, ERR_AGENT_MGMT_PERMISSION_DENIED,
    ERR_AGENT_MGMT_PLATFORM_NOT_FOUND, ERR_AGENT_MGMT_STREAM_TRUNCATED,
    ERR_AGENT_MGMT_UNINSTALL_FAILED, ERR_AGENT_MGMT_UNKNOWN_AGENT, ERR_AGENT_MGMT_UNSUPPORTED_TYPE,
    ERR_AGENT_NOT_FOUND, ERR_AGENT_RUNNER_UNAVAILABLE, ERR_API_KEY_AUTH_FAILED,
    ERR_APP_ALREADY_EXISTS, ERR_APP_NOT_FOUND, ERR_BACKEND_ERROR, ERR_CANCEL_FAILED, ERR_CONFLICT,
    ERR_CONTAINER_ERROR, ERR_CONTAINER_NOT_FOUND, ERR_FILE_NOT_FOUND, ERR_GRPC_ADDR_ERROR,
    ERR_GRPC_ERROR, ERR_HTTP_FALLBACK_FAILED, ERR_IMAGE_PULL_FAILED, ERR_INTERNAL_SERVER_ERROR,
    ERR_INVALID_PARAMS, ERR_INVALID_RESOURCE_LIMITS, ERR_INVALID_STATE, ERR_NOT_FOUND,
    ERR_OPERATION_NOT_SUPPORTED, ERR_PERMISSION_EXPIRED, ERR_PERMISSION_NOT_FOUND,
    ERR_PERMISSION_RESOLVE_FAILED, ERR_PROJECT_NOT_FOUND, ERR_PROXY_DISABLED,
    ERR_PROXY_SERVICE_UNAVAILABLE, ERR_RESOURCE_EXHAUSTED, ERR_RESUME_FAILED, ERR_RETRY_EXHAUSTED,
    ERR_SERVICE_UNAVAILABLE, ERR_SESSION_NOT_FOUND, ERR_STOP_FAILED, ERR_TOO_MANY_REQUESTS,
    ERR_UNKNOWN, ERR_VALIDATION, ERR_WORKSPACE_ERROR, SUCCESS, error_codes, get_error_description,
    get_error_message, get_i18n_message, get_i18n_message_default,
};

// Validation 模块
pub mod validation;
pub use validation::{garde_err_to_app_error, validate_identifier};

pub mod quantity;
pub use quantity::{parse_cpu_quantity, parse_memory_quantity, validate_k8s_storage_size};

// gRPC 模块 — 重导出自 shared_types_grpc（过渡期兼容）
pub use shared_types_grpc::grpc;

// 导出 URL 脱敏工具函数（re-export from shared_types_grpc）
pub use shared_types_grpc::mask_url;

// 导出 gRPC 脱敏包装器（重导出自 shared_types_grpc）
pub use shared_types_grpc::MaskedModelConfig;

pub use model::{
    AcpRequestPermission,
    AgentBinarySnapshot,
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
    HealthCheckResponse,
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
    ProjectExtendedFields,
    // Session trait
    SessionEntry,
    SessionMessageType,
    SessionNotify,
    SessionPromptEnd,
    SessionPromptError,
    SessionPromptStart,
    TextAttachment,
    UnifiedSessionMessage,
    VncStatusResponse,
};

// 导出多镜像配置相关类型
pub use k8s_config::{
    K8sGlobalDefaults, K8sServiceConfig, K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec,
    K8sVolumeType, KubernetesConfig,
};
pub use multi_image_config::{
    GlobalImageDefaults, ImageCacheConfig, ImageSelectionStrategy, MultiImageConfig,
    ProjectImageOverrides, create_default_multi_image_config, create_legacy_multi_image_config,
};
pub use service_config::{
    ServiceImageConfig, ServiceMountConfig, ServiceResourceLimits, ServiceSecurityConfig,
    default_agent_runner_service_config, default_rcoder_service_config,
};
pub use service_type::{
    MissingIdentifier, ServiceType, ServiceTypeError, get_enabled_service_types,
    get_supported_service_types,
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
pub mod agent_mgmt_types;
pub mod http_handlers;

// UserApp workspace manifest 类型（极轻量独立 crate，file-server build + app-cli runtime 共用）
pub use agent_mgmt_types::{
    AGENT_CACHE_DIR, AgentDetailInfo, AgentIdentity, AgentInfo, AgentInstallStatus,
    CheckAgentRequest, CheckAgentResponse, DEFAULT_ACP_AGENT_INSTALL_DIR, GetAgentRequest,
    InstallAction, InstallAgentResponse, InstallBinaryRequest, InstallFromPackageManagerRequest,
    InstallFromUrlRequest, InstallType, ListAgentsRequest, ListAgentsResponse, MAX_BINARY_SIZE,
    MAX_EXTRACTED_SIZE, PlatformEntry, RoutingParams, StaticCheckResult, SystemInfo,
    UPLOAD_CHUNK_SIZE, URL_DOWNLOAD_TIMEOUT_SECS, UninstallAgentRequest, UninstallAgentResponse,
};
pub use workspace_manifest::{
    BuildSection, DiscoverError, DiscoveredProject, HealthSection, LockedPingap, LockedService,
    LogFormat, LogSource, LogsSection, ManifestError, PingapMode, PingapSection, ProjectKind,
    ProjectManifest, ProjectMeta, ProjectType, ProxySection, ReleaseLock, ReleaseMetadata,
    RunSection, WorkspaceManifest, WorkspaceMeta, build_release_lock, discover_projects,
    parse_project, parse_workspace, validate_project, validate_service_id, validate_topology,
    validate_workspace,
};
