use axum::{
    extract::Request,
    http::HeaderMap,
    middleware::Next,
    response::{IntoResponse, Response},
};
use shared_types::{HttpResult, error_codes};
use std::sync::{Arc, RwLock};
use tracing::warn;

use crate::config::ApiKeyAuthConfig;

const API_KEY_HEADER: &str = "x-api-key";

// 豁免列表：不需要鉴权的端点路径
const EXEMPT_PATHS: &[&str] = &[
    "/health",       // 健康检查
    "/metrics",      // Prometheus 指标
    "/api/docs",     // Swagger UI 文档根路径
    "/proxy/status", // 代理服务状态
    "/proxy/stats",  // 代理统计信息
];

/// 检查路径是否应该豁免鉴权
fn is_exempt_path(path: &str) -> bool {
    // 精确匹配
    if EXEMPT_PATHS.contains(&path) {
        return true;
    }

    // 前缀匹配（用于 Swagger UI 的所有子路径）
    if path.starts_with("/api/docs/") {
        return true;
    }

    false
}

/// 中间件处理函数（支持配置热更新）
pub async fn api_key_middleware_handler(
    api_key_config: Arc<RwLock<ApiKeyAuthConfig>>,
    req: Request,
    next: Next,
) -> Response {
    // 读取最新配置（支持热更新）
    let (enabled, expected_key) = match api_key_config.read() {
        Ok(config) => (config.enabled, config.api_key.clone()),
        Err(e) => {
            // 锁被毒化,记录错误并拒绝请求
            tracing::error!("🔒 [API_KEY_AUTH] 读取配置失败(锁被毒化): {}", e);
            return HttpResult::<()>::error(
                error_codes::ERR_INTERNAL_SERVER_ERROR,
                "Configuration error",
            )
            .into_response();
        }
    };

    // 如果未启用鉴权，直接放行
    if !enabled {
        return next.run(req).await;
    }

    // 检查是否是豁免路径
    let path = req.uri().path();
    if is_exempt_path(path) {
        return next.run(req).await;
    }

    // 提取 x-api-key header
    let headers = req.headers();
    let api_key = extract_api_key(headers);

    match api_key {
        Some(key) if key == expected_key => {
            // 密钥匹配，放行
            next.run(req).await
        }
        Some(_) => {
            // 密钥不匹配
            warn!(
                "🔒 [API_KEY_AUTH] Invalid API key provided for path: {}",
                path
            );
            HttpResult::<()>::error(error_codes::ERR_API_KEY_AUTH_FAILED, "Invalid API key")
                .into_response()
        }
        None => {
            // 缺少 x-api-key header
            warn!(
                "🔒 [API_KEY_AUTH] Missing x-api-key header for path: {}",
                path
            );
            HttpResult::<()>::error(
                error_codes::ERR_API_KEY_AUTH_FAILED,
                "Missing x-api-key header",
            )
            .into_response()
        }
    }
}

/// 从请求头中提取 API Key
fn extract_api_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get(API_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_exempt_path() {
        // 精确匹配
        assert!(is_exempt_path("/health"));
        assert!(is_exempt_path("/metrics"));
        assert!(is_exempt_path("/api/docs"));
        assert!(is_exempt_path("/proxy/status"));
        assert!(is_exempt_path("/proxy/stats"));

        // Swagger UI 子路径
        assert!(is_exempt_path("/api/docs/openapi.json"));
        assert!(is_exempt_path("/api/docs/swagger-ui.css"));

        // 不豁免的路径
        assert!(!is_exempt_path("/chat"));
        assert!(!is_exempt_path("/agent/stop"));
        assert!(!is_exempt_path("/computer/chat"));
    }

    #[test]
    fn test_extract_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert(API_KEY_HEADER, "test-key-123".parse().unwrap());

        let result = extract_api_key(&headers);
        assert_eq!(result, Some("test-key-123".to_string()));
    }

    #[test]
    fn test_extract_api_key_missing() {
        let headers = HeaderMap::new();
        let result = extract_api_key(&headers);
        assert_eq!(result, None);
    }
}
