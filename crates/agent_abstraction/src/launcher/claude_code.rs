//! Claude Code ACP Agent 启动器
//!
//! 提供 Claude Code Agent 的启动逻辑，从 agent_runner 迁移而来。
//! 使用泛型 SessionNotifier 和 Client 替代直接依赖，提供更好的解耦。

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use dashmap::DashMap;

use agent_client_protocol::{
    Agent, Client, ClientSideConnection, Implementation, InitializeRequest, McpServer,
    McpServerStdio, NewSessionRequest, PromptRequest, SessionId,
};
use agent_config::{AgentInstallationManager, AgentServersConfig, ContextServerConfig};
use anyhow::{Context, Result};
use shared_types::{AgentLifecycle, ModelProviderConfig, ProjectAndAgentInfo};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::acp::CancelNotificationRequestWrapper;
use crate::traits::AgentStartConfig;
use crate::traits::session_notifier::SessionNotifier;
use crate::traits::session_registry::SessionRegistry;

// 导入生命周期管理
use super::lifecycle::AgentLifecycleGuard;
use super::channel::PromptHandlerConfig;

/// 使用默认版本
const VERSION: agent_client_protocol::ProtocolVersion =
    agent_client_protocol::ProtocolVersion::LATEST;

/// Agent 配置参数
#[derive(Debug, Clone)]
pub struct AgentLaunchConfig {
    /// 命令路径
    pub command: String,
    /// 命令参数
    pub args: Vec<String>,
    /// 环境变量
    pub env: HashMap<String, String>,
    /// Context 服务器配置 (MCP servers)
    pub context_servers: HashMap<String, ContextServerConfig>,
}

/// Agent 连接信息
#[derive(Debug)]
pub struct LauncherConnectionInfo {
    /// 会话 ID
    pub session_id: SessionId,
    /// 发送 Prompt 消息的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 发送取消请求的通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// 取消令牌
    pub cancel_token: CancellationToken,
    /// 子进程
    pub child: tokio::process::Child,
    /// stderr 任务句柄
    pub stderr_task: tokio::task::JoinHandle<()>,
}

/// Agent 连接信息（完整版，包含生命周期守卫）
///
/// 这是推荐使用的返回类型，包含了封装好的 AgentLifecycleGuard，
/// 提供 RAII 自动资源清理机制。
///
/// 注意：降级处理已移至 launch() 的 spawn_local 块内，
/// 通过 tokio::select! 在 LocalSet 中直接处理，避免跨线程问题。
pub struct LauncherConnectionInfoComplete {
    /// 会话 ID
    pub session_id: SessionId,
    /// 发送 Prompt 消息的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 发送取消请求的通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequestWrapper>,
    /// 生命周期守卫（自动清理资源）
    pub lifecycle_guard: Arc<AgentLifecycleGuard>,
}

