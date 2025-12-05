//! Claude Code ACP Agent 兼容层
//!
//! 这个模块提供了与现有 `agent_runner::proxy_agent::claude_code_agent`
//! 的兼容层，允许现有代码无缝迁移到新的 Agent 抽象层。
//!
//! Note: 实际的 ACP 连接逻辑在 `agent_runner::proxy_agent::claude_code_agent` 中实现。
//! 这个模块提供抽象层接口，用于未来的扩展。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use agent_client_protocol::SessionId;
use agent_config::AgentConfig;
use async_trait::async_trait;
use shared_types::ModelProviderConfig;
use tokio::sync::mpsc;
use tracing::info;
use uuid::Uuid;

use crate::acp::CancelNotificationRequestWrapper;
use crate::error::AgentAbstractionError;
use crate::launcher::SubprocessLauncher;
use crate::traits::agent::PromptMessage;
use crate::{Agent, AgentConnection, AgentLifecycleManager, AgentStatus, ProcessLaunchInfo};

/// Claude Code ACP Agent 配置
#[derive(Debug, Clone)]
pub struct ClaudeCodeAcpAgentConfig {
    /// 命令路径，默认为 "claude-code-acp"
    pub command: String,
    /// 命令参数
    pub args: Vec<String>,
    /// 是否启用调试模式
    pub debug: bool,
}

impl Default for ClaudeCodeAcpAgentConfig {
    fn default() -> Self {
        Self {
            command: "claude-code-acp".to_string(),
            args: Vec::new(),
            debug: false,
        }
    }
}

/// Claude Code ACP Agent 兼容实现
///
/// 这个结构体提供了 Agent trait 的实现框架。
/// 实际的 ACP 连接逻辑在 `agent_runner` 中实现。
pub struct ClaudeCodeAcpAgent {
    /// 代理 ID
    id: String,
    /// 代理配置（旧版配置，保持兼容）
    legacy_config: ClaudeCodeAcpAgentConfig,
    /// Agent 配置（新版标准配置）
    agent_config: Option<AgentConfig>,
    /// 生命周期管理器
    lifecycle_manager: Arc<AgentLifecycleManager>,
    /// 子进程启动器
    launcher: SubprocessLauncher,
}

impl ClaudeCodeAcpAgent {
    /// 创建新的 Claude Code ACP Agent 实例
    pub fn new(
        lifecycle_manager: Arc<AgentLifecycleManager>,
        config: Option<ClaudeCodeAcpAgentConfig>,
    ) -> Self {
        let id = format!("claude_code_acp_agent_{}", Uuid::new_v4());
        Self {
            id,
            legacy_config: config.unwrap_or_default(),
            agent_config: None,
            lifecycle_manager,
            launcher: SubprocessLauncher::new(),
        }
    }

    /// 创建带完整配置的 Claude Code ACP Agent 实例
    pub fn with_agent_config(
        lifecycle_manager: Arc<AgentLifecycleManager>,
        agent_config: AgentConfig,
    ) -> Self {
        let id = format!("claude_code_acp_agent_{}", Uuid::new_v4());
        let legacy_config = ClaudeCodeAcpAgentConfig {
            command: agent_config.command.clone(),
            args: agent_config.args.clone(),
            debug: false,
        };
        Self {
            id,
            legacy_config,
            agent_config: Some(agent_config),
            lifecycle_manager,
            launcher: SubprocessLauncher::new(),
        }
    }

    /// 获取旧版配置
    pub fn legacy_config(&self) -> &ClaudeCodeAcpAgentConfig {
        &self.legacy_config
    }

    /// 设置 Agent 配置
    pub fn set_agent_config(&mut self, config: AgentConfig) {
        self.legacy_config.command = config.command.clone();
        self.legacy_config.args = config.args.clone();
        self.agent_config = Some(config);
    }

