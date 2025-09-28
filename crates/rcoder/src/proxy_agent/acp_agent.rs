use std::{
    path::{Component, PathBuf},
    sync::LazyLock,
};

use agent_client_protocol::{ContentBlock, PromptRequest, SessionId, TextContent}; // bring trait into scope for session_notification

use dashmap::DashMap;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::{
    AgentType,
    model::{ChatPrompt, ChatPromptResponse, ProjectAndAgentInfo},
    proxy_agent::{
        claude_code_agent::start_claude_code_acp_agent_service,
        codex_agent::start_codex_acp_agent_service,
    },
    utils::{ContentBuilder, PromptBuilder},
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
    /// 模型提供商配置
    model_provider: Option<ModelProviderConfig>,
}

impl LocalSetAgentRequest {
    pub fn new(chat_prompt: ChatPrompt, model_provider: Option<ModelProviderConfig>) -> (Self, oneshot::Receiver<ChatPromptResponse>) {
        let (chat_prompt_tx, chat_prompt_rx) = oneshot::channel();

        (
            Self {
                chat_prompt,
                chat_prompt_tx,
                model_provider,
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
        match project_and_agent_info {
            Some(agent_info) => {
                // 发送 prompt 请求

                match build_prompt_to_acp_agent(chat_prompt, agent_info.session_id.clone()).await {
                    Ok(prompt_request) => {
                        if let Err(e) = agent_info.prompt_tx.send(prompt_request) {
                            error!("Failed to send prompt request: {:?}", e);
                        } else {
                            debug!("Prompt已发送");
                        }
                    }
                    Err(e) => {
                        error!("Failed to build prompt request for existing agent: {}", e);
                    }
                }

                // 发送回执消息
                if let Err(e) = request.chat_prompt_tx.send(ChatPromptResponse {
                    project_id: project_id.clone(),
                    session_id: agent_info.session_id.to_string(),
                }) {
                    error!("Failed to send chat prompt response: {:?}", e);
                }
            }
            None => {
                //获取 agent_type,判断使用 codex 还是 claude code
                let agent_type = request.chat_prompt.agent_type.clone();
                //使用传入的模型提供商配置
                let model_provider = request.model_provider.clone();

                //启动 agent 服务,返回 session_id 和 prompt_tx
                let start_agent_result = match agent_type {
                    AgentType::Claude => {
                        start_claude_code_acp_agent_service(
                            chat_prompt.clone(),
                            model_provider.clone(),
                        )
                        .await
                    }
                    AgentType::Codex => {
                        start_codex_acp_agent_service(chat_prompt.clone(), model_provider.clone())
                            .await
                    }
                };
                //创建 agent 服务
                match start_agent_result {
                    Ok(conn_info) => {
                        let project_and_agent_info = ProjectAndAgentInfo {
                            project_id: project_id.clone(),
                            session_id: conn_info.session_id.clone(),
                            prompt_tx: conn_info.prompt_tx.clone(),
                            cancel_tx: conn_info.cancel_tx.clone(),
                            model_provider: model_provider,
                            request_id: request.chat_prompt.request_id.clone(),
                        };
                        //记录项目project_id和 agent 服务信息的映射,一个project_id对应一个 agent 服务,方便复用agent 服务
                        PROJECT_AND_AGENT_INFO_MAP
                            .insert(project_id.clone(), project_and_agent_info.clone());

                        if let Ok(prompt_request) =
                            build_prompt_to_acp_agent(chat_prompt, conn_info.session_id.clone())
                                .await
                        {
                            if let Err(e) = conn_info.prompt_tx.send(prompt_request) {
                                error!("Failed to send prompt request: {:?}", e);
                            } else {
                                info!("Prompt 请求已发送");
                            }
                        } else {
                            error!("Failed to build prompt request");
                        }

                        // 发送回执消息
                        if let Err(e) = request.chat_prompt_tx.send(ChatPromptResponse {
                            project_id: project_id.clone(),
                            session_id: conn_info.session_id.to_string(),
                        }) {
                            error!("Failed to send chat prompt response: {:?}", e);
                        }
                    }
                    Err(e) => {
                        error!("Failed to start ACP agent service: {}", e);
                    }
                }
            }
        }
    }
    debug!("Agent worker finished");
    Ok(())
}

/// 构建 Prompt 请求
pub async fn build_prompt_to_acp_agent(
    prompt: ChatPrompt,
    session_id: SessionId,
) -> Result<PromptRequest> {
    // 构建最终提示词（包含系统提示词和用户输入）
    let final_prompt = PromptBuilder::new()
        .use_simple_prompt(prompt.use_simple_prompt)
        .build(&prompt.prompt);

    // 创建文本内容块
    let text_block = ContentBlock::Text(TextContent {
        text: final_prompt,
        annotations: None,
        meta: None,
    });

    // 创建内容块列表，以文本开始
    let mut content_blocks = vec![text_block];

    // 如果有附件，转换为内容块
    if !prompt.attachments.is_empty() {
        let attachment_blocks = ContentBuilder::attachments_to_content_blocks(
            &prompt.attachments,
            &prompt.project_path,
        )
        .await?;

        content_blocks.extend(attachment_blocks);
    }

    Ok(PromptRequest {
        session_id,
        prompt: content_blocks,
        meta: None,
    })
}
