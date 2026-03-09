//! agent_runner 库
//!
//! 提供 AI 代理运行时和 ACP 协议集成

pub mod agent_runtime;
pub mod api_key_manager;
mod config;
pub mod grpc;
mod handler;
mod model;
pub mod otel_tracing; // 🔥 设为 public，供其他模块使用
mod proxy_agent;
pub mod router;
pub mod service; // 🔥 设为 public，供测试使用
mod utils;

// 条件性编译：HTTP 服务器模块
#[cfg(feature = "http-server")]
pub mod http_server;

// 测试辅助模块 (仅在 testing feature 启用时编译)
#[cfg(feature = "testing")]
pub mod testing;

// 重新导出主要的类型和函数
pub use agent_runtime::*;
pub use config::*;
pub use model::*;
pub use otel_tracing::*;
pub use proxy_agent::*;
pub use service::*; // 重新导出 service 模块
pub use utils::*;

#[cfg(feature = "http-server")]
pub use http_server::{HttpServerConfig, start_http_server};
#[cfg(feature = "http-server")]
pub use http_server::start::HttpServerHandle;
