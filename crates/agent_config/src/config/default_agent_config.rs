//! 默认 Agent 配置定义
//!
//! 此模块从编译时嵌入的 JSON 配置文件加载默认配置。
//! 支持两种服务类型的配置：
//! - RCoder: `configs/default_agents.json`
//! - ComputerAgentRunner: `configs/computer_agent_default.json`

use std::collections::HashMap;
use std::sync::LazyLock;

use crate::types::agent_config::AgentConfig;
use crate::types::mcp_config::ContextServerConfig;

/// 编译时嵌入的 RCoder 默认配置 JSON
const DEFAULT_CONFIG_JSON: &str = include_str!("../../configs/default_agents.json");

/// 编译时嵌入的 ComputerAgentRunner 默认配置 JSON
const COMPUTER_AGENT_CONFIG_JSON: &str = include_str!("../../configs/computer_agent_default.json");

/// Claude Code ACP Agent 的默认 ID
pub const CLAUDE_CODE_ACP_AGENT_ID: &str = "claude-code-acp";

/// 默认配置的内部结构（用于 JSON 反序列化）
#[derive(serde::Deserialize)]
pub struct EmbeddedConfig {
    agent_servers: HashMap<String, AgentConfig>,
    #[serde(default)]
    context_servers: HashMap<String, ContextServerConfig>,
}

/// 懒加载的默认配置
///
/// 在首次访问时解析 JSON，之后复用解析结果。
/// 如果 JSON 解析失败，程序会 panic（这是编译时嵌入的文件，不应失败）。
static DEFAULT_CONFIG: LazyLock<EmbeddedConfig> = LazyLock::new(|| {
    serde_json::from_str(DEFAULT_CONFIG_JSON).expect(
        "Failed to parse embedded default_agents.json. This is a bug - please check the JSON syntax.",
    )
});

/// 懒加载的 ComputerAgentRunner 默认配置
///
/// 在首次访问时解析 JSON，之后复用解析结果。
static COMPUTER_AGENT_CONFIG: LazyLock<EmbeddedConfig> = LazyLock::new(|| {
    serde_json::from_str(COMPUTER_AGENT_CONFIG_JSON).expect(
        "Failed to parse embedded computer_agent_default.json. This is a bug - please check the JSON syntax.",
    )
});

/// 获取默认的 Claude Code ACP Agent 配置
///
/// 从 `configs/default_agents.json` 加载配置。
/// 修改 JSON 文件后重新编译即可生效。
///
/// # Panics
/// 如果 JSON 中不存在 `claude-code-acp` 配置，会 panic。
pub fn default_claude_code_agent() -> AgentConfig {
    DEFAULT_CONFIG
        .agent_servers
        .get(CLAUDE_CODE_ACP_AGENT_ID)
        .cloned()
        .expect("claude-code-acp not found in default_agents.json")
}

/// 获取默认的 Context Servers 配置
///
/// 从 `configs/default_agents.json` 加载配置。
/// 修改 JSON 文件后重新编译即可生效。
pub fn default_context_servers() -> HashMap<String, ContextServerConfig> {
    DEFAULT_CONFIG.context_servers.clone()
}

/// 获取默认的 Agent Servers 配置
///
/// 从 `configs/default_agents.json` 加载配置。
/// 修改 JSON 文件后重新编译即可生效。
pub fn default_agent_servers() -> HashMap<String, AgentConfig> {
    DEFAULT_CONFIG.agent_servers.clone()
}

/// 获取指定 Agent 的默认配置
///
/// # 参数
/// - `agent_id`: Agent 标识符
///
/// # 返回
/// 如果存在返回配置的克隆，否则返回 None
pub fn get_default_agent(agent_id: &str) -> Option<AgentConfig> {
    DEFAULT_CONFIG.agent_servers.get(agent_id).cloned()
}

