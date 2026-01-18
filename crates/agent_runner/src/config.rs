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
    /// 服务端口
    #[arg(short, long, help = "服务端口")]
    pub port: Option<u16>,

    /// 项目工作目录
    #[arg(short = 'd', long, help = "项目工作的根目录")]
    pub projects_dir: Option<PathBuf>,

    /// 启用反向代理
    #[arg(long, help = "启用基于端口的反向代理")]
    pub enable_proxy: bool,

    /// 代理监听端口
    #[arg(long, help = "代理服务监听端口")]
    pub proxy_port: Option<u16>,

    /// 默认后端端口
    #[arg(long, help = "默认后端服务端口")]
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
}

fn default_agent_id() -> String {
    "claude-code-acp".to_string()
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
                "idle_timeout_secs 必须在 {} 到 {} 之间，当前值: {}",
                Self::MIN_IDLE_TIMEOUT,
                Self::MAX_IDLE_TIMEOUT,
                self.idle_timeout_secs
            ));
        }

        if self.cleanup_interval_secs < Self::MIN_CLEANUP_INTERVAL
            || self.cleanup_interval_secs > Self::MAX_CLEANUP_INTERVAL
        {
            return Err(format!(
                "cleanup_interval_secs 必须在 {} 到 {} 之间，当前值: {}",
                Self::MIN_CLEANUP_INTERVAL,
                Self::MAX_CLEANUP_INTERVAL,
                self.cleanup_interval_secs
            ));
        }

        Ok(())
    }
}

