//! 配置加载器（从 config.rs 拆出）。

use std::env;
use std::fs;
use std::path::Path;

use tracing::{error, info, warn};

use super::sections::{AgentCleanupConfig, GrpcTimeoutConfig, HealthCheckConfig, ProxyConfig};
use super::{AppConfig, CliArgs};

/// 配置文件名
pub(super) const CONFIG_FILE: &str = "config.yml";
/// 代理默认监听端口（CLI --port 未指定时的回退值）
pub(super) const DEFAULT_PROXY_LISTEN_PORT: u16 = 8080;

pub fn load_config_with_args(cli_args: CliArgs) -> anyhow::Result<AppConfig> {
    // 1. 首先加载默认配置
    let mut config = AppConfig::default();

    // 2. 读取配置文件。"不存在"与"存在但解析失败"语义完全不同，必须分开处理
    //    （旧实现两者共用一条降级路径，与 rcoder/src/config/loader.rs 已修掉的模式同款）：
    //    - 不存在：容器镜像不带 config.yml（Dockerfile 只 COPY 二进制，k8s 的 config.yml
    //      configmap 只挂给 rcoder 主服务，动态创建 agent 容器时只注入 env），首启自建
    //      是刚需 → create-then-continue。写盘失败仅记日志：只读根文件系统等场景下
    //      内存里的默认值仍足够跑起来，不该把容器打进 CrashLoop。
    //    - 存在但解析失败：fail-fast 上抛。静默降级默认值会让报错与真因隔好几层
    //      （rcoder 侧 0.1.233 事故：configmap 缩进坏 → docker_config 无镜像 → CrashLoop
    //      排障一小时），而 serde_yaml 的错误自带行号列号，直接暴露才是最短路径。
    //      更关键的是旧降级路径会顺手覆盖掉那份坏文件，把唯一的排障证据销毁。
    if Path::new(CONFIG_FILE).exists() {
        config = load_config_from_file()?;
        info!("Loaded config from {}", CONFIG_FILE);
    } else if let Err(create_err) = create_default_config_file() {
        error!("Failed to create default config file: {}", create_err);
    } else {
        info!("Created default config file: {}", CONFIG_FILE);
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

    Ok(config)
}

/// 从文件加载配置
fn load_config_from_file() -> anyhow::Result<AppConfig> {
    let config_content = fs::read_to_string(CONFIG_FILE)
        .map_err(|e| anyhow::anyhow!("Failed to read config file: {}", e))?;

    let config: AppConfig = serde_yaml::from_str(&config_content)
        .map_err(|e| anyhow::anyhow!("Failed to parse config file: {}", e))?;

    Ok(config)
}

/// 创建默认配置文件（仅当 `config.yml` 不存在时）。
fn create_default_config_file() -> anyhow::Result<()> {
    write_default_config_file(Path::new(CONFIG_FILE))
}

/// 写出默认配置文件到指定路径。
///
/// **任何情况下都不覆盖已存在的文件**：旧实现直接 `fs::write`，而它唯一的调用点在
/// "读取失败"降级分支里 —— 配置被改坏时会把那份坏文件冲掉，销毁唯一的排障证据。
/// 现在解析失败已 fail-fast，这道守卫是防止将来再有人把它接回降级路径。
fn write_default_config_file(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        return Ok(());
    }

    let content = render_default_config_yaml()?;
    fs::write(path, content)
        .map_err(|e| anyhow::anyhow!("Failed to write config file {}: {}", path.display(), e))?;
    Ok(())
}

