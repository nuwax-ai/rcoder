//! Agent 抽象层错误类型

use thiserror::Error;

/// Agent 抽象层错误
#[derive(Error, Debug)]
pub enum AgentAbstractionError {
    #[error("生命周期管理错误: {0}")]
    Lifecycle(#[from] crate::lifecycle::AgentLifecycleError),

    #[error("连接错误: {0}")]
    Connection(String),

    #[error("注册表错误: {0}")]
    Registry(String),

    #[error("进程错误: {0}")]
    Process(String),

    #[error("MCP 服务器错误: {0}")]
    McpServer(String),

    #[error("其他错误: {0}")]
    Other(String),

    #[error("序列化错误: {0}")]
    Serialization(#[from] serde_json::Error),

    #[error("I/O 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("类型转换错误")]
    Cast,

    #[error("不存在: {0}")]
    NotFound(String),
}
