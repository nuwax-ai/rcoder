//! rcoder 库
//!
//! 提供 SACP 协议集成和 AI 代理管理功能

pub mod api_key_manager;
mod config;
pub mod grpc;
mod handler;
mod model;
pub mod otel_tracing; // 🔥 设为 public，供其他模块使用
mod proxy_agent;
pub mod router;
mod service;
mod utils;

// 测试辅助模块 (仅在 testing feature 启用时编译)
#[cfg(feature = "testing")]
pub mod testing;

// 重新导出主要的类型和函数
pub use model::*;
pub use otel_tracing::*;
pub use proxy_agent::*;
pub use utils::*;
