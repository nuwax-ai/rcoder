use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;
use utoipa::ToSchema;

/// 模型接口协议类型
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema, PartialEq)]
#[serde(rename_all = "lowercase")]
#[derive(Default)]
pub enum ModelApiProtocol {
    /// Anthropic Claude API 协议
    #[default]
    Anthropic,
    /// OpenAI 兼容 API 协议
    OpenAI,
}

impl FromStr for ModelApiProtocol {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "anthropic" => Ok(ModelApiProtocol::Anthropic),
            "openai" => Ok(ModelApiProtocol::OpenAI),
            // 未知值报错而非静默回退 —— 错误配置(如 "openai-compatible")按错误协议
            // 发请求只会到上游 400, 排障时无从分辨; 由消费方决定回退策略并留痕
            _ => Err(()),
        }
    }
}

impl fmt::Display for ModelApiProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelApiProtocol::Anthropic => f.write_str("anthropic"),
            ModelApiProtocol::OpenAI => f.write_str("openai"),
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
    /// 模型接口协议类型 (anthropic/openai)，未指定或未知值时按 anthropic 处理（未知值会打 warn 日志）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "openai")]
    pub api_protocol: Option<String>,
    /// 线路 API 格式: "chat" 表示 Chat Completions API, "response" 表示 Responses API (默认)
    /// 当 wire_api == "chat" 时，代理会将 Responses API 请求转换为 Chat API 请求
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(example = "chat")]
    pub wire_api: Option<String>,
}

impl ModelProviderConfig {
    /// 获取模型接口协议：未指定 → 默认 anthropic；未知值 → warn 留痕后回退 anthropic
    /// （保持既有行为兼容，但配置错误可观测，不再静默按错误协议发请求）
    pub fn get_api_protocol(&self) -> ModelApiProtocol {
        self.api_protocol
            .as_ref()
            .and_then(|s| match ModelApiProtocol::from_str(s) {
                Ok(p) => Some(p),
                Err(()) => {
                    tracing::warn!(protocol = %s, "unknown api_protocol, falling back to anthropic");
                    None
                }
            })
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
    /// 使用 char-based 切片避免 UTF-8 边界 panic
    fn mask_api_key(&self) -> String {
        let chars: Vec<char> = self.api_key.chars().collect();
        if chars.len() > 8 {
            let prefix: String = chars[..4].iter().collect();
            let suffix: String = chars[chars.len() - 4..].iter().collect();
            format!("{}***{}", prefix, suffix)
        } else {
            "***".to_string()
        }
    }
}

/// 实现 Display trait，方便日志打印（自动对 API Key 和 URL 进行脱敏）
impl fmt::Display for ModelProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 对 base_url 进行脱敏（使用 shared_types_grpc::mask_url）
        let masked_base_url = shared_types_grpc::mask_url(&self.base_url);

        write!(
            f,
            "{{id: {}, name: {}, model: {}, base_url: {}, api_key: {}, requires_openai_auth: {}, api_protocol: {}, wire_api: {}}}",
            self.id,
            self.name,
            self.default_model,
            masked_base_url,
            self.mask_api_key(),
            self.requires_openai_auth,
            self.api_protocol.as_deref().unwrap_or("None"),
            self.wire_api.as_deref().unwrap_or("None")
        )
    }
}

/// 自定义 Debug trait，脱敏敏感信息（与 Display 保持一致的输出格式）
impl fmt::Debug for ModelProviderConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // 对 base_url 进行脱敏
        let masked_base_url = shared_types_grpc::mask_url(&self.base_url);

        // 使用与 Display 相同的脱敏格式
        write!(
            f,
            "ModelProviderConfig {{id: {}, name: {}, model: {}, base_url: {}, api_key: {}, requires_openai_auth: {}, api_protocol: {}, wire_api: {}}}",
            self.id,
            self.name,
            self.default_model,
            masked_base_url,
            self.mask_api_key(),
            self.requires_openai_auth,
            self.api_protocol.as_deref().unwrap_or("None"),
            self.wire_api.as_deref().unwrap_or("None")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(protocol: Option<&str>) -> ModelProviderConfig {
        ModelProviderConfig {
            id: "p".into(),
            name: "n".into(),
            base_url: "https://example.com".into(),
            api_key: "k".into(),
            requires_openai_auth: false,
            default_model: "m".into(),
            api_protocol: protocol.map(Into::into),
            wire_api: None,
        }
    }

    #[test]
    fn from_str_rejects_unknown_protocol() {
        assert!("openai-compatible".parse::<ModelApiProtocol>().is_err());
        assert!("bogus".parse::<ModelApiProtocol>().is_err());
        assert_eq!(
            "openai".parse::<ModelApiProtocol>().unwrap(),
            ModelApiProtocol::OpenAI
        );
        // 大小写不敏感
        assert_eq!(
            "Anthropic".parse::<ModelApiProtocol>().unwrap(),
            ModelApiProtocol::Anthropic
        );
    }

    #[test]
    fn get_api_protocol_falls_back_with_observation() {
        // 未指定 → 默认 anthropic
        assert_eq!(config(None).get_api_protocol(), ModelApiProtocol::Anthropic);
        // 未知值 → 同样回退 anthropic（行为兼容），区别只在 warn 留痕
        assert_eq!(
            config(Some("openai-compatible")).get_api_protocol(),
            ModelApiProtocol::Anthropic
        );
        assert_eq!(
            config(Some("openai")).get_api_protocol(),
            ModelApiProtocol::OpenAI
        );
    }

    #[test]
    fn display_matches_serde_lowercase() {
        assert_eq!(ModelApiProtocol::OpenAI.to_string(), "openai");
        assert_eq!(ModelApiProtocol::Anthropic.to_string(), "anthropic");
    }
}
