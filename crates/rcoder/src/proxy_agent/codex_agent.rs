use std::sync::Arc;

use agent_client_protocol::{
    Agent, AgentSideConnection, ClientCapabilities, ClientSideConnection, InitializeRequest,
    NewSessionRequest, PromptRequest, SessionId, V1 as VERSION,
};
use agent_client_protocol::{Client, LoadSessionRequest}; // bring trait into scope for session_notification

use codex_acp_agent::{CodexAgent, FsBridge};
use codex_core::config::{Config, ConfigOverrides};
use codex_core::protocol::AskForApproval;
use codex_core::protocol_config_types::SandboxMode;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::model::{AgentType, ChatPrompt};
use crate::proxy_agent::agent_stop_handle::AgentLifecycleGuard;
use crate::{CancelNotificationRequest, utils::create_default_mcp_servers};
use anyhow::Result;

use super::{AcpAgentClient, AcpConnectionInfo};

/// 启动一个长驻的 ACP Agent 服务，返回会话信息和一个用于持续发送 Prompt 的通道
/// 默认启用 YOLO 模式（禁用沙箱和批准请求）
pub async fn start_codex_acp_agent_service(
    chat_prompt: ChatPrompt,
    model_provider: Option<ModelProviderConfig>,
) -> Result<AcpConnectionInfo> {
    let project_path = chat_prompt.project_path;

    // 会话更新与客户端通道（用于构建 CodexAgent）
    let (session_update_tx, mut session_update_rx) = mpsc::unbounded_channel();
    let (client_tx, _client_rx) = mpsc::unbounded_channel();

    // 用户发送 CancelNotification 消息的通道
    let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<CancelNotificationRequest>();

    //todo  暂时从环境变量便利加载配置
    let (cfg, _) = AgentType::codex_model_provider(model_provider.clone()).await?;

    info!("Loaded codex config: {:?}", cfg);

    // 默认启用 YOLO 模式配置覆盖
    let mut config_overrides = ConfigOverrides::default();

    info!("启用 YOLO 模式: 禁用沙箱，禁用批准请求");
    config_overrides.sandbox_mode = Some(SandboxMode::DangerFullAccess);
    config_overrides.approval_policy = Some(AskForApproval::Never);
    config_overrides.cwd = Some(project_path.clone());

    let config =
        Config::load_from_base_config_with_overrides(cfg, config_overrides, project_path.clone())
            .map_err(|e| {
            error!("Failed to load config: {}", e);
            anyhow::anyhow!("Failed to load config: {}", e)
        })?;

    // 创建 FsBridge
    let fs_bridge = FsBridge::start(client_tx.clone(), config.cwd.clone())
        .await
        .map_err(|e| {
            error!("Failed to start FsBridge: {}", e);
            anyhow::anyhow!("Failed to start FsBridge: {}", e)
        })?;

    // 创建 Agent
    let agent = CodexAgent::with_config(
        session_update_tx.clone(),
        client_tx.clone(),
        config,
        Some(fs_bridge),
    );

    // 管道
    let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
    let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);

    // 用于外部持续发送 prompt 的通道
    let (prompt_tx, prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();

    // 创建 CancellationToken 用于控制嵌入式 Codex agent 生命周期
    let cancel_token = CancellationToken::new();

    // 在 LocalSet 中启动服务
    let (session_id_tx, session_id_rx) = oneshot::channel::<SessionId>();

    let embedded_client = AcpAgentClient;

    // 两端连接
    let (server_conn, server_io_task) = AgentSideConnection::new(
        agent,
        agent_to_client_tx,
        client_to_agent_rx,
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );

    let (client_conn, client_io_task) = ClientSideConnection::new(
        embedded_client,
        client_to_agent_tx,
        agent_to_client_rx,
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );

    let client_conn = Arc::new(client_conn);

    // 保存任务句柄用于后续停止
    let mut io_task_handles = Vec::new();
    let mut channel_task_handles = Vec::new();

    // 持续运行 IO，并保存任务句柄
    let server_io_handle = tokio::task::spawn_local(server_io_task);
    let client_io_handle = tokio::task::spawn_local(client_io_task);
    io_task_handles.push(server_io_handle);
    io_task_handles.push(client_io_handle);

    // 转发 Agent 的 SessionNotification 到连接（触发 EmbeddedClient::session_notification）
    {
        let cancel_token = cancel_token.clone();
        tokio::task::spawn_local(async move {
            loop {
                tokio::select! {
                    _ = cancel_token.cancelled() => {
                        debug!("Codex agent session_update 转发任务收到取消信号，退出");
                        break;
                    }
                    maybe_msg = session_update_rx.recv() => {
                        match maybe_msg {
                            Some((session_notification, tx)) => {
                                let result = server_conn.session_notification(session_notification).await;
                                if let Err(e) = result {
                                    error!("failed to send session notification: {:?}", e);
                                    let _ = tx.send(());
                                    break;
                                }
                                let _ = tx.send(());
                            }
                            None => break,
                        }
                    }
                }
            }
        });
    }

    // 初始化 + 创建会话
    client_conn
        .initialize(InitializeRequest {
            protocol_version: VERSION,
            client_capabilities: ClientCapabilities::default(),
            meta: None,
        })
        .await?;

    // 创建 MCP 服务器配置（不使用 API key）
    let mcp_servers = create_default_mcp_servers(None);

    if !mcp_servers.is_empty() {
        info!(
            "🔧 配置了 {} 个 MCP 服务器: {}",
            mcp_servers.len(),
            mcp_servers
                .iter()
                .map(|s| match s {
                    agent_client_protocol::McpServer::Stdio { name, .. } => name.clone(),
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
            match client_conn
                .load_session(LoadSessionRequest {
                    session_id: session_id.clone(),
                    mcp_servers: mcp_servers.clone(),
                    cwd: project_path.clone(),
                    meta: None,
                })
                .await
            {
                Ok(resp) => {
                    info!("ACP 会话加载成功[load_session],{:?}", resp);
                    session_id
                }
                Err(e) => {
                    warn!("load_session 失败，回退为 new_session: {:?}", e);
                    let resp = client_conn
                        .new_session(NewSessionRequest {
                            mcp_servers: mcp_servers.clone(),
                            cwd: project_path.clone(),
                            meta: None,
                        })
                        .await?;
                    debug!("ACP 会话创建成功[new_session],{:?}", resp);
                    resp.session_id
                }
            }
        }
        None => {
            debug!("创建 ACP 会话[new_session]");
            let resp = client_conn
                .new_session(NewSessionRequest {
                    mcp_servers: mcp_servers.clone(),
                    cwd: project_path.clone(),
                    meta: None,
                })
                .await?;
            debug!("ACP 会话创建成功[new_session],{:?}", resp);
            resp.session_id
        }
    };

    let _ = session_id_tx.send(session_id.clone());

    // 使用共享的通道处理逻辑，并保存任务句柄
    let cancel_handle = super::channel_utils::spawn_cancel_handler_for_agent(
        client_conn.clone(),
        cancel_rx,
        &chat_prompt.project_id,
    );
    channel_task_handles.push(cancel_handle);

    let prompt_handle = super::channel_utils::spawn_prompt_handler_for_agent(
        client_conn.clone(),
        prompt_rx,
        session_id.clone(),
        &chat_prompt.project_id,
    );
    channel_task_handles.push(prompt_handle);

    let session_id = session_id_rx.await?;

    // 创建生命周期守卫
    let lifecycle_guard = AgentLifecycleGuard::new_codex(
        chat_prompt.project_id.clone(),
        session_id.clone(),
        client_conn.clone(),
        io_task_handles,
        channel_task_handles,
        cancel_token.clone(),
    );

    Ok(AcpConnectionInfo {
        session_id,
        prompt_tx,
        cancel_tx,
        stop_handle: Some(Arc::new(lifecycle_guard)),
    })
}
