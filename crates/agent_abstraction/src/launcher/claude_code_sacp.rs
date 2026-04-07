//! Claude Code ACP Agent 启动器 (SACP 版本)
//!
//! 使用 symposium-acp (sacp) 库的新实现，支持：
//! - 标准 tokio::spawn（无需 LocalSet）
//! - Builder 模式的连接构建
//! - 回调函数式的消息处理
//!
//! 此文件是 `claude_code.rs` 的 SACP 版本替代品。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[cfg(windows)]
use crate::path_env::TAURI_APP_DATA_DIR;
use agent_config::{AgentInstallationManager, AgentServersConfig, ContextServerConfig};
use anyhow::{Context, Result};
use process_wrap::tokio::CommandWrap;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
#[cfg(windows)]
use process_wrap::tokio::{CreationFlags, JobObject};
use shared_types::{error_codes, ModelProviderConfig, ProjectAndAgentInfo};
use tokio::sync::mpsc;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

// SACP 库导入
use sacp::schema::{
    CancelNotification, InitializeRequest, McpServer, McpServerStdio, NewSessionRequest,
    PermissionOptionKind, PromptRequest, ProtocolVersion, RequestPermissionOutcome,
    RequestPermissionRequest, RequestPermissionResponse, SelectedPermissionOutcome, SessionId,
    SessionNotification,
};
use sacp::{ClientToAgent, JrConnectionCx, JrRequestCx};

use crate::acp::CancelNotificationRequestWrapper;
use crate::traits::AgentStartConfig;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::session_registry::SessionRegistry;

// 导入生命周期管理
use super::lifecycle::AgentLifecycleGuard;
#[cfg(windows)]
use super::windows_launch::{
    CREATE_NO_WINDOW_FLAG, normalize_windows_command_for_no_window,
    resolve_windows_node_cli_command,
};
#[cfg(windows)]
use windows::Win32::System::Threading::PROCESS_CREATION_FLAGS;

/// 使用最新协议版本
const VERSION: ProtocolVersion = ProtocolVersion::LATEST;

/// API 密钥占位符（实际密钥由 Pingora 代理注入）
const API_KEY_PLACEHOLDER: &str = "PROXY_MANAGED_KEY";

/// 环境变量键名常量
const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const ENV_ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ENV_ANTHROPIC_MODEL: &str = "ANTHROPIC_MODEL";
const ENV_DISABLE_NONESSENTIAL: &str = "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";
const ENV_AGENT_SDK_SKIP_VERSION_CHECK: &str = "CLAUDE_AGENT_SDK_SKIP_VERSION_CHECK";
const ENV_RUST_LOG: &str = "RUST_LOG";
const ENV_AGENT_WORKING_DIR: &str = "AGENT_WORKING_DIR";
const ENV_AGENT_PROJECT_ID: &str = "AGENT_PROJECT_ID";

/// OpenAI 环境变量常量
const ENV_OPENAI_API_KEY: &str = "OPENAI_API_KEY";
const ENV_OPENAI_BASE_URL: &str = "OPENAI_BASE_URL";
/// nuwaxcode 使用 OPENCODE_MODEL 而不是 OPENAI_MODEL
const ENV_OPENCODE_MODEL: &str = "OPENCODE_MODEL";

/// 默认代理 Base URL（包含 UUID 占位符）
const DEFAULT_PROXY_BASE_URL: &str = "http://localhost:8088/api/{SERVICE_UUID}";

/// 获取各平台下常用的用户二进制目录
///
/// Linux/Mac:
/// - ~/.cargo/bin (Rust cargo)
/// - ~/.npm-global/bin (npm global)
/// - ~/.local/bin (uv, pipx)
/// - /opt/homebrew/bin (Homebrew on Apple Silicon)
/// - /usr/local/bin (Homebrew on Intel Mac / 通用)
/// - /home/linuxbrew/.linuxbrew/bin (Linuxbrew)
///
/// Windows:
/// - 返回空，Windows 环境变量已包含这些路径
///
/// 注意：使用 $HOME 变量而非硬编码 /root，HOME 不存在时自动回退到 /root
fn get_common_user_bins() -> Vec<&'static str> {
    #[cfg(windows)]
    {
        vec![] // Windows 用户路径已在系统 PATH 中
    }

    #[cfg(not(windows))]
    {
        vec![
            // Cargo (Rust)
            "$HOME/.cargo/bin",
            // NPM global
            "$HOME/.npm-global/bin",
            // UV / PIPX global
            "$HOME/.local/bin",
            // Homebrew (Apple Silicon)
            "/opt/homebrew/bin",
            // Homebrew (Intel Mac) / 通用本地安装
            "/usr/local/bin",
            // Linuxbrew
            "/home/linuxbrew/.linuxbrew/bin",
            // 系统级 cargo（某些容器环境）
            "/opt/cargo/bin",
        ]
    }
}

/// 确保子进程环境变量中包含 PATH / PATHEXT，便于解析可执行路径与 Windows .cmd 脚本
///
/// PATH 组成（优先级从高到低）：
/// 1. NUWAX_APP_RUNTIME_PATH — 应用自有运行时目录（node/uv/mcp-proxy 等）
/// 2. 当前系统 PATH — 保留所有现有路径（关键改进！）
/// 3. 常用用户目录 — cargo, npm, uv, homebrew 等
/// 4. 系统基础目录 — `/bin`, `/usr/bin` 等（仅在 PATH 为空时）
fn ensure_subprocess_path_env(merged_envs: &mut std::collections::HashMap<String, String>) {
    if !merged_envs.contains_key("PATH") {
        let path = build_mcp_server_path_env();
        if !path.is_empty() {
            merged_envs.insert("PATH".to_string(), path);
            debug!("[SACP] 📋 alreadybuilt PATH message ( message PATH message directory)");
        }
    }
    #[cfg(windows)]
    ensure_windows_subprocess_env(merged_envs);
}

/// 构建 MCP 服务器子进程所需的 PATH 环境变量
///
/// Claude Code SDK 在启动 MCP 服务器子进程时会用提供的 env 替换整个环境。
/// 如果 env 中缺少 PATH，mcp-proxy convert --config 模式下的孙进程
/// （如 uvx、npx）将因为找不到命令而静默失败。
///
/// 此函数与 ensure_subprocess_path_env 使用相同的逻辑构建 PATH：
/// 1. NUWAX_APP_RUNTIME_PATH — 应用自有运行时目录（优先）
/// 2. 当前系统 PATH — 保留所有现有路径（关键改进！）
/// 3. 平台特定目录：
///    - Windows: APPDATA 推导的 Tauri 应用 node/npm 路径
///    - Unix: 常用用户目录（cargo, npm, uv, homebrew 等）
/// 4. 系统基础目录 — 仅在 PATH 为空时兜底
fn build_mcp_server_path_env() -> String {
    let sep = if cfg!(windows) { ";" } else { ":" };
    let mut paths: Vec<String> = Vec::new();

    // 1. 优先：NUWAX_APP_RUNTIME_PATH
    if let Ok(runtime_path) = std::env::var("NUWAX_APP_RUNTIME_PATH") {
        for p in runtime_path.split(sep) {
            let p = p.trim();
            if !p.is_empty() && !paths.contains(&p.to_string()) {
                paths.push(p.to_string());
            }
        }
    }

    // 2. 追加：当前系统 PATH（关键改动！）
    if let Ok(current_path) = std::env::var("PATH") {
        for p in current_path.split(sep) {
            let p = p.trim();
            if !p.is_empty() && !paths.contains(&p.to_string()) {
                paths.push(p.to_string());
            }
        }
    }

    // 3. 追加：平台特定目录
    #[cfg(windows)]
    {
        // Windows: 从 APPDATA 推导 Tauri 应用的 node/npm 路径
        if let Ok(appdata) = std::env::var("APPDATA") {
            use std::path::PathBuf;
            let app_base = PathBuf::from(&appdata).join(TAURI_APP_DATA_DIR);

            let node_bin = app_base.join("runtime").join("node").join("bin");
            if node_bin.is_dir() {
                let node_bin_str = node_bin.to_string_lossy().to_string();
                if !paths.contains(&node_bin_str) {
                    paths.push(node_bin_str);
                }
            }

            let npm_bin = app_base.join("node_modules").join(".bin");
            if npm_bin.is_dir() {
                let npm_bin_str = npm_bin.to_string_lossy().to_string();
                if !paths.contains(&npm_bin_str) {
                    paths.push(npm_bin_str);
                }
            }
        }
    }

    #[cfg(not(windows))]
    {
        // Unix: 追加常用用户目录（自动扩展 $HOME）
        let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
        for bin in get_common_user_bins() {
            let expanded = bin.replace("$HOME", &home);
            let path = std::path::Path::new(&expanded);
            if path.is_dir() && !paths.contains(&expanded) {
                paths.push(expanded);
            }
        }
    }

    // 4. 兜底：系统基础目录（仅在 PATH 为空时）
    // 正常情况下不会走到这里，因为步骤 2 已经追加了当前系统 PATH
    // 这是为了防御性编程，确保即使在极端情况下也有可用的基础命令
    if paths.is_empty() {
        #[cfg(not(windows))]
        for sys_dir in &["/bin", "/usr/bin", "/usr/local/bin", "/sbin", "/usr/sbin"] {
            let s = sys_dir.to_string();
            if std::path::Path::new(sys_dir).is_dir() {
                paths.push(s);
            }
        }

        #[cfg(windows)]
        {
            // Windows: 直接返回系统 PATH 作为最后手段
            if let Ok(current_path) = std::env::var("PATH") {
                return current_path;
            }
        }
    }

    paths.join(sep)
}

