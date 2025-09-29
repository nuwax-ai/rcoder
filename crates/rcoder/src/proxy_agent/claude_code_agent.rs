use agent_client_protocol::{
    Agent, ClientCapabilities, ClientSideConnection, ContentBlock, InitializeRequest,
    LoadSessionRequest, NewSessionRequest, PromptRequest, SessionId, TextContent, V1 as VERSION,
};

use shared_types::ModelProviderConfig;
use std::{process::Stdio, sync::Arc};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::{
    AgentType, CancelNotificationRequest,
    model::ChatPrompt,
    proxy_agent::{
        AcpAgentClient, AcpConnectionInfo,
        agent_stop_handle::{AgentStopHandle, AgentStopHandleArc, ClaudeCodeAgentStopHandle},
    },
    utils::create_mcp_servers_with_context7,
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
                    .initialize(InitializeRequest {
                        protocol_version: VERSION,
                        client_capabilities: ClientCapabilities {
                            fs: agent_client_protocol::FileSystemCapability {
                                read_text_file: false,
                                write_text_file: false,
                                meta: None,
                            },
                            terminal: false,
                            meta: None,
                        },
                        meta: None,
                    })
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
                let mcp_servers = create_mcp_servers_with_context7(None);

                if !mcp_servers.is_empty() {
                    info!(
                        "🔧 配置了 {} 个 MCP 服务器: {}",
                        mcp_servers.len(),
                        mcp_servers
                            .iter()
                            .map(|s| match s {
                                agent_client_protocol::McpServer::Stdio { name, .. } =>
                                    name.clone(),
                                _ => "unknown".to_string(),
                            })
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                } else {
                    info!("📝 未配置 MCP 服务器");
                }

                // 创建会话
                let session_id = match chat_prompt.session_id {
                    Some(session_id) => {
                        debug!("创建 ACP 会话[load_session]");
                        let session_id = SessionId(session_id.into());
                        let resp = client_conn
                            .load_session(LoadSessionRequest {
                                session_id: session_id.clone(),
                                mcp_servers: mcp_servers.clone(),
                                cwd: project_path_for_closure.clone(),
                                meta: None,
                            })
                            .await?;
                        debug!("ACP 会话加载成功[load_session],{:?}", resp);
                        session_id
                    }
                    None => {
                        debug!("创建 ACP 会话[new_session]");
                        let resp = client_conn
                            .new_session(NewSessionRequest {
                                mcp_servers: mcp_servers.clone(),
                                cwd: project_path_for_closure.clone(),
                                meta: None,
                            })
                            .await?;
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
                    chat_prompt.request_id.clone(),
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
            let error_block = ContentBlock::Text(TextContent {
                text: format!("Claude Code ACP Agent 启动失败: {}", e),
                annotations: None,
                meta: None,
            });
            let _ = prompt_tx_for_closure.send(PromptRequest {
                session_id: SessionId("error".into()),
                prompt: vec![error_block],
                meta: None,
            });
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

    // 创建停止句柄 - 使用外部的CancellationToken和stderr流
    let stop_handle = Arc::new(AgentStopHandle::Claude(
        ClaudeCodeAgentStopHandle::with_cancellation_token_and_stderr(
            child,
            stderr,
            project_id_for_child.clone(),
            cancel_token.clone(),
        ),
    ));

    // 现在停止句柄包含了实际的子进程句柄，可以通过AgentStopHandle::Claude来控制
    // 同时内部也使用CancellationToken来实现协作式取消

    Ok(AcpConnectionInfo {
        session_id,
        prompt_tx,
        cancel_tx,
        stop_handle: Some(stop_handle),
    })
}
