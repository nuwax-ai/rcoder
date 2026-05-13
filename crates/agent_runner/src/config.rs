// 配置模块由 binary 使用；lib 内部不直接调用 load_config 等 helper。
#![allow(dead_code)]

use std::env;
use std::fs;
use std::path::PathBuf;

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

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
    /// Agent 并发配置
    #[serde(default)]
    pub agent_concurrency: Option<AgentConcurrencyConfig>,
    /// mcp-proxy 日志目录（可选）
    /// 当设置此值且日志级别为 debug 时，mcp-proxy convert 命令会自动追加
    /// --diagnostic 和 --log-dir 参数
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_proxy_log_dir: Option<String>,
}

fn default_agent_id() -> String {
    "claude-code-acp-ts".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheckConfig {
    pub enabled: bool,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
    pub healthy_threshold: u32,
    pub unhealthy_threshold: u32,
}

/// 代理配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProxyConfig {
    /// 代理监听端口
    pub listen_port: u16,
    /// 默认后端端口
    pub default_backend_port: u16,
    /// 后端服务主机
    pub backend_host: String,
    /// URL 中端口参数的名称
    pub port_param: String,
    /// 健康检查配置
    pub health_check: HealthCheckConfig,
}

/// Agent cleanup configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCleanupConfig {
    /// Idle timeout (seconds), default 300 (5 minutes)
    #[serde(default = "default_idle_timeout")]
    pub idle_timeout_secs: u64,
    /// Cleanup check interval (seconds), default 30
    #[serde(default = "default_cleanup_interval")]
    pub cleanup_interval_secs: u64,
}

/// Agent 并发配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConcurrencyConfig {
    /// 并发会话槽位数量，默认 10
    #[serde(default = "default_concurrency_limit")]
    pub concurrency_limit: usize,
}

/// Agent 并发配置常量
impl AgentConcurrencyConfig {
    /// 最小并发限制
    pub const MIN_CONCURRENCY_LIMIT: usize = 1;

    /// 验证配置值是否在有效范围内
    pub fn validate(&self) -> Result<(), String> {
        if self.concurrency_limit < Self::MIN_CONCURRENCY_LIMIT {
            return Err(format!(
                "concurrency_limit must be >= {}, current value: {}",
                Self::MIN_CONCURRENCY_LIMIT,
                self.concurrency_limit
            ));
        }
        Ok(())
    }
}

fn default_concurrency_limit() -> usize {
    10
}

impl Default for AgentConcurrencyConfig {
    fn default() -> Self {
        Self {
            concurrency_limit: default_concurrency_limit(),
        }
    }
}

/// gRPC timeout configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GrpcTimeoutConfig {
    /// Cancel session timeout (seconds), default 30
    #[serde(default = "default_cancel_timeout")]
    pub cancel_session_timeout_secs: u64,

    /// ACP session creation timeout (seconds), default 100
    #[serde(default = "default_acp_session_timeout")]
    pub acp_session_create_timeout_secs: u64,

    /// Agent cancel call timeout (seconds), default 10
    #[serde(default = "default_agent_cancel_timeout")]
    pub agent_cancel_timeout_secs: u64,

    /// Port check timeout (milliseconds), default 500
    #[serde(default = "default_port_check_timeout")]
    pub port_check_timeout_millis: u64,
}

/// gRPC timeout configuration constants
impl GrpcTimeoutConfig {
    /// Minimum cancel session timeout (5 seconds)
    pub const MIN_CANCEL_TIMEOUT: u64 = 5;
    /// Maximum cancel session timeout (300 seconds = 5 minutes)
    pub const MAX_CANCEL_TIMEOUT: u64 = 300;
    /// Minimum ACP session creation timeout (10 seconds)
    pub const MIN_ACP_SESSION_TIMEOUT: u64 = 10;
    /// Maximum ACP session creation timeout (300 seconds = 5 minutes)
    pub const MAX_ACP_SESSION_TIMEOUT: u64 = 300;
    /// Minimum Agent cancel call timeout (5 seconds)
    pub const MIN_AGENT_CANCEL_TIMEOUT: u64 = 5;
    /// Maximum Agent cancel call timeout (60 seconds)
    pub const MAX_AGENT_CANCEL_TIMEOUT: u64 = 60;
    /// Minimum port check timeout (100 milliseconds)
    pub const MIN_PORT_CHECK_TIMEOUT: u64 = 100;
    /// Maximum port check timeout (10000 milliseconds = 10 seconds)
    pub const MAX_PORT_CHECK_TIMEOUT: u64 = 10000;