/// 从配置文件加载 Agent 配置
///
/// 优先加载嵌入的JSON配置文件，如果加载失败则使用默认配置
/// 同时检查并自动安装 agent（如果需要）
pub async fn load_agent_config(
    model_provider: Option<&ModelProviderConfig>,
    service_type: &shared_types::ServiceType,
) -> Result<AgentLaunchConfig> {
    // 根据服务类型加载对应的配置
    let config = AgentServersConfig::load_or_default_for_service(service_type).await;

    // 获取 claude-code-acp 配置
    if let Some(agent_config) = config.get_agent("claude-code-acp") {
        info!("📋 从配置加载 Agent 参数: {}", agent_config.agent_id);

        // 检查并安装 agent（如果有 installation 配置且配置了 package_name）
        if agent_config.installation.package_name.is_some() {
            let installation_manager = AgentInstallationManager::new();
            match installation_manager
                .ensure_installed(&agent_config.installation, &agent_config.command)
                .await
            {
                Ok(result) => {
                    if result.already_installed {
                        debug!("Agent 已安装: {}", agent_config.command);
                    } else {
                        info!("✅ Agent 安装成功: {}", result.message);
                    }
                }
                Err(e) => {
                    warn!("⚠️ Agent 自动安装失败: {}，尝试继续启动", e);
                    // 不阻止启动，可能命令已经在 PATH 中了
                }
            }
        }

        // 解析环境变量占位符
        let mut resolved_env = agent_config.env.clone();

        if let Some(provider) = model_provider {
            // 解析占位符
            for (key, value) in resolved_env.iter_mut() {
                // ⚠️ 关键安全机制：ANTHROPIC_API_KEY 和 ANTHROPIC_BASE_URL 使用占位符值
                //
                // Agent 应该使用占位符密钥和代理 URL，真实密钥由 Pingora 注入：
                // - ANTHROPIC_API_KEY: sk-placeholder
                // - ANTHROPIC_BASE_URL: http://localhost:8088/api/{SERVICE_UUID}
                //
                // 这两个变量不进行替换，保持为占位符或代理URL
                // 其他变量正常替换 MODEL_PROVIDER_* 占位符
                if key == "ANTHROPIC_API_KEY" || key == "ANTHROPIC_BASE_URL" {
                    // 保持原值不处理，后面会单独处理这两个变量
                    continue;
                }

                // 其他环境变量正常替换
                *value = value
                    .replace("{MODEL_PROVIDER_API_KEY}", &provider.api_key)
                    .replace("{MODEL_PROVIDER_BASE_URL}", &provider.base_url)
                    .replace("{MODEL_PROVIDER_DEFAULT_MODEL}", &provider.default_model)
                    .replace("{MODEL_PROVIDER_NAME}", &provider.name);
            }

            // 单独处理 ANTHROPIC_API_KEY 和 ANTHROPIC_BASE_URL
            // 检查并设置占位符值
            for (key, value) in resolved_env.iter_mut() {
                if key == "ANTHROPIC_API_KEY" {
                    if value.contains("{MODEL_PROVIDER_API_KEY}") || value.is_empty() {
                        *value = "sk-placeholder".to_string();
                        debug!("🔒 设置 ANTHROPIC_API_KEY 为占位符密钥");
                    }
                } else if key == "ANTHROPIC_BASE_URL" {
                    if value.contains("{MODEL_PROVIDER_BASE_URL}") || value.is_empty() {
                        *value = "http://localhost:8088/api/{SERVICE_UUID}".to_string();
                        debug!("🔒 设置 ANTHROPIC_BASE_URL 为代理 URL");
                    }
                }
            }
        }

        // 🔒 禁用 Claude Code 非必要网络请求（遥测等）
        resolved_env.insert(
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
            "1".to_string(),
        );
        debug!("🔒 已禁用 Claude Code 遥测功能");

        Ok(AgentLaunchConfig {
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            env: resolved_env,
            context_servers: config.context_servers.clone(),
        })
    } else {
        // 配置中没有找到，使用默认值
        warn!("⚠️ 配置中未找到 claude-code-acp，使用默认配置");
        get_default_agent_config(model_provider, service_type)
    }
}

/// 获取默认的 Agent 配置（后备方案）
pub fn get_default_agent_config(
    model_provider: Option<&ModelProviderConfig>,
    service_type: &shared_types::ServiceType,
) -> Result<AgentLaunchConfig> {
    let env = if let Some(provider) = model_provider {
        let mut env = HashMap::new();
        // ⚠️ 关键：使用占位符密钥和代理URL，而不是真实值
        // Agent 应该使用占位符，真实密钥由 Pingora 代理注入
        if !provider.api_key.is_empty() {
            env.insert(
                "ANTHROPIC_API_KEY".to_string(),
                "sk-placeholder".to_string(),
            );
        }
        if !provider.base_url.is_empty() {
            env.insert(
                "ANTHROPIC_BASE_URL".to_string(),
                "http://localhost:8088/api/{SERVICE_UUID}".to_string(),
            );
        }
        if !provider.default_model.is_empty() {
            env.insert(
                "ANTHROPIC_MODEL".to_string(),
                provider.default_model.clone(),
            );
        }
        env.insert("RUST_LOG".to_string(), "info".to_string());
        // 🔒 禁用 Claude Code 非必要网络请求（遥测等）
        env.insert(
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
            "1".to_string(),
        );
        env
    } else {
        let mut env = HashMap::new();
        env.insert("RUST_LOG".to_string(), "info".to_string());
        // 🔒 禁用 Claude Code 非必要网络请求（遥测等）
        env.insert(
            "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC".to_string(),
            "1".to_string(),
        );
        env
    };

    Ok(AgentLaunchConfig {
        command: "claude-code-acp".to_string(),
        args: Vec::new(),
        env,
        context_servers: HashMap::new(), // 默认配置不包含 context servers
    })
}

