//! file-server: Rust 重写的 nuwax-file-server (留 rcoder pod)。

pub mod config;
pub mod error;
pub mod extract;
pub mod handlers;
pub mod logging;
pub mod models;
pub mod openapi;
pub mod ops;
pub mod path_safety;
pub mod response;
pub mod routes;
pub mod server;
pub mod service;
pub mod validation_rules;
pub mod workspace;

pub use workspace::{
    ComputerContext, LocalWorkspaceResolver, ProjectContext, SubvolumeWorkspaceResolver,
    WorkspacePathResolver, WorkspaceResolver,
};

use std::sync::Arc;

pub use crate::config::Config;

/// file-server 版本（编译期 crate 版本；health/openapi/启动日志共用单一事实源）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub use crate::server::{FileServer, FileServerBuilder};
pub use crate::service::build_manager::BuildManager;
pub use crate::service::dev_server::DevServerManager;
pub use crate::service::log_cache::LogCacheManager;
pub use crate::service::skill_download::SkillDownloader;
pub use crate::service::skills::{sync_agents, sync_target_version};

/// axum 共享状态: 持有工作区解析器 + 全局配置 + dev server 进程管理器。
#[derive(Clone)]
pub struct AppState {
    pub resolver: Arc<dyn WorkspaceResolver>,
    pub config: Arc<Config>,
    pub dev_server: Arc<DevServerManager>,
    pub build_manager: Arc<BuildManager>,
    pub log_cache: Arc<LogCacheManager>,
    pub skill_downloader: Arc<SkillDownloader>,
    pub started_at: std::time::Instant,
}
