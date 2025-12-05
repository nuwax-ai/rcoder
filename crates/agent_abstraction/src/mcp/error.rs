//! MCP 模块错误定义

use thiserror::Error;

/// MCP 错误类型
#[derive(Error, Debug)]
pub enum McpError {
    #[error("MCP 服务启动失败: {0}")]
    StartFailed(String),

    #[error("MCP 服务停止失败: {0}")]
    StopFailed(String),

    #[error("MCP 服务未运行: {0}")]
    NotRunning(String),

    #[error("MCP 服务已运行: {0}")]
    AlreadyRunning(String),

    #[error("MCP 初始化失败: {0}")]
    InitializeFailed(String),

    #[error("工具调用失败: {0}")]
    ToolCallFailed(String),

    #[error("工具未找到: {0}")]
    ToolNotFound(String),

    #[error("配置错误: {0}")]
    ConfigError(String),

    #[error("传输层错误: {0}")]
    Transport(String),

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("超时: {0}")]
    Timeout(String),

    #[error("已取消")]
    Cancelled,
}

/// MCP 结果类型别名
pub type McpResult<T> = Result<T, McpError>;
