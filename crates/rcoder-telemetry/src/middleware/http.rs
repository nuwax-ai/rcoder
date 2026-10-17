//! HTTP 指标中间件
//!
//! 为 Axum 提供 HTTP 请求指标收集功能。

use axum::{
    extract::MatchedPath,
    http::{Request, Response},
};
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
        // Router::layer runs after route matching. Never label arbitrary URI
        // segments, including unknown paths, with user-controlled values.
        let path = req
            .extensions()
            .get::<MatchedPath>()
            .map_or("__unmatched__", MatchedPath::as_str)
            .to_owned();
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, routing::get};
    use metrics_exporter_prometheus::PrometheusBuilder;
    use tower::ServiceExt;

    #[test]
    fn router_layer_uses_templates_for_parameters_wildcards_and_fixed_fallback() {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        metrics::with_local_recorder(&recorder, || {
            runtime.block_on(async {
                let app = Router::new()
                    .nest(
                        "/api",
                        Router::new()
                            .route("/progress/{session_id}", get(|| async { "progress" }))
                            .route("/assets/{*path}", get(|| async { "asset" })),
                    )
                    .layer(HttpMetricsLayer::new());
                for (path, status) in [
                    ("/api/progress/session-alpha", 200),
                    ("/api/progress/session-beta?value=private-query", 200),
                    ("/api/assets/folder-one/file-one", 200),
                    ("/api/assets/folder-two/file-two", 200),
                    ("/missing/arbitrary-one", 404),
                    ("/missing/arbitrary-two", 404),
                ] {
                    let response = app
                        .clone()
                        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    assert_eq!(response.status().as_u16(), status);
                }
            })
        });
        let output = handle.render();
        for path in [
            "/api/progress/{session_id}",
            "/api/assets/{*path}",
            "__unmatched__",
        ] {
            let label = format!("path=\"{path}\"");
            let counters: Vec<_> = output
                .lines()
                .filter(|line| line.starts_with("http_requests_total{") && line.contains(&label))
                .collect();
            assert_eq!(counters.len(), 1, "{path}: {output}");
            assert!(counters[0].ends_with(" 2"), "{output}");
        }
        for private in [
            "session-alpha",
            "session-beta",
            "private-query",
            "folder-one",
            "folder-two",
            "arbitrary-one",
            "arbitrary-two",
        ] {
            assert!(
                !output.contains(private),
                "raw URI escaped into metrics: {private}"
            );
        }
    }
}
