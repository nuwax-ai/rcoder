//! rcoder 库
//!
//! 提供 ACP 协议集成和 AI 代理管理功能

mod config;
mod handler;
mod model;
mod proxy_agent;
mod router;
mod service;
mod utils;

// 重新导出主要的类型和函数
pub use model::*;
pub use proxy_agent::*;
pub use utils::*;
