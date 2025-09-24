use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 模型提供商配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelProviderConfig {
    /// 提供商名称 (如: glm, anthropic, openai, qwen, ernie, moonshot)
    pub name: String,
    /// API 基础 URL
    pub base_url: String,
    /// 密钥
    pub api_key: String,
    /// 是否需要 OpenAI 兼容的认证
    pub requires_openai_auth: bool,
    /// 默认模型名称
    pub default_model: String,
    /// 额外的配置参数
    pub extra_params: HashMap<String, String>,
}
