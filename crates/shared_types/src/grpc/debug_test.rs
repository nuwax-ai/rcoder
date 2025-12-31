#[cfg(test)]
mod tests {
    use super::super::grpc::ModelProviderConfig;

    #[test]
    fn test_model_config_debug_masking() {
        let config = ModelProviderConfig {
            id: "198".to_string(),
            provider: "glm-4.6-anthropic".to_string(),
            model: "glm-4.6".to_string(),
            api_key: Some("785c26a9affc49c99d6d13f8be5836bc".to_string()),
            api_base: Some("https://test-code-api.xspaceagi.com/api/anthropic/session-62f9fbe19dff4e41bce2cfaf19c14b64".to_string()),
            requires_openai_auth: Some(true),
            api_protocol: Some("anthropic".to_string()),
        };

        let debug_output = format!("{:?}", config);

        // 验证 API Key 被脱敏
        assert!(!debug_output.contains("785c26a9affc49c99d6d13f8be5836bc"));
        assert!(debug_output.contains("785c***36bc"));

        // 验证 URL 中的 session ID 被脱敏
        assert!(!debug_output.contains("session-62f9fbe19dff4e41bce2cfaf19c14b64"));
        assert!(debug_output.contains("***"));

        // 验证其他字段正常显示
        assert!(debug_output.contains("198"));
        assert!(debug_output.contains("glm-4.6-anthropic"));
        assert!(debug_output.contains("glm-4.6"));

        println!("Debug output: {}", debug_output);
    }

    #[test]
    fn test_model_config_debug_with_short_api_key() {
        let config = ModelProviderConfig {
            id: "test".to_string(),
            provider: "test-provider".to_string(),
            model: "test-model".to_string(),
            api_key: Some("short".to_string()), // 短 API Key
            api_base: None,
            requires_openai_auth: None,
            api_protocol: None,
        };

        let debug_output = format!("{:?}", config);

        // 短 API Key 应该显示为 ***
        assert!(debug_output.contains("***"));
        assert!(!debug_output.contains("short"));
    }

    #[test]
    fn test_model_config_debug_without_session_in_url() {
        let config = ModelProviderConfig {
            id: "openai-test".to_string(),
            provider: "openai".to_string(),
            model: "gpt-4".to_string(),
            api_key: Some("sk-proj-1234567890abcdef".to_string()),
            api_base: Some("https://api.openai.com/v1/chat/completions".to_string()),
            requires_openai_auth: Some(false),
            api_protocol: Some("openai".to_string()),
        };

        let debug_output = format!("{:?}", config);

        // 验证 API Key 被脱敏
        assert!(!debug_output.contains("sk-proj-1234567890abcdef"));
        assert!(debug_output.contains("sk-p***cdef"));

        // 验证普通 API URL 仍然可读但进行了脱敏
        assert!(debug_output.contains("api.openai.com"));
        assert!(debug_output.contains("***"));
    }
}
