//! rcoder-engine：rcoder 引擎层（Phase 0 自 rcoder crate 机械迁入）。
//!
//! AppState 装配、config、grpc 池、service 容器编排、userapp_builder/forward 编排、
//! bootstrap、docker_init 启动门、cleanup/background 任务、file-server 内嵌与管理、
//! preview 装配、proxy 初始化。HTTP 面（handler/router/middleware/server）留在
//! rcoder crate（Phase 1 迁 http-server），经本 crate 的 pub API 消费；
//! 依赖方向 rcoder → rcoder_engine 单向无环。

pub mod app_state;
pub mod background_tasks;
pub mod batch_migrate;
pub mod bootstrap;
pub mod cleanup_task;
pub mod config;
pub mod config_watcher;
/// dial9 事件级 Tokio tracing 装配（`dial9` feature 专用；bin 的 main 手动
/// 构建 runtime 时经 `rcoder_engine::dial9_obs` 调用，须 pub）
#[cfg(feature = "dial9")]
pub mod dial9_obs;
pub mod docker_init;
pub mod file_server_admin;
pub mod file_server_embed;
pub mod grpc;
pub mod http_client;
pub mod preview_assembly;
pub mod proxy_init;
pub mod service;
pub mod shutdown;
pub mod skill_sync_reconciler;
pub mod storage;
pub mod userapp_builder;
pub mod userapp_forward;
pub mod userapp_recycle;
pub mod utils;
pub mod vnc;
pub mod workspace_migrate;

// 重新导出主要的类型和函数
pub use storage::{ProjectAdapter, ProjectStore, ProjectStoreBackend};
pub use utils::*;

// 重新导出 shared_types 中的类型（保持 crate::AppError 等根路径在引擎内可用，
// 与原 rcoder lib 的 re-export 面一致）
pub use shared_types::{
    AgentSessionUpdate, AgentStatus, AgentStatusResponse, AppError, Attachment, AttachmentError,
    AttachmentSource, AudioAttachment, CancelNotificationResponse, ChatPrompt, ChatPromptResponse,
    ChatResponse, DocumentAttachment, HttpResult, ImageAttachment, ImageDimensions,
    ModelProviderConfig, ModelProviderSafeInfo, ProjectAndAgentInfo, SessionMessageType,
    SessionNotify, SessionPromptEnd, SessionPromptStart, TextAttachment, UnifiedSessionMessage,
};
