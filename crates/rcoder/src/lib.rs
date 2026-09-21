//! rcoder 库
//!
//! Phase 0 后 rcoder = HTTP 面（handler/router/middleware/server，Phase 1 迁
//! http-server）+ 组合根（main.rs 经 `rcoder::` 引用）。引擎模块已整体迁
//! `rcoder-engine` crate，此处 re-export 保持 `crate::X` 路径兼容——HTTP 层
//! 与 bin 的既有引用零改动，行为零变化。
/// dial9 事件级 Tokio tracing 装配（`dial9` feature 专用；bin 的 main 手动
/// 构建 runtime 时经 `rcoder::dial9_obs` 调用，须 pub）
#[cfg(feature = "dial9")]
pub use rcoder_engine::dial9_obs;
pub use rcoder_engine::{
    app_state, background_tasks, batch_migrate, bootstrap, cleanup_task, config, config_watcher,
    docker_init, file_server_admin, file_server_embed, grpc, http_client, preview_assembly,
    proxy_init, service, shutdown, skill_sync_reconciler, storage, userapp_builder,
    userapp_forward, userapp_recycle, utils, vnc, workspace_migrate,
};

// HTTP 面（Phase 1 迁 http-server）
pub mod handler;
pub mod middleware;
pub mod router;
pub mod router_docs;
pub mod server;

// 重新导出主要的类型和函数
pub use storage::{ProjectAdapter, ProjectStore, ProjectStoreBackend};
pub use utils::*;

// 重新导出 shared_types 中的类型
pub use shared_types::{
    AgentSessionUpdate, AgentStatus, AgentStatusResponse, AppError, Attachment, AttachmentError,
    AttachmentSource, AudioAttachment, CancelNotificationResponse, ChatPrompt, ChatPromptResponse,
    ChatResponse, DocumentAttachment, HttpResult, ImageAttachment, ImageDimensions,
    ModelProviderConfig, ModelProviderSafeInfo, ProjectAndAgentInfo, SessionMessageType,
    SessionNotify, SessionPromptEnd, SessionPromptStart, TextAttachment, UnifiedSessionMessage,
};
