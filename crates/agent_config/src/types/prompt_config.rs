//! Prompt configuration structures.

use serde::{Deserialize, Serialize};

use super::system_prompt::DEFAULT_SYSTEM_PROMPT;

/// 系统提示词来源
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum SystemPromptSource {
    /// 使用编译时嵌入的默认提示词
    #[default]
    Embedded,
    /// 使用自定义模板内容
    Custom,
}

/// System prompt configuration
///
/// 支持两种模式：
/// 1. `source: "embedded"` - 使用编译时嵌入的默认提示词（忽略 template 字段）
/// 2. `source: "custom"` - 使用 template 字段的自定义内容
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemPromptConfig {
    /// 提示词来源
    /// - "embedded": 使用编译时嵌入的默认提示词
    /// - "custom": 使用 template 字段的自定义内容
    #[serde(default)]
    pub source: SystemPromptSource,

    /// Template content (仅当 source = "custom" 时使用)
    #[serde(default)]
    pub template: String,

    /// Whether enabled (false 时不传递系统提示词)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// User prompt configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserPromptConfig {
    /// Template content with {user_prompt} placeholder
    pub template: String,

    /// Whether enabled
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

/// Default enabled value
fn default_enabled() -> bool {
    true
}

impl Default for SystemPromptConfig {
    fn default() -> Self {
        Self {
            source: SystemPromptSource::Embedded,
            template: String::new(),
            enabled: true,
        }
    }
}

impl SystemPromptConfig {
    /// 创建使用嵌入默认提示词的配置
    pub fn embedded() -> Self {
        Self {
            source: SystemPromptSource::Embedded,
            template: String::new(),
            enabled: true,
        }
    }

    /// 创建使用自定义模板的配置
    pub fn custom(template: String) -> Self {
        Self {
            source: SystemPromptSource::Custom,
            template,
            enabled: true,
        }
    }

    /// 创建禁用的配置（不传递系统提示词）
    pub fn disabled() -> Self {
        Self {
            source: SystemPromptSource::Embedded,
            template: String::new(),
            enabled: false,
        }
    }

    /// 获取系统提示词内容
    ///
    /// - 如果 enabled = false，返回 None
    /// - 如果 source = "embedded"，返回编译时嵌入的默认提示词
    /// - 如果 source = "custom"，返回 template 字段的内容
    pub fn get_prompt(&self) -> Option<&str> {
        if !self.enabled {
            return None;
        }

        match self.source {
            SystemPromptSource::Embedded => Some(DEFAULT_SYSTEM_PROMPT),
            SystemPromptSource::Custom => {
                if self.template.is_empty() {
                    // custom 模式但 template 为空，回退到默认
                    Some(DEFAULT_SYSTEM_PROMPT)
                } else {
                    Some(&self.template)
                }
            }
        }
    }

    /// 获取系统提示词内容（兼容旧接口，始终返回有效提示词）
    ///
    /// 如果 enabled = false，仍返回默认提示词（用于需要始终有提示词的场景）
    pub fn get_prompt_or_default(&self) -> &str {
        match self.source {
            SystemPromptSource::Embedded => DEFAULT_SYSTEM_PROMPT,
            SystemPromptSource::Custom => {
                if self.template.is_empty() {
                    DEFAULT_SYSTEM_PROMPT
                } else {
                    &self.template
                }
            }
        }
    }

    /// 检查是否使用自定义模板
    pub fn is_custom(&self) -> bool {
        self.source == SystemPromptSource::Custom && !self.template.is_empty()
    }

    /// 检查是否使用嵌入的默认提示词
    pub fn is_embedded(&self) -> bool {
        self.source == SystemPromptSource::Embedded
    }
}

impl UserPromptConfig {
    /// Create a new user prompt config
    pub fn new(template: String) -> Self {
        Self {
            template,
            enabled: true,
        }
    }

    /// Create disabled config
    pub fn disabled() -> Self {
        Self {
            template: String::new(),
            enabled: false,
        }
    }

    /// Apply user prompt template
    pub fn apply(&self, user_input: &str) -> String {
        if self.enabled {
            self.template.replace("{user_prompt}", user_input)
        } else {
            user_input.to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_embedded_source() {
        let config = SystemPromptConfig::embedded();
        assert!(config.is_embedded());
        assert!(!config.is_custom());
        assert!(config.get_prompt().is_some());
        assert!(config.get_prompt().unwrap().contains("<SYSTEM_INSTRUCTIONS>"));
    }

    #[test]
    fn test_custom_source() {
        let config = SystemPromptConfig::custom("Custom prompt".to_string());
        assert!(config.is_custom());
        assert!(!config.is_embedded());
        assert_eq!(config.get_prompt(), Some("Custom prompt"));
    }

    #[test]
    fn test_disabled_config() {
        let config = SystemPromptConfig::disabled();
        assert!(config.get_prompt().is_none());
        // get_prompt_or_default 仍返回默认值
        assert!(config.get_prompt_or_default().contains("<SYSTEM_INSTRUCTIONS>"));
    }

    #[test]
    fn test_custom_empty_template_fallback() {
        let config = SystemPromptConfig {
            source: SystemPromptSource::Custom,
            template: String::new(),
            enabled: true,
        };
        // 空模板时回退到默认
        assert!(config.get_prompt().unwrap().contains("<SYSTEM_INSTRUCTIONS>"));
    }

    #[test]
    fn test_json_deserialization() {
        let json = r#"{"source": "embedded", "template": "", "enabled": true}"#;
        let config: SystemPromptConfig = serde_json::from_str(json).unwrap();
        assert!(config.is_embedded());
        assert!(config.enabled);
    }

    #[test]
    fn test_json_deserialization_custom() {
        let json = r#"{"source": "custom", "template": "My prompt", "enabled": true}"#;
        let config: SystemPromptConfig = serde_json::from_str(json).unwrap();
        assert!(config.is_custom());
        assert_eq!(config.get_prompt(), Some("My prompt"));
    }
}
