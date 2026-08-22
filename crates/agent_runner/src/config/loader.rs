//! 配置加载器（从 config.rs 拆出）。

use std::env;
use std::fs;

use tracing::{error, info, warn};

use super::sections::{AgentCleanupConfig, GrpcTimeoutConfig, HealthCheckConfig, ProxyConfig};
use super::{AppConfig, CliArgs};

/// 配置文件名
pub(super) const CONFIG_FILE: &str = "config.yml";
/// 代理默认监听端口（CLI --port 未指定时的回退值）
pub(super) const DEFAULT_PROXY_LISTEN_PORT: u16 = 8080;

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

    // 4. 命令行参数覆盖配置（优先级最高）
    if let Some(port) = cli_args.port {
        config.port = port;
        info!("Set port from CLI arg: {}", port);
    }

    if let Some(projects_dir) = cli_args.projects_dir {
        config.projects_dir = projects_dir.clone();
        info!("Set projects directory from CLI arg: {:?}", projects_dir);
    }

    // 5. 处理代理配置。必须在最终 HTTP port 确定后再推导 default_backend_port。
    if cli_args.enable_proxy {
        let proxy_config = ProxyConfig {
            listen_port: cli_args.proxy_port.unwrap_or(DEFAULT_PROXY_LISTEN_PORT),
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
        .map_err(|e| anyhow::anyhow!("Failed to write config file: {}", e))?;

    Ok(())
}
