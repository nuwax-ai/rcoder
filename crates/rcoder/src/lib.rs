//! rcoder 库
//!
//! 提供 ACP 协议集成和 codex 代理管理功能

pub mod codex_agent_client;

// 重新导出主要的类型和函数
pub use codex_agent_client::*;