    /// Validate that configuration values are within valid ranges
    pub fn validate(&self) -> Result<(), String> {
        if self.cancel_session_timeout_secs < Self::MIN_CANCEL_TIMEOUT
            || self.cancel_session_timeout_secs > Self::MAX_CANCEL_TIMEOUT
        {
            return Err(format!(
                "cancel_session_timeout_secs must be between {} and {}, current: {}",
                Self::MIN_CANCEL_TIMEOUT,
                Self::MAX_CANCEL_TIMEOUT,
                self.cancel_session_timeout_secs
            ));
        }

        if self.acp_session_create_timeout_secs < Self::MIN_ACP_SESSION_TIMEOUT
            || self.acp_session_create_timeout_secs > Self::MAX_ACP_SESSION_TIMEOUT
        {
            return Err(format!(
                "acp_session_create_timeout_secs must be between {} and {}, current: {}",
                Self::MIN_ACP_SESSION_TIMEOUT,
                Self::MAX_ACP_SESSION_TIMEOUT,
                self.acp_session_create_timeout_secs
            ));
        }

        if self.agent_cancel_timeout_secs < Self::MIN_AGENT_CANCEL_TIMEOUT
            || self.agent_cancel_timeout_secs > Self::MAX_AGENT_CANCEL_TIMEOUT
        {
            return Err(format!(
                "agent_cancel_timeout_secs must be between {} and {}, current: {}",
                Self::MIN_AGENT_CANCEL_TIMEOUT,
                Self::MAX_AGENT_CANCEL_TIMEOUT,
                self.agent_cancel_timeout_secs
            ));
        }

        if self.port_check_timeout_millis < Self::MIN_PORT_CHECK_TIMEOUT
            || self.port_check_timeout_millis > Self::MAX_PORT_CHECK_TIMEOUT
        {
            return Err(format!(
                "port_check_timeout_millis must be between {} and {}, current: {}",
                Self::MIN_PORT_CHECK_TIMEOUT,
                Self::MAX_PORT_CHECK_TIMEOUT,
                self.port_check_timeout_millis
            ));
        }

        Ok(())
    }
}

/// Agent 清理配置常量
impl AgentCleanupConfig {
    /// 最小闲置超时时间（10 秒）
    pub const MIN_IDLE_TIMEOUT: u64 = 10;
    /// 最大闲置超时时间（24 小时）
    pub const MAX_IDLE_TIMEOUT: u64 = 24 * 60 * 60;
    /// 最小清理检查间隔（5 秒）
    pub const MIN_CLEANUP_INTERVAL: u64 = 5;
    /// 最大清理检查间隔（1 小时）
    pub const MAX_CLEANUP_INTERVAL: u64 = 60 * 60;

    /// 验证配置值是否在有效范围内
    pub fn validate(&self) -> Result<(), String> {
        if self.idle_timeout_secs < Self::MIN_IDLE_TIMEOUT
            || self.idle_timeout_secs > Self::MAX_IDLE_TIMEOUT
        {
            return Err(format!(
                "idle_timeout_secs must be between {} and {}, current value: {}",
                Self::MIN_IDLE_TIMEOUT,
                Self::MAX_IDLE_TIMEOUT,
                self.idle_timeout_secs
            ));
        }

        if self.cleanup_interval_secs < Self::MIN_CLEANUP_INTERVAL
            || self.cleanup_interval_secs > Self::MAX_CLEANUP_INTERVAL
        {
            return Err(format!(
                "cleanup_interval_secs must be between {} and {}, current value: {}",
                Self::MIN_CLEANUP_INTERVAL,
                Self::MAX_CLEANUP_INTERVAL,
                self.cleanup_interval_secs
            ));
        }

        Ok(())
    }
}

#[cfg(feature = "http-server")]
fn default_idle_timeout() -> u64 {
    24 * 60 * 60 // 24 小时（Tauri 客户端模式）
}

