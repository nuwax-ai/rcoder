//! 版本信息处理器

use axum::Json;
use shared_types::HttpResult;

/// 版本信息端点
///
/// 返回 rcoder 服务版本号与跨平台系统信息（编译期 os/arch + 运行时
/// OS 版本/内核版本）。免鉴权（`EXEMPT_PATHS` 白名单），供调用方
/// 与运维排障探询。
#[utoipa::path(
    get,
    path = "/version",
    responses(
        (status = 200, description = "服务版本与系统信息", body = HttpResult<shared_types::VersionResponse>)
    ),
    tag = "system"
)]
pub async fn version_check() -> Json<HttpResult<shared_types::VersionResponse>> {
    let version_response =
        shared_types::VersionResponse::new("rcoder-ai-service", env!("CARGO_PKG_VERSION"));

    Json(HttpResult::success(version_response))
}

#[cfg(test)]
mod tests {
    use super::*;
    use arc_swap::ArcSwap;
    use axum::body::to_bytes;
    use axum::{
        Router,
        routing::{get, post},
    };
    use std::sync::Arc;
    use tower::ServiceExt;

    #[tokio::test]
    async fn version_check_returns_service_version_and_platform() {
        let Json(result) = version_check().await;
        let data = result.data.expect("success response should carry data");
        assert_eq!(data.service, "rcoder-ai-service");
        assert_eq!(data.version, env!("CARGO_PKG_VERSION"));
        assert!(!data.system_info.os.is_empty());
        assert!(!data.system_info.arch.is_empty());
        assert_eq!(result.code, "0000");
    }

    /// 免鉴权守卫：/version 过 API Key 中间件后无 key 仍 200，
    /// 业务路径（/chat）无 key 401——防 EXEMPT_PATHS 白名单回归。
    #[tokio::test]
    async fn version_is_exempt_from_api_key_auth() {
        let config = Arc::new(ArcSwap::from_pointee(shared_types::ApiKeyAuthConfig {
            enabled: true,
            api_key: "secret".to_string(),
        }));
        let app = Router::new()
            .route("/version", get(version_check))
            .route("/chat", post(|| async { "ok" }))
            .layer(axum::middleware::from_fn(move |req, next| {
                crate::middleware::api_key_middleware::api_key_middleware_handler(
                    Arc::clone(&config),
                    req,
                    next,
                )
            }));

        let version_resp = app
            .clone()
            .oneshot(
                axum::http::Request::builder()
                    .uri("/version")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(version_resp.status(), axum::http::StatusCode::OK);
        let body = to_bytes(version_resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json.get("data")
                .and_then(|d| d.get("version"))
                .and_then(|v| v.as_str()),
            Some(env!("CARGO_PKG_VERSION"))
        );

        let chat_resp = app
            .oneshot(
                axum::http::Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/chat")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(chat_resp.status(), axum::http::StatusCode::UNAUTHORIZED);
    }
}
