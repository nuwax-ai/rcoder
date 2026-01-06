use arc_swap::ArcSwap;
use axum::{
    extract::Request,
    middleware::Next,
    response::{IntoResponse, Response},
};
use shared_types::{ApiKeyAuthConfig, ApiKeyValidator, HttpResult, error_codes};
use std::sync::Arc;
use tracing::warn;

const API_KEY_HEADER: &str = "x-api-key";

/// 中间件处理函数（支持配置热更新，使用 ArcSwap 无锁读取）
pub async fn api_key_middleware_handler(
    api_key_config: Arc<ArcSwap<ApiKeyAuthConfig>>,
    req: Request,
    next: Next,
) -> Response {
    let path = req.uri().path();
    let headers = req.headers();

    // 提取 API Key
    let api_key = headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok());

    // 使用共享验证逻辑（无锁，同步）
    match ApiKeyValidator::validate(&api_key_config, path, api_key) {
        Ok(()) => next.run(req).await,
        Err("invalid") => {
            warn!(
                "🔒 [API_KEY_AUTH] Invalid API key provided for path: {}",
                path
            );
            HttpResult::<()>::error(error_codes::ERR_API_KEY_AUTH_FAILED, "Invalid API key")
                .into_response()
        }
        Err("missing") => {
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
        Err(_) => {
            tracing::error!("🔒 [API_KEY_AUTH] Configuration error");
            HttpResult::<()>::error(
                error_codes::ERR_INTERNAL_SERVER_ERROR,
                "Configuration error",
            )
            .into_response()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_api_key_header_name() {
        assert_eq!(API_KEY_HEADER, "x-api-key");
    }
}
