use agent_client_protocol::{
    Agent, ClientSideConnection, ContentBlock, Implementation, InitializeRequest,
    LoadSessionRequest, McpServer, McpServerStdio, NewSessionRequest, PromptRequest, SessionId,
    TextContent,
};

// 使用默认版本
const VERSION: agent_client_protocol::ProtocolVersion =
    agent_client_protocol::ProtocolVersion::LATEST;

use agent_config::{AgentInstallationManager, AgentServersConfig, ContextServerConfig};
use shared_types::ModelProviderConfig;
use std::{collections::HashMap, path::PathBuf, process::Stdio, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    CancelNotificationRequest,
    proxy_agent::{AcpAgentClient, AcpConnectionInfo, agent_stop_handle::AgentLifecycleGuard},
    utils::create_default_mcp_servers,
};
use agent_abstraction::AgentStartConfig;
use anyhow::{Context, Result};
use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Agent 配置参数
struct AgentLaunchConfig {
    /// 命令路径
    command: String,
    /// 命令参数
    args: Vec<String>,
    /// 环境变量
    env: HashMap<String, String>,
    /// Context 服务器配置 (MCP servers)
    context_servers: HashMap<String, ContextServerConfig>,
}

/// 从配置文件加载 Agent 配置
///
/// 优先加载嵌入的JSON配置文件，如果加载失败则使用默认配置
/// 同时检查并自动安装 agent（如果需要）
async fn load_agent_config(
    model_provider: Option<&ModelProviderConfig>,
) -> Result<AgentLaunchConfig> {
    // 使用 load_or_default() 加载嵌入的JSON配置文件
    let config = AgentServersConfig::load_or_default().await;

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
            for (_key, value) in resolved_env.iter_mut() {
                *value = value
                    .replace("{MODEL_PROVIDER_API_KEY}", &provider.api_key)
                    .replace("{MODEL_PROVIDER_BASE_URL}", &provider.base_url)
                    .replace("{MODEL_PROVIDER_DEFAULT_MODEL}", &provider.default_model)
                    .replace("{MODEL_PROVIDER_NAME}", &provider.name);
            }
        }

        Ok(AgentLaunchConfig {
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            env: resolved_env,
            context_servers: config.context_servers.clone(),
        })
    } else {
        // 配置中没有找到，使用默认值
        warn!("⚠️ 配置中未找到 claude-code-acp，使用默认配置");
        get_default_agent_config(model_provider)
    }
}

/// 获取默认的 Agent 配置（后备方案）
fn get_default_agent_config(
    model_provider: Option<&ModelProviderConfig>,
) -> Result<AgentLaunchConfig> {
    let env = if let Some(provider) = model_provider {
        let mut env = HashMap::new();
        if !provider.api_key.is_empty() {
            env.insert("ANTHROPIC_API_KEY".to_string(), provider.api_key.clone());
        }
        if !provider.base_url.is_empty() {
            env.insert("ANTHROPIC_BASE_URL".to_string(), provider.base_url.clone());
        }
        if !provider.default_model.is_empty() {
            env.insert("ANTHROPIC_MODEL".to_string(), provider.default_model.clone());
        }
        env.insert("RUST_LOG".to_string(), "info".to_string());
        env
    } else {
        let mut env = HashMap::new();
        env.insert("RUST_LOG".to_string(), "info".to_string());
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
fn convert_context_servers(configs: &HashMap<String, ContextServerConfig>) -> Vec<McpServer> {
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
            if let Some(env) = &c.env {
                let env_vars: Vec<agent_client_protocol::EnvVariable> = env
                    .iter()
                    .map(|(k, v)| agent_client_protocol::EnvVariable::new(k.clone(), v.clone()))
                    .collect();
                server = server.env(env_vars);
            }

            Some(McpServer::Stdio(server))
        })
        .collect()
}

/// 构建包含系统提示词的 AgentStartConfig
///
/// 使用 agent_abstraction 的 AgentStartConfig 来管理系统提示词和启动配置，
/// 通过 ACP 协议的 `meta.systemPrompt.append` 模式传递给 Agent。
///
/// 从 AgentServersConfig 加载系统提示词，支持配置文件覆盖默认值。
///
/// # 参数
/// - `config`: AgentServersConfig 配置
///
/// # 返回值
/// 返回配置了系统提示词的 AgentStartConfig
fn build_agent_start_config(config: &AgentServersConfig) -> AgentStartConfig {
    // 从配置获取系统提示词（优先使用配置，否则使用编译时嵌入的默认值）
    let system_prompt = config.get_system_prompt("claude-code-acp");

    info!("📝 已构建系统提示词配置（使用 AgentStartConfig + append 模式）");

    AgentStartConfig::new()
        .with_system_prompt(system_prompt)
}

