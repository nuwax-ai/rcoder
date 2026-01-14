// gRPC 脱敏包装器模块

use std::fmt;

/// ModelProviderConfig 的脱敏包装器
///
/// 由于 Rust 孤儿规则限制，无法直接为 prost 生成的类型实现自定义 Debug。
/// 使用 newtype 模式包装后，可以实现自定义的 Debug 输出，自动脱敏敏感信息。
pub struct MaskedModelConfig<'a>(pub &'a crate::grpc::ModelProviderConfig);

impl<'a> fmt::Debug for MaskedModelConfig<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let config = self.0;

        // 脱敏 API Key：只显示前 4 位和后 4 位
        let masked_api_key = config
            .api_key
            .as_ref()
            .map(|key: &String| {
                if key.len() > 8 {
                    format!("{}***{}", &key[..4], &key[key.len() - 4..])
                } else {
                    "***".to_string()
                }
            })
            .unwrap_or_else(|| "None".to_string());

        // 脱敏 API Base URL：对域名进行脱敏（保留路径）
        let masked_api_base = config
            .api_base
            .as_ref()
            .map(|url| crate::grpc_mask::mask_url(url))
            .unwrap_or_else(|| "None".to_string());

        f.debug_struct("ModelProviderConfig")
            .field("id", &config.id)
            .field("provider", &config.provider)
            .field("model", &config.model)
            .field("api_key", &masked_api_key)
            .field("api_base", &masked_api_base)
            .field("requires_openai_auth", &config.requires_openai_auth)
            .field("api_protocol", &config.api_protocol)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_masked_model_config_formatting() {
        // 创建测试用的 ModelProviderConfig
        let config = crate::grpc::ModelProviderConfig {
            id: "198".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            api_key: Some("785c26a9affc49c99d6d13f8be5836bc".to_string()),
            api_base: Some(
                "https://test-code-api.xspaceagi.com/api/anthropic/session-xxx".to_string(),
            ),
            requires_openai_auth: Some(true),
            api_protocol: Some("anthropic".to_string()),
        };

        let masked = MaskedModelConfig(&config);
        let output = format!("{:?}", masked);

        // 验证脱敏
        assert!(!output.contains("785c26a9affc49c99d6d13f8be5836bc"));
        assert!(output.contains("785c***36bc"));
        assert!(!output.contains("test-code-api.xspaceagi.com"));
        assert!(output.contains("tes***gi.com"));
        assert!(output.contains("/api/anthropic/session-xxx"));

        println!("Debug output: {}", output);
    }
}
