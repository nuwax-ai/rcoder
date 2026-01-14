use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use utoipa::ToSchema;

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
#[derive(Clone, Serialize, Deserialize, ToSchema)]
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

    /// 转换为安全的公开信息（不包含敏感字段）
    pub fn to_safe_info(&self) -> ModelProviderSafeInfo {
        ModelProviderSafeInfo {
            id: self.id.clone(),
            name: self.name.clone(),
            api_protocol: self.get_api_protocol(),
            default_model: self.default_model.clone(),
        }
    }

    /// 获取脱敏后的 API Key（只显示前4位和后4位）
    fn mask_api_key(&self) -> String {
        if self.api_key.len() > 8 {
            format!(
                "{}***{}",
                &self.api_key[..4],
                &self.api_key[self.api_key.len() - 4..]
            )
        } else {
            "***".to_string()
        }
    }
}

/// 实现 Display trait，方便日志打印（自动对 API Key 和 URL 进行脱敏）
impl fmt::Display for ModelProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 对 base_url 进行脱敏（使用 grpc_mask::mask_url）
        let masked_base_url = crate::grpc_mask::mask_url(&self.base_url);

        write!(
            f,
            "{{id: {}, name: {}, model: {}, base_url: {}, api_key: {}, requires_openai_auth: {}, api_protocol: {}}}",
            self.id,
            self.name,
            self.default_model,
            masked_base_url,
            self.mask_api_key(),
            self.requires_openai_auth,
            self.api_protocol.as_deref().unwrap_or("None")
        )
    }
}

/// 自定义 Debug trait，脱敏敏感信息（与 Display 保持一致的输出格式）
impl fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 对 base_url 进行脱敏
        let masked_base_url = crate::grpc_mask::mask_url(&self.base_url);

        // 使用与 Display 相同的脱敏格式
        write!(
            f,
            "ModelProviderConfig {{id: {}, name: {}, model: {}, base_url: {}, api_key: {}, requires_openai_auth: {}, api_protocol: {}}}",
            self.id,
            self.name,
            self.default_model,
            masked_base_url,
            self.mask_api_key(),
            self.requires_openai_auth,
            self.api_protocol.as_deref().unwrap_or("None")
        )
    }
}

/// 模型提供商安全信息（不包含敏感字段）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ModelProviderSafeInfo {
    /// 模型id
    #[schema(example = "id")]
    pub id: String,
    /// 提供商名称
    #[schema(example = "openai")]
    pub name: String,
    /// 模型接口协议类型
    #[schema(example = "openai")]
    pub api_protocol: ModelApiProtocol,
    /// 默认模型名称
    #[schema(example = "gpt-4")]
    pub default_model: String,
}