/// 获取指定 Context Server 的默认配置
///
/// # 参数
/// - `name`: Context Server 名称
///
/// # 返回
/// 如果存在返回配置的克隆，否则返回 None
pub fn get_default_context_server(name: &str) -> Option<ContextServerConfig> {
    DEFAULT_CONFIG.context_servers.get(name).cloned()
}

/// 根据服务类型获取默认配置
///
/// # 参数
/// - `service_type`: 服务类型（RCoder 或 ComputerAgentRunner）
///
/// # 返回
/// 对应服务类型的默认配置引用
pub fn get_default_config_by_service_type(
    service_type: &shared_types::ServiceType,
) -> &'static EmbeddedConfig {
    match service_type {
        shared_types::ServiceType::RCoder => &DEFAULT_CONFIG,
        shared_types::ServiceType::ComputerAgentRunner => &COMPUTER_AGENT_CONFIG,
    }
}

/// 根据服务类型获取默认 Agent Servers 配置
///
/// # 参数
/// - `service_type`: 服务类型
///
/// # 返回
/// Agent Servers 配置的克隆
pub fn default_agent_servers_for_service(
    service_type: &shared_types::ServiceType,
) -> HashMap<String, AgentConfig> {
    get_default_config_by_service_type(service_type)
        .agent_servers
        .clone()
}

/// 根据服务类型获取默认 Context Servers 配置
///
/// # 参数
/// - `service_type`: 服务类型
///
/// # 返回
/// Context Servers 配置的克隆
pub fn default_context_servers_for_service(
    service_type: &shared_types::ServiceType,
) -> HashMap<String, ContextServerConfig> {
    get_default_config_by_service_type(service_type)
        .context_servers
        .clone()
}

/// 根据服务类型获取指定 Agent 的默认配置
///
/// # 参数
/// - `agent_id`: Agent 标识符
/// - `service_type`: 服务类型
///
/// # 返回
/// 如果存在返回配置的克隆，否则返回 None
pub fn get_default_agent_for_service(
    agent_id: &str,
    service_type: &shared_types::ServiceType,
) -> Option<AgentConfig> {
    get_default_config_by_service_type(service_type)
        .agent_servers
        .get(agent_id)
        .cloned()
}

