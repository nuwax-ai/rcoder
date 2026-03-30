use arc_swap::ArcSwap;
use axum::{
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use shared_types::{ApiKeyAuthConfig, ApiKeyAuthError, ApiKeyValidator, HttpResult};
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
    let locale = shared_types::parse_accept_language(
        headers.get("accept-language").and_then(|v| v.to_str().ok()),
    );

    // 提取 API Key
    let api_key = headers.get(API_KEY_HEADER).and_then(|v| v.to_str().ok());

    // 使用共享验证逻辑（无锁，同步）
    match ApiKeyValidator::validate(&api_key_config, path, api_key) {
        Ok(()) => next.run(req).await,

        Err(err) => {
            match err {
                ApiKeyAuthError::Invalid | ApiKeyAuthError::Missing => {
                    warn!("🔒 [API_KEY_AUTH] {} for path: {}", err, path);
                    return api_key_error_response(
                        StatusCode::UNAUTHORIZED,
                        shared_types::error_codes::ERR_API_KEY_AUTH_FAILED,
                        locale,
                    );
                }
                ApiKeyAuthError::ConfigError => {
                    tracing::error!("🔒 [API_KEY_AUTH] {}", err);
                    return api_key_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        shared_types::error_codes::ERR_INTERNAL_SERVER_ERROR,
                        locale,
                    );
                }
            };
        }
    }
}

fn api_key_error_response(status: StatusCode, code: &str, locale: &'static str) -> Response {
    let body = HttpResult::<String>::error_with_locale(code, locale);
    (status, axum::Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::{Router, routing::get};
    use tower::ServiceExt;

    #[test]
    fn test_api_key_header_name() {
        assert_eq!(API_KEY_HEADER, "x-api-key");
    }

    #[tokio::test]
    async fn test_api_key_invalid_returns_json_and_localized_message() {
        let config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: true,
            api_key: "valid-key".to_string(),
        }));

        let app = Router::new()
            .route("/chat", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(move |req, next| {
                api_key_middleware_handler(Arc::clone(&config), req, next)
            }));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/chat")
                    .header("accept-language", "zh-CN")
                    .header("x-api-key", "wrong")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let content_type = response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok());
        assert_eq!(content_type, Some("application/json"));

        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json.get("code").and_then(|v| v.as_str()),
            Some(shared_types::error_codes::ERR_API_KEY_AUTH_FAILED)
        );
        let expected_message = shared_types::get_error_message(
            shared_types::error_codes::ERR_API_KEY_AUTH_FAILED,
            "zh-CN",
        );
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some(expected_message.as_str())
        );
    }

    #[tokio::test]
    async fn test_api_key_invalid_without_accept_language_falls_back_to_en_us() {
        let config = Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig {
            enabled: true,
            api_key: "valid-key".to_string(),
        }));

        let app = Router::new()
            .route("/chat", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(move |req, next| {
                api_key_middleware_handler(Arc::clone(&config), req, next)
            }));

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/chat")
                    .header("x-api-key", "wrong")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let expected_message = shared_types::get_error_message(
            shared_types::error_codes::ERR_API_KEY_AUTH_FAILED,
            "en-US",
        );
        assert_eq!(
            json.get("message").and_then(|v| v.as_str()),
            Some(expected_message.as_str())
        );
    }
}
