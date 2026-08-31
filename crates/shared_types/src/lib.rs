mod agent;
mod container;
mod model;
mod runtime_config;
mod userapp;

// 容器域类型（高内聚收拢于 container/ 模块）—— 条目/查找/清理/统计 + 服务与隔离枚举
pub use container::{
    AppRuntimeIpResolver, CLEANUP_CHANNEL_CAPACITY, CleanupRequest, ContainerEntry,
    ContainerLookup, IdleContainerInfo, IsolationType, IsolationTypeError, MissingIdentifier,
    ProjectScope, ServiceType, ServiceTypeError, StorageStats, get_enabled_service_types,
    get_supported_service_types,
};

// project/session/container 映射存储契约（内存与 PG 双后端统一接口）
pub mod project_store;
pub use project_store::ProjectStore;

// UserApp 域（高内聚收拢于 userapp/ 模块）—— 活动追踪/唤醒 + 业务元数据 + build 进度事件 + 开发资源回收契约
pub use userapp::activity::{
    ActivityPersistence, ActivityRow, AppAccessTracker, AppWakeControl, WakeOutcome,
};
pub use userapp::app_stage::{UserappStage, invalid_app_stage_error};
pub use userapp::build_event::BuildProgressEvent;
pub use userapp::db_admin::{
    DbAdminError, DbUserUpsertOutcome, UserappDbCreateDatabaseRequest,
    UserappDbResetPasswordRequest, create_pg_database, upsert_pg_user,
};
pub use userapp::db_align::{
    AlignCredentialsOutcome, AlignCredentialsRequest, AlignError, CommandOutcome, PgCommandRunner,
    align_pg_credentials,
};
pub use userapp::dev_cleanup::UserappDevCleanup;
pub use userapp::dev_locator::{UserappDevEnsure, UserappDevLocator};
pub use userapp::forward_contract::{
    APP_ID_HEADER, APP_STAGE_DEV, APP_STAGE_HEADER, APP_STAGE_PROD, SERVICE_TYPE_HEADER,
    SERVICE_TYPE_USERAPP, is_userapp_service_type_value,
};
pub use userapp::metadata::{AppMetadataPersistence, AppMetadataRecord};

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

// API Key 验证器模块
pub mod api_key_validator;
pub use api_key_validator::{ApiKeyAuthConfig, ApiKeyAuthError, ApiKeyValidator};

pub mod permission_types;
pub mod pg_utils;
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
    ERR_AGENT_BUSY, ERR_AGENT_CONTAINER_UNAVAILABLE, ERR_AGENT_ERROR,
    ERR_AGENT_MGMT_ALREADY_INSTALLED, ERR_AGENT_MGMT_ARCHIVE_BOMB, ERR_AGENT_MGMT_BINARY_TOO_LARGE,
    ERR_AGENT_MGMT_BUILTIN_PROTECTED, ERR_AGENT_MGMT_CHECK_FAILED,
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
    ERR_INVALID_PARAMS, ERR_INVALID_RESOURCE_LIMITS, ERR_INVALID_STATE, ERR_MODEL_UNAVAILABLE,
    ERR_NOT_FOUND, ERR_OPERATION_NOT_SUPPORTED, ERR_PERMISSION_EXPIRED, ERR_PERMISSION_NOT_FOUND,
    ERR_PERMISSION_RESOLVE_FAILED, ERR_PROJECT_NOT_FOUND, ERR_PROXY_DISABLED,
    ERR_PROXY_SERVICE_UNAVAILABLE, ERR_RESOURCE_EXHAUSTED, ERR_RESUME_FAILED, ERR_RETRY_EXHAUSTED,
    ERR_SERVICE_UNAVAILABLE, ERR_SESSION_NOT_FOUND, ERR_STOP_FAILED, ERR_TOO_MANY_REQUESTS,
    ERR_UNKNOWN, ERR_VALIDATION, ERR_WORKSPACE_ERROR, SUCCESS, error_codes, get_error_description,
    get_error_message, get_i18n_message, get_i18n_message_default,
};

// Validation 模块
pub mod validation;
pub use validation::{
    IDENTIFIER_RE, USERAPP_APP_ID_MAX_LEN, garde_err_to_app_error, validate_identifier,
};

// UserApp 日志域契约（rcoder ↔ app-cli 单一事实源；OpenAPI schema 同源派生）
pub mod app_cli_logs;
pub use app_cli_logs::{
    LogQueryRequest, LogQueryResponse, LogRecord, LogSelector, LogSourceInfo, MAX_CURSOR_BYTES,
    MAX_KEYWORD_BYTES, MAX_SERVICES, MAX_SOURCES, MAX_TAIL_PER_SOURCE, SourceError,
};

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

// 部署配置域（高内聚收拢于 runtime_config/ 模块）—— Docker/K8s 双运行时配置族 + Quantity 解析
pub use runtime_config::k8s::{
    K8sGlobalDefaults, K8sServiceConfig, K8sSidecarSpec, K8sVolumeMountSpec, K8sVolumeSpec,
    K8sVolumeType, KubernetesConfig,
};
pub use runtime_config::multi_image::{
    GlobalImageDefaults, IMAGE_CACHE_DEFAULT_MAX_ENTRIES, IMAGE_CACHE_DEFAULT_TTL_SECS,
    ImageCacheConfig, ImageSelectionStrategy, MultiImageConfig, ProjectImageOverrides,
    create_default_multi_image_config, create_legacy_multi_image_config,
};
pub use runtime_config::quantity::{
    parse_cpu_quantity, parse_memory_quantity, validate_k8s_storage_size,
};
pub use runtime_config::service::{
    ServiceImageConfig, ServiceMountConfig, ServiceResourceLimits, ServiceSecurityConfig,
    default_agent_runner_service_config, default_rcoder_service_config,
};

// 导出ChatPrompt的Builder
pub use model::chat_prompt::ChatPromptBuilder;

// Agent HTTP 契约域（高内聚收拢于 agent/ 模块）—— API 类型/服务 trait/通用 handler/安装管理/Chat 配置
pub use agent::chat_config::{
    AgentMode, AutoReloadConfig, ChatAgentConfig, ChatAgentServerConfig, ChatContextServerConfig,
    ModelEnvBinding, ModelEnvBindingSource, ToolApprovalAction, ToolApprovalRule, VALID_TOOL_KINDS,
};
pub use agent::computer_types::*;
// 模块 re-export：保住 agent_runner 的 `use shared_types::http_handlers;` 路径
pub use agent::http_handlers;
pub use agent::http_service::AgentHttpService;
pub use agent::rcoder_types::*;
pub use agent::types::*;

// UserApp workspace manifest 类型（极轻量独立 crate，file-server build + app-cli runtime 共用）
pub use agent::mgmt_types::{
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
