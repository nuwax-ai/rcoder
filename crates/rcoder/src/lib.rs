//! rcoder 库
//!
//! 提供 ACP 协议集成和 codex 代理管理功能

mod model;
mod proxy_agent;
mod config;
mod handler;
mod service;
mod router;

// 重新导出主要的类型和函数
pub use proxy_agent::*;
pub use model::*;