fn default_idle_timeout() -> u64 {
    300 // 5 分钟
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
            info!("成功从 {} 加载配置", CONFIG_FILE);
        }
        Err(e) => {
            warn!("无法读取配置文件 {}: {}, 使用默认配置", CONFIG_FILE, e);

            // 创建默认配置文件
            if let Err(create_err) = create_default_config_file(&config) {
                error!("创建默认配置文件失败: {}", create_err);
            } else {
                info!("已创建默认配置文件: {}", CONFIG_FILE);
            }
        }
    }

    // 3. 环境变量覆盖配置
    if let Ok(port) = env::var("RCODER_PORT") {
        match port.parse::<u16>() {
            Ok(p) => {
                config.port = p;
                info!("使用环境变量 RCODER_PORT 设置端口: {}", p);
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_PORT 值无效: {}, 使用配置文件中的端口: {}",
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
                if timeout >= AgentCleanupConfig::MIN_IDLE_TIMEOUT
                    && timeout <= AgentCleanupConfig::MAX_IDLE_TIMEOUT
                {
                    config
                        .agent_cleanup
                        .get_or_insert_with(Default::default)
                        .idle_timeout_secs = timeout;
                    info!(
                        "使用环境变量 RCODER_AGENT_IDLE_TIMEOUT_SECS 设置闲置超时: {} 秒",
                        timeout
                    );
                } else {
                    warn!(
                        "环境变量 RCODER_AGENT_IDLE_TIMEOUT_SECS 值无效: {} 秒，超出范围 [{}, {}]，使用配置文件中的值",
                        timeout,
                        AgentCleanupConfig::MIN_IDLE_TIMEOUT,
                        AgentCleanupConfig::MAX_IDLE_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_AGENT_IDLE_TIMEOUT_SECS 值格式无效: {}，使用配置文件中的值",
                    idle_timeout
                );
            }
        }
    }

    if let Ok(cleanup_interval) = env::var("RCODER_AGENT_CLEANUP_INTERVAL_SECS") {
        match cleanup_interval.parse::<u64>() {
            Ok(interval) => {
                // 🔒 验证范围
                if interval >= AgentCleanupConfig::MIN_CLEANUP_INTERVAL
                    && interval <= AgentCleanupConfig::MAX_CLEANUP_INTERVAL
                {
                    config
                        .agent_cleanup
                        .get_or_insert_with(Default::default)
                        .cleanup_interval_secs = interval;
                    info!(
                        "使用环境变量 RCODER_AGENT_CLEANUP_INTERVAL_SECS 设置清理间隔: {} 秒",
                        interval
                    );
                } else {
                    warn!(
                        "环境变量 RCODER_AGENT_CLEANUP_INTERVAL_SECS 值无效: {} 秒，超出范围 [{}, {}]，使用配置文件中的值",
                        interval,
                        AgentCleanupConfig::MIN_CLEANUP_INTERVAL,
                        AgentCleanupConfig::MAX_CLEANUP_INTERVAL
                    );
                }
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_AGENT_CLEANUP_INTERVAL_SECS 值格式无效: {}，使用配置文件中的值",
                    cleanup_interval
                );
            }
        }
    }

    // 🆕 gRPC 超时配置：支持环境变量覆盖
    if let Ok(cancel_timeout) = env::var("RCODER_CANCEL_SESSION_TIMEOUT_SECS") {
        match cancel_timeout.parse::<u64>() {
            Ok(timeout) => {
                if timeout >= GrpcTimeoutConfig::MIN_CANCEL_TIMEOUT
                    && timeout <= GrpcTimeoutConfig::MAX_CANCEL_TIMEOUT
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .cancel_session_timeout_secs = timeout;
                    info!(
                        "使用环境变量 RCODER_CANCEL_SESSION_TIMEOUT_SECS 设置取消会话超时: {} 秒",
                        timeout
                    );
                } else {
                    warn!(
                        "环境变量 RCODER_CANCEL_SESSION_TIMEOUT_SECS 值无效: {} 秒，超出范围 [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_CANCEL_TIMEOUT,
                        GrpcTimeoutConfig::MAX_CANCEL_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_CANCEL_SESSION_TIMEOUT_SECS 值格式无效: {}",
                    cancel_timeout
                );
            }
        }
    }

    if let Ok(acp_timeout) = env::var("RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS") {
        match acp_timeout.parse::<u64>() {
            Ok(timeout) => {
                if timeout >= GrpcTimeoutConfig::MIN_ACP_SESSION_TIMEOUT
                    && timeout <= GrpcTimeoutConfig::MAX_ACP_SESSION_TIMEOUT
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .acp_session_create_timeout_secs = timeout;
                    info!(
                        "使用环境变量 RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS 设置 ACP 会话创建超时: {} 秒",
                        timeout
                    );
                } else {
                    warn!(
                        "环境变量 RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS 值无效: {} 秒，超出范围 [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_ACP_SESSION_TIMEOUT,
                        GrpcTimeoutConfig::MAX_ACP_SESSION_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS 值格式无效: {}",
                    acp_timeout
                );
            }
        }
    }

    if let Ok(agent_cancel_timeout) = env::var("RCODER_AGENT_CANCEL_TIMEOUT_SECS") {
        match agent_cancel_timeout.parse::<u64>() {
            Ok(timeout) => {
                if timeout >= GrpcTimeoutConfig::MIN_AGENT_CANCEL_TIMEOUT
                    && timeout <= GrpcTimeoutConfig::MAX_AGENT_CANCEL_TIMEOUT
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .agent_cancel_timeout_secs = timeout;
                    info!(
                        "使用环境变量 RCODER_AGENT_CANCEL_TIMEOUT_SECS 设置 Agent 取消调用超时: {} 秒",
                        timeout
                    );
                } else {
                    warn!(
                        "环境变量 RCODER_AGENT_CANCEL_TIMEOUT_SECS 值无效: {} 秒，超出范围 [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_AGENT_CANCEL_TIMEOUT,
                        GrpcTimeoutConfig::MAX_AGENT_CANCEL_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_AGENT_CANCEL_TIMEOUT_SECS 值格式无效: {}",
                    agent_cancel_timeout
                );
            }
        }
    }

    if let Ok(port_check_timeout) = env::var("RCODER_PORT_CHECK_TIMEOUT_MILLIS") {
        match port_check_timeout.parse::<u64>() {
            Ok(timeout) => {
                if timeout >= GrpcTimeoutConfig::MIN_PORT_CHECK_TIMEOUT
                    && timeout <= GrpcTimeoutConfig::MAX_PORT_CHECK_TIMEOUT
                {
                    config
                        .grpc_timeouts
                        .get_or_insert_with(Default::default)
                        .port_check_timeout_millis = timeout;
                    info!(
                        "使用环境变量 RCODER_PORT_CHECK_TIMEOUT_MILLIS 设置端口检查超时: {} 毫秒",
                        timeout
                    );
                } else {
                    warn!(
                        "环境变量 RCODER_PORT_CHECK_TIMEOUT_MILLIS 值无效: {} 毫秒，超出范围 [{}, {}]",
                        timeout,
                        GrpcTimeoutConfig::MIN_PORT_CHECK_TIMEOUT,
                        GrpcTimeoutConfig::MAX_PORT_CHECK_TIMEOUT
                    );
                }
            }
            Err(_) => {
                warn!(
                    "环境变量 RCODER_PORT_CHECK_TIMEOUT_MILLIS 值格式无效: {}",
                    port_check_timeout
                );
            }
        }
    }

    // 🆕 验证最终配置的有效性
    if let Some(ref cleanup_config) = config.agent_cleanup {
        if let Err(e) = cleanup_config.validate() {
            warn!("Agent 清理配置验证失败: {}，使用默认配置", e);
            config.agent_cleanup = Some(AgentCleanupConfig::default());
        }
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
        info!("启用反向代理，监听端口: {}", proxy_config.listen_port);
        config.proxy_config = Some(proxy_config);
    }

    // 5. 命令行参数覆盖配置（优先级最高）
    if let Some(port) = cli_args.port {
        config.port = port;
        info!("使用命令行参数设置端口: {}", port);
    }

    if let Some(projects_dir) = cli_args.projects_dir {
        config.projects_dir = projects_dir.clone();
        info!("使用命令行参数设置项目目录: {:?}", projects_dir);
    }

    info!(
        "最终配置: port={}, projects_dir={:?}, default_agent_id={}, proxy_enabled={}",
        config.port,
        config.projects_dir,
        config.default_agent_id,
        config.proxy_config.is_some()
    );

    // 🆕 验证 gRPC 超时配置的有效性
    if let Some(ref grpc_timeouts) = config.grpc_timeouts {
        if let Err(e) = grpc_timeouts.validate() {
            warn!(
                "gRPC 超时配置验证失败: {}，使用默认配置",
                e
            );
            config.grpc_timeouts = Some(GrpcTimeoutConfig::default());
        }
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
    let config_content =
        fs::read_to_string(CONFIG_FILE).map_err(|e| anyhow::anyhow!("读取配置文件失败: {}", e))?;

    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("解析配置文件失败: {}", e))?;

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
        grpc_timeouts.port_check_timeout_millis
    );

    fs::write(CONFIG_FILE, content_with_comments)
        .map_err(|e| anyhow::anyhow!("写入配置文件失败: {}", e))?;

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
