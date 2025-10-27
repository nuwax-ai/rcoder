//! ACP 代理服务的公共 trait 定义

use crate::model::{ChatPrompt, ProjectAndAgentInfo};
use anyhow::Result;
use shared_types::ModelProviderConfig;
use tracing::info;

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

    /// 停止agent服务（立即停止）
    async fn stop_agent_service(&self, agent_info: &ProjectAndAgentInfo) -> Result<()>;
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

    async fn stop_agent_service(&self, agent_info: &ProjectAndAgentInfo) -> Result<()> {
        info!(
            "停止[{}] agent服务，项目ID: {}",
            self.agent_type_name(),
            agent_info.project_id
        );
        agent_info.lifecycle_guard.stop_async().await
    }
}
