//! 遥测资源 Guard：持有 TracerProvider / Prometheus Handle 生命周期，
//! Drop 时自动关闭 OTLP 导出并渲染指标。

use metrics_exporter_prometheus::PrometheusHandle;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::info;
use tracing_appender::non_blocking::WorkerGuard;

use crate::otlp;

/// 遥测系统 Guard
///
/// 持有遥测资源的生命周期，Drop 时自动清理。
/// 同时提供 Prometheus 指标渲染功能。
pub struct TelemetryGuard {
    /// OTLP TracerProvider（可选）
    pub(crate) tracer_provider: Option<SdkTracerProvider>,
    /// Prometheus Handle（可选）
    pub(crate) prometheus_handle: Option<PrometheusHandle>,
    /// 服务名称
    pub(crate) service_name: String,
    /// 额外 layer 关联的 WorkerGuard（如 file-server 独立日志的 non_blocking guard）
    pub(crate) _extra_layer_guard: Option<WorkerGuard>,
}

impl TelemetryGuard {
    /// 渲染 Prometheus 指标
    ///
    /// 返回 Prometheus 文本格式的指标数据，
    /// 可直接作为 `/metrics` 端点的响应。
    ///
    /// # Returns
    ///
    /// 如果 Prometheus 已启用，返回 `Some(metrics_text)`；
    /// 否则返回 `None`。
    pub fn render_metrics(&self) -> Option<String> {
        self.prometheus_handle.as_ref().map(|h| h.render())
    }

    /// 检查 OTLP 是否已启用
    pub fn is_otlp_enabled(&self) -> bool {
        self.tracer_provider.is_some()
    }

    /// 检查 Prometheus 是否已启用
    pub fn is_prometheus_enabled(&self) -> bool {
        self.prometheus_handle.is_some()
    }

    /// 获取服务名称
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

}