#[cfg(not(feature = "http-server"))]
fn default_idle_timeout() -> u64 {
    300 // 5 分钟（CLI 模式）
}

fn default_cleanup_interval() -> u64 {
    30 // 30 秒
}

fn default_cancel_timeout() -> u64 {
    30 // 30 秒
}

fn default_acp_session_timeout() -> u64 {
    100 // 100 秒
}

fn default_agent_cancel_timeout() -> u64 {
    10 // 10 秒
}

fn default_port_check_timeout() -> u64 {
    500 // 500 毫秒
}

impl Default for AgentCleanupConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout(),
            cleanup_interval_secs: default_cleanup_interval(),
        }
    }
}

impl Default for GrpcTimeoutConfig {
    fn default() -> Self {
        Self {
            cancel_session_timeout_secs: default_cancel_timeout(),
            acp_session_create_timeout_secs: default_acp_session_timeout(),
            agent_cancel_timeout_secs: default_agent_cancel_timeout(),
            port_check_timeout_millis: default_port_check_timeout(),
        }
    }
}

/// 配置文件路径
const CONFIG_FILE: &str = "config.yml";

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            default_agent_id: default_agent_id(),
            projects_dir: PathBuf::from("./project_workspace"),
            port: 8086,
            proxy_config: Some(ProxyConfig::default()),
            agent_cleanup: Some(AgentCleanupConfig::default()),
            grpc_timeouts: Some(GrpcTimeoutConfig::default()),
            agent_concurrency: Some(AgentConcurrencyConfig::default()),
            mcp_proxy_log_dir: None,
        }
    }
}

impl Default for ProxyConfig {
    fn default() -> Self {
        Self {
            listen_port: 8088,
            default_backend_port: 8086,
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            health_check: HealthCheckConfig {
                enabled: true,
                interval_seconds: 5,
                timeout_seconds: 1,
                healthy_threshold: 2,
                unhealthy_threshold: 3,
            },
        }
    }
}