/// 将配置中的 Context 服务器转换为 ACP 协议的 McpServer
pub fn convert_context_servers(configs: &HashMap<String, ContextServerConfig>) -> Vec<McpServer> {
    // 🔧 关键修复：从系统环境或 /tmp/dbus-session-env 文件读取 D-Bus 会话地址
    // 这对于容器内的输入法支持（fcitx5）至关重要
    let dbus_address = std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().or_else(|| {
        // 尝试从文件读取
        std::fs::read_to_string("/tmp/dbus-session-env")
            .ok()
            .and_then(|content| {
                // 解析格式：DBUS_SESSION_BUS_ADDRESS='unix:path=...';
                content
                    .lines()
                    .find(|line| line.starts_with("DBUS_SESSION_BUS_ADDRESS="))
                    .and_then(|line| {
                        line.split_once('=').map(|(_, val)| {
                            // 正确的清理顺序：先 trim 空格，再去分号，最后去引号
                            val.trim()
                                .trim_end_matches(';')
                                .trim_matches('\'')
                                .trim_matches('"')
                                .to_string()
                        })
                    })
            })
    });

    if let Some(ref addr) = dbus_address {
        tracing::debug!("✓ MCP 服务器将使用 D-Bus 会话地址: {}", addr);
    } else {
        tracing::warn!("⚠️  未找到 DBUS_SESSION_BUS_ADDRESS，输入法可能无法工作");
    }

    configs
        .iter()
        .filter(|(_, c)| c.enabled)
        .filter_map(|(name, c)| {
            let command = c.command.as_ref()?;
            let mut server = McpServerStdio::new(name, PathBuf::from(command));

            // 添加参数
            if let Some(args) = &c.args {
                server = server.args(args.clone());
            }

            // 添加环境变量
            let mut env_vars: Vec<agent_client_protocol::EnvVariable> = if let Some(env) = &c.env {
                env.iter()
                    .map(|(k, v)| agent_client_protocol::EnvVariable::new(k.clone(), v.clone()))
                    .collect()
            } else {
                Vec::new()
            };

            // 🔧 关键修复：自动注入 DBUS_SESSION_BUS_ADDRESS（如果尚未设置）
            if let Some(ref addr) = dbus_address {
                if !env_vars
                    .iter()
                    .any(|e| e.name == "DBUS_SESSION_BUS_ADDRESS")
                {
                    env_vars.push(agent_client_protocol::EnvVariable::new(
                        "DBUS_SESSION_BUS_ADDRESS".to_string(),
                        addr.clone(),
                    ));
                    tracing::debug!("✓ 为 MCP 服务器 '{}' 注入 DBUS_SESSION_BUS_ADDRESS", name);
                }
            }

            if !env_vars.is_empty() {
                server = server.env(env_vars);
            }

            Some(McpServer::Stdio(server))
        })
        .collect()
}

/// Claude Code ACP Agent 启动器
///
/// 提供启动 Claude Code Agent 子进程和建立 ACP 连接的功能。
/// 使用泛型 SessionNotifier 和 Client 替代直接依赖，提供更好的解耦。
///
/// # 类型参数
/// - `N`: SessionNotifier 实现，用于推送 SSE 消息
pub struct ClaudeCodeLauncher<N: SessionNotifier> {
    /// 会话通知器
    notifier: Arc<N>,
}

