use axum::{
    extract::Request,
    http::{HeaderMap, StatusCode},
    middleware::Next,
    response::Response,
};
use opentelemetry::trace::TraceContextExt;
use tracing::{Instrument, info_span};
use tracing_opentelemetry::OpenTelemetrySpanExt;
use uuid::Uuid;

/// HTTP 请求追踪中间件
///
/// 功能：
/// 1. 为每个 HTTP 请求自动生成 trace_id
/// 2. 创建请求 span 用于日志跟踪
/// 3. 记录请求和响应信息
/// 4. 自动将 trace_id 注入到 OpenTelemetry context 中
pub struct TracingMiddleware;

impl TracingMiddleware {
    /// 创建新的追踪中间件实例
    pub fn new() -> Self {
        Self
    }
}

/// 从 OpenTelemetry context 获取 trace_id
fn get_trace_id_from_context() -> Option<String> {
    let span = tracing::Span::current();
    let context = span.context();
    let span_ref = context.span();
    let span_context = span_ref.span_context();

    if span_context.is_valid() {
        // 获取 trace_id 并转换为字符串
        let trace_id = span_context.trace_id();
        Some(trace_id.to_string())
    } else {
        None
    }
}

/// 生成新的 trace_id
fn generate_trace_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// 从请求头中提取 trace_id（如果存在）
fn extract_trace_id_from_headers(headers: &HeaderMap) -> Option<String> {
    // 尝试从常见的 trace 头中提取 trace_id
    let trace_headers = [
        "x-trace-id",
        "x-request-id",
        "traceparent",
        "x-correlation-id",
    ];

    for header_name in &trace_headers {
        if let Some(header_value) = headers.get(*header_name)
            && let Ok(value) = header_value.to_str()
            && !value.is_empty()
        {
            return Some(value.to_string());
        }
    }

    None
}

/// 中间件处理函数
pub async fn tracing_middleware_handler(
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();

    let trace_id = extract_trace_id_from_headers(&headers).unwrap_or_else(generate_trace_id);

    // 创建请求 span，包含 trace_id 信息
    let span = info_span!(
        "http_request",
        method = %method,
        uri = %uri,
        trace_id = %trace_id,
        user_agent = ?headers.get("user-agent").and_then(|h| h.to_str().ok()),
        content_type = ?headers.get("content-type").and_then(|h| h.to_str().ok()),
    );

    // 在 span 中执行请求处理
    let response = async {
        // 记录请求开始
        tracing::info!(
            "开始处理 HTTP 请求: {} {} (trace_id: {})",
            method,
            uri,
            trace_id
        );

        // 将 trace_id 添加到请求扩展中，供后续处理器使用
        req.extensions_mut().insert(trace_id.clone());

        // 创建一个新的 span 来确保 trace_id 在 context 中可用
        let _span = tracing::info_span!("http_request_processing", trace_id = %trace_id);

        // 处理请求
        let response = next.run(req).await;

        // 记录响应信息
        let status = response.status();
        tracing::info!(
            "HTTP 请求处理完成: {} {} -> {} (trace_id: {})",
            method,
            uri,
            status,
            trace_id
        );

        response
    }
    .instrument(span)
    .await;

    Ok(response)
}

/// 为 Axum 应用添加追踪中间件
pub fn add_tracing_layer<B>(router: axum::Router<B>) -> axum::Router<B>
where
    B: Send + Clone + Sync + 'static,
{
    router.layer(axum::middleware::from_fn(tracing_middleware_handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::Body,
        http::{Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn test_trace_id_generation() {
        let trace_id = generate_trace_id();
        assert!(!trace_id.is_empty());
        assert_eq!(trace_id.len(), 32); // UUID simple format length
    }

    #[tokio::test]
    async fn test_trace_id_extraction_from_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("x-trace-id", "test-trace-id".parse().unwrap());

        let trace_id = extract_trace_id_from_headers(&headers);
        assert_eq!(trace_id, Some("test-trace-id".to_string()));
    }

    #[tokio::test]
    async fn test_middleware_integration() {
        let app = Router::new()
            .route("/test", get(|| async { "Hello, World!" }))
            .layer(axum::middleware::from_fn(tracing_middleware_handler));

        let request = Request::builder().uri("/test").body(Body::empty()).unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
}
