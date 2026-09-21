//! http-server：rcoder 的 HTTP 面（Phase 1 自 rcoder crate 机械迁入）。
//!
//! axum handlers（含 chat_forward）、router 装配、utoipa OpenAPI 文档、middleware、
//! hyper 连接管理。消费 rcoder-engine 的 pub API（AppState 引擎句柄、grpc 池、
//! service 编排、userapp_builder/forward 控制面）；依赖方向
//! http-server → rcoder-engine 单向，rcoder（组合根）→ http-server 消费。
//!
//! 引擎模块在此 re-export 以保持迁移代码的 `crate::X` 路径零改写（与 Phase 0
//! rcoder 侧同款策略，行为零变化）。
pub use rcoder_engine::{
    app_state, background_tasks, batch_migrate, bootstrap, cleanup_task, config, config_watcher,
    docker_init, file_server_admin, file_server_embed, grpc, http_client, preview_assembly,
    proxy_init, service, shutdown, skill_sync_reconciler, storage, userapp_builder,
    userapp_forward, userapp_recycle, utils, vnc, workspace_migrate,
};

// HTTP 面（本 crate 主体）
pub mod handler;
pub mod middleware;
pub mod router;
pub mod router_docs;
pub mod server;

// 重新导出主要的类型和函数
pub use storage::{ProjectAdapter, ProjectStore, ProjectStoreBackend};
pub use utils::*;

// 重新导出 shared_types 中的类型（保持 crate::AppError 等根路径可用）
pub use shared_types::{
    AgentSessionUpdate, AgentStatus, AgentStatusResponse, AppError, Attachment, AttachmentError,
    AttachmentSource, AudioAttachment, CancelNotificationResponse, ChatPrompt, ChatPromptResponse,
    ChatResponse, DocumentAttachment, HttpResult, ImageAttachment, ImageDimensions,
    ModelProviderConfig, ModelProviderSafeInfo, ProjectAndAgentInfo, SessionMessageType,
    SessionNotify, SessionPromptEnd, SessionPromptStart, TextAttachment, UnifiedSessionMessage,
};