impl<N: SessionNotifier + 'static> ClaudeCodeLauncher<N> {
    /// 创建新的启动器
    pub fn new(notifier: Arc<N>) -> Self {
        Self { notifier }
    }

    /// 启动 Claude Code ACP Agent 服务
    ///
    /// # 参数
    /// - `project_id`: 项目 ID
    /// - `project_path`: 项目工作目录
    /// - `model_provider`: 模型提供商配置
    /// - `start_config`: Agent 启动配置（包含系统提示词、resume_session_id 等）
    /// - `client`: ACP 客户端实现
    /// - `registry`: 会话注册表（用于降级时更新）
    /// - `shared_api_key_manager`: 共享的 API 密钥管理器（用于自动清理）
    /// - `project_uuid_map`: project_id -> service_uuid 映射（用于清理时查找）
    /// - `service_uuid`: 与此 Agent 关联的唯一 UUID
    ///
    /// # Resume 机制
    /// 如果需要恢复会话，通过 `start_config.resume_session_id` 传递 session_id，
    /// 会自动构建 `_meta.claudeCode.options.resume` 结构传递给 Agent。
    /// 当 resume 失败时，会在 Prompt 处理器内部自动降级并更新 registry。
    ///
    /// # 返回值
    /// 返回 LauncherConnectionInfoComplete，包含会话信息和生命周期守卫
    pub async fn launch<C: Client + 'static, R: SessionRegistry + 'static>(
        &self,
        project_id: String,
        project_path: PathBuf,
        model_provider: Option<ModelProviderConfig>,
        start_config: AgentStartConfig,
        client: C,
        registry: Arc<R>,
        shared_api_key_manager: Option<Arc<DashMap<String, shared_types::ModelProviderConfig>>>,
        project_uuid_map: Option<Arc<DashMap<String, String>>>,
        service_uuid: Option<String>,
    ) -> Result<LauncherConnectionInfoComplete>
    where
        R::Entry: Into<ProjectAndAgentInfo> + From<ProjectAndAgentInfo>,
    {
        // 从配置加载 Agent 参数（传递 service_type）
        let agent_config =
            load_agent_config(model_provider.as_ref(), &start_config.service_type).await?;
        let command_path = &agent_config.command;
        let command_args = &agent_config.args;
        info!("Claude Code ACP 命令: {} {:?}", command_path, command_args);

        // 创建通道
        let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<CancelNotificationRequestWrapper>();
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();
        let (session_id_tx, session_id_rx) = tokio::sync::oneshot::channel::<SessionId>();

        // 创建 CancellationToken
        let cancel_token = CancellationToken::new();

        // 克隆用于闭包
        let project_path_for_closure = project_path.clone();
        let project_id_for_child = project_id.clone();
        let cancel_token_for_closure = cancel_token.clone();
        let notifier = self.notifier.clone();

        info!(
            "项目工作目录: {}",
            &project_path_for_closure.to_string_lossy()
        );

        // 优先使用 start_config 中的 MCP 服务器（worker 层已处理 HTTP 覆盖）
        let mcp_servers = if start_config.has_mcp_servers() {
            info!("📦 使用 AgentStartConfig 中的 MCP 服务器（支持 HTTP 覆盖）");
            start_config.mcp_servers.clone()
        } else if !agent_config.context_servers.is_empty() {
            // 回退方案：兼容未通过 worker 层调用的场景（如直接调用 launcher）
            // 注意：通过 acp_worker 的正常流程不会走到这里，因为 worker 层已处理默认配置
            info!("📦 使用配置文件中的 MCP 服务器（回退方案）");
            convert_context_servers(&agent_config.context_servers)
        } else {
            info!("📝 未配置 MCP 服务器");
            Vec::new()
        };

        // 🆕 不在这里构建 system_prompt_meta，而是在 initialize() 成功后根据 list_sessions 检查结果动态构建
        // 需要将 start_config 传递到闭包内部

        // 启动子进程
        let spawn_args = command_args.clone();
        let mut merged_envs = agent_config.env.clone();
        // 添加工作目录环境变量，方便 agent 获取当前项目路径
        merged_envs.insert(
            "AGENT_WORKING_DIR".to_string(),
            project_path_for_closure.to_string_lossy().to_string(),
        );
        // 添加项目 ID 环境变量
        merged_envs.insert("AGENT_PROJECT_ID".to_string(), project_id.clone());

        // 🔥 将 UUID 注入到环境变量中（替换 {SERVICE_UUID} 占位符）
        // 注意：agent_config.env 已经包含 {SERVICE_UUID} 占位符（来自 docker_manager）
        // 这里直接替换占位符为实际 UUID 值
        if let Some(ref uuid) = service_uuid {
            for (_key, value) in merged_envs.iter_mut() {
                *value = value.replace("{SERVICE_UUID}", uuid);
            }
        }
        let mut child = tokio::process::Command::new(command_path)
            .args(&spawn_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .current_dir(&project_path_for_closure)
            .envs(merged_envs)
            .spawn()
            .context("无法启动 claude-code-acp 子进程")?;

        let child_pid = child.id().unwrap_or(0);
        info!("Claude Code ACP 子进程已启动，PID: {}", child_pid);

        // 获取 stdio 句柄
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取子进程 stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取子进程 stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("无法获取子进程 stderr"))?;

        // 创建兼容的流
        let outgoing = stdin.compat_write();
        let incoming = stdout.compat();

        // 创建连接（使用传入的 client）
        let (client_conn, handle_io) =
            ClientSideConnection::new(client, outgoing, incoming, |fut| {
                tokio::task::spawn_local(fut);
            });

        // 启动 I/O 处理任务
        tokio::task::spawn_local(handle_io);

        let client_conn = Arc::new(client_conn);

        // 克隆用于 Prompt 处理器的降级配置
        let registry_for_handler = registry.clone();
        let cancel_tx_for_handler = cancel_tx.clone();
        let model_provider_for_handler = model_provider.clone();

        // ⚠️ 重要: 启动后台任务来管理 ACP 连接
        //
        // 调用链说明:
        //   main.rs:301 (LocalSet::new())
        //     → local_set.run_until(async move {
        //         → agent_worker_with_heartbeat() (acp_agent.rs:233)
        //           → worker.process_request()
        //             → launcher.launch() (此处)
        //       })
        //
        // 因此此函数已经在 LocalSet 上下文中,可以直接使用 spawn_local
        // 不需要创建嵌套的 LocalSet
        tokio::task::spawn_local(async move {
            let result = async move {
                let client_conn = client_conn.clone();

                // 初始化连接
                debug!("初始化 ACP 连接[initialize]");
                let init_result = client_conn
                    .initialize(
                        InitializeRequest::new(VERSION).client_info(
                            Implementation::new(
                                "rcoder-agent-runner",
                                env!("CARGO_PKG_VERSION"),
                            )
                            .title("RCoder Agent Runner"),
                        ),
                    )
                    .await;

                match init_result {
                    Ok(_) => {
                        info!("ACP 连接初始化成功");
                    }
                    Err(e) => {
                        error!("ACP 连接初始化失败: {:?}", e);
                        return Err(anyhow::anyhow!(
                            "Failed to initialize ACP connection: {:?}",
                            e
                        ));
                    }
                }

                if !mcp_servers.is_empty() {
                    info!(
                        "🔧 [ACP] 配置了 {} 个 MCP 服务器: {}",
                        mcp_servers.len(),
                        mcp_servers
                            .iter()
                            .map(|s| match s {
                                agent_client_protocol::McpServer::Stdio(server) =>
                                    server.name.clone(),
                                _ => "unknown".to_string(),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    // 🆕 添加警告: MCP 服务器可能导致启动延迟
                    warn!(
                        "⚠️ [ACP] 已配置 {} 个 MCP 服务器,可能导致 new_session 延迟,已启用 100 秒超时保护",
                        mcp_servers.len()
                    );
                }

                // 🆕 Resume 会话预检查：使用 list_sessions API 验证会话是否存在
                // 返回 (system_prompt_meta, actual_is_resume_session)
                let (system_prompt_meta, actual_is_resume_session) = if let Some(
                    ref resume_session_id,
                ) =
                    start_config.resume_session_id
                {
                    // 调用 list_sessions API 检查会话是否存在（带缓存）
                    match crate::session::check_session_exists_via_api(
                        &client_conn,
                        resume_session_id,
                        &project_path_for_closure.to_string_lossy(),
                    )
                    .await
                    {
                        Ok(true) => {
                            info!("✅ 目标会话存在，将使用 resume: {}", resume_session_id);
                            // 会话存在，使用包含 resume 的 meta
                            (start_config.build_meta(), true)
                        }
                        Ok(false) => {
                            warn!(
                                "⚠️ 目标会话不存在，跳过 resume，创建新会话: {}",
                                resume_session_id
                            );
                            // 会话不存在，使用不包含 resume 的 meta，且标记为非 resume 会话
                            (start_config.build_meta_without_resume(), false)
                        }
                        Err(e) => {
                            // API 失败，降级到文件扫描验证
                            info!(
                                "ℹ️ list_sessions API 失败，降级到文件扫描: {} (error: {:?})",
                                resume_session_id, e
                            );

                            let exists = crate::session::check_session_file_exists(
                                resume_session_id,
                                &project_path_for_closure.to_string_lossy(),
                            )
                            .await;

                            if exists {
                                info!(
                                    "✅ [文件扫描] 会话存在，使用 resume: {}",
                                    resume_session_id
                                );
                                (start_config.build_meta(), true)
                            } else {
                                warn!(
                                    "⚠️ [文件扫描] 会话不存在，创建新会话: {}",
                                    resume_session_id
                                );
                                (start_config.build_meta_without_resume(), false)
                            }
                        }
                    }
                } else {
                    // 没有 resume_session_id，正常创建新会话
                    (start_config.build_meta(), false)
                };

                // 创建会话（统一使用 new_session，resume 通过 meta 传递）
                let start = std::time::Instant::now();
                debug!("🔵 [ACP] 开始创建 ACP 会话[new_session]");

                let new_session_request =
                    NewSessionRequest::new(project_path_for_closure.clone())
                        .mcp_servers(mcp_servers.clone())
                        .meta(system_prompt_meta);

                // 添加 100 秒超时保护 (MCP 工具较多时启动较慢)
                let resp = tokio::time::timeout(
                    tokio::time::Duration::from_secs(100),
                    client_conn.new_session(new_session_request),
                )
                .await
                .map_err(|_| {
                    let elapsed = start.elapsed();
                    error!(
                        "⏰ [ACP] new_session 超时 (100s)! 耗时: {:?}, MCP 服务器: {:?}, 项目: {}",
                        elapsed, mcp_servers, project_id_for_child
                    );
                    anyhow::anyhow!(
                        "ACP 会话创建超时 (100s) - Agent 子进程可能卡在 MCP 服务器连接或处理大量工具"
                    )
                })?
                .context("ACP 会话创建失败")?;

                let elapsed = start.elapsed();
                debug!(
                    "✅ [ACP] ACP 会话创建成功[new_session], session_id={}, 耗时: {:?}",
                    resp.session_id.0, elapsed
                );

                // 🆕 如果耗时较长,发出警告
                if elapsed.as_secs() > 10 {
                    warn!(
                        "⚠️ [ACP] new_session 耗时较长: {:?} (MCP 服务器数量: {})",
                        elapsed,
                        mcp_servers.len()
                    );
                }

                let session_id = resp.session_id;

                // 发送会话 ID
                if session_id_tx.send(session_id.clone()).is_err() {
                    error!("无法发送会话 ID：接收方已关闭");
                    return Err(anyhow::anyhow!("无法发送会话 ID"));
                }

                // 创建生命周期句柄（在 spawn_local 外部创建，这里只是引用）
                // 注意：lifecycle_handle 将在 launch() 返回后由 session_manager 保存
                let lifecycle_handle_for_handler: Option<Arc<dyn AgentLifecycle>> = None;

                // 启动通道处理器
                super::channel::spawn_cancel_handler_for_agent(
                    client_conn.clone(),
                    cancel_rx,
                    &project_id_for_child,
                );

                // 启动 Prompt 处理器（包含降级逻辑）
                super::channel::spawn_prompt_handler_for_agent(
                    client_conn.clone(),
                    prompt_rx,
                    session_id.clone(),
                    &project_id_for_child,
                    PromptHandlerConfig {
                        is_resume_session: actual_is_resume_session,
                        project_path: project_path_for_closure.clone(),
                        mcp_servers: mcp_servers.clone(),
                        registry: registry_for_handler,
                        cancel_tx: cancel_tx_for_handler,
                        lifecycle_handle: lifecycle_handle_for_handler,
                        model_provider: model_provider_for_handler,
                        notifier: notifier.clone(),
                    },
                );

                // 使用 tokio::select! 正确处理取消信号
                // Prompt 处理器已经在后台 spawn_local 任务中运行
                // 这里只需要等待取消信号即可
                tokio::select! {
                    _ = cancel_token_for_closure.cancelled() => {
                        info!("Claude Code ACP Agent 收到取消信号，将清理资源并退出");
                        Ok(())
                    }
                    // 注意: 不需要其他分支,因为 Prompt 处理器已经在后台运行
                    // select! 会一直等待直到取消信号到来
                }
            }.await;

            if let Err(e) = result {
                error!("Claude Code ACP Agent 后台任务失败: {}", e);
            }
        });

        // 等待会话 ID
        let session_id = session_id_rx.await.map_err(|e| {
            error!("等待会话 ID 失败: {}", e);
            anyhow::anyhow!("等待会话 ID 失败: {}", e)
        })?;

        info!(
            "Claude Code ACP Agent 服务启动完成，会话 ID: {}",
            session_id.0
        );

        // 创建 stderr 任务
        let cancel_token_for_stderr = cancel_token.clone();
        let stderr_task = tokio::task::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut stderr_reader = tokio::io::BufReader::new(stderr);
            let mut stderr_buffer = String::new();

            loop {
                if cancel_token_for_stderr.is_cancelled() {
                    info!("Claude Code Agent stderr 任务收到取消信号，退出读取");
                    break;
                }

                match stderr_reader.read_line(&mut stderr_buffer).await {
                    Ok(0) => {
                        info!("Claude Code Agent stderr 流已关闭");
                        break;
                    }
                    Ok(bytes_read) => {
                        let line = &stderr_buffer[..bytes_read];
                        if !line.trim().is_empty() {
                            warn!("Claude Code Agent stderr: {}", line.trim());
                        }
                        stderr_buffer.clear();
                    }
                    Err(e) => {
                        error!("读取 Claude Code Agent stderr 失败: {}", e);
                        break;
                    }
                }
            }
        });

        // 创建生命周期守卫（传入密钥管理器、UUID 映射和 UUID）
        let lifecycle_guard = AgentLifecycleGuard::new_claude_with_key_manager(
            project_id.clone(),
            session_id.clone(),
            child,
            stderr_task,
            cancel_token.clone(),
            shared_api_key_manager,
            project_uuid_map,
            service_uuid,
        );

        Ok(LauncherConnectionInfoComplete {
            session_id,
            prompt_tx,
            cancel_tx,
            lifecycle_guard: Arc::new(lifecycle_guard),
        })
    }
}