/// 生成默认配置文件内容：静态注释头 + serde 序列化的 [`AppConfig::default`]。
///
/// 注释里刻意不复述任何具体数值。旧实现是 75 行手写 `format!` 模板（18 处插值），
/// 注释与插值各自维护，已经漂移过：注释写 `idle_timeout_secs: 300 (5分钟)`，而插进去
/// 的值来自 `AgentCleanupConfig::default()`，在 `http-server` feature 下是 86400 ——
/// 生成出来的文件自己和自己矛盾。数值交给 serde，注释只描述字段与覆盖它的环境变量，
/// 范围指向 sections.rs 的 MIN_/MAX_ 常量，无重复即无漂移。
fn render_default_config_yaml() -> anyhow::Result<String> {
    let header = "\
# rcoder agent_runner 配置文件
# 该文件在首次启动时自动生成，下方数值即当前生效的默认值。
#
# 字段说明（此处刻意不复述数值，避免注释与实际默认值漂移）：
#   default_agent_id   默认使用的 Agent ID
#   projects_dir       项目工作目录
#   port               主服务端口
#   proxy_config       Pingora 反向代理：listen_port / default_backend_port /
#                      backend_host / port_param / health_check。
#                      仅在 --enable-proxy 时生效，且由 CLI 参数整体重建
#   agent_cleanup      Agent 闲置清理：
#                        idle_timeout_secs     <- RCODER_AGENT_IDLE_TIMEOUT_SECS
#                        cleanup_interval_secs <- RCODER_AGENT_CLEANUP_INTERVAL_SECS
#   grpc_timeouts      gRPC 各环节超时：
#                        cancel_session_timeout_secs     <- RCODER_CANCEL_SESSION_TIMEOUT_SECS
#                        acp_session_create_timeout_secs <- RCODER_ACP_SESSION_CREATE_TIMEOUT_SECS
#                        agent_cancel_timeout_secs       <- RCODER_AGENT_CANCEL_TIMEOUT_SECS
#                        port_check_timeout_millis       <- RCODER_PORT_CHECK_TIMEOUT_MILLIS
#   mcp_proxy_log_dir  mcp-proxy 诊断日志目录（省略即不启用）
#
# 优先级：CLI 参数 > 环境变量 > 本文件 > 内置默认值。
# 各数值的有效范围见 crates/agent_runner/src/config/sections.rs 的 MIN_/MAX_ 常量。

";

    let body = serde_yaml::to_string(&AppConfig::default())
        .map_err(|e| anyhow::anyhow!("Failed to serialize default config: {}", e))?;

    Ok(format!("{header}{body}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 防漂移的根本手段：生成的 YAML 必须能被原样读回，且等于 AppConfig::default()。
    /// 手写模板时代没有任何测试覆盖这条链路，漂移因此能长期存在。
    #[test]
    fn rendered_default_config_roundtrips_to_default() {
        let yaml = render_default_config_yaml().expect("render default config");
        let parsed: AppConfig = serde_yaml::from_str(&yaml).expect("生成的 YAML 必须能被读回");
        assert_eq!(parsed, AppConfig::default());
    }

    /// 注释头不得再复述具体数值 —— 数值只能来自 serde，否则又会漂移。
    /// 判定口径：注释行里 ASCII 冒号后紧跟数字即算硬编码，这样能同时抓住
    /// "idle_timeout_secs: 300 (5分钟)" 与 "有效范围: 10 - 86400" 两种漂移形态。
    #[test]
    fn rendered_header_carries_no_hardcoded_values() {
        let yaml = render_default_config_yaml().expect("render default config");
        for line in yaml.lines().take_while(|l| l.starts_with('#')) {
            let hardcoded = line
                .split_once(": ")
                .is_some_and(|(_, rest)| rest.starts_with(|c: char| c.is_ascii_digit()));
            assert!(
                !hardcoded,
                "注释头出现硬编码数值，会与 serde 输出漂移: {line}"
            );
        }
    }

    #[test]
    fn never_overwrites_existing_config_file() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.yml");
        // 模拟用户改坏的文件：既是排障证据，也绝不能被默认配置冲掉
        let corrupt = "port: not-a-number\n  bad_indent: [\n";
        fs::write(&path, corrupt).expect("write corrupt config");

        write_default_config_file(&path).expect("已存在应是 no-op 而非错误");

        assert_eq!(fs::read_to_string(&path).expect("read back"), corrupt);
    }

    #[test]
    fn writes_default_config_when_absent() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("config.yml");

        write_default_config_file(&path).expect("write default config");

        let content = fs::read_to_string(&path).expect("read back");
        let parsed: AppConfig = serde_yaml::from_str(&content).expect("parse generated file");
        assert_eq!(parsed, AppConfig::default());
    }
}
