//! System prompt configuration types
//!
//! This module provides compile-time embedded default system prompt constants and related helper functions.
//! Default system prompt is embedded at compile time from external files, with runtime override support via configuration.

/// Compile-time embedded default system prompt
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../../configs/prompts/frontend_expert.txt");

/// Prompt builder (kept for compatibility, recommend using SystemPromptConfig::get_prompt() directly)
#[derive(Debug, Clone)]
pub struct PromptBuilder;

impl PromptBuilder {
    /// Build user prompt (without system prompt)
    pub fn build_user_prompt(user_prompt: &str) -> String {
        user_prompt.to_string()
    }

    /// Build user prompt with data sources
    pub fn build_user_prompt_with_data_sources(
        user_prompt: &str,
        data_sources: &[String],
    ) -> String {
        if data_sources.is_empty() {
            return user_prompt.to_string();
        }

        let data_sources_section = format_data_sources(data_sources);
        format!(
            "{}\n\n\
            <DATA_SOURCES>\n\
            The following are available data source information, including backend API endpoints, database connections, and other external data sources.\n\
            When developing frontend applications, you can use these data sources to fetch real data, such as querying Bitcoin transaction volumes, stock prices, weather information, etc.\n\
            Please use these data sources reasonably according to development needs, and ensure the frontend application can correctly call the relevant interfaces.\n\
            Use Axios client or Fetch API for API calls, or according to the interface calling method of the current framework.\n\n\
            {}\n\
            </DATA_SOURCES>",
            user_prompt, data_sources_section
        )
    }
}

/// Format data source information into readable text
fn format_data_sources(data_sources: &[String]) -> String {
    if data_sources.is_empty() {
        return "No data sources".to_string();
    }

    let mut formatted = String::new();

    for (index, data_source) in data_sources.iter().enumerate() {
        formatted.push_str(&format!("Data source {}:\n", index + 1));

        // 尝试解析 JSON 字符串并格式化
        match serde_json::from_str::<serde_json::Value>(data_source) {
            Ok(json_value) => {
                // 成功解析，格式化为易读的 JSON
                match serde_json::to_string_pretty(&json_value) {
                    Ok(pretty_json) => {
                        formatted.push_str(&pretty_json);
                    }
                    Err(_) => {
                        // 格式化失败，使用原始字符串
                        formatted.push_str(data_source);
                    }
                }
            }
            Err(_) => {
                // 不是有效的 JSON，直接使用原始字符串
                formatted.push_str(data_source);
            }
        }

        formatted.push('\n');
    }

    formatted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_system_prompt_loaded() {
        // 验证默认提示词被正确加载
        assert!(!DEFAULT_SYSTEM_PROMPT.is_empty());
        assert!(DEFAULT_SYSTEM_PROMPT.contains("<SYSTEM_INSTRUCTIONS>"));
        assert!(DEFAULT_SYSTEM_PROMPT.contains("</SYSTEM_INSTRUCTIONS>"));
    }

    #[test]
    fn test_format_data_sources() {
        let data_sources = vec![r#"{"api": "https://api.example.com"}"#.to_string()];
        let formatted = format_data_sources(&data_sources);

        assert!(formatted.contains("Data source 1"));
        assert!(formatted.contains("api.example.com"));
    }

    #[test]
    fn test_format_empty_data_sources() {
        let data_sources: Vec<String> = vec![];
        let formatted = format_data_sources(&data_sources);
        assert_eq!(formatted, "No data sources");
    }
}
