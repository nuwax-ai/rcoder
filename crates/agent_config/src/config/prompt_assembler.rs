//! 提示词配置组装工具
//!
//! 将用户入参组装为内部使用的配置结构。
//! 简化设计：直接使用入参，无优先级冲突。

use shared_types::{ChatAgentConfig, ChatAgentServerConfig};
use std::collections::HashMap;

use super::servers_config::AgentServersConfig;
use crate::types::agent_config::AgentConfig;
use crate::types::mcp_config::ContextServerConfig;

/// 提示词配置组装器
///
/// 职责：
/// 1. 组装系统提示词（入参 > 默认配置）
/// 2. 应用用户提示词模板
/// 3. 组装 Agent 服务器配置
/// 4. 合并 MCP 服务器配置
pub struct PromptConfigAssembler {
    /// 系统提示词入参
    system_prompt: Option<String>,
    /// 用户提示词模板入参
    user_prompt_template: Option<String>,
    /// Agent 运行时配置入参
    agent_config: Option<ChatAgentConfig>,
    /// 默认配置
    default_config: AgentServersConfig,
}

impl PromptConfigAssembler {
    /// 创建新的配置组装器
    ///
    /// # 参数
    /// - `default_config`: 默认配置（从文件或内置配置加载）
    pub fn new(default_config: AgentServersConfig) -> Self {
        Self {
            system_prompt: None,
            user_prompt_template: None,
            agent_config: None,
            default_config,
        }
    }

    /// 设置系统提示词覆盖
    pub fn with_system_prompt(mut self, system_prompt: Option<String>) -> Self {
        self.system_prompt = system_prompt;
        self
    }

    /// 设置用户提示词模板覆盖
    pub fn with_user_prompt_template(mut self, template: Option<String>) -> Self {
        self.user_prompt_template = template;
        self
    }

    /// 设置 Agent 运行时配置覆盖
    pub fn with_agent_config(mut self, config: Option<ChatAgentConfig>) -> Self {
        self.agent_config = config;
        self
    }

    /// 获取最终的系统提示词
    ///
    /// 逻辑：入参有值则使用入参，否则使用默认配置
    pub fn get_system_prompt(&self, agent_id: &str) -> String {
        // 入参有值且非空，直接使用
        if let Some(ref sp) = self.system_prompt
            && !sp.is_empty() {
                return sp.clone();
            }

        // 使用默认配置
        self.default_config.get_system_prompt(agent_id)
    }

    /// 应用用户提示词模板
    ///
    /// 逻辑：
    /// 1. 如果有模板入参，使用模板替换 `{user_prompt}`
    /// 2. 如果没有模板入参，检查默认配置
    /// 3. 都没有，直接返回原始输入
    pub fn apply_user_prompt(&self, agent_id: &str, user_input: &str) -> String {
        // 入参有模板且非空，使用入参模板
        if let Some(ref template) = self.user_prompt_template
            && !template.is_empty() {
                return template.replace("{user_prompt}", user_input);
            }

        // 检查默认配置中的 user_prompt 模板
        if let Some(agent) = self.default_config.get_agent(agent_id)
            && let Some(ref prompt_config) = agent.user_prompt
                && prompt_config.enabled {
                    return prompt_config.apply(user_input);
                }

        // 无模板，直接返回原始输入
        user_input.to_string()
    }

    /// 获取最终的 Agent 服务器配置
    ///
    /// 逻辑：
    /// 1. 如果入参有 agent_server，与默认配置合并（入参字段覆盖默认值）
    /// 2. 如果入参没有 agent_server，使用默认配置
    pub fn get_agent_server_config(&self, default_agent_id: &str) -> AgentConfig {
        // 获取默认的 Agent 配置
        let default_agent = self
            .default_config
            .get_agent(default_agent_id)
            .cloned()
            .unwrap_or_default();

        // 如果入参有 agent_server 配置，合并覆盖
        if let Some(ref config) = self.agent_config
            && let Some(ref agent_server) = config.agent_server {
                return self.merge_agent_config(&default_agent, agent_server);
            }

        // 使用默认配置
        default_agent
    }

