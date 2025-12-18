//! 系统提示词配置类型
//!
//! 这个模块提供编译时嵌入的默认系统提示词常量和相关辅助功能。
//! 默认系统提示词在编译时从外部文件嵌入，支持运行时通过配置覆盖。

/// 编译时嵌入的默认系统提示词
pub const DEFAULT_SYSTEM_PROMPT: &str = include_str!("../../configs/prompts/frontend_expert.txt");

/// 提示词构建器（为了兼容性保留，推荐直接使用 SystemPromptConfig::get_prompt()）
#[derive(Debug, Clone)]
pub struct PromptBuilder;

impl PromptBuilder {
    /// 构建用户提示词（不包含系统提示词）
    pub fn build_user_prompt(user_prompt: &str) -> String {
        user_prompt.to_string()
    }

    /// 构建带数据源的用户提示词
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
            以下是可供使用的数据源信息，包含了后端API接口、数据库连接等外部数据源。\n\
            在开发前端应用时，你可以使用这些数据源来获取真实数据，例如查询比特币交易额、股票价格、天气信息等。\n\
            请根据开发需求合理使用这些数据源，并确保前端应用能够正确调用相关接口。\n\
            使用 Axios 客户端或 Fetch API 进行 API 调用,或者根据当前框架的接口调用方式,来使用。\n\n\
            {}\n\
            </DATA_SOURCES>",
            user_prompt, data_sources_section
        )
    }
}

/// 格式化数据源信息为可读文本
fn format_data_sources(data_sources: &[String]) -> String {
    if data_sources.is_empty() {
        return "无数据源".to_string();
    }

    let mut formatted = String::new();

    for (index, data_source) in data_sources.iter().enumerate() {
        formatted.push_str(&format!("数据源 {}:\n", index + 1));

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

        assert!(formatted.contains("数据源 1"));
        assert!(formatted.contains("api.example.com"));
    }

    #[test]
    fn test_format_empty_data_sources() {
        let data_sources: Vec<String> = vec![];
        let formatted = format_data_sources(&data_sources);
        assert_eq!(formatted, "无数据源");
    }
}