/// Windows 专属：确保 PATHEXT 存在，以便解析 .cmd/.bat 等脚本
#[cfg(windows)]
fn ensure_windows_subprocess_env(merged_envs: &mut std::collections::HashMap<String, String>) {
    if !merged_envs.contains_key("PATHEXT") {
        if let Ok(pathext) = std::env::var("PATHEXT") {
            merged_envs.insert("PATHEXT".to_string(), pathext);
            debug!("[SACP] 📋 already message PATHEXT message ");
        }
    }
}

/// 根据 proxy feature 决定使用占位符还是真实 API Key
fn resolve_api_key(provider: &ModelProviderConfig) -> String {
    if cfg!(feature = "proxy") {
        API_KEY_PLACEHOLDER.to_string()
    } else {
        provider.api_key.clone()
    }
}

/// 根据 proxy feature 决定使用代理 URL 还是真实 Base URL
fn resolve_base_url(provider: &ModelProviderConfig) -> String {
    if cfg!(feature = "proxy") {
        DEFAULT_PROXY_BASE_URL.to_string()
    } else {
        provider.base_url.clone()
    }
}

/// Agent 配置参数 (与旧版兼容)
#[derive(Debug, Clone)]
pub struct SacpAgentLaunchConfig {
    /// 命令路径
    pub command: String,
    /// 命令参数
    pub args: Vec<String>,
    /// 环境变量
    pub env: HashMap<String, String>,
    /// Context 服务器配置 (MCP servers)
    pub context_servers: HashMap<String, ContextServerConfig>,
}

/// Agent 连接信息（SACP 版本）
pub struct SacpLauncherConnectionInfo {
    /// 会话 ID
    pub session_id: SessionId,
    /// 发送 Prompt 消息的通道（有界通道，提供背压保护）
    pub prompt_tx: mpsc::Sender<PromptRequest>,
    /// 发送取消请求的通道（有界通道，提供背压保护）
    pub cancel_tx: mpsc::Sender<CancelNotificationRequestWrapper>,
    /// 生命周期守卫（自动清理资源）
    pub lifecycle_guard: Arc<AgentLifecycleGuard>,
}

/// 从配置文件加载 Agent 配置
///
/// 优先加载嵌入的JSON配置文件，如果加载失败则使用默认配置
/// 同时检查并自动安装 agent（如果需要）
pub async fn load_sacp_agent_config(
    model_provider: Option<&ModelProviderConfig>,
    service_type: &shared_types::ServiceType,
) -> Result<SacpAgentLaunchConfig> {
    // 复用旧版配置加载逻辑
    let config = AgentServersConfig::load_or_default_for_service(service_type).await;

    if let Some(agent_config) = config.get_agent("claude-code-acp-ts") {
        debug!(
            "📋 [SACP] message default Agent config: {}",
            agent_config.agent_id
        );

        // 检查并安装 agent - 临时禁用以测试本地 claude-code-acp-ts
        // if agent_config.installation.package_name.is_some() {
        //     let installation_manager = AgentInstallationManager::new();
        //     match installation_manager
        //         .ensure_installed(&agent_config.installation, &agent_config.command)
        //         .await
        //     {
        //         Ok(result) => {
        //             if result.already_installed {
        //                 debug!("[SACP] Agent already message : {}", agent_config.command);
        //             } else {
        //                 info!("[SACP] Agent message succeeded: {}", result.message);
        //             }
        //         }
        //         Err(e) => {
        //             warn!(
        //                 "[SACP] Agent message Installation failed: {}, message started",
        //                 e
        //             );
        //         }
        //     }
        // }

        // 解析环境变量
        let mut resolved_env = agent_config.env.clone();

        if let Some(provider) = model_provider {
            // 统一替换所有环境变量中的模板
            // proxy 模式下：API_KEY 和 BASE_URL 使用占位符/代理URL，由 Pingora 代理注入真实值
            // 非 proxy 模式下：直接使用真实的 API Key 和 Base URL
            let resolved_key = resolve_api_key(provider);
            let resolved_url = resolve_base_url(provider);
            for (_key, value) in resolved_env.iter_mut() {
                *value = value
                    .replace("{MODEL_PROVIDER_API_KEY}", &resolved_key)
                    .replace("{MODEL_PROVIDER_BASE_URL}", &resolved_url)
                    .replace("{MODEL_PROVIDER_DEFAULT_MODEL}", &provider.default_model)
                    .replace("{MODEL_PROVIDER_NAME}", &provider.name);
            }
        }

        // 禁用 Claude Code 非必要网络请求
        resolved_env.insert(ENV_DISABLE_NONESSENTIAL.to_string(), "1".to_string());
        // 跳过 Agent SDK 版本检查
        resolved_env.insert(ENV_AGENT_SDK_SKIP_VERSION_CHECK.to_string(), "1".to_string());

        // debug: 打印最终环境变量（API Key 已脱敏）
        let mask_key = |v: &String| -> String {
            if v.len() > 8 {
                format!("{}***{}", &v[..4], &v[v.len()-4..])
            } else {
                "***".to_string()
            }
        };
        debug!(
            "[SACP] Final env config: command={}, ANTHROPIC_API_KEY={}, ANTHROPIC_BASE_URL={}, ANTHROPIC_MODEL={}, \
             OPENAI_API_KEY={}, OPENAI_BASE_URL={}, OPENCODE_MODEL={}, \
             RUST_LOG={}, CLAUDE_CODE_MAX_TOKENS={}, CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC={}",
            agent_config.command,
            resolved_env.get("ANTHROPIC_API_KEY").map(|v| mask_key(v)).unwrap_or_default(),
            resolved_env.get("ANTHROPIC_BASE_URL").unwrap_or(&"<unset>".to_string()),
            resolved_env.get("ANTHROPIC_MODEL").unwrap_or(&"<unset>".to_string()),
            resolved_env.get("OPENAI_API_KEY").map(|v| mask_key(v)).unwrap_or_default(),
            resolved_env.get("OPENAI_BASE_URL").unwrap_or(&"<unset>".to_string()),
            resolved_env.get("OPENCODE_MODEL").unwrap_or(&"<unset>".to_string()),
            resolved_env.get("RUST_LOG").unwrap_or(&"<unset>".to_string()),
            resolved_env.get("CLAUDE_CODE_MAX_TOKENS").unwrap_or(&"<unset>".to_string()),
            resolved_env.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC").unwrap_or(&"<unset>".to_string()),
        );

        Ok(SacpAgentLaunchConfig {
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            env: resolved_env,
            context_servers: config.context_servers.clone(),
        })
    } else {
        warn!("[SACP] config message not message claude-code-acp-ts, message defaultconfig");
        get_default_sacp_agent_config(model_provider, service_type)
    }
}

/// 获取默认的 Agent 配置（后备方案）
pub fn get_default_sacp_agent_config(
    model_provider: Option<&ModelProviderConfig>,
    _service_type: &shared_types::ServiceType,
) -> Result<SacpAgentLaunchConfig> {
    let mut env = HashMap::new();

    if let Some(provider) = model_provider {
        let resolved_key = resolve_api_key(provider);
        let resolved_url = resolve_base_url(provider);

        // Anthropic 环境变量
        if !provider.api_key.is_empty() {
            env.insert(ENV_ANTHROPIC_API_KEY.to_string(), resolved_key.clone());
        }
        if !provider.base_url.is_empty() {
            env.insert(ENV_ANTHROPIC_BASE_URL.to_string(), resolved_url.clone());
        }
        if !provider.default_model.is_empty() {
            env.insert(
                ENV_ANTHROPIC_MODEL.to_string(),
                provider.default_model.clone(),
            );
        }

        // OpenAI 环境变量 (支持 OpenAI 兼容的 Agent)
        if !provider.api_key.is_empty() {
            env.insert(ENV_OPENAI_API_KEY.to_string(), resolved_key);
        }
        if !provider.base_url.is_empty() {
            env.insert(ENV_OPENAI_BASE_URL.to_string(), resolved_url);
        }
        if !provider.default_model.is_empty() {
            // nuwaxcode 使用 OPENCODE_MODEL，model_name 中已包含 openai-compatible/ 前缀
            env.insert(
                ENV_OPENCODE_MODEL.to_string(),
                provider.default_model.clone(),
            );
        }
    }

    env.insert(ENV_RUST_LOG.to_string(), "info".to_string());
    env.insert(ENV_DISABLE_NONESSENTIAL.to_string(), "1".to_string());
    env.insert(ENV_AGENT_SDK_SKIP_VERSION_CHECK.to_string(), "1".to_string());

    // Resolve the claude-code-acp-ts command path.
    // Priority: CLAUDE_CODE_ACP_PATH env var > `which` crate lookup > bare command name.
    // Tauri apps may not inherit the user's shell PATH, so we try `which` crate to get
    // an absolute path at build/launch time.
    let command = if let Ok(path) = std::env::var("CLAUDE_CODE_ACP_PATH") {
        path
    } else {
        match which::which("claude-code-acp-ts") {
            Ok(resolved_path) => {
                tracing::info!(
                    "Resolved claude-code-acp-ts path via `which` crate: {}",
                    resolved_path.display()
                );
                resolved_path.to_string_lossy().to_string()
            }
            Err(_) => "claude-code-acp-ts".to_string(),
        }
    };

    Ok(SacpAgentLaunchConfig {
        command,
        args: Vec::new(),
        env,
        context_servers: HashMap::new(),
    })
}

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
const ENV_MCP_PROXY_LOG_DIR: &str = "MCP_PROXY_LOG_DIR";