/// 根据服务类型获取指定 Context Server 的默认配置
///
/// # 参数
/// - `name`: Context Server 名称
/// - `service_type`: 服务类型
///
/// # 返回
/// 如果存在返回配置的克隆，否则返回 None
pub fn get_default_context_server_for_service(
    name: &str,
    service_type: &shared_types::ServiceType,
) -> Option<ContextServerConfig> {
    get_default_config_by_service_type(service_type)
        .context_servers
        .get(name)
        .cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_loads_successfully() {
        // 确保 JSON 能成功解析
        let _ = &*DEFAULT_CONFIG;
    }

    #[test]
    fn test_default_claude_code_agent() {
        let agent = default_claude_code_agent();
        assert_eq!(agent.agent_id, CLAUDE_CODE_ACP_AGENT_ID);
        assert_eq!(agent.command, "claude-code-acp");
        assert!(agent.enabled);
        assert!(agent.env.contains_key("ANTHROPIC_API_KEY"));
    }

    #[test]
    fn test_default_context_servers() {
        let servers = default_context_servers();
        // 验证 JSON 中定义的 context servers
        assert!(servers.contains_key("fetch") || servers.contains_key("context7"));
    }

    #[test]
    fn test_default_agent_servers() {
        let agents = default_agent_servers();
        assert!(agents.contains_key(CLAUDE_CODE_ACP_AGENT_ID));
    }

    #[test]
    fn test_get_default_agent() {
        let agent = get_default_agent(CLAUDE_CODE_ACP_AGENT_ID);
        assert!(agent.is_some());
        assert_eq!(agent.unwrap().agent_id, CLAUDE_CODE_ACP_AGENT_ID);

        let non_existent = get_default_agent("non-existent-agent");
        assert!(non_existent.is_none());
    }

    #[test]
    fn test_get_default_context_server() {
        // 测试存在的 server
        let servers = default_context_servers();
        if let Some(first_server_name) = servers.keys().next() {
            let server = get_default_context_server(first_server_name);
            assert!(server.is_some());
        }

        // 测试不存在的 server
        let non_existent = get_default_context_server("non-existent-server");
        assert!(non_existent.is_none());
    }

    #[test]
    fn test_computer_agent_config_loads_successfully() {
        // 确保 ComputerAgentRunner 配置能成功解析
        let _ = &*COMPUTER_AGENT_CONFIG;
    }

    #[test]
    fn test_get_default_config_by_service_type() {
        // 测试 RCoder 配置
        let rcoder_config =
            get_default_config_by_service_type(&shared_types::ServiceType::RCoder);
        assert!(rcoder_config
            .agent_servers
            .contains_key(CLAUDE_CODE_ACP_AGENT_ID));

        // 测试 ComputerAgentRunner 配置
        let car_config = get_default_config_by_service_type(
            &shared_types::ServiceType::ComputerAgentRunner,
        );
        assert!(car_config
            .agent_servers
            .contains_key(CLAUDE_CODE_ACP_AGENT_ID));

        // ComputerAgentRunner 应该有更多 context servers
        assert!(
            car_config.context_servers.contains_key("chrome-devtools")
                || car_config.context_servers.contains_key("puppeteer")
        );
    }

    #[test]
    fn test_default_agent_servers_for_service() {
        // RCoder
        let rcoder_servers =
            default_agent_servers_for_service(&shared_types::ServiceType::RCoder);
        assert!(!rcoder_servers.is_empty());

        // ComputerAgentRunner
        let car_servers =
            default_agent_servers_for_service(&shared_types::ServiceType::ComputerAgentRunner);
        assert!(!car_servers.is_empty());
    }

    #[test]
    fn test_default_context_servers_for_service() {
        // RCoder - 基础 MCP servers
        let rcoder_contexts =
            default_context_servers_for_service(&shared_types::ServiceType::RCoder);
        assert!(
            rcoder_contexts.contains_key("fetch") || rcoder_contexts.contains_key("context7")
        );

        // ComputerAgentRunner - 增强 MCP servers
        let car_contexts =
            default_context_servers_for_service(&shared_types::ServiceType::ComputerAgentRunner);
        // 至少应该包含浏览器相关的 server
        assert!(car_contexts.len() >= rcoder_contexts.len());
    }

    #[test]
    fn test_get_default_agent_for_service() {
        // 测试 RCoder
        let agent = get_default_agent_for_service(
            CLAUDE_CODE_ACP_AGENT_ID,
            &shared_types::ServiceType::RCoder,
        );
        assert!(agent.is_some());

        // 测试 ComputerAgentRunner
        let agent = get_default_agent_for_service(
            CLAUDE_CODE_ACP_AGENT_ID,
            &shared_types::ServiceType::ComputerAgentRunner,
        );
        assert!(agent.is_some());

        // 测试不存在的 agent
        let agent = get_default_agent_for_service(
            "non-existent-agent",
            &shared_types::ServiceType::RCoder,
        );
        assert!(agent.is_none());
    }

    #[test]
    fn test_get_default_context_server_for_service() {
        // RCoder context server
        let rcoder_contexts =
            default_context_servers_for_service(&shared_types::ServiceType::RCoder);
        if let Some(first_server_name) = rcoder_contexts.keys().next() {
            let server = get_default_context_server_for_service(
                first_server_name,
                &shared_types::ServiceType::RCoder,
            );
            assert!(server.is_some());
        }

        // ComputerAgentRunner context server
        let _server = get_default_context_server_for_service(
            "chrome-devtools",
            &shared_types::ServiceType::ComputerAgentRunner,
        );
        // 可能存在或不存在，取决于配置

        // 测试不存在的 server
        let server = get_default_context_server_for_service(
            "non-existent-server",
            &shared_types::ServiceType::RCoder,
        );
        assert!(server.is_none());
    }
}
