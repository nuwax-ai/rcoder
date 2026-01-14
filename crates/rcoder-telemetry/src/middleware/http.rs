//! HTTP 指标中间件
//!
//! 为 Axum 提供 HTTP 请求指标收集功能。

use axum::http::{Request, Response};
use pin_project_lite::pin_project;
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Instant,
};
use tower::{Layer, Service};

use crate::prometheus::{record_http_duration, record_http_request};

/// HTTP 指标中间件层
///
/// 自动收集 HTTP 请求的指标：
/// - 请求计数（按 method, path, status 分组）
/// - 请求耗时（按 method, path 分组）
///
/// # Example
///
/// ```ignore
/// use axum::Router;
/// use rcoder_telemetry::middleware::HttpMetricsLayer;
///
/// let app: Router<()> = Router::new()
///     // ... routes
///     .layer(HttpMetricsLayer::new());
/// ```
#[derive(Debug, Clone, Default)]
pub struct HttpMetricsLayer;

impl HttpMetricsLayer {
    /// 创建新的 HTTP 指标层
    pub fn new() -> Self {
        Self
    }
}

impl<S> Layer<S> for HttpMetricsLayer {
    type Service = HttpMetricsService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        HttpMetricsService { inner }
    }
}

/// HTTP 指标服务
#[derive(Debug, Clone)]
pub struct HttpMetricsService<S> {
    inner: S,
}

impl<S, ReqBody, ResBody> Service<Request<ReqBody>> for HttpMetricsService<S>
where
    S: Service<Request<ReqBody>, Response = Response<ResBody>> + Clone + Send + 'static,
    S::Future: Send,
    ReqBody: Send + 'static,
    ResBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = HttpMetricsFuture<S::Future>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<ReqBody>) -> Self::Future {
        let method = req.method().to_string();
        let path = normalize_path(req.uri().path());
        let start = Instant::now();

        let future = self.inner.call(req);

        HttpMetricsFuture {
            inner: future,
            method,
            path,
            start,
        }
    }
}

pin_project! {
    /// HTTP 指标 Future
    pub struct HttpMetricsFuture<F> {
        #[pin]
        inner: F,
        method: String,
        path: String,
        start: Instant,
    }
}

impl<F, ResBody, E> Future for HttpMetricsFuture<F>
where
    F: Future<Output = Result<Response<ResBody>, E>>,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        match this.inner.poll(cx) {
            Poll::Ready(result) => {
                let duration = this.start.elapsed();

                // 记录指标
                if let Ok(ref response) = result {
                    let status = response.status().as_u16();
                    record_http_request(this.method, this.path, status);
                }
                record_http_duration(this.method, this.path, duration);

                Poll::Ready(result)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 标准化路径
///
/// 移除路径参数中的具体值，保留路径模式。
/// 例如：`/api/users/123` -> `/api/users/:id`
fn normalize_path(path: &str) -> String {
    // 简单实现：直接返回路径
    // 可以根据需要扩展为更复杂的路径模式匹配
    let parts: Vec<&str> = path.split('/').collect();
    let normalized: Vec<String> = parts
        .iter()
        .map(|part| {
            // 如果是纯数字或 UUID 格式，替换为 :id
            if part.chars().all(|c| c.is_ascii_digit()) && !part.is_empty() {
                ":id".to_string()
            } else if is_uuid(part) {
                ":uuid".to_string()
            } else {
                part.to_string()
            }
        })
        .collect();

    normalized.join("/")
}

/// 检查是否是 UUID 格式
fn is_uuid(s: &str) -> bool {
    if s.len() != 36 {
        return false;
    }
    // 简单检查 UUID 格式：8-4-4-4-12
    let parts: Vec<&str> = s.split('-').collect();
    parts.len() == 5
        && parts[0].len() == 8
        && parts[1].len() == 4
        && parts[2].len() == 4
        && parts[3].len() == 4
        && parts[4].len() == 12
        && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_path() {
        assert_eq!(normalize_path("/api/users/123"), "/api/users/:id");
        assert_eq!(normalize_path("/health"), "/health");
        assert_eq!(normalize_path("/api/v1/projects"), "/api/v1/projects");
    }

    #[test]
    fn test_is_uuid() {
        assert!(is_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!is_uuid("not-a-uuid"));
        assert!(!is_uuid("123"));
    }

    #[test]
    fn test_normalize_path_with_uuid() {
        assert_eq!(
            normalize_path("/api/sessions/550e8400-e29b-41d4-a716-446655440000/progress"),
            "/api/sessions/:uuid/progress"
        );
    }
}
