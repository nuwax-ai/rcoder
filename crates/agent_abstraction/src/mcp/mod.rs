//! MCP server management module.
//!
//! 提供 MCP (Model Context Protocol) 服务器的管理功能：
//! - 启动/停止 MCP 服务进程
//! - 查询服务状态和可用工具
//! - 调用 MCP 工具进行验证
//!
//! ## 使用示例
//!
//! ```rust,ignore
//! use agent_abstraction::mcp::{McpServerManager, ToolCallRequest};
//! use agent_config::McpServerConfig;
//!
//! // 创建管理器
//! let manager = McpServerManager::new();
//!
//! // 注册并启动服务
//! let config = McpServerConfig::custom("git-server".into(), "uvx".into());
//! manager.start_new_server("git", config).await?;
//!
//! // 列出可用工具
//! let tools = manager.list_tools("git").await?;
//!
//! // 停止服务
//! manager.stop_server("git").await?;
//! ```

mod error;
mod instance;
mod manager;
mod types;

pub use error::{McpError, McpResult};
pub use instance::McpServerInstance;
pub use manager::McpServerManager;
pub use types::{McpServerInfo, McpServerStatus, ToolCallRequest, ToolCallResponse};

// Re-export rmcp types that are useful for consumers
pub use rmcp::model::Tool as McpTool;
