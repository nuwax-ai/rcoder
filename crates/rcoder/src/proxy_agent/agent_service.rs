//! ACP 代理服务的公共 trait 定义

use crate::model::{ChatPrompt, ProjectAndAgentInfo};
use agent_client_protocol::ClientCapabilities;
use anyhow::Result;
use shared_types::ModelProviderConfig;
use std::time::Duration;
use tracing::{debug, info};

/// ACP 代理服务 trait
///
/// 定义了启动和管理 ACP 代理服务的统一接口
#[async_trait::async_trait(?Send)]
pub trait AcpAgentService {
    /// 启动 ACP 代理服务
    ///
    /// # 参数
    /// * `chat_prompt` - 聊天提示词
    /// * `model_provider` - 模型提供商配置
    ///
    /// # 返回值
    /// 返回 ACP 连接信息
    async fn start_agent_service(
        &self,
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<super::AcpConnectionInfo>;

    /// 获取代理类型名称
    fn agent_type_name(&self) -> &'static str;

    /// 获取客户端能力配置
    fn get_client_capabilities(&self) -> ClientCapabilities;

    /// 取消agent服务（协作式取消）
    fn cancel_agent_service(&self, agent_info: &ProjectAndAgentInfo);

    /// 停止agent服务（立即停止）
    async fn stop_agent_service(&self, agent_info: &ProjectAndAgentInfo) -> Result<()>;

    /// 检查agent是否闲置
    fn is_agent_idle(&self, agent_info: &ProjectAndAgentInfo, timeout: Duration) -> bool;

    /// 获取agent的CancellationToken（用于任务协作取消）
    fn get_cancellation_token(&self, agent_info: &ProjectAndAgentInfo) -> Option<tokio_util::sync::CancellationToken>;
}

/// 为 AgentType 实现 AcpAgentService trait
#[async_trait::async_trait(?Send)]
impl AcpAgentService for crate::model::AgentType {
    async fn start_agent_service(
        &self,
        chat_prompt: ChatPrompt,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<super::AcpConnectionInfo> {
        match self {
            crate::model::AgentType::Claude => {
                super::claude_code_agent::start_claude_code_acp_agent_service(
                    chat_prompt,
                    model_provider,
                )
                .await
            }
            crate::model::AgentType::Codex => {
                super::codex_agent::start_codex_acp_agent_service(chat_prompt, model_provider).await
            }
        }
    }

    fn agent_type_name(&self) -> &'static str {
        match self {
            crate::model::AgentType::Claude => "Claude",
            crate::model::AgentType::Codex => "Codex",
        }
    }

    fn get_client_capabilities(&self) -> ClientCapabilities {
        match self {
            crate::model::AgentType::Claude => ClientCapabilities {
                fs: agent_client_protocol::FileSystemCapability {
                    read_text_file: false,
                    write_text_file: false,
                    meta: None,
                },
                terminal: false,
                meta: None,
            },
            crate::model::AgentType::Codex => ClientCapabilities::default(),
        }
    }

    fn cancel_agent_service(&self, agent_info: &ProjectAndAgentInfo) {
        if let Some(stop_handle) = &agent_info.stop_handle {
            info!(
                "发送取消信号到[{}] agent服务，项目ID: {}",
                self.agent_type_name(),
                agent_info.project_id
            );
            stop_handle.cancel();
        } else {
            info!(
                "[{}] agent服务没有停止句柄，项目ID: {}",
                self.agent_type_name(),
                agent_info.project_id
            );
        }
    }

    async fn stop_agent_service(&self, agent_info: &ProjectAndAgentInfo) -> Result<()> {
        if let Some(stop_handle) = &agent_info.stop_handle {
            info!(
                "停止[{}] agent服务，项目ID: {}",
                self.agent_type_name(),
                agent_info.project_id
            );
            stop_handle.stop_async().await?;
        } else {
            info!(
                "[{}] agent服务没有停止句柄，项目ID: {}",
                self.agent_type_name(),
                agent_info.project_id
            );
        }
        Ok(())
    }

    fn is_agent_idle(&self, agent_info: &ProjectAndAgentInfo, timeout: Duration) -> bool {
        use crate::model::AgentStatus;
        use chrono::Utc;

        match agent_info.status {
            AgentStatus::Idle => {
                let idle_duration = Utc::now().signed_duration_since(agent_info.last_activity);
                idle_duration.num_seconds() > timeout.as_secs() as i64
            }
            _ => false,
        }
    }

    fn get_cancellation_token(&self, agent_info: &ProjectAndAgentInfo) -> Option<tokio_util::sync::CancellationToken> {
        if let Some(stop_handle) = &agent_info.stop_handle {
            Some(stop_handle.cancellation_token())
        } else {
            None
        }
    }
}
