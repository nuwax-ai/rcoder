use std::collections::HashMap;
use std::path::PathBuf;

use agent_client_protocol::schema::{McpServer, McpServerStdio};
use agent_config::ContextServerConfig;
use tracing::{debug, info};

use super::env::build_mcp_server_path_env;

/// 从文件内容中解析 D-Bus 会话地址
fn parse_dbus_address_from_content(content: &str) -> Option<String> {
    content
        .lines()
        .find(|line| line.starts_with("DBUS_SESSION_BUS_ADDRESS="))
        .and_then(|line| line.split_once('='))
        .map(|(_, val)| {
            val.trim()
                .trim_end_matches(';')
                .trim_matches('\'')
                .trim_matches('"')
                .to_string()
        })
}

/// 获取 D-Bus 会话地址
/// 优先从环境变量读取，否则从文件读取
fn get_dbus_session_address() -> Option<String> {
    std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().or_else(|| {
        std::fs::read_to_string("/tmp/dbus-session-env")
            .ok()
            .and_then(|content| parse_dbus_address_from_content(&content))
    })
}

/// mcp-proxy 日志目录环境变量名
pub(crate) const ENV_MCP_PROXY_LOG_DIR: &str = "MCP_PROXY_LOG_DIR";

/// 检测命令是否为 mcp-proxy（简化版，只检测命令名）
pub(crate) fn is_mcp_proxy_command(command: &str) -> bool {
    command == "mcp-proxy"
}

/// 检测参数中是否有 convert 子命令
pub(crate) fn has_convert_subcommand(args: &[String]) -> bool {
    args.iter().any(|arg| arg == "convert")
}

/// 检测当前日志级别是否为 debug
fn is_debug_log_level() -> bool {
    // 优先检查 RUST_LOG 环境变量
    if let Ok(rust_log) = std::env::var("RUST_LOG") {
        let log_lower = rust_log.to_lowercase();
        return log_lower.contains("debug") || log_lower.contains("trace");
    }
    // 使用 tracing 的 enabled! 宏检测
    tracing::enabled!(tracing::Level::DEBUG)
}

/// 获取 mcp-proxy 日志目录（如果配置了的话）
pub(crate) fn get_mcp_proxy_log_dir() -> Option<String> {
    std::env::var(ENV_MCP_PROXY_LOG_DIR).ok()
}

/// 检查参数中是否已有 --log-dir 或 --log-file 参数
pub(crate) fn has_log_dir_arg(args: &[String]) -> bool {
    args.iter().any(|arg| {
        arg == "--log-dir"
            || arg.starts_with("--log-dir=")
            || arg == "--log-file"
            || arg.starts_with("--log-file=")
    })
}

/// 为 mcp-proxy convert 命令追加诊断参数
///
/// 当检测到以下条件时，自动追加 `--diagnostic` 参数：
/// 1. 命令是 `mcp-proxy`
/// 2. 参数包含 `convert` 子命令
/// 3. 当前日志级别是 debug
///
/// 只有配置了 `MCP_PROXY_LOG_DIR` 环境变量时才追加 `--log-dir` 参数
///
/// 重复检查逻辑：
/// 1. 如果参数中已有 --diagnostic，跳过注入
/// 2. 如果参数中已有 --log-dir 或 --log-file，不追加 --log-dir（避免覆盖用户配置）
pub(crate) fn enhance_mcp_proxy_args(command: &str, args: Vec<String>) -> Vec<String> {
    // 检查是否为 mcp-proxy convert 命令
    if !is_mcp_proxy_command(command) || !has_convert_subcommand(&args) {
        return args;
    }

    // 检查日志级别是否为 debug
    if !is_debug_log_level() {
        debug!("[MCP] mcp-proxy convert detected, but not debug, skip diagnostic params");
        return args;
    }

    // 检查是否已有 --diagnostic 参数
    let has_diagnostic = args.iter().any(|arg| arg == "--diagnostic");
    if has_diagnostic {
        debug!("[MCP] mcp-proxy already has --diagnostic params, skipping");
        return args;
    }

    let mut enhanced_args = args;

    // 追加 --diagnostic 参数
    enhanced_args.push("--diagnostic".to_string());
    info!("[MCP] added --diagnostic params to mcp-proxy");

    // 🔒 关键检查：如果用户已配置 --log-dir 或 --log-file，不覆盖
    if has_log_dir_arg(&enhanced_args) {
        debug!("[MCP] already has config params, skip --log-dir");
        return enhanced_args;
    }

    // 只有配置了 MCP_PROXY_LOG_DIR 环境变量时才追加 --log-dir 参数
    if let Some(log_dir) = get_mcp_proxy_log_dir() {
        enhanced_args.push("--log-dir".to_string());
        enhanced_args.push(log_dir.clone());
        info!(
            "[MCP] adding mcp-proxy convert --log-dir {} params",
            log_dir
        );
    }

    enhanced_args
}

