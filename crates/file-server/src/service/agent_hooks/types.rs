//! agent_hooks 公共输入类型 (对齐 nuwax writeAgentHookConfigs 的 options)。

/// 单个 hook 外挂脚本 (对齐 nuwax hookScripts 数组项: {path, content})。
#[derive(Debug, Clone, serde::Deserialize)]
pub struct HookScript {
    pub path: String,
    pub content: String,
}

/// write_agent_hook_configs 输入 (对齐 nuwax writeAgentHookConfigs 的 options)。
#[derive(Debug, Default)]
pub struct HookConfigInput {
    /// mcpServersConfig: JSON 字符串, 解析后写入 `.mcp.json` 的 mcpServers 字段。
    pub mcp_servers_config: Option<String>,
    /// hooksConfig: JSON 字符串, 解析后写入 `.claude/settings.json` hooks + 转 Codex hooks.json。
    pub hooks_config: Option<String>,
    /// permissionsConfig: JSON 字符串, 解析后写入 `.claude/settings.json` permissions 字段。
    pub permissions_config: Option<String>,
    /// hookScripts: 外挂脚本数组, 写入 `.claude/hooks/`。
    pub hook_scripts: Option<Vec<HookScript>>,
}
