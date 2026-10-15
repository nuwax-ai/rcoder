//! Prometheus /metrics 端点（从 router.rs 拆出）。

use std::sync::Arc;

use axum::Router;
use axum::response::IntoResponse;
use axum::routing::get;
use rcoder_telemetry::TelemetryGuard;

/// Prometheus 指标处理器
async fn metrics_handler(telemetry: Arc<TelemetryGuard>) -> impl IntoResponse {
    match telemetry.render_metrics() {
        Some(metrics) => (
            axum::http::StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            metrics,
        ),
        None => (
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            "Prometheus metrics not enabled".to_string(),
        ),
    }
}

/// 挂载 /metrics（仅启用 Prometheus 时）。
pub(super) fn mount(router: Router, telemetry: &Arc<TelemetryGuard>) -> Router {
    let guard_clone = Arc::clone(telemetry);
    router.route(
        "/metrics",
        get(move || {
            let guard = Arc::clone(&guard_clone);
            async move { metrics_handler(guard).await }
        }),
    )
}
