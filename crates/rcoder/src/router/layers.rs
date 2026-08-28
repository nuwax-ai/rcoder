//! 全局中间件与安全响应头（从 router.rs 拆出；层序敏感，语义见各段注释）。

use std::sync::Arc;

use arc_swap::ArcSwap;

use axum::{
    Router, extract::DefaultBodyLimit, http::Request, middleware::Next, response::Response,
};
use rcoder_telemetry::HttpMetricsLayer;

async fn locale_context_middleware(mut req: Request<axum::body::Body>, next: Next) -> Response {
    let locale = shared_types::parse_accept_language(
        req.headers()
            .get("accept-language")
            .and_then(|v| v.to_str().ok()),
    );

    req.extensions_mut().insert(locale);

    shared_types::scope_request_locale(locale, async move { next.run(req).await }).await
}

/// 业务面全局中间件链：body 限制 → Trace（traceparent 贯通）→ HTTP 指标 →
/// API Key 鉴权（支持热更新）→ locale 注入。
///
/// internal / file-server 两面在此链**之后** merge = 不受 API Key 约束
/// （router.rs 既有语义，见 create_router 装配顺序）。
pub(super) fn apply_global_middleware(
    router: Router,
    api_key_config: Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>,
) -> Router {
    router
        .layer(DefaultBodyLimit::max(50 * 1024 * 1024)) // 50MB body 大小限制
        // HTTP 请求日志（target: tower_http → rcoder.log）+ W3C traceparent 提取
        // （入站 trace 贯通：e2e 注入 traceparent 时请求 span 继承远端 trace）
        .layer(
            tower_http::trace::TraceLayer::new_for_http().make_span_with(
                |req: &Request<axum::body::Body>| {
                    rcoder_telemetry::make_span_with_trace_parent(req)
                },
            ),
        )
        .layer(HttpMetricsLayer::new()) // HTTP 指标中间件
        // API Key 鉴权中间件（支持热更新）
        .layer(axum::middleware::from_fn(move |req, next| {
            crate::middleware::api_key_middleware::api_key_middleware_handler(
                Arc::clone(&api_key_config),
                req,
                next,
            )
        }))
        .layer(axum::middleware::from_fn(locale_context_middleware))
}

/// 安全响应头五连（覆盖全部路由面，包括 internal / file-server）。
pub(super) fn apply_security_headers(router: Router) -> Router {
    router
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-frame-options"),
            axum::http::HeaderValue::from_static("DENY"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-content-type-options"),
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("strict-transport-security"),
            axum::http::HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("referrer-policy"),
            axum::http::HeaderValue::from_static("strict-origin-when-cross-origin"),
        ))
        .layer(tower_http::set_header::SetResponseHeaderLayer::overriding(
            axum::http::header::HeaderName::from_static("x-xss-protection"),
            axum::http::HeaderValue::from_static("0"),
        ))
}
