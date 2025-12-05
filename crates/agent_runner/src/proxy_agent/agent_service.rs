//! ACP 代理服务的公共 trait 定义

use anyhow::Result;
use shared_types::ModelProviderConfig;

/// ACP 代理服务 trait
///
/// 定义了启动和管理 ACP 代理服务的统一接口
#[async_trait::async_trait(?Send)]
pub trait AcpAgentService {
    /// 启动 ACP 代理服务
    ///
    /// # 参数
    /// * `prompt_message` - Agent 抽象层的 Prompt 消息
    /// * `model_provider` - 模型提供商配置
    ///
    /// # 返回值
    /// 返回 ACP 连接信息
    async fn start_agent_service(
        &self,
        prompt_message: agent_abstraction::PromptMessage,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<super::AcpConnectionInfo>;

    /// 获取代理类型名称
    fn agent_type_name(&self) -> &'static str;
}

/// Claude Agent 服务
pub struct ClaudeAgentService;

#[async_trait::async_trait(?Send)]
impl AcpAgentService for ClaudeAgentService {
    async fn start_agent_service(
        &self,
        prompt_message: agent_abstraction::PromptMessage,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<super::AcpConnectionInfo> {
        super::claude_code_agent::start_claude_code_acp_agent_service(
            prompt_message,
            model_provider,
        )
        .await
    }

    fn agent_type_name(&self) -> &'static str {
        "Claude"
    }
}

/// 获取默认的 Agent 服务
pub fn get_default_agent_service() -> ClaudeAgentService {
    ClaudeAgentService
}
