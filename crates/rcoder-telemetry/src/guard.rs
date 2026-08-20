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
    /// tracing-flame FlushGuard（flame feature；Drop 时 flush 折叠栈数据）。
    /// Mutex<Option<..>> 槽包装：Arc<TelemetryGuard> 克隆会进入 router（metrics
    /// handler 等），进程退出时引用计数不一定归零——纯 Drop flush 不可靠，
    /// 由 `flush_flame()` 显式 take→drop 保证确定性落盘。
    #[cfg(feature = "flame")]
    pub(crate) _flame_guard: Option<
        std::sync::Mutex<Option<tracing_flame::FlushGuard<std::io::BufWriter<std::fs::File>>>>,
    >,
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

    /// 显式 flush tracing-flame 折叠栈数据（进程优雅退出时调用）
    ///
    /// FlushGuard 依赖 Drop flush，但 guard 以 Arc 共享进 router 后，进程退出
    /// 时引用计数不一定归零——此方法 take→drop 保证确定性落盘。
    /// flame feature 未启用或未配置时为 no-op；重复调用幂等。
    #[cfg(feature = "flame")]
    pub fn flush_flame(&self) {
        if let Some(slot) = &self._flame_guard
            && let Ok(mut guard) = slot.lock()
            && let Some(flush_guard) = guard.take()
        {
            info!("[Telemetry] flushing tracing-flame folded stacks...");
            drop(flush_guard); // FlushGuard::Drop → 折叠栈写入输出文件
            info!("[Telemetry] tracing-flame folded stacks flushed");
        }
    }

    /// 显式 flush tracing-flame 折叠栈数据（flame feature 未启用时为 no-op）
    #[cfg(not(feature = "flame"))]
    pub fn flush_flame(&self) {}
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        if self.tracer_provider.is_some() {
            otlp::shutdown_tracer_provider();
        }
        info!(
            "[Telemetry] Telemetry system shutdown: {}",
            self.service_name
        );
    }
}
