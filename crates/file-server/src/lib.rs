//! file-server: Rust 重写的 nuwax-file-server (留 rcoder pod)。

pub mod config;
pub mod error;
pub mod handler;
pub mod path_safety;
pub mod response;
pub mod routes;
pub mod service;
pub mod workspace;

pub use workspace::{ComputerContext, LocalWorkspaceResolver, ProjectContext, WorkspaceResolver};

use std::sync::Arc;

pub use crate::config::Config;
pub use crate::service::dev_server::DevServerManager;

/// axum 共享状态: 持有工作区解析器 + 全局配置 + dev server 进程管理器。
#[derive(Clone)]
pub struct AppState {
    pub resolver: Arc<dyn WorkspaceResolver>,
    pub config: Arc<Config>,
    pub dev_server: Arc<DevServerManager>,
}