    /// 合并 Agent 配置（入参覆盖默认值）
    fn merge_agent_config(
        &self,
        default: &AgentConfig,
        override_config: &ChatAgentServerConfig,
    ) -> AgentConfig {
        let mut merged = default.clone();

        // agent_id: 入参有值则覆盖
        if let Some(ref agent_id) = override_config.agent_id {
            merged.agent_id = agent_id.clone();
        }

        // command: 入参有值则覆盖
        if let Some(ref command) = override_config.command {
            merged.command = command.clone();
        }

        // args: 入参有值则覆盖（替换而非追加）
        if let Some(ref args) = override_config.args {
            merged.args = args.clone();
        }

        // env: 入参有值则合并（入参优先）
        if let Some(ref env) = override_config.env {
            for (key, value) in env {
                merged.env.insert(key.clone(), value.clone());
            }
        }

        // metadata: 入参有值则合并
        if let Some(ref metadata) = override_config.metadata {
            for (key, value) in metadata {
                merged.metadata.insert(key.clone(), value.clone());
            }
        }

        merged
    }

    /// 获取最终的 MCP 服务器配置
    ///
    /// 逻辑：
    /// 1. 如果入参明确提供了非空的 context_servers，使用入参
    /// 2. 否则使用默认配置
    ///
    /// 注意：即使提供了 agent_config，但 context_servers 为空时，仍使用默认配置
    pub fn get_context_servers(&self) -> HashMap<String, ContextServerConfig> {
        // 入参有非空的 MCP 配置，使用入参
        if let Some(ref config) = self.agent_config
            && config.has_context_servers() {
                return config
                    .context_servers
                    .iter()
                    .map(|(name, chat_config)| {
                        let ctx_config = ContextServerConfig {
                            source: chat_config.source.clone(),
                            enabled: chat_config.enabled,
                            command: chat_config.command.clone(),
                            args: chat_config.args.clone(),
                            env: chat_config.env.clone(),
                        };
                        (name.clone(), ctx_config)
                    })
                    .collect();
            }

        // context_servers 为空或未提供，使用默认配置
        self.default_config.context_servers.clone()
    }

    /// 获取使用的 Agent ID
    ///
    /// 逻辑：入参有指定则使用入参，否则使用默认
    pub fn get_agent_id(&self, default_agent_id: &str) -> String {
        if let Some(ref config) = self.agent_config
            && let Some(ref agent_server) = config.agent_server {
                return agent_server.get_agent_id().to_string();
            }
        default_agent_id.to_string()
    }

    /// 检查是否有系统提示词覆盖
    pub fn has_system_prompt_override(&self) -> bool {
        self.system_prompt.as_ref().is_some_and(|s| !s.is_empty())
    }

    /// 检查是否有用户提示词模板覆盖
    pub fn has_user_prompt_template_override(&self) -> bool {
        self.user_prompt_template
            .as_ref()
            .is_some_and(|s| !s.is_empty())
    }

