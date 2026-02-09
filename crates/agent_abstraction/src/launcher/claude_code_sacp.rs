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

use agent_config::{AgentInstallationManager, AgentServersConfig, ContextServerConfig};
use anyhow::{Context, Result};
use process_wrap::tokio::CommandWrap;
#[cfg(unix)]
use process_wrap::tokio::ProcessGroup;
#[cfg(windows)]
use process_wrap::tokio::JobObject;
use shared_types::{ModelProviderConfig, ProjectAndAgentInfo};
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

/// 使用最新协议版本
const VERSION: ProtocolVersion = ProtocolVersion::LATEST;

/// API 密钥占位符（实际密钥由 Pingora 代理注入）
const API_KEY_PLACEHOLDER: &str = "PROXY_MANAGED_KEY";

/// 环境变量键名常量
const ENV_ANTHROPIC_API_KEY: &str = "ANTHROPIC_API_KEY";
const ENV_ANTHROPIC_BASE_URL: &str = "ANTHROPIC_BASE_URL";
const ENV_ANTHROPIC_MODEL: &str = "ANTHROPIC_MODEL";
const ENV_DISABLE_NONESSENTIAL: &str = "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC";
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

    if let Some(agent_config) = config.get_agent("claude-code-acp") {
        debug!("📋 [SACP] 加载默认 Agent 配置: {}", agent_config.agent_id);

        // 检查并安装 agent
        if agent_config.installation.package_name.is_some() {
            let installation_manager = AgentInstallationManager::new();
            match installation_manager
                .ensure_installed(&agent_config.installation, &agent_config.command)
                .await
            {
                Ok(result) => {
                    if result.already_installed {
                        debug!("[SACP] Agent 已安装: {}", agent_config.command);
                    } else {
                        info!("✅ [SACP] Agent 安装成功: {}", result.message);
                    }
                }
                Err(e) => {
                    warn!("⚠️ [SACP] Agent 自动安装失败: {}，尝试继续启动", e);
                }
            }
        }

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

        Ok(SacpAgentLaunchConfig {
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            env: resolved_env,
            context_servers: config.context_servers.clone(),
        })
    } else {
        warn!("⚠️ [SACP] 配置中未找到 claude-code-acp，使用默认配置");
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
            env.insert(
                ENV_ANTHROPIC_API_KEY.to_string(),
                resolved_key.clone(),
            );
        }
        if !provider.base_url.is_empty() {
            env.insert(
                ENV_ANTHROPIC_BASE_URL.to_string(),
                resolved_url.clone(),
            );
        }
        if !provider.default_model.is_empty() {
            env.insert(
                ENV_ANTHROPIC_MODEL.to_string(),
                provider.default_model.clone(),
            );
        }

        // OpenAI 环境变量 (支持 OpenAI 兼容的 Agent)
        if !provider.api_key.is_empty() {
            env.insert(
                ENV_OPENAI_API_KEY.to_string(),
                resolved_key,
            );
        }
        if !provider.base_url.is_empty() {
            env.insert(
                ENV_OPENAI_BASE_URL.to_string(),
                resolved_url,
            );
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

    Ok(SacpAgentLaunchConfig {
        command: "claude-code-acp".to_string(),
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

            if let Some(args) = &c.args {
                server = server.args(args.clone());
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
                        "🔧 [SACP] 已替换自定义环境变量模板, model={}",
                        provider.default_model
                    );
                }

                info!(
                    "🎯 [SACP] 使用自定义 Agent: agent_id={}, command={} {:?}",
                    agent_server_override.get_agent_id(),
                    cmd,
                    args
                );
                (cmd, args, env)
            } else {
                // 使用默认配置
                info!(
                    "📋 [SACP] 使用默认 Agent: {} {:?}",
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

        info!("[SACP] 项目工作目录: {}", &project_path.to_string_lossy());

        // 准备 MCP 服务器
        let mcp_servers = if start_config.has_mcp_servers() {
            info!("📦 [SACP] 使用 AgentStartConfig 中的 MCP 服务器");
            start_config.mcp_servers.clone()
        } else if !default_agent_config.context_servers.is_empty() {
            info!("📦 [SACP] 使用配置文件中的 MCP 服务器");
            convert_context_servers_sacp(&default_agent_config.context_servers)
        } else {
            info!("📝 [SACP] 未配置 MCP 服务器");
            Vec::new()
        };

        // 准备环境变量（在 base_env 基础上添加项目相关变量）
        let mut merged_envs = base_env;
        merged_envs.insert(
            ENV_AGENT_WORKING_DIR.to_string(),
            project_path.to_string_lossy().to_string(),
        );
        merged_envs.insert(ENV_AGENT_PROJECT_ID.to_string(), project_id.clone());

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

                debug!("[SACP] 🔒 已强制替换敏感环境变量为占位符/代理 URL");
            }
        } else {
            debug!("[SACP] 🔓 非代理模式，使用真实 API Key 和 Base URL");
        }

        // 🔍 打印传递给 Agent 的完整环境变量（用于调试）
        // 注意：敏感字段（API Key）需要脱敏处理，防止日志泄露
        debug!(
            "[SACP] 📋 启动 Agent 命令: {} {:?}",
            command_path, command_args
        );
        debug!("[SACP] 📋 工作目录: {:?}", project_path);
        info!(
            "[SACP] 📋 传递给 Agent 的环境变量 ({} 个):",
            merged_envs.len()
        );

        // 需要脱敏的环境变量 key 列表
        const SENSITIVE_ENV_KEYS: &[&str] = &[
            ENV_ANTHROPIC_API_KEY,
            ENV_OPENAI_API_KEY,
        ];

        // 按字母顺序排序并打印所有环境变量
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
                info!("[SACP] 📋   {} = {}", key, masked);
            } else {
                info!("[SACP] 📋   {} = {}", key, value);
            }
        }

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
            .context("[SACP] 无法启动 claude-code-acp 子进程")?;

        #[cfg(windows)]
        let mut child = cmd_wrap
            .wrap(JobObject::new())
            .spawn()
            .context("[SACP] 无法启动 claude-code-acp 子进程")?;

        #[cfg(not(any(unix, windows)))]
        compile_error!("仅支持 unix 和 windows 平台");

        let child_pid = child.id().unwrap_or(0);
        info!("[SACP] Claude Code ACP 子进程已启动，PID: {}", child_pid);

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
                error!("[SACP] Claude Code ACP Agent 连接失败: {}", e);
            }
        });

        // 等待会话 ID
        let session_id = session_id_rx.await.map_err(|e| {
            error!("[SACP] 智能体初始化超时: {}", e);
            anyhow::anyhow!("智能体初始化超时，请重试（过多的MCP可能导致超时）。如果持续失败请重启智能体电脑（点击PC端右上图标展开后，在[...]里点击重启智能体电脑）")
        })?;

        info!(
            "[SACP] Claude Code ACP Agent 服务启动完成，会话 ID: {}",
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
                        debug!("[SACP] stderr 读取任务收到取消信号");
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
                                error!("[SACP] 读取 stderr 失败: {}", e);
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
        .ok_or_else(|| anyhow::anyhow!("[SACP] 无法获取子进程 {}", name))
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
                                        "[SACP] 跳过非 session/update 通知"
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
                                        "[SACP] SessionNotification 解析失败，跳过此消息但保持连接"
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
                debug!("[SACP] 初始化 ACP 连接...");
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
                info!("[SACP] ACP 连接初始化成功");

                // 2. 构建 meta（包含系统提示词和可能的 resume）
                let system_prompt_meta = start_config.build_meta();

                // 3. 创建新会话
                info!("[SACP] 创建 ACP 会话...");
                let new_session_request = NewSessionRequest::new(project_path.clone())
                    .mcp_servers(mcp_servers.clone())
                    .meta(system_prompt_meta);

                info!("new_session_request: {:?}", new_session_request);

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
                        format!("[SACP] new_session 超时 ({}s)", timeout_secs)
                    )
                })??;

                let session_id = session_response.session_id;
                info!("[SACP] ACP 会话创建成功, session_id={}", session_id);

                // 发送会话 ID 到主任务
                if session_id_tx.send(session_id.clone()).is_err() {
                    error!("[SACP] 无法发送会话 ID");
                    return Err(sacp::Error::new(-32001, "[SACP] 无法发送会话 ID"));
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
                                    "[SACP] Agent 进程异常退出，发送 SSE 错误通知并断开连接: project_id={}, session_id={}",
                                    project_id_for_prompt, session_id
                                );
                                if let Err(e) = notifier_for_prompt
                                    .notify_prompt_error(
                                        &project_id_for_prompt,
                                        &session_id.to_string(),
                                        sacp::Error::new(-32001, "Agent 进程异常退出，请重试"),
                                        None, // request_id 可能已经不可用
                                    )
                                    .await
                                {
                                    error!("[SACP] 发送 Agent 退出错误通知失败: {:?}", e);
                                } else {
                                    info!("[SACP] 已发送 Agent 退出错误通知: project_id={}", project_id_for_prompt);
                                }
                            } else {
                                // 🔥 修复：正常取消时也要发送 PromptEnd，确保状态回退 Idle
                                // 避免 Agent 一直卡在 Active 状态无法回收
                                if let Err(e) = notifier_for_prompt
                                    .notify_prompt_end(
                                        &project_id_for_prompt,
                                        &session_id.to_string(),
                                        sacp::schema::StopReason::Cancelled,
                                        Some("用户取消任务".to_string()),
                                        None,
                                    )
                                    .await
                                {
                                    error!("[SACP] 发送 PromptEnd (Cancelled) 通知失败: {:?}", e);
                                } else {
                                    info!(
                                        "[SACP] 已发送 PromptEnd (Cancelled) 通知，状态将回退 Idle: project_id={}, session_id={}",
                                        project_id_for_prompt, session_id
                                    );
                                }
                            }
                            break;
                        }
                        Some(cancel_request) = cancel_rx.recv() => {
                            let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
                            info!("[SACP] 收到取消请求: session_id={}", session_id_str);
                            // 构建 SACP 版本的 CancelNotification 并发送到 Agent
                            let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                            let cancel_notification = CancelNotification::new(sacp_session_id);
                            if let Err(e) = cx.send_notification(cancel_notification) {
                                error!("[SACP] 发送取消通知失败: {:?}", e);
                                // 通知调用方取消失败
                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                                    format!("发送取消通知失败: {:?}", e)
                                ));
                            } else {
                                info!("[SACP] 取消通知已发送");
                                // 通知调用方取消成功
                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                            }
                        }
                        Some(prompt_request) = prompt_rx.recv() => {
                            debug!("[SACP] 处理 Prompt 请求");

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
                                error!("[SACP] 发送 PromptStart 通知失败: {:?}", e);
                            } else {
                                info!(
                                    "[SACP] PromptStart 通知已发送: session_id={}, request_id={:?}",
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
                                                "[SACP] Prompt 处理中检测到 Agent 进程异常退出: project_id={}, session_id={}",
                                                project_id_for_prompt, session_id
                                            );
                                            break Err(sacp::Error::new(-32001, "Agent 进程异常退出，请重试"));
                                        } else {
                                            // 正常取消（用户主动取消或 Agent 正常退出）
                                            info!(
                                                "[SACP] Prompt 处理中收到取消信号: project_id={}, session_id={}",
                                                project_id_for_prompt, session_id
                                            );
                                            break Err(sacp::Error::new(-32002, "会话已取消"));
                                        }
                                    }
                                    // 取消后的超时保护（只有 is_cancelled 为 true 时才有意义）
                                    _ = &mut cancel_timeout, if is_cancelled => {
                                        // 取消后超时，强制返回错误
                                        warn!("[SACP] 取消后等待 Prompt 响应超时 (10s)，强制返回");
                                        break Err(sacp::Error::new(-32001, "取消后等待响应超时"));
                                    }
                                    // 检查取消请求（无论是否已取消都要接收，避免调用方超时）
                                    Some(cancel_request) = cancel_rx.recv() => {
                                        if is_cancelled {
                                            // 🎯 已经在取消中，直接返回成功（通知已发送）
                                            info!("[SACP] 已有取消请求在处理中，直接返回成功");
                                            let _ = cancel_request.result_tx.send(shared_types::CancelResult::Success);
                                        } else {
                                            let session_id_str = cancel_request.cancel_notification.session_id.0.to_string();
                                            info!("[SACP] 在 Prompt 处理中收到取消请求: session_id={}", session_id_str);
                                            // 发送取消通知给 Agent
                                            let sacp_session_id = SessionId::new(Arc::from(session_id_str.as_str()));
                                            let cancel_notification = CancelNotification::new(sacp_session_id);
                                            if let Err(e) = cx.send_notification(cancel_notification) {
                                                error!("[SACP] 发送取消通知失败: {:?}", e);
                                                // 发送失败立即返回错误
                                                let _ = cancel_request.result_tx.send(shared_types::CancelResult::Failed(
                                                    format!("发送取消通知失败: {:?}", e)
                                                ));
                                            } else {
                                                info!("[SACP] 取消通知已发送");
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
                                    debug!("[SACP] Prompt 响应: stop_reason={:?}", response.stop_reason);
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
                                        error!("[SACP] 发送 PromptEnd 通知失败: {:?}", e);
                                    } else {
                                        info!(
                                            "[SACP] PromptEnd 通知已发送: session_id={}, request_id={:?}",
                                            session_id, request_id
                                        );
                                    }
                                }
                                Err(e) => {
                                    // 🎯 区分"取消超时"和"真正的错误"
                                    if is_cancelled {
                                        // 取消超时：发送 PromptEnd (Cancelled) 而非 PromptError
                                        info!("[SACP] 取消超时，发送 PromptEnd (Cancelled): session_id={}", session_id);
                                        if let Err(notify_err) = notifier_for_prompt
                                            .notify_prompt_end(
                                                &project_id_for_prompt,
                                                &session_id.to_string(),
                                                sacp::schema::StopReason::Cancelled,
                                                Some("用户取消任务（Agent 响应超时）".to_string()),
                                                request_id.clone(),
                                            )
                                            .await
                                        {
                                            error!("[SACP] 发送 PromptEnd (Cancelled) 通知失败: {:?}", notify_err);
                                        }
                                    } else {
                                        // 真正的错误：发送 PromptError
                                        error!("[SACP] Prompt 请求失败: {:?}", e);
                                        if let Err(notify_err) = notifier_for_prompt
                                            .notify_prompt_error(
                                                &project_id_for_prompt,
                                                &session_id.to_string(),
                                                e,
                                                request_id.clone(),
                                            )
                                            .await
                                        {
                                            error!("[SACP] 发送 PromptError 通知失败: {:?}", notify_err);
                                        }
                                    }

                                    // 🔥 关键：如果 cancel_token 已取消，直接退出外层 loop
                                    // 避免回到外层 loop 时再次触发 cancel_token.cancelled() 导致重复发送通知
                                    if cancel_token.is_cancelled() {
                                        info!("[SACP] Prompt 处理完成且 cancel_token 已取消，退出");
                                        break;
                                    }
                                }
                            }
                        }
                        else => {
                            // 所有通道已关闭
                            info!("[SACP] 通道已关闭，退出");
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
            "[SACP] 推送会话更新失败: project_id={}, session_id={}, error={:?}",
            project_id, session_id, e
        );
    }
}

/// 处理 RequestPermission 回调
async fn handle_permission_request(
    request: RequestPermissionRequest,
    request_cx: JrRequestCx<RequestPermissionResponse>,
) -> Result<(), sacp::Error> {
    debug!("[SACP] 权限请求: {:?}", request);

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
        assert_eq!(config.command, "claude-code-acp");
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
}