/// 加载配置
/// 配置优先级：命令行参数 > 环境变量 > 配置文件 > 默认配置
pub fn load_config_with_args(cli_args: CliArgs) -> AppConfig {
    // 1. 首先加载默认配置
    let mut config = AppConfig::default();

    // 2. 尝试从当前目录读取配置文件
    match load_config_from_file() {
        Ok(file_config) => {
            config = file_config;
            info!("Loaded config from {}", CONFIG_FILE);
        }
        Err(e) => {
            warn!(
                "Failed to read config file {}: {}, using defaults",
                CONFIG_FILE, e
            );

            // 创建默认配置文件
            if let Err(create_err) = create_default_config_file(&config) {
                error!("Failed to create default config file: {}", create_err);
            } else {
                info!("Created default config file: {}", CONFIG_FILE);
            }
        }
    }

    // 3. 环境变量覆盖配置
    if let Ok(port) = env::var("RCODER_PORT") {
        match port.parse::<u16>() {
            Ok(p) => {
                config.port = p;
                info!("Set port from env RCODER_PORT: {}", p);
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_PORT env value: {}, keeping config port: {}",
                    port, config.port
                );
            }
        }
    }

    // 🆕 Agent 清理配置：支持环境变量覆盖
    if let Ok(idle_timeout) = env::var("RCODER_AGENT_IDLE_TIMEOUT_SECS") {
        match idle_timeout.parse::<u64>() {
            Ok(timeout) => {
                // 🔒 验证范围
                if (AgentCleanupConfig::MIN_IDLE_TIMEOUT..=AgentCleanupConfig::MAX_IDLE_TIMEOUT)
                    .contains(&timeout)
                {
                    config
                        .agent_cleanup
                        .get_or_insert_with(Default::default)
                        .idle_timeout_secs = timeout;
                    info!(
                        "Set idle timeout from env RCODER_AGENT_IDLE_TIMEOUT_SECS: {} seconds",
                        timeout
                    );
                } else {
                    warn!(
                        "Invalid RCODER_AGENT_IDLE_TIMEOUT_SECS: {} seconds, out of range [{}, {}], keeping config value",
                        timeout,
                        AgentCleanupConfig::MIN_IDLE_TIMEOUT,
                        AgentCleanupConfig::MAX_IDLE_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_AGENT_IDLE_TIMEOUT_SECS format: {}, keeping config value",
                    idle_timeout
                );
            }
        }
    }

    if let Ok(cleanup_interval) = env::var("RCODER_AGENT_CLEANUP_INTERVAL_SECS") {
        match cleanup_interval.parse::<u64>() {
            Ok(interval) => {
                // 🔒 验证范围
                if (AgentCleanupConfig::MIN_CLEANUP_INTERVAL
                    ..=AgentCleanupConfig::MAX_CLEANUP_INTERVAL)
                    .contains(&interval)
                {
                    config
                        .agent_cleanup
                        .get_or_insert_with(Default::default)
                        .cleanup_interval_secs = interval;
                    info!(
                        "Set cleanup interval from env RCODER_AGENT_CLEANUP_INTERVAL_SECS: {} seconds",
                        interval
                    );
                } else {
                    warn!(
                        "Invalid RCODER_AGENT_CLEANUP_INTERVAL_SECS: {} seconds, out of range [{}, {}], keeping config value",
                        interval,
                        AgentCleanupConfig::MIN_CLEANUP_INTERVAL,
                        AgentCleanupConfig::MAX_CLEANUP_INTERVAL
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_AGENT_CLEANUP_INTERVAL_SECS format: {}, keeping config value",
                    cleanup_interval
                );
            }
        }
    }

    // 🆕 Agent 并发配置：支持环境变量覆盖
    if let Ok(concurrency_limit) = env::var("RCODER_AGENT_CONCURRENCY_LIMIT") {
        match concurrency_limit.parse::<usize>() {
            Ok(limit) => {
                if limit >= AgentConcurrencyConfig::MIN_CONCURRENCY_LIMIT {
                    config
                        .agent_concurrency
                        .get_or_insert_with(Default::default)
                        .concurrency_limit = limit;
                    info!(
                        "Set concurrency limit from env RCODER_AGENT_CONCURRENCY_LIMIT: {}",
                        limit
                    );
                } else {
                    warn!(
                        "Invalid RCODER_AGENT_CONCURRENCY_LIMIT: {}, must be >= {}, keeping config value",
                        limit,
                        AgentConcurrencyConfig::MIN_CONCURRENCY_LIMIT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_AGENT_CONCURRENCY_LIMIT format: {}, keeping config value",
                    concurrency_limit
                );
            }
        }
    }

    // 🆕 gRPC 超时配置：支持环境变量覆盖
    if let Ok(cancel_timeout) = env::var("RCODER_CANCEL_SESSION_TIMEOUT_SECS") {
        match cancel_timeout.parse::<u64>() {
            Ok(timeout) => {
                if (GrpcTimeoutConfig::MIN_CANCEL_TIMEOUT..=GrpcTimeoutConfig::MAX_CANCEL_TIMEOUT)
                    .contains(&timeout)
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .cancel_session_timeout_secs = timeout;
                    info!(
                        "Set cancel-session timeout from env RCODER_CANCEL_SESSION_TIMEOUT_SECS: {} seconds",
                        timeout
                    );
                } else {
                    warn!(
                        "Invalid RCODER_CANCEL_SESSION_TIMEOUT_SECS: {} seconds, out of range [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_CANCEL_TIMEOUT,
                        GrpcTimeoutConfig::MAX_CANCEL_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_CANCEL_SESSION_TIMEOUT_SECS format: {}",
                    cancel_timeout
                );
            }
        }
    }

    if let Ok(acp_timeout) = env::var("RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS") {
        match acp_timeout.parse::<u64>() {
            Ok(timeout) => {
                if (GrpcTimeoutConfig::MIN_ACP_SESSION_TIMEOUT
                    ..=GrpcTimeoutConfig::MAX_ACP_SESSION_TIMEOUT)
                    .contains(&timeout)
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .acp_session_create_timeout_secs = timeout;
                    info!(
                        "Set ACP session-create timeout from env RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS: {} seconds",
                        timeout
                    );
                } else {
                    warn!(
                        "Invalid RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS: {} seconds, out of range [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_ACP_SESSION_TIMEOUT,
                        GrpcTimeoutConfig::MAX_ACP_SESSION_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS format: {}",
                    acp_timeout
                );
            }
        }
    }

    if let Ok(agent_cancel_timeout) = env::var("RCODER_AGENT_CANCEL_TIMEOUT_SECS") {
        match agent_cancel_timeout.parse::<u64>() {
            Ok(timeout) => {
                if (GrpcTimeoutConfig::MIN_AGENT_CANCEL_TIMEOUT
                    ..=GrpcTimeoutConfig::MAX_AGENT_CANCEL_TIMEOUT)
                    .contains(&timeout)
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .agent_cancel_timeout_secs = timeout;
                    info!(
                        "Set agent-cancel timeout from env RCODER_AGENT_CANCEL_TIMEOUT_SECS: {} seconds",
                        timeout
                    );
                } else {
                    warn!(
                        "Invalid RCODER_AGENT_CANCEL_TIMEOUT_SECS: {} seconds, out of range [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_AGENT_CANCEL_TIMEOUT,
                        GrpcTimeoutConfig::MAX_AGENT_CANCEL_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_AGENT_CANCEL_TIMEOUT_SECS format: {}",
                    agent_cancel_timeout
                );
            }
        }
    }

    if let Ok(port_check_timeout) = env::var("RCODER_PORT_CHECK_TIMEOUT_MILLIS") {
        match port_check_timeout.parse::<u64>() {
            Ok(timeout) => {
                if (GrpcTimeoutConfig::MIN_PORT_CHECK_TIMEOUT
                    ..=GrpcTimeoutConfig::MAX_PORT_CHECK_TIMEOUT)
                    .contains(&timeout)
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .port_check_timeout_millis = timeout;
                    info!(
                        "Set port-check timeout from env RCODER_PORT_CHECK_TIMEOUT_MILLIS: {} ms",
                        timeout
                    );
                } else {
                    warn!(
                        "Invalid RCODER_PORT_CHECK_TIMEOUT_MILLIS: {} ms, out of range [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_PORT_CHECK_TIMEOUT,
                        GrpcTimeoutConfig::MAX_PORT_CHECK_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "Invalid RCODER_PORT_CHECK_TIMEOUT_MILLIS format: {}",
                    port_check_timeout
                );
            }
        }
    }

    // 🆕 验证最终配置的有效性
    if let Some(ref cleanup_config) = config.agent_cleanup
        && let Err(e) = cleanup_config.validate()
    {
        warn!(
            "Agent cleanup config validation failed: {}, using defaults",
            e
        );
        config.agent_cleanup = Some(AgentCleanupConfig::default());
    }

    // 🆕 验证 Agent 并发配置的有效性
    if let Some(ref concurrency_config) = config.agent_concurrency
        && let Err(e) = concurrency_config.validate()
    {
        warn!(
            "Agent concurrency config validation failed: {}, using defaults",
            e
        );
        config.agent_concurrency = Some(AgentConcurrencyConfig::default());
    }

    // 4. 处理代理配置
    if cli_args.enable_proxy {
        let proxy_config = ProxyConfig {
            listen_port: cli_args.proxy_port.unwrap_or(8080),
            default_backend_port: cli_args.default_backend_port.unwrap_or(config.port),
            backend_host: "127.0.0.1".to_string(),
            port_param: "port".to_string(),
            health_check: HealthCheckConfig {
                enabled: true,
                interval_seconds: 5,
                timeout_seconds: 1,
                healthy_threshold: 2,
                unhealthy_threshold: 3,
            },
        };
        info!(
            "Reverse proxy enabled, listening on port: {}",
            proxy_config.listen_port
        );
        config.proxy_config = Some(proxy_config);
    }

    // 5. 命令行参数覆盖配置（优先级最高）
    if let Some(port) = cli_args.port {
        config.port = port;
        info!("Set port from CLI arg: {}", port);
    }

    if let Some(projects_dir) = cli_args.projects_dir {
        config.projects_dir = projects_dir.clone();
        info!("Set projects directory from CLI arg: {:?}", projects_dir);
    }

    info!(
        "最终配置: port={}, projects_dir={:?}, default_agent_id={}, proxy_enabled={}",
        config.port,
        config.projects_dir,
        config.default_agent_id,
        config.proxy_config.is_some()
    );

    // 🆕 验证 gRPC 超时配置的有效性
    if let Some(ref grpc_timeouts) = config.grpc_timeouts
        && let Err(e) = grpc_timeouts.validate()
    {
        warn!(
            "gRPC timeout config validation failed: {}, using defaults",
            e
        );
        config.grpc_timeouts = Some(GrpcTimeoutConfig::default());
    }

    config
}

/// 加载配置（保留旧接口以保持兼容性）
pub fn load_config() -> AppConfig {
    let cli_args = CliArgs {
        port: None,
        projects_dir: None,
        enable_proxy: false,
        proxy_port: None,
        default_backend_port: None,
    };
    load_config_with_args(cli_args)
}

/// 从文件加载配置
fn load_config_from_file() -> anyhow::Result<AppConfig> {
    let config_content = fs::read_to_string(CONFIG_FILE)
        .map_err(|e| anyhow::anyhow!("Failed to read config file: {}", e))?;

    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file: {}", e))?;

    Ok(config)
}

/// 创建默认配置文件
fn create_default_config_file(config: &AppConfig) -> anyhow::Result<()> {
    // 获取 proxy_config，如果不存在则使用默认值
    let proxy_config = config.proxy_config.as_ref().cloned().unwrap_or_default();

    // 获取 agent_cleanup 配置，如果不存在则使用默认值
    let agent_cleanup = config.agent_cleanup.as_ref().cloned().unwrap_or_default();

    // 获取 grpc_timeouts 配置，如果不存在则使用默认值
    let grpc_timeouts = config.grpc_timeouts.as_ref().cloned().unwrap_or_default();

    // 获取 agent_concurrency 配置，如果不存在则使用默认值
    let agent_concurrency = config
        .agent_concurrency
        .as_ref()
        .cloned()
        .unwrap_or_default();

    // 手动构建带注释的 YAML 内容
    let content_with_comments = format!(
        r#"# rcoder 配置文件
# 该文件在首次启动时自动生成

# 默认使用的 Agent ID
default_agent_id: {}

# 项目工作目录
projects_dir: {}

# 主服务端口
port: {}

# Pingora 反向代理配置
proxy_config:
  # 代理服务监听端口 (用于接收外部请求)
  listen_port: {}
  # 默认后端服务端口 (当请求未指定端口时使用)
  default_backend_port: {}
  # 后端服务主机地址
  backend_host: "{}"
  # URL 中端口参数的名称 (用于从路径中提取端口号)
  port_param: "{}"
  # 健康检查配置
  health_check:
    enabled: {}
    interval_seconds: {}
    timeout_seconds: {}
    healthy_threshold: {}
    unhealthy_threshold: {}

# Agent 清理配置
# 如果省略此配置块，将使用以下默认值：
#   - idle_timeout_secs: 300 (5分钟)
#   - cleanup_interval_secs: 30 (30秒)
agent_cleanup:
  # Agent 闲置超时时间（秒）
  # Agent 在闲置超过此时间后会被自动清理以释放资源
  # 有效范围: 10 - 86400 秒（10秒 - 24小时）
  # 可通过环境变量 RCODER_AGENT_IDLE_TIMEOUT_SECS 覆盖
  idle_timeout_secs: {}
  # 清理检查间隔（秒）
  # 系统每隔此时间检查一次是否有闲置的 Agent 需要清理
  # 有效范围: 5 - 3600 秒（5秒 - 1小时）
  # 可通过环境变量 RCODER_AGENT_CLEANUP_INTERVAL_SECS 覆盖
  cleanup_interval_secs: {}

# gRPC 超时配置
# 如果省略此配置块，将使用以下默认值：
#   - cancel_session_timeout_secs: 30 (30秒)
#   - acp_session_create_timeout_secs: 100 (100秒)
#   - agent_cancel_timeout_secs: 10 (10秒)
#   - port_check_timeout_millis: 500 (500毫秒)
grpc_timeouts:
  # 取消会话超时（秒）
  # gRPC 取消会话请求的最大等待时间
  # 有效范围: 5 - 300 秒
  # 可通过环境变量 RCODER_CANCEL_SESSION_TIMEOUT_SECS 覆盖
  cancel_session_timeout_secs: {}
  # ACP 会话创建超时（秒）
  # Agent 创建新会话的最大等待时间（MCP 工具较多时可能需要更长时间）
  # 有效范围: 10 - 300 秒
  # 可通过环境变量 RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS 覆盖
  acp_session_create_timeout_secs: {}
  # Agent 取消调用超时（秒）
  # Agent 内部取消操作的最大等待时间
  # 有效范围: 5 - 60 秒
  # 可通过环境变量 RCODER_AGENT_CANCEL_TIMEOUT_SECS 覆盖
  agent_cancel_timeout_secs: {}
  # 端口检查超时（毫秒）
  # 检查端口可用性的最大等待时间
  # 有效范围: 100 - 10000 毫秒
  # 可通过环境变量 RCODER_PORT_CHECK_TIMEOUT_MILLIS 覆盖
  port_check_timeout_millis: {}

# Agent 并发配置
# 如果省略此配置块，将使用以下默认值：
#   - concurrency_limit: 10 (10个并发会话)
agent_concurrency:
  # Agent 并发会话槽位数量
  # 决定可以同时处理的 Agent 会话数量
  # 有效范围: >= 1
  # 可通过环境变量 RCODER_AGENT_CONCURRENCY_LIMIT 覆盖
  concurrency_limit: {}
"#,
        config.default_agent_id,
        config.projects_dir.display(),
        config.port,
        proxy_config.listen_port,
        proxy_config.default_backend_port,
        proxy_config.backend_host,
        proxy_config.port_param,
        proxy_config.health_check.enabled,
        proxy_config.health_check.interval_seconds,
        proxy_config.health_check.timeout_seconds,
        proxy_config.health_check.healthy_threshold,
        proxy_config.health_check.unhealthy_threshold,
        agent_cleanup.idle_timeout_secs,
        agent_cleanup.cleanup_interval_secs,
        grpc_timeouts.cancel_session_timeout_secs,
        grpc_timeouts.acp_session_create_timeout_secs,
        grpc_timeouts.agent_cancel_timeout_secs,
        grpc_timeouts.port_check_timeout_millis,
        agent_concurrency.concurrency_limit
    );

    fs::write(CONFIG_FILE, content_with_comments)
        .map_err(|e| anyhow::anyhow!("Failed to write config file: {}", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_cleanup_default_values() {
        let config = AgentCleanupConfig::default();
        assert_eq!(config.idle_timeout_secs, 300);
        assert_eq!(config.cleanup_interval_secs, 30);
    }

    #[test]
    fn test_agent_cleanup_validate_valid_range() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 600,    // 10 分钟
            cleanup_interval_secs: 60, // 1 分钟
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_agent_cleanup_validate_min_boundaries() {
        // 测试最小边界值
        let config = AgentCleanupConfig {
            idle_timeout_secs: AgentCleanupConfig::MIN_IDLE_TIMEOUT,
            cleanup_interval_secs: AgentCleanupConfig::MIN_CLEANUP_INTERVAL,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_agent_cleanup_validate_max_boundaries() {
        // 测试最大边界值
        let config = AgentCleanupConfig {
            idle_timeout_secs: AgentCleanupConfig::MAX_IDLE_TIMEOUT,
            cleanup_interval_secs: AgentCleanupConfig::MAX_CLEANUP_INTERVAL,
        };
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_agent_cleanup_validate_idle_timeout_too_small() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 5, // 小于最小值 10
            cleanup_interval_secs: 30,
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("idle_timeout_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_idle_timeout_too_large() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 100000, // 大于最大值 86400
            cleanup_interval_secs: 30,
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("idle_timeout_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_cleanup_interval_too_small() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 180,
            cleanup_interval_secs: 2, // 小于最小值 5
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("cleanup_interval_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_cleanup_interval_too_large() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 180,
            cleanup_interval_secs: 5000, // 大于最大值 3600
        };
        assert!(config.validate().is_err());
        let err = config.validate().unwrap_err();
        assert!(err.contains("cleanup_interval_secs"));
    }

    #[test]
    fn test_agent_cleanup_validate_both_invalid() {
        let config = AgentCleanupConfig {
            idle_timeout_secs: 0,
            cleanup_interval_secs: 0,
        };
        assert!(config.validate().is_err());
        // 应该先检测到 idle_timeout_secs 的错误
        let err = config.validate().unwrap_err();
        assert!(err.contains("idle_timeout_secs"));
    }
}
