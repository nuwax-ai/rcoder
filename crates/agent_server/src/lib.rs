//! Agent Server - Docker 容器内的 Agent 服务管理器
//!
//! 这个模块提供了在 Docker 容器内运行的 Agent 服务，负责：
//! - 启动和管理 Agent 服务
//! - 处理聊天请求
//! - 管理会话生命周期
//! - 提供进度通知
//! - 处理 Agent 取消和停止请求

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

pub mod agent;
pub mod api;
pub mod config;
pub mod handlers;
pub mod models;
pub mod server;
pub mod shutdown;

pub use agent::*;
pub use api::*;
pub use config::*;
pub use handlers::*;
pub use models::*;
pub use server::*;
pub use shutdown::*;

/// Agent Server 主运行函数
pub async fn run(cli: Cli) -> Result<()> {
    // 从环境变量获取额外参数
    let extra_args = std::env::args().skip(1).collect::<Vec<_>>();

    start_agent_server(cli, extra_args).await
}

/// Agent Server 命令行接口
#[derive(Parser, Debug)]
#[command(name = "agent-server")]
#[command(about = "RCoder Agent Server - Docker 容器内的 Agent 服务管理器")]
pub struct Cli {
    /// 服务端口
    #[arg(short, long, default_value_t = 8086)]
    pub port: u16,

    /// Agent 类型 (claude, codex)
    #[arg(short, long, default_value = "claude")]
    pub agent_type: String,

    /// 项目 ID
    #[arg(short, long)]
    pub project_id: String,

    /// 会话 ID (可选)
    #[arg(short, long)]
    pub session_id: Option<String>,

    /// 工作目录
    #[arg(short, long, default_value = "/app/workspace")]
    pub work_dir: String,

    /// 日志级别
    #[arg(short, long, default_value = "info")]
    pub log_level: String,

    /// 子命令
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// 可用的子命令
#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// 启动 Agent 服务
    Start {
        /// 额外的启动参数
        #[arg(short, long)]
        args: Vec<String>,
    },
    /// 停止 Agent 服务
    Stop,
    /// 重启 Agent 服务
    Restart,
    /// 查看服务状态
    Status,
}

/// Agent Server 主入口
#[tokio::main]
pub async fn main() -> Result<()> {
    let cli = Cli::parse();

    // 初始化日志
    init_logging(&cli.log_level);

    // 执行命令
    match cli.command.clone().unwrap_or(Commands::Start { args: vec![] }) {
        Commands::Start { args } => {
            info!("启动 Agent Server...");
            start_agent_server(cli, args).await
        }
        Commands::Stop => {
            info!("停止 Agent Server...");
            stop_agent_server().await
        }
        Commands::Restart => {
            info!("重启 Agent Server...");
            restart_agent_server().await
        }
        Commands::Status => {
            info!("查看 Agent Server 状态...");
            check_agent_server_status().await
        }
    }
}

/// 初始化日志系统
fn init_logging(level: &str) {
    use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().json())
        .init();

    info!("日志系统初始化完成，级别: {}", level);
}

/// 启动 Agent 服务
async fn start_agent_server(cli: Cli, extra_args: Vec<String>) -> Result<()> {
    info!("开始启动 Agent 服务");
    info!("端口: {}", cli.port);
    info!("Agent 类型: {}", cli.agent_type);
    info!("项目 ID: {}", cli.project_id);
    info!("工作目录: {}", cli.work_dir);

    // 创建配置
    let config = AgentServerConfig {
        port: cli.port,
        agent_type: cli.agent_type.parse().map_err(|e| anyhow::anyhow!("Invalid agent type: {}", e))?,
        project_id: cli.project_id,
        session_id: cli.session_id,
        work_dir: cli.work_dir.into(),
        extra_args,
        ..Default::default()
    };

    // 创建并启动服务
    let server = AgentServer::new(config).await?;
    server.start().await?;

    Ok(())
}

/// 停止 Agent 服务
async fn stop_agent_server() -> Result<()> {
    // TODO: 实现服务停止逻辑
    info!("停止 Agent 服务功能待实现");
    Ok(())
}

/// 重启 Agent 服务
async fn restart_agent_server() -> Result<()> {
    // TODO: 实现服务重启逻辑
    info!("重启 Agent 服务功能待实现");
    Ok(())
}

/// 检查服务状态
async fn check_agent_server_status() -> Result<()> {
    // TODO: 实现状态检查逻辑
    info!("检查服务状态功能待实现");
    Ok(())
}

/// Agent Server 错误类型
#[derive(thiserror::Error, Debug)]
pub enum AgentServerError {
    #[error("配置错误: {0}")]
    ConfigError(String),

    #[error("验证错误: {0}")]
    ValidationError(String),

    #[error("Agent 启动失败: {0}")]
    AgentStartError(String),

    #[error("HTTP 服务错误: {0}")]
    HttpServerError(String),

    #[error("会话错误: {0}")]
    SessionError(String),

    #[error("ACP 协议错误: {0}")]
    AcpError(String),

    #[error("IO 错误: {0}")]
    IoError(#[from] std::io::Error),

    #[error("序列化错误: {0}")]
    SerializationError(#[from] serde_json::Error),

    #[error("其他错误: {0}")]
    Other(String),
}

/// Agent Server 结果类型
pub type AgentServerResult<T> = Result<T, AgentServerError>;

// 为 AgentServerError 实现 IntoResponse trait
impl axum::response::IntoResponse for AgentServerError {
    fn into_response(self) -> axum::response::Response {
        use axum::{
            http::StatusCode,
            response::{IntoResponse, Json},
        };
        use serde_json::json;

        let (status, error_message) = match self {
            AgentServerError::ConfigError(msg) => (StatusCode::BAD_REQUEST, msg),
            AgentServerError::ValidationError(msg) => (StatusCode::BAD_REQUEST, msg),
            AgentServerError::AgentStartError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AgentServerError::HttpServerError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AgentServerError::SessionError(msg) => (StatusCode::NOT_FOUND, msg),
            AgentServerError::AcpError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AgentServerError::IoError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()),
            AgentServerError::SerializationError(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.to_string()),
            AgentServerError::Other(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        let body = Json(json!({
            "success": false,
            "error": {
                "code": status.as_u16(),
                "message": error_message
            }
        }));

        (status, body).into_response()
    }
}