/// 启动一个长驻的 Claude Code ACP Agent 服务，返回会话信息和一个用于持续发送 Prompt 的通道
/// 使用 claude-code-acp 作为代理服务，通过子进程方式启动
pub async fn start_claude_code_acp_agent_service(
    prompt_message: agent_abstraction::PromptMessage,
    model_provider: Option<ModelProviderConfig>,
) -> Result<AcpConnectionInfo> {
    let project_path = prompt_message.project_path;

    // 加载配置（用于获取系统提示词）
    let servers_config = AgentServersConfig::load_or_default().await;

    // 从配置加载 Agent 参数（使用嵌入的JSON配置文件）
    let agent_config = load_agent_config(model_provider.as_ref()).await?;
    let command_path = &agent_config.command;
    let command_args = &agent_config.args;
    info!("Claude Code ACP 命令: {} {:?}", command_path, command_args);

    // 用户发送 CancelNotification 消息的通道
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<CancelNotificationRequest>();

    // 用于外部持续发送 prompt 的通道
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();
    let (session_id_tx, session_id_rx) = oneshot::channel::<SessionId>();

    // 创建 CancellationToken 用于控制子进程生命周期
    let cancel_token = CancellationToken::new();

    // 克隆用于闭包
    let prompt_tx_for_closure = prompt_tx.clone();
    let project_path_for_closure = project_path.clone();
    let project_id_for_child = prompt_message.project_id.clone();
    let project_id_for_lifecycle = prompt_message.project_id.clone();  // 为 AgentLifecycleGuard 单独克隆
    let session_id_for_closure = prompt_message.session_id.clone();
    let cancel_token_for_closure = cancel_token.clone();
    let servers_config_for_closure = servers_config.clone();  // 为闭包克隆配置

    info!(
        "项目工作目录: {}",
        &project_path_for_closure.to_string_lossy()
    );

    // 转换配置中的 Context 服务器为 ACP 协议格式
    let config_mcp_servers = convert_context_servers(&agent_config.context_servers);

    // 启动子进程并获取句柄
    let spawn_args = command_args.clone();
    // 使用配置中的环境变量
    let merged_envs = agent_config.env.clone();
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

    // 获取 stdio 句柄 - 注意：这些会被移动到闭包中
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

    // 创建客户端，并获取会话通知接收端
    let client = AcpAgentClient;

    // 创建连接
    let (client_conn, handle_io) = ClientSideConnection::new(client, outgoing, incoming, |fut| {
        tokio::task::spawn_local(fut);
    });

    // 启动 I/O 处理任务
    tokio::task::spawn_local(handle_io);

    let client_conn = Arc::new(client_conn);

    // 注意：由于 Child 不能克隆，我们无法创建独立的监控任务
    // 子进程的生命周期管理将通过 AgentStopHandle 来处理
    // 当 CancellationToken 被取消时，Child 对象会被 drop，从而自动杀死子进程

    // 启动后台任务来管理 ACP 连接
    let task_handle = tokio::task::spawn_local(async move {
        // 创建 LocalSet 来运行非 Send 的 ACP 连接
        let local_set = LocalSet::new();

        let result = local_set
            .run_until(async {
                let client_conn = client_conn.clone();
                // 初始化连接
                debug!("初始化 ACP 连接[initialize]");
                let init_result = client_conn
                    .initialize(
                        InitializeRequest::new(VERSION).client_info(
                            Implementation::new("rcoder-agent-runner", env!("CARGO_PKG_VERSION"))
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

                // 创建 MCP 服务器配置
                // 优先使用配置文件中的 MCP 服务器，如果没有配置则使用默认的
                let mcp_servers = if !config_mcp_servers.is_empty() {
                    info!("📦 使用配置文件中的 MCP 服务器");
                    config_mcp_servers.clone()
                } else {
                    info!("📦 使用默认 MCP 服务器配置");
                    create_default_mcp_servers(None)
                };

                if !mcp_servers.is_empty() {
                    info!(
                        "🔧 配置了 {} 个 MCP 服务器: {}",
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
                } else {
                    info!("📝 未配置 MCP 服务器");
                }

                // 创建会话（兼容未来 SDK 的 load_session，失败则回退 new_session）
                // 使用 AgentStartConfig 构建系统提示词 meta（在创建会话时传递，而不是每次 Prompt 都传递）
                let start_config = build_agent_start_config(&servers_config_for_closure);
                let system_prompt_meta = start_config.build_meta();

                let session_id = match session_id_for_closure {
                    Some(session_id) => {
                        debug!("尝试加载 ACP 会话[load_session]");
                        let given_session_id = SessionId::new(session_id);
                        match client_conn
                            .load_session(LoadSessionRequest::new(
                                given_session_id.clone(),
                                project_path_for_closure.clone(),
                            ))
                            .await
                        {
                            Ok(resp) => {
                                debug!("ACP 会话加载成功[load_session],{:?}", resp);
                                given_session_id
                            }
                            Err(e) => {
                                warn!(
                                    "load_session 失败或未实现，回退创建新会话[new_session]: {:?}",
                                    e
                                );
                                // 使用 meta 传递系统提示词（ACP 协议分离模式）
                                let new_session_request =
                                    NewSessionRequest::new(project_path_for_closure.clone())
                                        .mcp_servers(mcp_servers.clone())
                                        .meta(system_prompt_meta.clone());
                                let resp = client_conn.new_session(new_session_request).await?;
                                debug!("ACP 会话创建成功[new_session],{:?}", resp);
                                resp.session_id
                            }
                        }
                    }
                    None => {
                        debug!("创建 ACP 会话[new_session]");
                        // 使用 meta 传递系统提示词（ACP 协议分离模式）
                        let new_session_request =
                            NewSessionRequest::new(project_path_for_closure.clone())
                                .mcp_servers(mcp_servers)
                                .meta(system_prompt_meta);
                        let resp = client_conn.new_session(new_session_request).await?;
                        debug!("ACP 会话创建成功[new_session],{:?}", resp);
                        resp.session_id
                    }
                };

                // 发送会话 ID 到主线程
                if session_id_tx.send(session_id.clone()).is_err() {
                    error!("无法发送会话 ID：接收方已关闭");
                    return Err(anyhow::anyhow!("无法发送会话 ID"));
                }

                // 使用共享的通道处理逻辑
                super::channel_utils::spawn_cancel_handler_for_agent(
                    client_conn.clone(),
                    cancel_rx,
                    &project_id_for_child,
                );
                super::channel_utils::spawn_prompt_handler_for_agent(
                    client_conn.clone(),
                    prompt_rx,
                    session_id.clone(),
                    &project_id_for_child,
                );

                // Rust 最佳实践：直接等待取消信号，不需要轮询
                // 进程存活由 ACP 连接和通道处理器保持，不需要额外的 keep_alive 任务
                cancel_token_for_closure.cancelled().await;
                info!("Claude Code ACP Agent 收到取消信号，将清理资源并退出");
                // 当收到取消信号时，Child 对象会被 drop，kill_on_drop(true) 会自动杀死子进程
                Ok(())
            })
            .await;

        if let Err(e) = result {
            error!("Claude Code ACP Agent 后台任务失败: {}", e);
            // 通知主线程任务失败 - 发送一个错误提示作为信号
            let error_block = ContentBlock::Text(TextContent::new(format!(
                "Claude Code ACP Agent 启动失败: {}",
                e
            )));
            let _ = prompt_tx_for_closure.send(PromptRequest::new(
                SessionId::new("error"),
                vec![error_block],
            ));
        }
    });

    // 等待会话 ID 并立即返回
    let session_id = session_id_rx.await.map_err(|e| {
        error!("等待会话 ID 失败: {}", e);
        anyhow::anyhow!("等待会话 ID 失败: {}", e)
    })?;

    info!(
        "Claude Code ACP Agent 服务启动完成，会话 ID: {}",
        session_id.0
    );

    // 创建stderr任务来处理子进程的stderr输出
    let cancel_token_for_stderr = cancel_token.clone();
    let stderr_task = tokio::task::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut stderr_reader = tokio::io::BufReader::new(stderr);
        let mut stderr_buffer = String::new();

        loop {
            // 检查取消令牌
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

    // 创建生命周期守卫
    let lifecycle_guard = AgentLifecycleGuard::new_claude(
        project_id_for_lifecycle,
        session_id.clone(),
        child,
        stderr_task,
        cancel_token.clone(),
    );

    Ok(AcpConnectionInfo {
        session_id,
        prompt_tx,
        cancel_tx,
        stop_handle: Some(Arc::new(lifecycle_guard)),
    })
}
