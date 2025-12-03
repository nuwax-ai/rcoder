use agent_client_protocol::{
    Agent, ClientSideConnection, ContentBlock, Implementation, InitializeRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, SessionId, TextContent,
};

// 使用默认版本
const VERSION: agent_client_protocol::ProtocolVersion =
    agent_client_protocol::ProtocolVersion::LATEST;

use shared_types::ModelProviderConfig;
use std::{process::Stdio, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    AgentType, CancelNotificationRequest,
    model::ChatPrompt,
    proxy_agent::{AcpAgentClient, AcpConnectionInfo, agent_stop_handle::AgentLifecycleGuard},
    utils::create_default_mcp_servers,
};
use anyhow::{Context, Result};
use tokio::task::LocalSet;
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// 启动一个长驻的 Claude Code ACP Agent 服务，返回会话信息和一个用于持续发送 Prompt 的通道
/// 使用 claude-code-acp 作为代理服务，通过子进程方式启动
pub async fn start_claude_code_acp_agent_service(
    chat_prompt: ChatPrompt,
    model_provider: Option<ModelProviderConfig>,
) -> Result<AcpConnectionInfo> {
    let project_path = chat_prompt.project_path;

    // 直接使用 claude-code-acp 命令
    let command_path = "claude-code-acp";
    let command_args = Vec::<String>::new(); // 空参数列表，可以根据需要添加
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
    let project_id_for_child = chat_prompt.project_id.clone();
    let cancel_token_for_closure = cancel_token.clone();

    info!(
        "项目工作目录: {}",
        &project_path_for_closure.to_string_lossy()
    );

    // 启动子进程并获取句柄
    let spawn_args = command_args.clone();
    //todo  暂时从环境变量便利加载配置 ,ANTHROPIC_* 环境变量
    let merged_envs = AgentType::claude_model_provider(model_provider.clone())?;
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

                // 创建 MCP 服务器配置（不使用 API key）
                let mcp_servers = create_default_mcp_servers(None);

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
                let session_id = match chat_prompt.session_id {
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
                                let new_session_request =
                                    NewSessionRequest::new(project_path_for_closure.clone())
                                        .mcp_servers(mcp_servers);
                                let resp = client_conn.new_session(new_session_request).await?;
                                debug!("ACP 会话创建成功[new_session],{:?}", resp);
                                resp.session_id
                            }
                        }
                    }
                    None => {
                        debug!("创建 ACP 会话[new_session]");
                        let new_session_request =
                            NewSessionRequest::new(project_path_for_closure.clone())
                                .mcp_servers(mcp_servers);
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
                    &chat_prompt.project_id,
                );
                super::channel_utils::spawn_prompt_handler_for_agent(
                    client_conn.clone(),
                    prompt_rx,
                    session_id.clone(),
                    &chat_prompt.project_id,
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
    let project_id_for_stderr = project_id_for_child.clone();
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
        project_id_for_child.clone(),
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
