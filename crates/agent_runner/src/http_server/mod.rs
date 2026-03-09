//! HTTP 服务器模块
//!
//! 仅在 `http-server` feature 启用时编译
//! 提供 Computer Agent HTTP REST API

pub mod handlers;
pub mod router;
pub mod start;

pub use router::{AppState, create_router};
pub use start::{HttpServerConfig, start_http_server};
