//! 提示词构建工具
//!
//! 用于构建 AI 代理的提示词，包括系统提示词、用户输入和数据源信息的组合

/// 提示词构建器
#[derive(Debug, Default)]
pub struct PromptBuilder {
    /// 系统提示词
    system_prompt: Option<String>,
}

impl PromptBuilder {
    /// 创建新的提示词构建器
    pub fn new() -> Self {
        Self::default()
    }

    /// 构建基础提示词
    pub fn build(&self, user_input: &str) -> String {
        match &self.system_prompt {
            Some(system) => format!("{}\n\n{}", system, user_input),
            None => user_input.to_string(),
        }
    }

    /// 带数据源构建提示词
    pub fn build_with_data_sources(&self, user_input: &str, _data_sources: &[String]) -> String {
        // 简化实现，实际应该处理数据源
        self.build(user_input)
    }

    /// 设置系统提示词
    pub fn with_system_prompt(mut self, system_prompt: &str) -> Self {
        self.system_prompt = Some(system_prompt.to_string());
        self
    }
}