/// 检测命令是否为 mcp-proxy（简化版，只检测命令名）
fn is_mcp_proxy_command(command: &str) -> bool {
    command == "mcp-proxy"
}

/// 检测参数中是否有 convert 子命令
fn has_convert_subcommand(args: &[String]) -> bool {
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
fn get_mcp_proxy_log_dir() -> Option<String> {
    std::env::var(ENV_MCP_PROXY_LOG_DIR).ok()
}

/// 检查参数中是否已有 --log-dir 或 --log-file 参数
fn has_log_dir_arg(args: &[String]) -> bool {
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
fn enhance_mcp_proxy_args(command: &str, args: Vec<String>) -> Vec<String> {
    // 检查是否为 mcp-proxy convert 命令
    if !is_mcp_proxy_command(command) || !has_convert_subcommand(&args) {
        return args;
    }

    // 检查日志级别是否为 debug
    if !is_debug_log_level() {
        debug!(
            "[MCP] mcp-proxy convert detect message, message debug, skip message params message "
        );
        return args;
    }

    // 检查是否已有 --diagnostic 参数
    let has_diagnostic = args.iter().any(|arg| arg == "--diagnostic");
    if has_diagnostic {
        debug!("[MCP] mcp-proxy convert already message --diagnostic params, skip message ");
        return args;
    }

    let mut enhanced_args = args;

    // 追加 --diagnostic 参数
    enhanced_args.push("--diagnostic".to_string());
    info!("[MCP] message mcp-proxy convert message --diagnostic params");

    // 🔒 关键检查：如果用户已配置 --log-dir 或 --log-file，不覆盖
    if has_log_dir_arg(&enhanced_args) {
        debug!("[MCP] message alreadyconfig message params, skip --log-dir message ");
        return enhanced_args;
    }

    // 只有配置了 MCP_PROXY_LOG_DIR 环境变量时才追加 --log-dir 参数
    if let Some(log_dir) = get_mcp_proxy_log_dir() {
        enhanced_args.push("--log-dir".to_string());
        enhanced_args.push(log_dir.clone());
        info!(
            "[MCP] message mcp-proxy convert message --log-dir {} params",
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

            let mut env_vars: Vec<sacp::schema::EnvVariable> = if let Some(env) = &c.env {
                env.iter()
                    .map(|(k, v)| sacp::schema::EnvVariable::new(k.clone(), v.clone()))
                    .collect()
            } else {
                Vec::new()
            };

            // 注入 D-Bus 会话地址
            if let Some(ref addr) = dbus_address {
                if !env_vars
                    .iter()
                    .any(|e| e.name == "DBUS_SESSION_BUS_ADDRESS")
                {
                    env_vars.push(sacp::schema::EnvVariable::new(
                        "DBUS_SESSION_BUS_ADDRESS".to_string(),
                        addr.clone(),
                    ));
                }
            }

            // 注入镜像源环境变量（npx/bunx/uvx 子进程使用）
            for (key, val) in crate::mirror_env::collect_mirror_env_vars() {
                if !env_vars.iter().any(|e| e.name == key) {
                    env_vars.push(sacp::schema::EnvVariable::new(key, val));
                }
            }

            // 注入 PATH 环境变量（关键！）
            // Claude Code SDK 用 MCP server 的 env 替换整个子进程环境。
            // 如果 env 中没有 PATH，mcp-proxy convert --config 模式下
            // 无法找到 uvx/npx 等命令来启动 MCP 子服务。
            if !env_vars.iter().any(|e| e.name == "PATH") {
                let path_value = build_mcp_server_path_env();
                if !path_value.is_empty() {
                    env_vars.push(sacp::schema::EnvVariable::new(
                        "PATH".to_string(),
                        path_value,
                    ));
                }
            }

            // 注入 HOME 环境变量（uvx/npx 等工具需要 HOME 来定位缓存目录）
            #[cfg(not(windows))]
            if !env_vars.iter().any(|e| e.name == "HOME") {
                if let Ok(home) = std::env::var("HOME") {
                    env_vars.push(sacp::schema::EnvVariable::new("HOME".to_string(), home));
                }
            }

            // Windows: 注入 USERPROFILE 和 PATHEXT
            #[cfg(windows)]
            {
                if !env_vars.iter().any(|e| e.name == "USERPROFILE") {
                    if let Ok(profile) = std::env::var("USERPROFILE") {
                        env_vars.push(sacp::schema::EnvVariable::new(
                            "USERPROFILE".to_string(),
                            profile,
                        ));
                    }
                }
                if !env_vars.iter().any(|e| e.name == "PATHEXT") {
                    if let Ok(pathext) = std::env::var("PATHEXT") {
                        env_vars.push(sacp::schema::EnvVariable::new(
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

/// Claude Code ACP Agent 启动器 (SACP 版本)
///
/// 使用 SACP 库的 Builder 模式和回调函数，无需 LocalSet。
pub struct SacpClaudeCodeLauncher<N: SessionNotifier> {
    /// 会话通知器
    notifier: Arc<N>,
}

impl<N: SessionNotifier + 'static> SacpClaudeCodeLauncher<N> {
    /// 创建新的启动器
    pub fn new(notifier: Arc<N>) -> Self {
        Self { notifier }
    }

    /// 启动 Claude Code ACP Agent 服务
    ///
    /// 使用 SACP 库的 Builder 模式，支持标准 tokio::spawn
    pub async fn launch<R: SessionRegistry + 'static>(
        &self,
        project_id: String,
        project_path: PathBuf,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        _registry: Arc<R>,
        service_uuid: Option<String>,
    ) -> Result<SacpLauncherConnectionInfo>
    where
        R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
    {
        // 从配置加载默认 Agent 参数
        let default_agent_config =
            load_sacp_agent_config(model_provider.as_ref(), &start_config.service_type).await?;

        // 🎯 关键：检查是否有自定义 agent_server 配置覆盖
        let (command_path, command_args, base_env) =
            if let Some(ref agent_server_override) = start_config.agent_server_override {
                // 使用自定义 command（如果提供），否则用默认
                let cmd = agent_server_override
                    .command
                    .clone()
                    .unwrap_or_else(|| default_agent_config.command.clone());

                // 使用自定义 args（如果提供），否则用默认
                let args = agent_server_override
                    .args
                    .clone()
                    .unwrap_or_else(|| default_agent_config.args.clone());

                // 合并环境变量：默认配置 + 自定义配置（自定义覆盖默认）
                let mut env = default_agent_config.env.clone();
                if let Some(custom_env) = &agent_server_override.env {
                    // 使用 extend 替代循环，更高效
                    env.extend(custom_env.iter().map(|(k, v)| (k.clone(), v.clone())));
                }

                // 🔧 关键修复：替换自定义环境变量中的模板变量
                // 用户可能传入 {MODEL_PROVIDER_API_KEY} 等模板，需要替换为实际值
                // proxy 模式下：API_KEY 和 BASE_URL 使用占位符/代理URL，由 Pingora 代理注入真实值
                // 非 proxy 模式下：直接使用真实的 API Key 和 Base URL
                if let Some(ref provider) = model_provider {
                    let resolved_key = resolve_api_key(provider);
                    let resolved_url = resolve_base_url(provider);
                    for (_key, value) in env.iter_mut() {
                        *value = value
                            .replace("{MODEL_PROVIDER_API_KEY}", &resolved_key)
                            .replace("{MODEL_PROVIDER_BASE_URL}", &resolved_url)
                            .replace("{MODEL_PROVIDER_DEFAULT_MODEL}", &provider.default_model)
                            .replace("{MODEL_PROVIDER_NAME}", &provider.name);
                    }
                    debug!(
                        "🔧 [SACP] Replaced custom env var template, model={}",
                        provider.default_model
                    );
                }

                info!(
                    "🎯 [SACP] Using custom Agent: agent_id={}, command={} {:?}",
                    agent_server_override.get_agent_id(),
                    cmd,
                    args
                );
                (cmd, args, env)
            } else {
                // 使用默认配置
                info!(
                    "📋 [SACP] Using default Agent: {} {:?}",
                    default_agent_config.command, default_agent_config.args
                );
                (
                    default_agent_config.command.clone(),
                    default_agent_config.args.clone(),
                    default_agent_config.env.clone(),
                )
            };

        // 创建通道（使用有界通道防止 OOM）
        // 容量由常量定义，足够处理突发请求，同时提供背压保护
        let (cancel_tx, cancel_rx) = mpsc::channel::<CancelNotificationRequestWrapper>(
            shared_types::AGENT_CANCEL_CHANNEL_CAPACITY,
        );
        let (prompt_tx, prompt_rx) =
            mpsc::channel::<PromptRequest>(shared_types::AGENT_PROMPT_CHANNEL_CAPACITY);
        let (session_id_tx, session_id_rx) = tokio::sync::oneshot::channel::<SessionId>();

        // 创建 CancellationToken
        let cancel_token = CancellationToken::new();

        info!(
            "[SACP] projectworkdirectory: {}",
            &project_path.to_string_lossy()
        );

        // 准备 MCP 服务器
        let mcp_servers = if start_config.has_mcp_servers() {
            info!("[SACP] message AgentStartConfig message MCP message ");
            start_config.mcp_servers.clone()
        } else if !default_agent_config.context_servers.is_empty() {
            info!("[SACP] message configfile message MCP message ");
            convert_context_servers_sacp(&default_agent_config.context_servers)
        } else {
            info!("📝 [SACP] notconfig MCP message ");
            Vec::new()
        };

        let mut command_path = command_path;
        let mut command_args = command_args;

        #[cfg(windows)]
        if let Some((resolved_program, resolved_args)) =
            resolve_windows_node_cli_command(&command_path, &command_args)
        {
            let entry = resolved_args.first().cloned().unwrap_or_default();
            info!(
                "[SACP] Windows direct node startup: {} -> {} {}",
                command_path, resolved_program, entry
            );
            command_path = resolved_program;
            command_args = resolved_args;
        }

        // 准备环境变量（在 base_env 基础上添加项目相关变量）
        let mut merged_envs = base_env;
        merged_envs.insert(
            ENV_AGENT_WORKING_DIR.to_string(),
            project_path.to_string_lossy().to_string(),
        );
        merged_envs.insert(ENV_AGENT_PROJECT_ID.to_string(), project_id.clone());

        ensure_subprocess_path_env(&mut merged_envs);

        // 替换 UUID 占位符
        if let Some(ref uuid) = service_uuid {
            for (_key, value) in merged_envs.iter_mut() {
                *value = value.replace("{SERVICE_UUID}", uuid);
            }
        }

        // 🔒 安全防护：proxy 模式下强制将敏感环境变量替换为占位符/代理 URL，防止密钥泄露
        // 即使用户在配置中直接写了真实的 API_KEY 或 BASE_URL，也会被替换
        if cfg!(feature = "proxy") {
            if model_provider.is_some() {
                // 强制替换 Anthropic 敏感变量
                if merged_envs.contains_key(ENV_ANTHROPIC_API_KEY) {
                    merged_envs.insert(
                        ENV_ANTHROPIC_API_KEY.to_string(),
                        API_KEY_PLACEHOLDER.to_string(),
                    );
                }
                // ANTHROPIC_AUTH_TOKEN 也需要替换（某些场景下可能存在）
                if merged_envs.contains_key("ANTHROPIC_AUTH_TOKEN") {
                    merged_envs.insert(
                        "ANTHROPIC_AUTH_TOKEN".to_string(),
                        API_KEY_PLACEHOLDER.to_string(),
                    );
                }
                if merged_envs.contains_key(ENV_ANTHROPIC_BASE_URL) {
                    merged_envs.insert(
                        ENV_ANTHROPIC_BASE_URL.to_string(),
                        service_uuid
                            .as_ref()
                            .map(|uuid| DEFAULT_PROXY_BASE_URL.replace("{SERVICE_UUID}", uuid))
                            .unwrap_or_else(|| DEFAULT_PROXY_BASE_URL.to_string()),
                    );
                }

                // 强制替换 OpenAI 敏感变量
                if merged_envs.contains_key(ENV_OPENAI_API_KEY) {
                    merged_envs.insert(
                        ENV_OPENAI_API_KEY.to_string(),
                        API_KEY_PLACEHOLDER.to_string(),
                    );
                }
                if merged_envs.contains_key(ENV_OPENAI_BASE_URL) {
                    merged_envs.insert(
                        ENV_OPENAI_BASE_URL.to_string(),
                        service_uuid
                            .as_ref()
                            .map(|uuid| DEFAULT_PROXY_BASE_URL.replace("{SERVICE_UUID}", uuid))
                            .unwrap_or_else(|| DEFAULT_PROXY_BASE_URL.to_string()),
                    );
                }

                debug!("[SACP] 🔒 alreadyforce message /proxy URL");
            }
        } else {
            debug!("[SACP] 🔓 message proxy message, message API Key message Base URL");
        }

        // 🔍 打印传递给 Agent 的完整环境变量（用于调试）
        // 注意：敏感字段（API Key）需要脱敏处理，防止日志泄露
        debug!(
            "[SACP] 📋 Start Agent command: {} {:?}",
            command_path, command_args
        );
        debug!("[SACP] 📋 work directory: {:?}", project_path);
        debug!(
            "[SACP] 📋 Environment variables passed to Agent ({} items):",
            merged_envs.len()
        );

        // 需要脱敏的环境变量 key 列表（即使在 debug 日志中也不暴露完整值）
        const SENSITIVE_ENV_KEYS: &[&str] = &[
            ENV_ANTHROPIC_API_KEY,
            ENV_OPENAI_API_KEY,
            "ANTHROPIC_AUTH_TOKEN",
        ];

        // 按字母顺序排序并打印所有环境变量（仅在 debug 级别）
        let mut env_keys: Vec<_> = merged_envs.keys().collect();
        env_keys.sort();

        for key in env_keys.iter() {
            let value = merged_envs.get(*key).unwrap();
            if SENSITIVE_ENV_KEYS.contains(&key.as_str()) {
                // 脱敏：只显示前4个字符 + ***
                let masked = if value.len() > 4 {
                    format!("{}***", &value[..4])
                } else {
                    "***".to_string()
                };
                debug!("[SACP] 📋   {} = {}", key, masked);
            } else {
                debug!("[SACP] 📋   {} = {}", key, value);
            }
        }

        // 🔧 Windows：将 .cmd/.bat 等规范化为不弹窗的 node.exe + JS 形式（逻辑在 windows_launch 中）
        #[cfg(windows)]
        let (command_path, command_args) =
            normalize_windows_command_for_no_window(command_path, command_args);

        // 启动子进程（使用进程组/Job Object 来管理整个进程树）
        // Unix: ProcessGroup::leader() 创建进程组，确保能够清理所有孙进程
        // Windows: JobObject 管理进程树
        let mut cmd_wrap = CommandWrap::with_new(&command_path, |cmd| {
            cmd.args(&command_args)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .current_dir(&project_path);
            cmd.envs(&merged_envs);
        });

        #[cfg(unix)]
        let mut child = cmd_wrap
            .wrap(ProcessGroup::leader())
            .spawn()
            .context("[SACP] Failed to start claude-code-acp-ts subprocess")?;

        #[cfg(windows)]
        let mut child = cmd_wrap
            .wrap(CreationFlags(PROCESS_CREATION_FLAGS(CREATE_NO_WINDOW_FLAG)))
            .wrap(JobObject)
            .spawn()
            .context("[SACP] Failed to start claude-code-acp-ts subprocess")?;

        #[cfg(not(any(unix, windows)))]
        compile_error!(" message unix message windows message ");

        let child_pid = child.id().unwrap_or(0);
        info!(
            "[SACP] Claude Code ACP child processalreadystarted, PID: {}",
            child_pid
        );

        // 获取 stdio 句柄（process_wrap 使用方法访问 stdio）
        let stdin = take_stdio(&mut child.stdin(), "stdin")?;
        let stdout = take_stdio(&mut child.stdout(), "stdout")?;
        let stderr = take_stdio(&mut child.stderr(), "stderr")?;

        // 创建 SACP transport
        let transport = sacp::ByteStreams::new(stdin.compat_write(), stdout.compat());

        // 🔥 新增：创建共享的异常退出标志
        // 此标志在 reaper_task 检测到子进程异常退出时设置为 true
        // SACP 连接层可以检测此标志并发送相应的错误通知
        let abnormal_exit_flag = Arc::new(AtomicBool::new(false));

        // 克隆用于闭包
        let project_path_clone = project_path.clone();
        let project_id_clone = project_id.clone();
        let cancel_token_clone = cancel_token.clone();
        let notifier_clone = self.notifier.clone();
        let abnormal_exit_flag_clone = abnormal_exit_flag.clone();

        // 🔥 使用标准 tokio::spawn（无需 LocalSet！）
        tokio::spawn(async move {
            let params = SacpConnectionParams {
                project_path: project_path_clone,
                project_id: project_id_clone,
                mcp_servers,
                start_config,
                session_id_tx,
                prompt_rx,
                cancel_rx,
                cancel_token: cancel_token_clone,
                notifier: notifier_clone,
                abnormal_exit_flag: abnormal_exit_flag_clone,
            };
            let result = run_sacp_connection(transport, params).await;

            if let Err(e) = result {
                error!("[SACP] Claude Code ACP Agent connectionfailed: {}", e);
            }
        });

        // 等待会话 ID
        let session_id = session_id_rx.await.map_err(|e| {
            error!("[SACP] message initialize timeout: {}", e);
            anyhow::anyhow!("{}", error_codes::get_i18n_message_default("error.agent_init_timeout"))
        })?;

        info!(
            "[SACP] Claude Code ACP Agent service started successfully, session ID: {}",
            session_id
        );

        // 创建 stderr 任务
        let cancel_token_for_stderr = cancel_token.clone();
        let stderr_task = tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();

            loop {
                tokio::select! {
                                   biased; // 优先检查取消信号

                                   _ = cancel_token_for_stderr.cancelled() => {
                debug!("[SACP] stderr message cancel message ");
                                       break;
                                   }
                                   result = lines.next_line() => {
                                       match result {
                                           Ok(Some(line)) if !line.trim().is_empty() => {
                                               warn!("[SACP] Claude Code Agent stderr: {}", line.trim());
                                           }
                                           Ok(Some(_)) => {} // 空行，忽略
                                           Ok(None) => break, // EOF
                                           Err(e) => {
                error!("[SACP] message stderr failed: {}", e);
                                               break;
                                           }
                                       }
                                   }
                               }
            }
        });

        // 创建生命周期守卫（带异常退出标志）
        let lifecycle_guard = AgentLifecycleGuard::new_claude_with_abnormal_flag(
            project_id.clone(),
            session_id.clone(),
            child,
            stderr_task,
            cancel_token.clone(),
            abnormal_exit_flag,
        );

        Ok(SacpLauncherConnectionInfo {
            session_id,
            prompt_tx,
            cancel_tx,
            lifecycle_guard: Arc::new(lifecycle_guard),
        })
    }
}

/// 从 Option 中取出 stdio 句柄，失败时返回错误
fn take_stdio<T>(opt: &mut Option<T>, name: &str) -> Result<T> {
    opt.take()
        .ok_or_else(|| anyhow::anyhow!("[SACP] Failed to get subprocess {}", name))
}

/// SACP 连接参数（封装 run_sacp_connection 的参数）
struct SacpConnectionParams<N: SessionNotifier> {
    project_path: PathBuf,
    project_id: String,
    mcp_servers: Vec<McpServer>,
    start_config: AgentStartConfig,
    session_id_tx: tokio::sync::oneshot::Sender<SessionId>,
    prompt_rx: mpsc::Receiver<PromptRequest>,
    cancel_rx: mpsc::Receiver<CancelNotificationRequestWrapper>,
    cancel_token: CancellationToken,
    notifier: Arc<N>,
    /// 🔥 新增：共享的异常退出标志（子进程异常退出时设置为 true）
    abnormal_exit_flag: Arc<AtomicBool>,
}

/// 运行 SACP 连接
///
/// 使用 SACP 的 Builder 模式建立连接并处理消息
async fn run_sacp_connection<N: SessionNotifier + 'static>(
    transport: sacp::ByteStreams<
        tokio_util::compat::Compat<tokio::process::ChildStdin>,
        tokio_util::compat::Compat<tokio::process::ChildStdout>,
    >,
    params: SacpConnectionParams<N>,
) -> Result<()> {
    // 解构参数
    let SacpConnectionParams {
        project_path,
        project_id,
        mcp_servers,
        start_config,
        session_id_tx,
        mut prompt_rx,
        mut cancel_rx,
        cancel_token,
        notifier,
        abnormal_exit_flag,
    } = params;

    // 克隆变量供 handlers 使用
    let notifier_for_handlers = notifier.clone();
    let project_id_for_handlers = project_id.clone();
    // 克隆 notifier 和 project_id 供 prompt 结束通知使用
    let notifier_for_prompt_end = notifier.clone();
    let project_id_for_prompt_end = project_id.clone();

    // 使用 SACP Builder 模式
    ClientToAgent::builder()
        .name("rcoder-agent-runner-sacp")
        // 使用容错的消息处理器（处理未类型化消息，手动解析以捕获错误）
        // 注意：必须在类型化 handlers 之前注册，以便作为 fallback
        .on_receive_message(
            {
                async move |msg: sacp::MessageCx<sacp::UntypedMessage, sacp::UntypedMessage>,
                            _cx: JrConnectionCx<ClientToAgent>| {
                    match msg {
                        sacp::MessageCx::Notification(untyped_notif) => {
                            // 提前克隆需要的字段
                            let method = untyped_notif.method.clone();
                            let params = untyped_notif.params.clone();

                            // 尝试解析为 SessionNotification
                            let parse_result = sacp::MessageCx::Notification(untyped_notif)
                                .into_notification::<SessionNotification>();

                            match parse_result {
                                Ok(Ok(notification)) => {
                                    let notifier = notifier_for_handlers.clone();
                                    let project_id = project_id_for_handlers.clone();
                                    // 解析成功，处理通知
                                    handle_session_notification(notification, notifier, project_id).await;
                                    Ok(sacp::Handled::Yes)
                                }
                                Ok(Err(_)) => {
                                    // 方法名不匹配，不是 session/update 通知，跳过
                                    debug!(
                                        method = %method,
                                        "[SACP] Skipping non session/update notification"
                                    );
                                    // 继续传递给其他 handlers
                                    Ok(sacp::Handled::No {
                                        message: sacp::MessageCx::Notification(sacp::UntypedMessage {
                                            method,
                                            params,
                                        }),
                                        retry: false,
                                    })
                                }
                                Err(ref err) => {
                                    // 解析失败（如缺少 data 字段），记录警告但不断开连接
                                    warn!(
                                        ?err,
                                        method = %method,
                                        params = ?params,
                                        "[SACP] SessionNotification parse failed, skipping message but keeping connection"
                                    );
                                    // 跳过此消息但不断开连接
                                    Ok(sacp::Handled::Yes)
                                }
                            }
                        }
                        sacp::MessageCx::Request(request, request_cx) => {
                            // 请求消息继续传递给 RequestPermission handler
                            Ok(sacp::Handled::No {
                                message: sacp::MessageCx::Request(request, request_cx),
                                retry: false,
                            })
                        }
                    }
                }
            },
            sacp::on_receive_message!(),
        )
        // 处理 RequestPermission
        .on_receive_request(
            move |request: RequestPermissionRequest,
                  request_cx: JrRequestCx<RequestPermissionResponse>,
                  _cx: JrConnectionCx<ClientToAgent>| {
                async move { handle_permission_request(request, request_cx).await }
            },
            sacp::on_receive_request!(),
        )
        // 主连接逻辑
        .run_until(transport, move |cx: JrConnectionCx<ClientToAgent>| {
            let project_path = project_path.clone();
            let mcp_servers = mcp_servers.clone();
            let start_config = start_config.clone();
            let notifier_for_prompt = notifier_for_prompt_end.clone();
            let project_id_for_prompt = project_id_for_prompt_end.clone();
            let abnormal_exit_flag = abnormal_exit_flag.clone();

            async move {
                // 1. 初始化连接
 debug!("[SACP] initialize ACP connection...");
                let _init_response = cx
                    .send_request(
                        InitializeRequest::new(VERSION)
                            .client_info(sacp::schema::Implementation::new(
                                "rcoder-agent-runner",
                                env!("CARGO_PKG_VERSION"),
                            )),
                    )
                    .block_task()
                    .await?;
 info!("[SACP] ACP connectioninitializesucceeded");

                // 2. 构建 meta（包含系统提示词和可能的 resume）
                let system_prompt_meta = start_config.build_meta();

                // 3. 创建新会话
 info!("[SACP] created ACP session...");
                let new_session_request = NewSessionRequest::new(project_path.clone())
                    .mcp_servers(mcp_servers.clone())
                    .meta(system_prompt_meta);

                debug!("new_session_request: {:?}", new_session_request);

                // 从配置获取超时值，默认 100 秒
                let timeout_secs = start_config
                    .acp_session_create_timeout_secs
                    .unwrap_or(100);
                let session_response = tokio::time::timeout(
                    tokio::time::Duration::from_secs(timeout_secs),
                    cx.send_request(new_session_request).block_task(),
                )
                .await
                .map_err(|_| {
                    sacp::Error::new(
                        -32000,
                        format!("[SACP] new_session timeout ({}s)", timeout_secs)
                    )
                })??;

                let session_id = session_response.session_id;
 info!("[SACP] ACP sessioncreatedsucceeded, session_id={}", session_id);

                // 发送会话 ID 到主任务
                if session_id_tx.send(session_id.clone()).is_err() {
                    error!("[SACP] unable to send session ID");
                    return Err(sacp::Error::new(
                        -32001,
                        error_codes::get_i18n_message_default("error.sacp_session_id_send_failed"),
                    ));
                }

                // 4. 处理 Prompt 和 Cancel 请求
                loop {
                    tokio::select! {
                        _ = cancel_token.cancelled() => {
                            // 🔥 检测取消原因，区分"正常取消"和"Agent 进程退出"
                            // 注意：如果在 prompt 处理中检测到取消，会在内层 loop 发送通知
                            // 这里只处理"没有正在处理的 prompt"时的情况
                            let is_abnormal = abnormal_exit_flag.load(Ordering::SeqCst);

                            if is_abnormal {
                                // Agent 进程异常退出，发送 SSE 错误通知
                                warn!(
                                    "[SACP] Agent process exited abnormally, sending SSE error notification and disconnecting: project_id={}, session_id={}",
                                    project_id_for_prompt, session_id
                                );
                                if let Err(e) = notifier_for_prompt
                                    .notify_prompt_error(
                                        &project_id_for_prompt,
                                        &session_id.to_string(),
                                        sacp::Error::new(
                                            -32001,
                                            error_codes::get_i18n_message_default("error.agent_process_abnormal_exit"),
                                        ),
                                        None, // request_id 可能已经不可用
                                    )
                                    .await
                                {
 error!("[SACP] send Agent message errornotificationfailed: {:?}", e);
                                } else {
 info!("[SACP] alreadysend Agent message errornotification: project_id={}", project_id_for_prompt);
                                }
                            } else {
                                // 🔥 修复：正常取消时也要发送 PromptEnd，确保状态回退 Idle
                                // 避免 Agent 一直卡在 Active 状态无法回收
                                if let Err(e) = notifier_for_prompt
                                    .notify_prompt_end(
                                        &project_id_for_prompt,
                                        &session_id.to_string(),
                                        sacp::schema::StopReason::Cancelled,
                                        Some(error_codes::get_i18n_message_default("error.session_cancelled")),
                                        None,
                                    )
                                    .await
                                {
 error!("[SACP] send PromptEnd (Cancelled) notificationfailed: {:?}", e);
                                } else {
                                    info!(
                                        "[SACP] Sent PromptEnd (Cancelled) notification, state will revert to Idle: project_id={}, session_id={}",
                                        project_id_for_prompt, session_id
                                    );
                                }
                            }
                            break;
                        }
                        Some(cancel_request) = cancel_rx.recv() => {
                            let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
 info!("[SACP] message cancelrequest: session_id={}", session_id_str);
                            // 构建 SACP 版本的 CancelNotification 并发送到 Agent
                            let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                            let cancel_notification = CancelNotification::new(sacp_session_id);
                            if let Err(e) = cx.send_notification(cancel_notification) {
                                error!("[SACP] send cancel notification failed: {:?}", e);
                                // 通知调用方取消失败
                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                                    format!("Failed to send cancel notification: {:?}", e)
                                ));
                            } else {
 info!("[SACP] cancelnotificationalreadysend");
                                // 通知调用方取消成功
                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                            }
                        }
                        Some(prompt_request) = prompt_rx.recv() => {
 debug!("[SACP] message Prompt request");

                            // 从 meta 中提取 request_id
                            let request_id = prompt_request
                                .meta
                                .as_ref()
                                .and_then(|meta| meta.get("request_id"))
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string());

                            // 🎯 关键修复：通知状态管理器 Agent 开始处理 prompt
                            // 此时状态从 Pending -> Active，确保状态与 agent 实际执行同步
                            let session_id_str = session_id.to_string();
                            if let Err(e) = notifier_for_prompt
                                .notify_prompt_start(
                                    &project_id_for_prompt,
                                    &session_id_str,
                                    request_id.clone(),
                                )
                                .await
                            {
                                error!("[SACP] send PromptStart notification failed: {:?}", e);
                            } else {
                                info!(
                                    "[SACP] PromptStart notification sent: session_id={}, request_id={:?}",
                                    session_id_str, request_id
                                );
                            }

                            // 创建 Prompt 响应的 Future，使用 pin! 来固定它
                            let prompt_future = cx.send_request(prompt_request).block_task();
                            tokio::pin!(prompt_future);

                            // 取消后的超时保护：收到取消请求后最多等待 10 秒
                            let cancel_timeout = tokio::time::sleep(std::time::Duration::from_secs(3600)); // 初始设置一个很长的超时
                            tokio::pin!(cancel_timeout);
                            let mut is_cancelled = false;

                            // 在等待 Prompt 响应时也监听取消请求
                            let prompt_result = loop {
                                tokio::select! {
                                    biased;
                                    // 🔥 监听 cancel_token（Agent 进程退出时会触发）
                                    _ = cancel_token.cancelled() => {
                                        let is_abnormal = abnormal_exit_flag.load(Ordering::SeqCst);
                                        if is_abnormal {
                                            warn!(
                                                "[SACP] Detected Agent process abnormal exit during prompt processing: project_id={}, session_id={}",
                                                project_id_for_prompt, session_id
                                            );
                                            break Err(sacp::Error::new(
                                                -32001,
                                                error_codes::get_i18n_message_default("error.agent_process_abnormal_exit"),
                                            ));
                                        } else {
                                            // 正常取消（用户主动取消或 Agent 正常退出）
                                            info!(
                                                "[SACP] Received cancel signal during prompt processing: project_id={}, session_id={}",
                                                project_id_for_prompt, session_id
                                            );
                                            break Err(sacp::Error::new(
                                                -32002,
                                                error_codes::get_i18n_message_default("error.session_cancelled"),
                                            ));
                                        }
                                    }
                                    // 取消后的超时保护（只有 is_cancelled 为 true 时才有意义）
                                    _ = &mut cancel_timeout, if is_cancelled => {
                                        // 取消后超时，强制返回错误
                                        warn!("[SACP] cancel message Prompt response timeout (10s), force exit");
                                        break Err(sacp::Error::new(
                                            -32001,
                                            error_codes::get_i18n_message_default("error.cancel_response_timeout"),
                                        ));
                                    }
                                    // 检查取消请求（无论是否已取消都要接收，避免调用方超时）
                                    Some(cancel_request) = cancel_rx.recv() => {
                                        if is_cancelled {
                                            // 🎯 已经在取消中，直接返回成功（通知已发送）
                                            info!("[SACP] already sent cancel request, notification succeeded");
                                            let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                                        } else {
                                            let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
                                            info!("[SACP] received Prompt message cancel request: session_id={}", session_id_str);
                                            // 发送取消通知给 Agent
                                            let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                                            let cancel_notification = CancelNotification::new(sacp_session_id);
                                            if let Err(e) = cx.send_notification(cancel_notification) {
                                                error!("[SACP] send cancel notification failed: {:?}", e);
                                                // 发送失败立即返回错误
                                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                                                    format!("Failed to send cancel notification: {:?}", e)
                                                ));
                                            } else {
                                                info!("[SACP] cancel notification already sent");
                                                // 🎯 立即返回成功，不阻塞调用方
                                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                                                is_cancelled = true;
                                                // 设置超时保护：取消后最多等待 10 秒让 prompt 完成
                                                cancel_timeout.as_mut().reset(tokio::time::Instant::now() + std::time::Duration::from_secs(10));
                                            }
                                        }
                                        // 继续等待 Prompt 响应（Agent 应该会因为取消而提前返回）
                                    }
                                    result = &mut prompt_future => {
                                        // Prompt 响应完成
                                        break result;
                                    }
                                }
                            };

                            // 处理 Prompt 响应结果
                            match prompt_result {
                                Ok(response) => {
 debug!("[SACP] Prompt response: stop_reason={:?}", response.stop_reason);
                                    // 发送 PromptEnd 通知
                                    if let Err(e) = notifier_for_prompt
                                        .notify_prompt_end(
                                            &project_id_for_prompt,
                                            &session_id.to_string(),
                                            response.stop_reason,
                                            None,
                                            request_id.clone(),
                                        )
                                        .await
                                    {
                                        error!("[SACP] send PromptEnd notification failed: {:?}", e);
                                    } else {
                                        info!(
                                            "[SACP] PromptEnd notification sent: session_id={}, request_id={:?}",
                                            session_id, request_id
                                        );
                                    }
                                }
                                Err(e) => {
                                    // 🎯 区分"取消超时"和"真正的错误"
                                    if is_cancelled {
                                        // 取消超时：发送 PromptEnd (Cancelled) 而非 PromptError
                                        info!("[SACP] cancel timeout, send PromptEnd (Cancelled): session_id={}", session_id);
                                        if let Err(notify_err) = notifier_for_prompt
                                            .notify_prompt_end(
                                                &project_id_for_prompt,
                                                &session_id.to_string(),
                                                sacp::schema::StopReason::Cancelled,
                                                Some(error_codes::get_i18n_message_default("error.session_cancelled_timeout")),
                                                request_id.clone(),
                                            )
                                            .await
                                        {
                                            error!("[SACP] send PromptEnd (Cancelled) notification failed: {:?}", notify_err);
                                        }
                                    } else {
                                        // 真正的错误：发送 PromptError
 error!("[SACP] Prompt requestfailed: {:?}", e);
                                        if let Err(notify_err) = notifier_for_prompt
                                            .notify_prompt_error(
                                                &project_id_for_prompt,
                                                &session_id.to_string(),
                                                e,
                                                request_id.clone(),
                                            )
                                            .await
                                        {
 error!("[SACP] send PromptError notificationfailed: {:?}", notify_err);
                                        }
                                    }

                                    // 🔥 关键：如果 cancel_token 已取消，直接退出外层 loop
                                    // 避免回到外层 loop 时再次触发 cancel_token.cancelled() 导致重复发送通知
                                    if cancel_token.is_cancelled() {
 info!("[SACP] Prompt message completed message cancel_token alreadycancel, message ");
                                        break;
                                    }
                                }
                            }
                        }
                        else => {
                            // 所有通道已关闭
 info!("[SACP] message alreadyclosed, message ");
                            break;
                        }
                    }
                }

                Ok(())
            }
        })
        .await?;

    Ok(())
}

