use arc_swap::ArcSwap;
use std::sync::Arc;

/// API Key 鉴权配置
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ApiKeyAuthConfig {
    /// 是否启用 API Key 鉴权
    pub enabled: bool,
    /// API Key 值
    pub api_key: String,
}

impl Default for ApiKeyAuthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(), // 默认为空，实际使用时应该生成随机密钥
        }
    }
}

/// API Key 验证器
pub struct ApiKeyValidator;

impl ApiKeyValidator {
    /// 豁免路径列表（无需鉴权）
    const EXEMPT_PATHS: &'static [&'static str] = &[
        "/health",
        "/metrics",
        "/api/docs",
        "/proxy/status",
        "/proxy/stats",
    ];

    /// 检查路径是否豁免鉴权
    pub fn is_exempt_path(path: &str) -> bool {
        // 精确匹配
        if Self::EXEMPT_PATHS.contains(&path) {
            return true;
        }

        // 前缀匹配（用于 Swagger UI 的所有子路径）
        if path.starts_with("/api/docs/") {
            return true;
        }

        false
    }

    /// 验证 API Key (无锁版本，使用 ArcSwap)
    ///
    /// 返回:
    /// - Ok(()) - 验证通过
    /// - Err("missing") - 缺少 API Key
    /// - Err("invalid") - API Key 无效
    pub fn validate(
        api_key_config: &Arc<ArcSwap<ApiKeyAuthConfig>>,
        path: &str,
        api_key: Option<&str>,
    ) -> Result<(), &'static str> {
        // 🚀 无锁读取配置（原子操作，极快）
        let config = api_key_config.load();

        // 如果未启用鉴权，直接放行
        if !config.enabled {
            return Ok(());
        }

        // 检查是否豁免路径
        if Self::is_exempt_path(path) {
            return Ok(());
        }

        // 验证 API Key（直接比较，无需 clone）
        match api_key {
            Some(key) if key == config.api_key.as_str() => Ok(()),
            Some(_) => Err("invalid"),
            None => Err("missing"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_exempt_path() {
        assert!(ApiKeyValidator::is_exempt_path("/health"));
        assert!(ApiKeyValidator::is_exempt_path("/metrics"));
        assert!(ApiKeyValidator::is_exempt_path("/api/docs"));
        assert!(ApiKeyValidator::is_exempt_path("/api/docs/openapi.json"));
        assert!(!ApiKeyValidator::is_exempt_path("/chat"));
        assert!(!ApiKeyValidator::is_exempt_path(
            "/computer/vnc/user1/proj1/vnc.html"
        ));
    }

    #[test]
    fn test_validate() {
        let config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: true,
            api_key: "test-key-123".to_string(),
        }));

        // 正确的 API Key
        assert!(ApiKeyValidator::validate(&config, "/chat", Some("test-key-123")).is_ok());

        // 错误的 API Key
        assert_eq!(
            ApiKeyValidator::validate(&config, "/chat", Some("wrong-key")),
            Err("invalid")
        );

        // 缺少 API Key
        assert_eq!(
            ApiKeyValidator::validate(&config, "/chat", None),
            Err("missing")
        );

        // 豁免路径
        assert!(ApiKeyValidator::validate(&config, "/health", None).is_ok());
    }

    #[test]
    fn test_validate_disabled() {
        let config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: false,
            api_key: "test-key-123".to_string(),
        }));

        // 未启用鉴权时，任何请求都应该通过
        assert!(ApiKeyValidator::validate(&config, "/chat", None).is_ok());
        assert!(ApiKeyValidator::validate(&config, "/chat", Some("wrong-key")).is_ok());
    }

    #[test]
    fn test_hot_reload() {
        // 测试配置热更新
        let config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: true,
            api_key: "old-key".to_string(),
        }));

        // 使用旧密钥验证通过
        assert!(ApiKeyValidator::validate(&config, "/chat", Some("old-key")).is_ok());
        assert_eq!(
            ApiKeyValidator::validate(&config, "/chat", Some("new-key")),
            Err("invalid")
        );

        // 🔄 热更新配置
        config.store(Arc::new(ApiKeyAuthConfig {
            enabled: true,
            api_key: "new-key".to_string(),
        }));

        // 使用新密钥验证通过，旧密钥失败
        assert!(ApiKeyValidator::validate(&config, "/chat", Some("new-key")).is_ok());
        assert_eq!(
            ApiKeyValidator::validate(&config, "/chat", Some("old-key")),
            Err("invalid")
        );
    }
}
