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

/// axum 共享状态: 持有工作区解析器 + 全局配置。
///
/// 后续 task 会追加 dev server 进程池 / 端口池 / 模板缓存 等字段。
#[derive(Clone)]
pub struct AppState {
    pub resolver: Arc<dyn WorkspaceResolver>,
    pub config: Arc<Config>,
}
