use std::{
    path::{Component, PathBuf},
    sync::LazyLock,
};

use agent_client_protocol::{
    self as acp, Agent, AgentSideConnection, ClientCapabilities, ClientSideConnection,
    ContentBlock, ExtNotification, ExtRequest, ExtResponse, InitializeRequest,
    KillTerminalCommandResponse, NewSessionRequest, NewSessionResponse, PromptRequest,
    PromptResponse, SessionId, SessionNotification, SetSessionModeResponse, TextContent,
    V1 as VERSION,
};

use codex_acp_agent::CodexAgent;
use codex_core::config::Config;
use dashmap::DashMap;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{TokioAsyncReadCompatExt as _, TokioAsyncWriteCompatExt as _};
use tracing::{error, info};

use crate::model::{
    AgentType, ChatPrompt, ChatPromptResponse, EmbeddedClient, ProjectAndAgentInfo,
};
use anyhow::Result;

/// 使用 OnceLock 和 DashMap 管理 ProjectAndAgentInfo
pub static PROJECT_AND_AGENT_INFO_MAP: LazyLock<DashMap<String, ProjectAndAgentInfo>> =
    LazyLock::new(|| DashMap::new());

/// 在 LocalSet 中运行的实际 Agent 请求
#[derive(Debug)]
pub struct LocalSetAgentRequest {
    /// 用户端发送的 prompt 请求
    chat_prompt: ChatPrompt,
    /// 发送 agent 通知执行prompt 完毕的回执消息
    chat_prompt_tx: oneshot::Sender<ChatPromptResponse>,
}

impl LocalSetAgentRequest {
    pub fn new(chat_prompt: ChatPrompt) -> (Self, oneshot::Receiver<ChatPromptResponse>) {
        let (chat_prompt_tx, chat_prompt_rx) = oneshot::channel();

        (
            Self {
                chat_prompt,
                chat_prompt_tx,
            },
            chat_prompt_rx,
        )
    }
}

/// AgentSideConnection , ClientSideConnection 没实现 Send trait ,需要在 LocalSet 中运行,另外 agent服务数量是动态的,和 project_id 是一一对应的,一个 project_id 对应一个 agent服务