    /// 检查是否有 Agent 配置覆盖
    pub fn has_agent_config_override(&self) -> bool {
        self.agent_config.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_default_config() -> AgentServersConfig {
        AgentServersConfig::default()
    }

    #[test]
    fn test_system_prompt_with_override() {
        let config = create_test_default_config();
        let assembler = PromptConfigAssembler::new(config)
            .with_system_prompt(Some("自定义系统提示词".to_string()));

        let result = assembler.get_system_prompt("claude-code-acp-ts");
        assert_eq!(result, "自定义系统提示词");
    }

    #[test]
    fn test_system_prompt_without_override() {
        let config = create_test_default_config();
        let assembler = PromptConfigAssembler::new(config);

        let result = assembler.get_system_prompt("claude-code-acp-ts");
        // 应该返回默认系统提示词
        assert!(!result.is_empty());
    }

    #[test]
    fn test_system_prompt_empty_override() {
        let config = create_test_default_config();
        let assembler = PromptConfigAssembler::new(config).with_system_prompt(Some("".to_string()));

        let result = assembler.get_system_prompt("claude-code-acp-ts");
        // 空字符串应该回退到默认值
        assert!(!result.is_empty());
    }

    #[test]
    fn test_user_prompt_with_template() {
        let config = create_test_default_config();
        let assembler = PromptConfigAssembler::new(config)
            .with_user_prompt_template(Some("请用 Rust 完成：{user_prompt}".to_string()));

        let result = assembler.apply_user_prompt("claude-code-acp-ts", "Hello World");
        assert_eq!(result, "请用 Rust 完成：Hello World");
    }

    #[test]
    fn test_user_prompt_without_template() {
        let config = create_test_default_config();
        let assembler = PromptConfigAssembler::new(config);

        let result = assembler.apply_user_prompt("claude-code-acp-ts", "Hello World");
        // 没有模板时应该返回原始输入
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_context_servers_with_override() {
        let config = create_test_default_config();
        let mut context_servers = HashMap::new();
        context_servers.insert(
            "my-mcp".to_string(),
            shared_types::ChatContextServerConfig {
                source: "custom".to_string(),
                enabled: true,
                command: Some("bunx".to_string()),
                args: Some(vec!["-y".to_string(), "my-mcp-server".to_string()]),
                env: None,
            },
        );

        let agent_config = ChatAgentConfig {
            agent_server: None,
            context_servers,
            resource_limits: None,
        };

        let assembler = PromptConfigAssembler::new(config).with_agent_config(Some(agent_config));

        let result = assembler.get_context_servers();
        assert!(result.contains_key("my-mcp"));
        assert_eq!(
            result.get("my-mcp").unwrap().command,
            Some("bunx".to_string())
        );
    }

    #[test]
    fn test_context_servers_without_override() {
        let config = create_test_default_config();
        let assembler = PromptConfigAssembler::new(config);

        let result = assembler.get_context_servers();
        // 应该返回默认的 context servers
        assert!(!result.is_empty());
    }

    #[test]
    fn test_agent_id_with_override() {
        let config = create_test_default_config();
        let agent_config = ChatAgentConfig {
            agent_server: Some(shared_types::ChatAgentServerConfig {
                agent_id: Some("custom-agent".to_string()),
                command: None,
                args: None,
                env: None,
                metadata: None,
            }),
            context_servers: HashMap::new(),
            resource_limits: None,
        };

        let assembler = PromptConfigAssembler::new(config).with_agent_config(Some(agent_config));

        let result = assembler.get_agent_id("claude-code-acp-ts");
        assert_eq!(result, "custom-agent");
    }

    #[test]
    fn test_agent_id_without_override() {
        let config = create_test_default_config();
        let assembler = PromptConfigAssembler::new(config);

        let result = assembler.get_agent_id("claude-code-acp-ts");
        assert_eq!(result, "claude-code-acp-ts");
    }

    #[test]
    fn test_merge_agent_config_env() {
        let config = create_test_default_config();
        let mut override_env = HashMap::new();
        override_env.insert("NEW_VAR".to_string(), "new_value".to_string());

        let agent_config = ChatAgentConfig {
            agent_server: Some(shared_types::ChatAgentServerConfig {
                agent_id: None,
                command: None,
                args: None,
                env: Some(override_env),
                metadata: None,
            }),
            context_servers: HashMap::new(),
            resource_limits: None,
        };

        let assembler = PromptConfigAssembler::new(config).with_agent_config(Some(agent_config));

        let result = assembler.get_agent_server_config("claude-code-acp-ts");
        assert!(result.env.contains_key("NEW_VAR"));
        assert_eq!(result.env.get("NEW_VAR"), Some(&"new_value".to_string()));
    }
}
