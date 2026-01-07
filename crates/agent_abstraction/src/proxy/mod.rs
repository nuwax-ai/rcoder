//! # Proxy Chain 支持模块
//!
//! 提供 SACP Proxy Chain 功能，用于：
//! - 注入 MCP 服务器到会话请求
//! - 修改系统提示词
//! - 管理 Proxy 配置
//!
//! ## 架构
//!
//! ```text
//! Client <--> RCoderProxy <--> Agent
//!                  |
//!                  +-- MCP Servers (注入到 NewSessionRequest)
//!                  +-- System Prompt Override
//! ```
//!
//! ## 使用示例
//!
//! ```ignore
//! use agent_abstraction::proxy::{RCoderProxy, McpServerConfig, McpTransportType};
//!
//! let proxy = RCoderProxy::builder("rcoder-proxy")
//!     .with_system_prompt("You are a helpful coding assistant.")
//!     .with_mcp_server(McpServerConfig {
//!         name: "file-system".to_string(),
//!         description: Some("File system access".to_string()),
//!         transport: McpTransportType::Stdio {
//!             command: "node".to_string(),
//!             args: vec!["mcp-fs.js".to_string()],
//!             env: vec![],
//!         },
//!     })
//!     .build();
//!
//! // 将 MCP 服务器注入到 NewSessionRequest
//! let mut mcp_servers = vec![];
//! proxy.inject_mcp_servers(&mut mcp_servers);
//! ```
//!
//! ## Feature Flag
//!
//! 此模块通过 `sacp` feature 启用。

mod rcoder_proxy;

pub use rcoder_proxy::{
    McpServerConfig, McpTransportType, ProxyConfig, RCoderProxy, RCoderProxyBuilder,
};
