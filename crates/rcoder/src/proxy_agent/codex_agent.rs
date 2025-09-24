use agent_client_protocol::{
    self as acp, Agent, AgentSideConnection, ClientCapabilities, ClientSideConnection,
    ContentBlock, ExtNotification, ExtRequest, ExtResponse, InitializeRequest,
    KillTerminalCommandResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionId, SessionNotification, SetSessionModeResponse, TextContent,
    V1 as VERSION,
};
use agent_client_protocol::{Client, LoadSessionRequest}; // bring trait into scope for session_notification

use codex_acp_agent::CodexAgent;
use codex_core::config::{
    Config, ConfigOverrides, ConfigToml, find_codex_home, load_config_as_toml,
};
use codex_core::protocol::AskForApproval;
use codex_core::protocol_config_types::SandboxMode;
use dashmap::DashMap;
use serde_json::json;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{debug, error, info};

use crate::model::{AgentType, ChatPrompt, ChatPromptResponse, ProjectAndAgentInfo};
use anyhow::Result;

use super::AcpAgentClient;

/// 启动一个长驻的 ACP Agent 服务，返回会话信息和一个用于持续发送 Prompt 的通道
/// 默认启用 YOLO 模式（禁用沙箱和批准请求）
pub async fn start_codex_acp_agent_service(
    chat_prompt: ChatPrompt,
    model_provider: Option<ModelProviderConfig>,
) -> Result<(SessionId, mpsc::UnboundedSender<PromptRequest>)> {
    let project_path = chat_prompt.project_path;

    // 会话更新与客户端通道（用于构建 CodexAgent）
    let (session_update_tx, mut session_update_rx) = mpsc::unbounded_channel();
    let (client_tx, _client_rx) = mpsc::unbounded_channel();

    //todo  暂时从环境变量便利加载配置
    let (cfg, _) = AgentType::codex_model_provider(model_provider.clone())?;

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

    // 创建 Agent
    let agent = CodexAgent::with_config(session_update_tx.clone(), client_tx.clone(), config);

    // 管道
    let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
    let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);

    // 用于外部持续发送 prompt 的通道
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();

    // 在 LocalSet 中启动服务
    let (session_id_tx, session_id_rx) = oneshot::channel::<SessionId>();

    let embedded_client = AcpAgentClient;

    // 两端连接
    let (server_conn, server_io_task) = AgentSideConnection::new(
        agent.clone(),
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

    // 持续运行 IO
    tokio::task::spawn_local(server_io_task);
    tokio::task::spawn_local(client_io_task);

    // 转发 Agent 的 SessionNotification 到连接（触发 EmbeddedClient::session_notification）
    tokio::task::spawn_local(async move {
        loop {
            match session_update_rx.recv().await {
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
    });

    // 初始化 + 创建会话
    client_conn
        .initialize(InitializeRequest {
            protocol_version: VERSION,
            client_capabilities: ClientCapabilities::default(),
            meta: None,
        })
        .await?;
    // let session_resp = client_conn
    //     .new_session(NewSessionRequest {
    //         mcp_servers: Vec::new(),
    //         cwd: project_path.clone(),
    //         meta: None,
    //     })
    //     .await?;
    // 创建会话
    let session_id = match chat_prompt.session_id {
        Some(session_id) => {
            debug!("创建 ACP 会话[new_session]");
            let session_id = SessionId(session_id.into());
            let resp = client_conn
                .load_session(LoadSessionRequest {
                    session_id: session_id.clone(),
                    mcp_servers: Vec::new(),
                    cwd: project_path.clone(),
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
                    mcp_servers: Vec::new(),
                    cwd: project_path.clone(),
                    meta: None,
                })
                .await?;
            debug!("ACP 会话创建成功[new_session],{:?}", resp);
            resp.session_id
        }
    };

    let _ = session_id_tx.send(session_id.clone());

    // 长驻循环：接收外部 prompt 并转发到 ACP
    tokio::task::spawn_local(async move {
        while let Some(mut req) = prompt_rx.recv().await {
            if req.session_id.0.is_empty() {
                req.session_id = session_id.clone();
            }
            match client_conn.prompt(req).await {
                Ok(resp) => {
                    debug!("Prompt 发送成功, stop_reason={:?}", resp.stop_reason);
                }
                Err(e) => {
                    error!("发送 Prompt 失败: {:?}", e);
                }
            }
        }
    });

    let session_id = session_id_rx.await?;
    Ok((session_id, prompt_tx))
}
