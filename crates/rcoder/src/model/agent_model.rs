use std::collections::HashMap;

use agent_client_protocol::SessionId;
use agent_client_protocol::{CancelNotification, PromptRequest};
use anyhow::Result;
use chrono::{DateTime, Utc};
use codex_core::WireApi;
use codex_core::{ModelProviderInfo, config::ConfigToml};
use serde::{Deserialize, Serialize};
use shared_types::{ModelProviderConfig, ModelProviderSafeInfo};
use tokio::sync::{mpsc, oneshot};
use tracing::{error, info, warn};
use utoipa::ToSchema;

use crate::{ChatPrompt, proxy_agent::AcpConnectionInfo};

use crate::proxy_agent::agent_stop_handle::AgentLifecycleGuard;
use codex_core::config::{find_codex_home, load_config_as_toml};

pub static CUSTOM_MODEL_PROVIDER_NAME: &str = "custom";

/// 使用Agent代理的工具类型,都是使用ACP协议包装过的agent代理
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum AgentType {
    /// OpenAI Codex 代理
    Codex,
    /// Claude Code 代理
    Claude,
}

impl Default for AgentType {
    fn default() -> Self {
        Self::Claude
    }
}

impl AgentType {
    /// 根据模型提供商配置自动选择 Agent 类型
    /// - Anthropic 协议使用 Claude Code agent
    /// - OpenAI 或未知协议使用 Codex agent
    /// - 强制使用 Docker 容器模式运行
    pub fn from_model_provider(model_provider: Option<&ModelProviderConfig>) -> Self {
        match model_provider {
            Some(config) => match config.get_api_protocol() {
                shared_types::ModelApiProtocol::Anthropic => AgentType::Claude,
                shared_types::ModelApiProtocol::OpenAI => AgentType::Codex,
            },
            None => AgentType::default(), // 默认使用 Claude
        }
    }

    /// 获取 Agent 类型名称
    pub fn agent_type_name(&self) -> &'static str {
        match self {
            AgentType::Codex => "codex",
            AgentType::Claude => "claude",
        }
    }


    /// 启动 Agent 服务
    /// 强制使用 Docker 容器模式：每个 project_id 对应一个独立的 agent 容器
    pub async fn start_agent_service(
        &self,
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<AcpConnectionInfo> {
        // 强制使用 Docker 容器模式：每个 project_id 创建一个独立的容器来运行 agent 服务
        let docker_manager = std::sync::Arc::new(
            docker_manager::DockerManager::with_default_config().await?
        );

        // 在容器中启动 agent_runer 服务，挂载项目目录，让 agent 修改开发项目
        crate::proxy_agent::docker_container_agent::start_docker_container_agent_service(
            chat_prompt, model_provider, docker_manager
        ).await
    }

}

/// 取消通知请求
pub struct CancelNotificationRequest {
    pub cancel_notification: CancelNotification,
    pub tx: oneshot::Sender<CancelNotificationResponse>,
}

/// 取消通知响应
#[derive(Debug)]
pub struct CancelNotificationResponse {
    pub success: bool,
    pub message: Option<String>,
}
/// Agent 服务状态
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, ToSchema)]
pub enum AgentStatus {
    /// 活跃状态 - 正在处理请求
    Active,
    /// 空闲状态 - 等待新请求
    Idle,
    /// 正在终止
    Terminating,
}

/// 项目id与 Agent 服务池，一个项目对应一个 Agent 服务
///
/// Clone trait 是必需的，因为 DashMap::insert() 要求值类型实现 Clone
#[derive(Clone)]
pub struct ProjectAndAgentInfo {
    /// 项目ID
    pub project_id: String,
    /// 会话ID,agent 服务启动时会创建一个会话ID
    pub session_id: SessionId,
    /// 用于发送 Prompt 的通道
    pub prompt_tx: mpsc::UnboundedSender<PromptRequest>,
    /// 用于发送取消通知的通道
    pub cancel_tx: mpsc::UnboundedSender<CancelNotificationRequest>,
    /// 模型提供商配置
    pub model_provider: Option<ModelProviderConfig>,
    /// 当前活跃的请求ID，用于标识用户请求
    pub request_id: Option<String>,
    /// Agent 服务状态
    pub status: AgentStatus,
    /// 最后活动时间
    pub last_activity: DateTime<Utc>,
    /// 创建时间
    pub created_at: DateTime<Utc>,
    /// Agent生命周期守卫，绑定生命周期，drop 时自动清理
    pub lifecycle_guard: AgentLifecycleGuard,
}

impl Drop for ProjectAndAgentInfo {
    fn drop(&mut self) {
        // 生命周期守卫会自动在drop时清理agent资源
        info!(
            "ProjectAndAgentInfo被drop，生命周期守卫将自动清理agent服务，项目ID: {}",
            self.project_id
        );
    }
}

/// Agent 状态查询响应
#[derive(Debug, Clone, serde::Serialize, ToSchema)]
pub struct AgentStatusResponse {
    /// 项目ID
    #[schema(example = "test_project")]
    pub project_id: String,
    /// Agent 是否存活
    #[schema(example = true)]
    pub is_alive: bool,
    /// 会话ID（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "session123")]
    pub session_id: Option<String>,
    /// Agent 服务状态（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<AgentStatus>,
    /// 最后活动时间（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "2024-01-01T12:00:00Z")]
    pub last_activity: Option<DateTime<Utc>>,
    /// 创建时间（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(example = "2024-01-01T10:00:00Z")]
    pub created_at: Option<DateTime<Utc>>,
    /// 模型提供商安全信息（仅当 is_alive 为 true 时存在）
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_provider: Option<ModelProviderSafeInfo>,
}
