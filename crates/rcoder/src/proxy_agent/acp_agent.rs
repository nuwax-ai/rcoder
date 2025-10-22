use std::{
    path::{Component, PathBuf},
    sync::LazyLock,
};

use agent_client_protocol::{ContentBlock, PromptRequest, SessionId, TextContent}; // bring trait into scope for session_notification

use chrono::Utc;
use dashmap::DashMap;
use shared_types::ModelProviderConfig;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info};

use crate::{
    AgentStatus, ChatPrompt, ChatPromptResponse, ProjectAndAgentInfo,
};

use anyhow::Result;

/// 使用 OnceLock 和 DashMap 管理 ProjectAndAgentInfo
pub static PROJECT_AND_AGENT_INFO_MAP: LazyLock<DashMap<String, ProjectAndAgentInfo>> =
    LazyLock::new(DashMap::new);

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
    pub fn new(
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> (Self, oneshot::Receiver<ChatPromptResponse>) {
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
