//! rcoder 库
//!
//! 提供 ACP 协议集成和 AI 代理管理功能

// 单树化：全部模块在 lib 树声明唯一一份（bin 的 main.rs 只做编排入口经 `rcoder::`
// 引用）——消灭 lib/bin 双树对同一源文件的双份编译与类型分裂；config 的
// load_config* 消费者（bootstrap/config_watcher）进 lib 树后 dead_code 自然消失。
pub mod app_state;
pub mod background_tasks;
pub mod batch_migrate;
pub mod bootstrap;
pub mod cleanup_task;
pub mod config;
pub mod config_watcher;
/// tokio-console 观测装配（`console` feature 专用）
#[cfg(feature = "console")]
pub(crate) mod console_obs;
pub mod docker_init;
pub mod file_server_admin;
pub mod file_server_embed;
pub mod grpc;
pub mod handler;
pub mod http_client;
pub mod middleware;
pub mod proxy_init;
pub mod router;
pub mod router_docs;
pub mod server;
pub(crate) mod service;
pub mod shutdown;
pub mod skill_sync_reconciler;
pub mod storage;
pub mod userapp_forward;
pub mod userapp_publish;
pub(crate) mod userapp_recycle;
pub(crate) mod utils;
pub mod vnc;
pub(crate) mod workspace_migrate;

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
