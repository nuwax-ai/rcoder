use std::fmt;

/// 系统提示词配置
#[derive(Debug, Clone)]
pub struct SystemPromptConfig {
    /// 基础系统提示词
    pub base_prompt: String,
    /// 代码助手角色定义
    pub role_definition: String,
    /// 代码输出格式要求
    pub code_format_rules: String,
    /// MCP 工具使用指导
    pub mcp_tool_guidance: String,
    /// 思考过程要求
    pub thinking_requirements: String,
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            base_prompt: String::from(
                "You are an advanced AI coding assistant integrated with MCP (Model Context Protocol) tools. \
                You are designed to emulate the world's most proficient developers and are always up-to-date \
                with the latest technologies and best practices. Your goal is to deliver clear, efficient, \
                concise, and innovative coding solutions while maintaining a friendly and approachable demeanor.",
            ),
            role_definition: String::from(
                "You have access to various MCP tools including context7 for web search and documentation retrieval. \
                You can analyze code, search for documentation, write code in multiple programming languages, \
                and assist with various development tasks. Always use the available tools when they can help \
                provide better answers.",
            ),
            code_format_rules: String::from(
                "When writing code:\n\
                1. Always write complete, runnable code snippets\n\
                2. Include necessary imports and dependencies\n\
                3. Follow language-specific best practices\n\
                4. Add comments for complex logic\n\
                5. Ensure code is properly formatted and readable\n\
                6. Consider error handling and edge cases\n\
                7. Use appropriate variable and function names",
            ),
            mcp_tool_guidance: String::from(
                "Available MCP Tools:\n\
                - context7: Search the web, retrieve documentation, and gather information\n\
                \n\
                Guidelines for tool usage:\n\
                1. Use context7 when you need to search for current information, documentation, or examples\n\
                2. Always formulate specific search queries for better results\n\
                3. Synthesize information from multiple sources when possible\n\
                4. Provide citations or references when using external information",
            ),
            thinking_requirements: String::from(
                "Before responding, always:\n\
                1. Analyze the user's request carefully\n\
                2. Determine if any tools are needed to gather information\n\
                3. Plan your approach step by step\n\
                4. Consider the best programming language and framework for the task\n\
                5. Think about potential edge cases and error handling",
            ),
        }
    }
}

impl SystemPromptConfig {
    /// 创建完整的系统提示词
    pub fn build_system_prompt(&self) -> String {
        format!(
            "<SYSTEM_INSTRUCTIONS>\n\n\
            {}\n\n\
            <ROLE_DEFINITION>\n\
            {}\n\n\
            <CODE_FORMAT_RULES>\n\
            {}\n\n\
            <MCP_TOOL_GUIDANCE>\n\
            {}\n\n\
            <THINKING_REQUIREMENTS>\n\
            {}\n\n\
            </SYSTEM_INSTRUCTIONS>",
            self.base_prompt,
            self.role_definition,
            self.code_format_rules,
            self.mcp_tool_guidance,
            self.thinking_requirements
        )
    }

    /// 包装用户提示词
    pub fn wrap_user_prompt(&self, user_prompt: &str) -> String {
        let system_prompt = self.build_system_prompt();
        format!(
            "{}\n\n\
            <USER_REQUEST>\n\
            {}\n\
            </USER_REQUEST>",
            system_prompt, user_prompt
        )
    }
}

/// 提示词构建器
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    config: SystemPromptConfig,
}

impl PromptBuilder {
    pub fn new() -> Self {
        Self {
            config: SystemPromptConfig::default(),
        }
    }

    /// 使用自定义配置
    pub fn with_config(mut self, config: SystemPromptConfig) -> Self {
        self.config = config;
        self
    }

    /// 构建最终提示词
    pub fn build(&self, user_prompt: &str) -> String {
        self.config.wrap_user_prompt(user_prompt)
    }
}

impl Default for PromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_system_prompt_config() {
        let config = SystemPromptConfig::default();
        assert!(!config.base_prompt.is_empty());
        assert!(!config.role_definition.is_empty());
        assert!(!config.code_format_rules.is_empty());
        assert!(!config.mcp_tool_guidance.is_empty());
        assert!(!config.thinking_requirements.is_empty());
    }

    #[test]
    fn test_build_system_prompt() {
        let config = SystemPromptConfig::default();
        let system_prompt = config.build_system_prompt();

        assert!(system_prompt.contains("<SYSTEM_INSTRUCTIONS>"));
        assert!(system_prompt.contains("<ROLE_DEFINITION>"));
        assert!(system_prompt.contains("<CODE_FORMAT_RULES>"));
        assert!(system_prompt.contains("<MCP_TOOL_GUIDANCE>"));
        assert!(system_prompt.contains("<THINKING_REQUIREMENTS>"));
        assert!(system_prompt.contains("</SYSTEM_INSTRUCTIONS>"));
    }

    #[test]
    fn test_wrap_user_prompt() {
        let config = SystemPromptConfig::default();
        let user_prompt = "Write a hello world function in Rust";
        let wrapped = config.wrap_user_prompt(user_prompt);

        assert!(wrapped.contains("<SYSTEM_INSTRUCTIONS>"));
        assert!(wrapped.contains("<USER_REQUEST>"));
        assert!(wrapped.contains(user_prompt));
        assert!(wrapped.contains("</USER_REQUEST>"));
    }

    #[test]
    fn test_prompt_builder() {
        let user_prompt = "Create a React component";

        // 测试默认构建器
        let default_prompt = PromptBuilder::new().build(user_prompt);
        assert!(default_prompt.contains("<SYSTEM_INSTRUCTIONS>"));
        assert!(default_prompt.contains("<USER_REQUEST>"));
        assert!(default_prompt.contains(user_prompt));
    }
}