/// Agent worker 任务，在本地线程中运行 Agent
pub async fn agent_worker(
    mut request_rx: mpsc::UnboundedReceiver<LocalSetAgentRequest>,
) -> Result<()> {
    while let Some(request) = request_rx.recv().await {
        let mut chat_prompt = request.chat_prompt.clone();

        let original_path = chat_prompt.project_path;
        // 规范化路径：
        // - 如果是相对路径，先与当前目录拼接
        // - 去除路径中的 "./"（CurDir 组件），不依赖文件系统
        let joined_path = if original_path.is_absolute() {
            original_path
        } else {
            std::env::current_dir().unwrap().join(original_path)
        };
        let project_path: PathBuf = joined_path
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        // 将规范化后的路径写回，确保后续使用统一
        chat_prompt.project_path = project_path.clone();

        // 创建项目目录
        if !project_path.exists() {
            info!(
                "Project path does not exist,project_id={}",
                request.chat_prompt.project_id
            );
            //自动创建目录
            if let Err(e) = tokio::fs::create_dir_all(&project_path).await {
                error!("Failed to create project directory: {:?}", e);
                continue;
            }
        }

        // 检查 project_id 有对应的agent 服务,没有则创建
        let project_id = request.chat_prompt.project_id.clone();
        let project_and_agent_info = PROJECT_AND_AGENT_INFO_MAP.get(&project_id);
        if project_and_agent_info.is_none() {
            //创建 agent 服务
            match start_acp_agent_service(chat_prompt.clone()).await {
                Ok((session_id, prompt_tx)) => {
                    let project_and_agent_info = ProjectAndAgentInfo {
                        project_id: project_id.clone(),
                        session_id: session_id.clone(),
                        prompt_tx: prompt_tx.clone(),
                    };
                    PROJECT_AND_AGENT_INFO_MAP
                        .insert(project_id.clone(), project_and_agent_info.clone());

                    if let Ok(prompt_request) =
                        build_prompt_to_acp_agent(chat_prompt, session_id.clone()).await
                    {
                        if let Err(e) = prompt_tx.send(prompt_request) {
                            error!("Failed to send prompt request: {:?}", e);
                            //TODO  后续优化,如何处理异常,这里暂时不处理
                        }
                    }

                    // 发送回执消息
                    if let Err(e) = request.chat_prompt_tx.send(ChatPromptResponse {
                        project_id: project_id.clone(),
                        session_id: session_id.to_string(),
                    }) {
                        error!("Failed to send chat prompt response: {:?}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to start ACP agent service: {}", e);
                }
            }
        } else {
            // 发送 prompt 请求

            let info = project_and_agent_info.unwrap();
            if let Ok(prompt_request) =
                build_prompt_to_acp_agent(chat_prompt, info.session_id.clone()).await
            {
                if let Err(e) = info.prompt_tx.send(prompt_request) {
                    error!("Failed to send prompt request: {:?}", e);
                    //TODO  后续优化,如何处理异常,这里暂时不处理
                }
            }

            // 发送回执消息
            if let Err(e) = request.chat_prompt_tx.send(ChatPromptResponse {
                project_id: project_id.clone(),
                session_id: info.session_id.to_string(),
            }) {
                error!("Failed to send chat prompt response: {:?}", e);
            }
        }
    }

    info!("Agent worker finished");
    Ok(())
}

/// 构建 Prompt 请求
pub async fn build_prompt_to_acp_agent(
    prompt: ChatPrompt,
    session_id: SessionId,
) -> Result<PromptRequest> {
    let text_block = ContentBlock::Text(TextContent {
        text: prompt.prompt,
        annotations: None,
        meta: None,
    });

    Ok(PromptRequest {
        session_id,
        prompt: vec![text_block],
        meta: None,
    })
}

/// 启动一个长驻的 ACP Agent 服务，返回会话信息和一个用于持续发送 Prompt 的通道
pub async fn start_acp_agent_service(
    chat_prompt: ChatPrompt,
) -> Result<(SessionId, mpsc::UnboundedSender<PromptRequest>)> {
    let project_path = chat_prompt.project_path;

    // 会话更新与客户端通道（用于构建 CodexAgent）
    let (session_update_tx, _session_update_rx) = mpsc::unbounded_channel();
    let (client_tx, _client_rx) = mpsc::unbounded_channel();

    // 加载配置
    let config = Config::load_from_base_config_with_overrides(
        Default::default(),
        codex_core::config::ConfigOverrides::default(),
        project_path.clone(),
    )
    .map_err(|e| {
        error!("Failed to load config: {}", e);
        anyhow::anyhow!("Failed to load config: {}", e)
    })?;

    // 创建 Agent
    let agent = CodexAgent::with_config(session_update_tx.clone(), client_tx.clone(), config);

    // 管道
    let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
    let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);

    // LocalSet 环境
    let local_set = tokio::task::LocalSet::new();

    // 用于外部持续发送 prompt 的通道
    let (prompt_tx, mut prompt_rx) = mpsc::unbounded_channel::<PromptRequest>();

    // 在 LocalSet 中启动服务
    let (session_id_tx, session_id_rx) = oneshot::channel::<SessionId>();

    local_set
        .run_until(async move {
            let embedded_client = EmbeddedClient {};

            // 两端连接
            let (_server_conn, server_io_task) = AgentSideConnection::new(
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

            // 初始化 + 创建会话
            client_conn
                .initialize(InitializeRequest {
                    protocol_version: VERSION,
                    client_capabilities: ClientCapabilities::default(),
                    meta: None,
                })
                .await?;
            let session_resp = client_conn
                .new_session(NewSessionRequest {
                    mcp_servers: Vec::new(),
                    cwd: project_path.clone(),
                    meta: None,
                })
                .await?;

            let _ = session_id_tx.send(session_resp.session_id.clone());

            // 长驻循环：接收外部 prompt 并转发到 ACP
            tokio::task::spawn_local(async move {
                while let Some(mut req) = prompt_rx.recv().await {
                    if req.session_id.0.is_empty() {
                        req.session_id = session_resp.session_id.clone();
                    }
                    let _ = client_conn.prompt(req).await; // 错误可在上层处理或加日志
                }
            });

            // 阻塞在一个挂起的 future 上，保持 LocalSet 不退出
            futures::future::pending::<()>().await;
            #[allow(unreachable_code)]
            Ok::<(), anyhow::Error>(())
        })
        .await?;

    let session_id = session_id_rx.await?;
    Ok((session_id, prompt_tx))
}

// Helper function to create a bidirectional connection
fn create_connection_pair(
    client: &EmbeddedClient,
    agent: &CodexAgent,
) -> (ClientSideConnection, AgentSideConnection) {
    let (client_to_agent_rx, client_to_agent_tx) = piper::pipe(1024);
    let (agent_to_client_rx, agent_to_client_tx) = piper::pipe(1024);

    let (agent_conn, agent_io_task) = ClientSideConnection::new(
        client.clone(),
        client_to_agent_tx,
        agent_to_client_rx,
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );

    let (client_conn, client_io_task) = AgentSideConnection::new(
        agent.clone(),
        agent_to_client_tx,
        client_to_agent_rx,
        |fut| {
            tokio::task::spawn_local(fut);
        },
    );

    // Spawn the IO tasks
    tokio::task::spawn_local(agent_io_task);
    tokio::task::spawn_local(client_io_task);

    (agent_conn, client_conn)
}
