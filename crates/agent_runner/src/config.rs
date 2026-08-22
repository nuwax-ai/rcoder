// 配置模块由 binary 使用；lib 内部不直接调用 load_config 等 helper。
#![allow(dead_code)]

use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};

/// 命令行参数
#[derive(Parser, Debug)]
#[command(name = "rcoder")]
#[command(about = "AI-powered development platform")]
#[command(version)]
pub struct CliArgs {
    /// Service port
    #[arg(short, long, help = "Service port")]
    pub port: Option<u16>,

    /// Project workspace directory
    #[arg(short = 'd', long, help = "Root directory for project workspace")]
    pub projects_dir: Option<PathBuf>,

    /// Enable port-based reverse proxy
    #[arg(long, help = "Enable port-based reverse proxy")]
    pub enable_proxy: bool,

    /// Proxy listener port
    #[arg(long, help = "Proxy service listener port")]
    pub proxy_port: Option<u16>,

    /// Default backend port
    #[arg(long, help = "Default backend service port")]
    pub default_backend_port: Option<u16>,
}

/// 应用配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 默认使用的 Agent ID
    #[serde(default = "default_agent_id")]
    pub default_agent_id: String,
    /// 项目工作的根目录,根据启动命令的当前目录来确定
    pub projects_dir: PathBuf,
    /// 服务端口
    pub port: u16,
    /// 代理配置
    pub proxy_config: Option<ProxyConfig>,
    /// Agent 清理配置
    #[serde(default)]
    pub agent_cleanup: Option<AgentCleanupConfig>,
    /// gRPC 超时配置
    #[serde(default)]
    pub grpc_timeouts: Option<GrpcTimeoutConfig>,
    /// Deprecated no-op. Kept only so old config files still deserialize.
    #[serde(default)]
    pub agent_concurrency: Option<AgentConcurrencyConfig>,
    /// mcp-proxy 日志目录（可选）
    /// 当设置此值且日志级别为 debug 时，mcp-proxy convert 命令会自动追加
    /// --diagnostic 和 --log-dir 参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_proxy_log_dir: Option<String>,
}

fn default_agent_id() -> String {
    shared_types::DEFAULT_AGENT_ID.to_string()
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent_id: default_agent_id(),
            projects_dir: PathBuf::from("./project_workspace"),
            port: 8086,
            proxy_config: Some(ProxyConfig::default()),
            agent_cleanup: Some(AgentCleanupConfig::default()),
            grpc_timeouts: Some(GrpcTimeoutConfig::default()),
            agent_concurrency: None,
            mcp_proxy_log_dir: None,
        }
    }
}

mod loader;
mod sections;

pub use loader::load_config_with_args;
pub use sections::{
    AgentCleanupConfig, AgentConcurrencyConfig, GrpcTimeoutConfig, HealthCheckConfig, ProxyConfig,
};
