use std::{
    path::{Component, PathBuf},
    sync::LazyLock,
};

use agent_client_protocol::{
    AgentSideConnection, ClientSideConnection, ContentBlock, PromptRequest, SessionId,
    TextContent,
}; // bring trait into scope for session_notification

use codex_acp_agent::CodexAgent;
use dashmap::DashMap;
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info};

use super::codex_agent::{EmbeddedCodexClient, start_codex_acp_agent_service};
use crate::{model::{ChatPrompt, ChatPromptResponse, ProjectAndAgentInfo}, proxy_agent::claude_code_agent::start_claude_code_acp_agent_service};
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
            match start_claude_code_acp_agent_service(chat_prompt.clone()).await {
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

// Helper function to create a bidirectional connection
fn create_connection_pair(
    client: &EmbeddedCodexClient,
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
