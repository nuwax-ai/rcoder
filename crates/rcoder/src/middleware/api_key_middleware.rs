use arc_swap::ArcSwap;
use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use shared_types::{ApiKeyAuthConfig, ApiKeyAuthError, ApiKeyValidator};
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

        Err(err) => {
            // 根据错误类型确定 HTTP 状态码
            let status_code = match err {
                ApiKeyAuthError::Invalid | ApiKeyAuthError::Missing => {
                    warn!("🔒 [API_KEY_AUTH] {} for path: {}", err, path);
                    StatusCode::UNAUTHORIZED
                }
                ApiKeyAuthError::ConfigError => {
                    tracing::error!("🔒 [API_KEY_AUTH] {}", err);
                    StatusCode::INTERNAL_SERVER_ERROR
                }
            };

            // 返回标准 HTTP 错误响应
            (status_code, err.to_string()).into_response()
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
