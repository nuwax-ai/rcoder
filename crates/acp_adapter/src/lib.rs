//! 通用 ACP (Agent Client Protocol) 适配器
//!
//! 此模块提供了与 ACP 兼容的 AI 代理通信的核心功能，
//! 包括连接管理、会话生命周期、消息处理和 MCP 集成。

pub mod config;
pub mod connection;
pub mod session;
pub mod mcp;
pub mod process;
pub mod types;

// 重新导出主要的公共类型
pub use config::AcpConfig;
pub use connection::{AcpConnection, ConnectionManager};
pub use session::{Session, SessionHandle, SessionManager, SessionStatistics};
pub use types::{SessionState, ConnectionState};
pub use mcp::{McpManager, McpAdapter, McpTool, McpResource};
pub use config::McpServerConfig;
pub use process::{ProcessManager, ProcessHandle};
pub use types::{SessionId, StreamUpdate, ToolCallId, UserMessageId, Tool};
pub use agent_client_protocol::{ToolCall, ToolCallStatus};

use std::sync::Arc;

/// ACP 适配器错误类型
#[derive(Debug, thiserror::Error)]
pub enum AcpAdapterError {
    #[error("连接错误: {0}")]
    Connection(String),

    #[error("进程错误: {0}")]
    Process(String),

    #[error("会话错误: {0}")]
    Session(String),

    #[error("配置错误: {0}")]
    Config(String),

    #[error("协议错误: {0}")]
    Protocol(String),

    #[error("MCP 错误: {0}")]
    Mcp(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),

    #[error("其他错误: {0}")]
    Other(#[from] anyhow::Error),
}

impl AcpAdapterError {
    pub fn connection<S: Into<String>>(msg: S) -> Self {
        AcpAdapterError::Connection(msg.into())
    }

    pub fn process<S: Into<String>>(msg: S) -> Self {
        AcpAdapterError::Process(msg.into())
    }

    pub fn session<S: Into<String>>(msg: S) -> Self {
        AcpAdapterError::Session(msg.into())
    }

    pub fn config<S: Into<String>>(msg: S) -> Self {
        AcpAdapterError::Config(msg.into())
    }

    pub fn protocol<S: Into<String>>(msg: S) -> Self {
        AcpAdapterError::Protocol(msg.into())
    }

    pub fn mcp<S: Into<String>>(msg: S) -> Self {
        AcpAdapterError::Mcp(msg.into())
    }
}

/// ACP 适配器结果类型
pub type AcpResult<T> = Result<T, AcpAdapterError>;

/// ACP 适配器主结构
#[derive(Clone)]
pub struct AcpAdapter {
    config: Arc<AcpConfig>,
    connection_manager: Arc<ConnectionManager>,
    mcp_manager: Arc<McpManager>,
}

impl AcpAdapter {
    /// 创建新的 ACP 适配器实例
    pub fn new(config: AcpConfig) -> Self {
        Self {
            config: Arc::new(config),
            connection_manager: Arc::new(ConnectionManager::new()),
            mcp_manager: Arc::new(McpManager::new()),
        }
    }

    /// 获取配置引用
    pub fn config(&self) -> &AcpConfig {
        &self.config
    }

    /// 获取连接管理器
    pub fn connection_manager(&self) -> &ConnectionManager {
        &self.connection_manager
    }

    /// 获取 MCP 管理器
    pub fn mcp_manager(&self) -> &McpManager {
        &self.mcp_manager
    }

    /// 创建新会话
    pub async fn create_session(&self) -> AcpResult<SessionHandle> {
        let session = Session::new(self.config.clone());
        let handle = session.handle();

        // 注册会话到连接管理器
        self.connection_manager.register_session(handle.id().clone(), Arc::new(session)).await?;

        Ok(handle)
    }

    /// 初始化适配器（启动进程、建立连接等）
    pub async fn initialize(&self) -> AcpResult<()> {
        // 启动连接管理器
        self.connection_manager.start(&self.config).await?;

        // 初始化 MCP 管理器
        if self.config.mcp_enabled {
            self.mcp_manager.initialize(&self.config).await?;
        }

        Ok(())
    }

    /// 关闭适配器
    pub async fn shutdown(&self) -> AcpResult<()> {
        // 关闭所有会话
        self.connection_manager.shutdown().await?;

        // 关闭 MCP 管理器
        self.mcp_manager.shutdown().await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_adapter_creation() {
        let config = AcpConfig::default();
        let adapter = AcpAdapter::new(config);

        assert!(adapter.config().name.is_empty());
    }

    #[tokio::test]
    async fn test_session_creation() {
        let config = AcpConfig::default();
        let adapter = AcpAdapter::new(config);

        let session = adapter.create_session().await.unwrap();
        assert!(!session.id().to_string().is_empty());
    }
}