/// 处理 SessionNotification 回调
async fn handle_session_notification<N: SessionNotifier>(
    notification: SessionNotification,
    notifier: Arc<N>,
    project_id: String,
) {
    let session_id = notification.session_id.to_string();

    debug!(
        "[SACP] SessionNotification: project_id={}, session_id={}, update={:?}",
        project_id, session_id, notification.update
    );

    // 提取 request_id（如果有）
    let request_id = notification
        .meta
        .as_ref()
        .and_then(|meta| meta.get("request_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // 通过 notifier 推送会话更新
    // 注意：sacp::schema::SessionUpdate 与 agent_client_protocol::SessionUpdate 是同一类型
    if let Err(e) = notifier
        .notify_session_update(&project_id, &session_id, notification.update, request_id)
        .await
    {
        error!(
            "[SACP] Push session update failed: project_id={}, session_id={}, error={:?}",
            project_id, session_id, e
        );
    }
}

/// 处理 RequestPermission 回调
async fn handle_permission_request(
    request: RequestPermissionRequest,
    request_cx: JrRequestCx<RequestPermissionResponse>,
) -> Result<(), sacp::Error> {
    debug!("[SACP] message request: {:?}", request);

    // 自动允许：优先选择 AllowAlways，其次 AllowOnce
    let selected = request
        .options
        .iter()
        .find(|o| o.kind == PermissionOptionKind::AllowAlways)
        .or_else(|| {
            request
                .options
                .iter()
                .find(|o| o.kind == PermissionOptionKind::AllowOnce)
        })
        .or_else(|| request.options.first());

    if let Some(option) = selected {
        request_cx.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                option.option_id.clone(),
            )),
        ))
    } else {
        // 无可选项则取消
        request_cx.respond(RequestPermissionResponse::new(
            RequestPermissionOutcome::Cancelled,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_config::ContextServerConfig;

    #[test]
    fn test_default_config() {
        let config = get_default_sacp_agent_config(None, &shared_types::ServiceType::RCoder);
        assert!(config.is_ok());
        let config = config.unwrap();

        // 命令应该是 "claude-code-acp-ts" 或其绝对路径（如果 which crate 能找到）
        // 两种情况都是正确的
        let cmd = &config.command;
        assert!(
            cmd == "claude-code-acp-ts" || cmd.ends_with("claude-code-acp-ts"),
            "Expected command to be 'claude-code-acp-ts' or an absolute path ending with 'claude-code-acp-ts', got: {}",
            cmd
        );
    }

    #[test]
    fn test_default_config_with_model_provider() {
        let provider = ModelProviderConfig {
            id: "test-id".to_string(),
            name: "test-provider".to_string(),
            api_key: "sk-test-key".to_string(),
            base_url: "https://api.test.com".to_string(),
            default_model: "test-model".to_string(),
            requires_openai_auth: false,
            api_protocol: None,
        };

        let config =
            get_default_sacp_agent_config(Some(&provider), &shared_types::ServiceType::RCoder);
        assert!(config.is_ok());
        let config = config.unwrap();

        // 验证 API Key：proxy 模式下为占位符，非 proxy 模式下为真实值
        assert!(config.env.contains_key("ANTHROPIC_API_KEY"));
        if cfg!(feature = "proxy") {
            assert_eq!(
                config.env.get("ANTHROPIC_API_KEY"),
                Some(&API_KEY_PLACEHOLDER.to_string())
            );
        } else {
            assert_eq!(
                config.env.get("ANTHROPIC_API_KEY"),
                Some(&"sk-test-key".to_string())
            );
        }

        // 应该包含模型设置
        assert!(config.env.contains_key("ANTHROPIC_MODEL"));
        assert_eq!(
            config.env.get("ANTHROPIC_MODEL"),
            Some(&"test-model".to_string())
        );
    }

    #[test]
    fn test_default_config_disables_nonessential_traffic() {
        let config = get_default_sacp_agent_config(None, &shared_types::ServiceType::RCoder);
        assert!(config.is_ok());
        let config = config.unwrap();

        assert_eq!(
            config.env.get("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC"),
            Some(&"1".to_string())
        );
    }

    #[test]
    fn test_default_config_with_openai_provider() {
        let provider = ModelProviderConfig {
            id: "test-openai".to_string(),
            name: "openai".to_string(),
            api_key: "sk-test-openai-key".to_string(),
            base_url: "https://api.openai.com/v1".to_string(),
            default_model: "openai-compatible/gpt-4".to_string(), // model_name 已包含前缀
            requires_openai_auth: true,
            api_protocol: Some("openai".to_string()),
        };

        let config =
            get_default_sacp_agent_config(Some(&provider), &shared_types::ServiceType::RCoder);
        assert!(config.is_ok());
        let config = config.unwrap();

        // 验证 OpenAI 环境变量
        assert!(config.env.contains_key("OPENAI_API_KEY"));
        assert!(config.env.contains_key("OPENAI_BASE_URL"));

        if cfg!(feature = "proxy") {
            assert_eq!(
                config.env.get("OPENAI_API_KEY"),
                Some(&API_KEY_PLACEHOLDER.to_string())
            );
            assert_eq!(
                config.env.get("OPENAI_BASE_URL"),
                Some(&DEFAULT_PROXY_BASE_URL.to_string())
            );
        } else {
            assert_eq!(
                config.env.get("OPENAI_API_KEY"),
                Some(&"sk-test-openai-key".to_string())
            );
            assert_eq!(
                config.env.get("OPENAI_BASE_URL"),
                Some(&"https://api.openai.com/v1".to_string())
            );
        }

        // nuwaxcode 使用 OPENCODE_MODEL，直接使用 model_name（已包含 openai-compatible/ 前缀）
        assert!(config.env.contains_key("OPENCODE_MODEL"));
        assert_eq!(
            config.env.get("OPENCODE_MODEL"),
            Some(&"openai-compatible/gpt-4".to_string())
        );

        // 同时验证 Anthropic 环境变量也存在 (兼容性)
        assert!(config.env.contains_key("ANTHROPIC_API_KEY"));
        assert!(config.env.contains_key("ANTHROPIC_BASE_URL"));
    }

    #[test]
    fn test_sensitive_env_vars_protection() {
        // 测试默认配置中的环境变量值
        // proxy 模式下：API_KEY 和 BASE_URL 为占位符/代理 URL
        // 非 proxy 模式下：API_KEY 和 BASE_URL 为真实值
        let provider = ModelProviderConfig {
            id: "test".to_string(),
            name: "test".to_string(),
            api_key: "sk-real-key-should-be-replaced".to_string(),
            base_url: "https://real-url-should-be-replaced.com".to_string(),
            default_model: "openai-compatible/gpt-4".to_string(),
            requires_openai_auth: true,
            api_protocol: Some("openai".to_string()),
        };

        let config =
            get_default_sacp_agent_config(Some(&provider), &shared_types::ServiceType::RCoder);
        assert!(config.is_ok());
        let config = config.unwrap();

        if cfg!(feature = "proxy") {
            // proxy 模式下：敏感变量应该是占位符
            assert_eq!(
                config.env.get("ANTHROPIC_API_KEY"),
                Some(&API_KEY_PLACEHOLDER.to_string())
            );
            assert_eq!(
                config.env.get("OPENAI_API_KEY"),
                Some(&API_KEY_PLACEHOLDER.to_string())
            );
            assert_eq!(
                config.env.get("ANTHROPIC_BASE_URL"),
                Some(&DEFAULT_PROXY_BASE_URL.to_string())
            );
            assert_eq!(
                config.env.get("OPENAI_BASE_URL"),
                Some(&DEFAULT_PROXY_BASE_URL.to_string())
            );
        } else {
            // 非 proxy 模式下：使用真实值
            assert_eq!(
                config.env.get("ANTHROPIC_API_KEY"),
                Some(&"sk-real-key-should-be-replaced".to_string())
            );
            assert_eq!(
                config.env.get("OPENAI_API_KEY"),
                Some(&"sk-real-key-should-be-replaced".to_string())
            );
            assert_eq!(
                config.env.get("ANTHROPIC_BASE_URL"),
                Some(&"https://real-url-should-be-replaced.com".to_string())
            );
            assert_eq!(
                config.env.get("OPENAI_BASE_URL"),
                Some(&"https://real-url-should-be-replaced.com".to_string())
            );
        }
    }

    #[test]
    fn test_convert_context_servers_empty() {
        let configs: HashMap<String, ContextServerConfig> = HashMap::new();
        let servers = convert_context_servers_sacp(&configs);
        assert!(servers.is_empty());
    }

    #[test]
    fn test_convert_context_servers_disabled() {
        let mut configs = HashMap::new();
        configs.insert(
            "disabled-server".to_string(),
            ContextServerConfig {
                source: "local".to_string(),
                enabled: false,
                command: Some("node".to_string()),
                args: None,
                env: None,
            },
        );

        let servers = convert_context_servers_sacp(&configs);
        assert!(servers.is_empty()); // disabled 的服务器应该被过滤
    }

    #[test]
    fn test_convert_context_servers_no_command() {
        let mut configs = HashMap::new();
        configs.insert(
            "no-command-server".to_string(),
            ContextServerConfig {
                source: "local".to_string(),
                enabled: true,
                command: None, // 没有命令
                args: None,
                env: None,
            },
        );

        let servers = convert_context_servers_sacp(&configs);
        assert!(servers.is_empty()); // 没有命令的服务器应该被过滤
    }

    #[test]
    fn test_convert_context_servers_stdio() {
        let mut configs = HashMap::new();
        configs.insert(
            "test-mcp".to_string(),
            ContextServerConfig {
                source: "local".to_string(),
                enabled: true,
                command: Some("node".to_string()),
                args: Some(vec![
                    "server.js".to_string(),
                    "--port".to_string(),
                    "3000".to_string(),
                ]),
                env: Some({
                    let mut env = HashMap::new();
                    env.insert("NODE_ENV".to_string(), "production".to_string());
                    env
                }),
            },
        );

        let servers = convert_context_servers_sacp(&configs);
        assert_eq!(servers.len(), 1);

        // 验证是 Stdio 类型
        match &servers[0] {
            McpServer::Stdio(stdio) => {
                assert_eq!(stdio.name, "test-mcp");
            }
            _ => panic!("Expected Stdio variant"),
        }
    }

    #[test]
    fn test_convert_context_servers_multiple() {
        let mut configs = HashMap::new();
        configs.insert(
            "server1".to_string(),
            ContextServerConfig {
                source: "local".to_string(),
                enabled: true,
                command: Some("node".to_string()),
                args: Some(vec!["server1.js".to_string()]),
                env: None,
            },
        );
        configs.insert(
            "server2".to_string(),
            ContextServerConfig {
                source: "local".to_string(),
                enabled: true,
                command: Some("python".to_string()),
                args: Some(vec!["server2.py".to_string()]),
                env: None,
            },
        );
        configs.insert(
            "disabled".to_string(),
            ContextServerConfig {
                source: "local".to_string(),
                enabled: false,
                command: Some("ruby".to_string()),
                args: None,
                env: None,
            },
        );

        let servers = convert_context_servers_sacp(&configs);
        // 应该只有 2 个 enabled 的服务器
        assert_eq!(servers.len(), 2);
    }

    #[test]
    fn test_sacp_agent_launch_config_fields() {
        let config = SacpAgentLaunchConfig {
            command: "test-cmd".to_string(),
            args: vec!["arg1".to_string(), "arg2".to_string()],
            env: {
                let mut env = HashMap::new();
                env.insert("KEY".to_string(), "VALUE".to_string());
                env
            },
            context_servers: HashMap::new(),
        };

        assert_eq!(config.command, "test-cmd");
        assert_eq!(config.args.len(), 2);
        assert_eq!(config.env.get("KEY"), Some(&"VALUE".to_string()));
        assert!(config.context_servers.is_empty());
    }

    #[test]
    fn test_sacp_agent_launch_config_debug() {
        let config = SacpAgentLaunchConfig {
            command: "test".to_string(),
            args: vec![],
            env: HashMap::new(),
            context_servers: HashMap::new(),
        };

        let debug_str = format!("{:?}", config);
        assert!(debug_str.contains("SacpAgentLaunchConfig"));
        assert!(debug_str.contains("test"));
    }

    // === mcp-proxy convert 诊断参数测试 ===

    #[test]
    fn test_is_mcp_proxy_command_simple() {
        // 简化版只检测精确的命令名
        assert!(is_mcp_proxy_command("mcp-proxy"));
        // 不再检测大小写变体和路径
        assert!(!is_mcp_proxy_command("MCP-PROXY"));
        assert!(!is_mcp_proxy_command("Mcp-Proxy"));
    }

    #[test]
    fn test_is_mcp_proxy_command_not_mcp_proxy() {
        assert!(!is_mcp_proxy_command("node"));
        assert!(!is_mcp_proxy_command("bunx"));
        assert!(!is_mcp_proxy_command("/usr/bin/uvx"));
        assert!(!is_mcp_proxy_command("mcp-proxy-other"));
        // 路径形式不再匹配（简化版）
        assert!(!is_mcp_proxy_command("/usr/local/bin/mcp-proxy"));
        assert!(!is_mcp_proxy_command("C:\\Users\\test\\mcp-proxy.exe"));
    }

    #[test]
    fn test_has_convert_subcommand() {
        assert!(has_convert_subcommand(&["convert".to_string()]));
        assert!(has_convert_subcommand(&[
            "convert".to_string(),
            "http://example.com".to_string()
        ]));
        assert!(has_convert_subcommand(&[
            "--config".to_string(),
            "config.json".to_string(),
            "convert".to_string()
        ]));
    }

    #[test]
    fn test_has_convert_subcommand_no_convert() {
        assert!(!has_convert_subcommand(&[]));
        assert!(!has_convert_subcommand(&["serve".to_string()]));
        assert!(!has_convert_subcommand(&[
            "--config".to_string(),
            "config.json".to_string()
        ]));
    }

    #[test]
    fn test_enhance_mcp_proxy_args_non_mcp_proxy() {
        // 非 mcp-proxy 命令，应该原样返回
        let args = vec!["arg1".to_string(), "arg2".to_string()];
        let result = enhance_mcp_proxy_args("node", args.clone());
        assert_eq!(result, args);
    }

    #[test]
    fn test_enhance_mcp_proxy_args_no_convert() {
        // mcp-proxy 但没有 convert 子命令，应该原样返回
        let args = vec!["serve".to_string()];
        let result = enhance_mcp_proxy_args("mcp-proxy", args.clone());
        assert_eq!(result, args);
    }

    #[test]
    fn test_enhance_mcp_proxy_args_already_has_diagnostic() {
        // 已有 --diagnostic 参数
        let args = vec![
            "convert".to_string(),
            "--diagnostic".to_string(),
            "--log-dir".to_string(),
            "/tmp/logs".to_string(),
        ];
        let result = enhance_mcp_proxy_args("mcp-proxy", args.clone());
        // 应该原样返回，不重复添加
        assert_eq!(result, args);
    }

    #[test]
    fn test_get_mcp_proxy_log_dir_none_when_unset() {
        // 清除环境变量以测试返回 None
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
        }
        let log_dir = get_mcp_proxy_log_dir();
        assert_eq!(log_dir, None);
    }

    #[test]
    fn test_get_mcp_proxy_log_dir_from_env() {
        let custom_dir = "/custom/mcp-proxy-logs";
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::set_var(ENV_MCP_PROXY_LOG_DIR, custom_dir);
        }
        let log_dir = get_mcp_proxy_log_dir();
        assert_eq!(log_dir, Some(custom_dir.to_string()));
        // 清理环境变量
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
        }
    }

    #[test]
    fn test_has_log_dir_arg() {
        // 检测 --log-dir 参数
        assert!(has_log_dir_arg(&[
            "--log-dir".to_string(),
            "/tmp".to_string()
        ]));
        assert!(has_log_dir_arg(&["--log-dir=/tmp".to_string()]));
        assert!(has_log_dir_arg(&[
            "convert".to_string(),
            "--log-dir".to_string()
        ]));

        // 检测 --log-file 参数
        assert!(has_log_dir_arg(&[
            "--log-file".to_string(),
            "/tmp/log.txt".to_string()
        ]));
        assert!(has_log_dir_arg(&["--log-file=/tmp/log.txt".to_string()]));
    }

    #[test]
    fn test_has_log_dir_arg_no_log_args() {
        assert!(!has_log_dir_arg(&[]));
        assert!(!has_log_dir_arg(&["convert".to_string()]));
        assert!(!has_log_dir_arg(&["--diagnostic".to_string()]));
        assert!(!has_log_dir_arg(&[
            "--config".to_string(),
            "config.json".to_string()
        ]));
    }

    #[test]
    fn test_enhance_args_respects_existing_log_dir() {
        // 模拟 debug 日志级别
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::set_var("RUST_LOG", "debug");
            std::env::set_var(ENV_MCP_PROXY_LOG_DIR, "/env/path");
        }

        // 用户已配置 --log-dir，不应覆盖
        let args = vec![
            "convert".to_string(),
            "--log-dir".to_string(),
            "/custom/path".to_string(),
        ];
        let result = enhance_mcp_proxy_args("mcp-proxy", args);

        // 应该只追加 --diagnostic，不重复追加 --log-dir
        assert!(result.contains(&"--diagnostic".to_string()));
        // 只应有一个 --log-dir
        assert_eq!(result.iter().filter(|a| *a == "--log-dir").count(), 1);
        // --log-dir 的值应该是用户配置的 /custom/path
        let log_dir_idx = result.iter().position(|a| a == "--log-dir").unwrap();
        assert_eq!(
            result.get(log_dir_idx + 1),
            Some(&"/custom/path".to_string())
        );

        // 清理环境变量
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::remove_var("RUST_LOG");
            std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
        }
    }

    #[test]
    fn test_enhance_args_respects_existing_log_file() {
        // 模拟 debug 日志级别
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::set_var("RUST_LOG", "debug");
            std::env::set_var(ENV_MCP_PROXY_LOG_DIR, "/env/path");
        }

        // 用户已配置 --log-file，不应追加 --log-dir
        let args = vec![
            "convert".to_string(),
            "--log-file=/custom/file.log".to_string(),
        ];
        let result = enhance_mcp_proxy_args("mcp-proxy", args);

        // 应该只追加 --diagnostic
        assert!(result.contains(&"--diagnostic".to_string()));
        // 不应有 --log-dir
        assert!(!result.iter().any(|a| a == "--log-dir"));

        // 清理环境变量
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::remove_var("RUST_LOG");
            std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
        }
    }

    #[test]
    fn test_enhance_args_adds_log_dir_when_env_set() {
        // 模拟 debug 日志级别和配置了 MCP_PROXY_LOG_DIR
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::set_var("RUST_LOG", "debug");
            std::env::set_var(ENV_MCP_PROXY_LOG_DIR, "/var/log/mcp");
        }

        let args = vec!["convert".to_string()];
        let result = enhance_mcp_proxy_args("mcp-proxy", args);

        // 应该追加 --diagnostic 和 --log-dir
        assert!(result.contains(&"--diagnostic".to_string()));
        assert!(result.contains(&"--log-dir".to_string()));
        assert!(result.contains(&"/var/log/mcp".to_string()));

        // 清理环境变量
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::remove_var("RUST_LOG");
            std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
        }
    }

    #[test]
    fn test_enhance_args_no_log_dir_when_env_unset() {
        // 模拟 debug 日志级别但没有配置 MCP_PROXY_LOG_DIR
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::set_var("RUST_LOG", "debug");
            std::env::remove_var(ENV_MCP_PROXY_LOG_DIR);
        }

        let args = vec!["convert".to_string()];
        let result = enhance_mcp_proxy_args("mcp-proxy", args);

        // 应该只追加 --diagnostic，不应有 --log-dir
        assert!(result.contains(&"--diagnostic".to_string()));
        assert!(!result.iter().any(|a| a == "--log-dir"));

        // 清理环境变量
        // SAFETY: 测试环境中修改环境变量是安全的
        unsafe {
            std::env::remove_var("RUST_LOG");
        }
    }
}