/// 将配置中的 Context 服务器转换为 SACP 协议的 McpServer
pub fn convert_context_servers_sacp(
    configs: &HashMap<String, ContextServerConfig>,
) -> Vec<McpServer> {
    let dbus_address = get_dbus_session_address();

    configs
        .iter()
        .filter(|(_, c)| c.enabled)
        .filter_map(|(name, c)| {
            let command = c.command.as_ref()?;
            let mut server = McpServerStdio::new(name, PathBuf::from(command));

            // 处理参数，可能需要为 mcp-proxy convert 追加诊断参数
            let final_args = if let Some(args) = &c.args {
                enhance_mcp_proxy_args(command, args.clone())
            } else {
                Vec::new()
            };

            if !final_args.is_empty() {
                server = server.args(final_args);
            }

            let mut env_vars: Vec<agent_client_protocol::schema::EnvVariable> =
                if let Some(env) = &c.env {
                    env.iter()
                        .map(|(k, v)| {
                            agent_client_protocol::schema::EnvVariable::new(k.clone(), v.clone())
                        })
                        .collect()
                } else {
                    Vec::new()
                };

            // 注入 D-Bus 会话地址
            if let Some(ref addr) = dbus_address
                && !env_vars
                    .iter()
                    .any(|e| e.name == "DBUS_SESSION_BUS_ADDRESS")
            {
                env_vars.push(agent_client_protocol::schema::EnvVariable::new(
                    "DBUS_SESSION_BUS_ADDRESS".to_string(),
                    addr.clone(),
                ));
            }

            // 注入镜像源环境变量（npx/bunx/uvx 子进程使用）
            for (key, val) in crate::mirror_env::collect_mirror_env_vars() {
                if !env_vars.iter().any(|e| e.name == key) {
                    env_vars.push(agent_client_protocol::schema::EnvVariable::new(key, val));
                }
            }

            // 注入 PATH 环境变量（关键！）
            // Claude Code SDK 用 MCP server 的 env 替换整个子进程环境。
            // 如果 env 中没有 PATH，mcp-proxy convert --config 模式下
            // 无法找到 uvx/npx 等命令来启动 MCP 子服务。
            if !env_vars.iter().any(|e| e.name == "PATH") {
                let path_value = build_mcp_server_path_env();
                if !path_value.is_empty() {
                    env_vars.push(agent_client_protocol::schema::EnvVariable::new(
                        "PATH".to_string(),
                        path_value,
                    ));
                }
            }

            // 注入 HOME 环境变量（uvx/npx 等工具需要 HOME 来定位缓存目录）
            #[cfg(not(windows))]
            if !env_vars.iter().any(|e| e.name == "HOME")
                && let Ok(home) = std::env::var("HOME")
            {
                env_vars.push(agent_client_protocol::schema::EnvVariable::new(
                    "HOME".to_string(),
                    home,
                ));
            }

            // Windows: 注入 USERPROFILE 和 PATHEXT
            #[cfg(windows)]
            {
                if !env_vars.iter().any(|e| e.name == "USERPROFILE") {
                    if let Ok(profile) = std::env::var("USERPROFILE") {
                        env_vars.push(agent_client_protocol::schema::EnvVariable::new(
                            "USERPROFILE".to_string(),
                            profile,
                        ));
                    }
                }
                if !env_vars.iter().any(|e| e.name == "PATHEXT") {
                    if let Ok(pathext) = std::env::var("PATHEXT") {
                        env_vars.push(agent_client_protocol::schema::EnvVariable::new(
                            "PATHEXT".to_string(),
                            pathext,
                        ));
                    }
                }
            }

            if !env_vars.is_empty() {
                server = server.env(env_vars);
            }

            Some(McpServer::Stdio(server))
        })
        .collect()
}
