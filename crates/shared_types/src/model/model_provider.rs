
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use std::str::FromStr;

/// 模型接口协议类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ModelApiProtocol {
    /// Anthropic Claude API 协议
    Anthropic,
    /// OpenAI 兼容 API 协议
    OpenAI,
}

impl Default for ModelApiProtocol {
    fn default() -> Self {
        Self::Anthropic
    }
}

impl FromStr for ModelApiProtocol {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "anthropic" => Ok(ModelApiProtocol::Anthropic),
            "openai" => Ok(ModelApiProtocol::OpenAI),
            _ => Ok(ModelApiProtocol::Anthropic), // 未知协议默认为 Anthropic
        }
    }
}

impl ToString for ModelApiProtocol {
    fn to_string(&self) -> String {
        match self {
            ModelApiProtocol::Anthropic => "Anthropic".to_string(),
            ModelApiProtocol::OpenAI => "Openai".to_string(),
        }
    }
}

/// 模型提供商配置
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelProviderConfig {
    /// 模型id,确保唯一性
    #[schema(example = "id")]
    pub id: String,
    /// 提供商名称 (如: glm, anthropic, openai, qwen, ernie, moonshot)
    #[schema(example = "openai")]
    pub name: String,
    /// API 基础 URL
    #[schema(example = "https://api.openai.com/v1")]
    pub base_url: String,
    /// 密钥
    #[schema(example = "sk-...")]
    pub api_key: String,
    /// 是否需要 OpenAI 兼容的认证
    #[schema(example = true)]
    pub requires_openai_auth: bool,
    /// 默认模型名称
    #[schema(example = "gpt-4")]
    pub default_model: String,
    /// 模型接口协议类型 (anthropic/openai)，默认为 openai
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "openai")]
    pub api_protocol: Option<String>,
}

impl ModelProviderConfig {
    /// 获取模型接口协议，如果未指定则默认为 OpenAI
    pub fn get_api_protocol(&self) -> ModelApiProtocol {
        self.api_protocol
            .as_ref()
            .map(|s| ModelApiProtocol::from_str(s).unwrap_or_default())
            .unwrap_or_default()
    }
}
