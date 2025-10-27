//! 通用 ACP (Agent Client Protocol) 适配器
//!
//! 此模块提供了与 ACP 兼容的 AI 代理通信的核心功能，
//! 包括连接管理、会话生命周期、消息处理和 MCP 集成。


pub mod mention;
pub mod types;


pub use types::{
    ConnectionState, Plan, PlanEntry, PlanEntryPriority, PlanEntryStatus, PlanStats, SessionState,
};
pub use types::{StreamUpdate, Tool, ToolCallId, UserMessageId};