    /// 构建环境变量
    pub fn build_environment(
        &self,
        model_provider: Option<ModelProviderConfig>,
    ) -> Result<HashMap<String, String>, AgentAbstractionError> {
        let mut env = HashMap::new();

        // 添加默认环境变量
        env.insert("RUST_LOG".to_string(), "info".to_string());

        // 从 ModelProviderConfig 提取环境变量
        if let Some(config) = model_provider {
            if !config.api_key.is_empty() {
                env.insert("ANTHROPIC_API_KEY".to_string(), config.api_key);
            }

            if !config.base_url.is_empty() {
                env.insert("ANTHROPIC_BASE_URL".to_string(), config.base_url);
            }

            if !config.default_model.is_empty() {
                env.insert("ANTHROPIC_MODEL".to_string(), config.default_model);
            }
        }

        Ok(env)
    }
}

#[async_trait(?Send)]
impl Agent for ClaudeCodeAcpAgent {
    async fn start(&self, prompt: PromptMessage) -> Result<AgentConnection, AgentAbstractionError> {
        // 直接使用 PromptMessage 的类型安全字段
        let project_path = prompt.project_path.clone();
        let project_id = prompt.project_id.clone();
        let session_id = prompt.session_id.clone();
        let service_type = prompt.service_type;

        info!(
            "Claude Code ACP Agent start called for project: {}",
            project_id
        );

        // 创建占位符通道
        let (cancel_tx, _cancel_rx) = mpsc::unbounded_channel();
        let (prompt_tx, _prompt_rx) = mpsc::unbounded_channel();

        // 创建 AgentConnection
        let connection = AgentConnection::new(
            project_id,
            service_type,
            session_id.map(SessionId::new),
            Arc::new(prompt_tx),
            Arc::new(cancel_tx),
        );

        Ok(connection)
    }

    async fn stop(&self, _session_id: &SessionId) -> Result<(), AgentAbstractionError> {
        self.lifecycle_manager
            .stop_agent(&self.id)
            .await
            .map_err(AgentAbstractionError::Lifecycle)
    }

    async fn get_status(
        &self,
        session_id: &SessionId,
    ) -> Result<AgentStatus, AgentAbstractionError> {
        self.lifecycle_manager
            .get_agent_status(&self.id, session_id)
            .await
            .map_err(AgentAbstractionError::Lifecycle)
    }

    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        "claude-code-acp"
    }

    fn config(&self) -> Option<&AgentConfig> {
        self.agent_config.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_agent() {
        let lifecycle_manager = Arc::new(AgentLifecycleManager::new());
        let agent = ClaudeCodeAcpAgent::new(lifecycle_manager, None);

        assert_eq!(agent.name(), "claude-code-acp");
        assert!(agent.id().starts_with("claude_code_acp_agent_"));
    }

    #[test]
    fn test_default_config() {
        let config = ClaudeCodeAcpAgentConfig::default();
        assert_eq!(config.command, "claude-code-acp");
        assert!(config.args.is_empty());
        assert!(!config.debug);
    }

    #[test]
    fn test_build_environment() {
        let lifecycle_manager = Arc::new(AgentLifecycleManager::new());
        let agent = ClaudeCodeAcpAgent::new(lifecycle_manager, None);

        // 测试空配置
        let env = agent.build_environment(None).unwrap();
        assert!(env.contains_key("RUST_LOG"));

        // 测试带 API key 的配置
        let model_provider = Some(ModelProviderConfig {
            id: "test".to_string(),
            name: "anthropic".to_string(),
            api_key: "test-key".to_string(),
            base_url: "https://api.anthropic.com".to_string(),
            requires_openai_auth: false,
            default_model: "claude-3".to_string(),
            api_protocol: None,
        });
        let env = agent.build_environment(model_provider).unwrap();
        assert_eq!(env.get("ANTHROPIC_API_KEY"), Some(&"test-key".to_string()));
        assert_eq!(env.get("ANTHROPIC_MODEL"), Some(&"claude-3".to_string()));
    }
}
