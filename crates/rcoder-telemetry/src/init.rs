//! 遥测初始化入口：`init`（完整栈编排）与 `init_prometheus_only`（仅指标）。

use anyhow::Result;
use tracing::info;

use crate::config::TelemetryConfig;
use crate::guard::TelemetryGuard;
use crate::{otlp, prometheus, propagation::set_global_propagator, subscriber};

/// 一键Initializing telemetry system
///
/// 根据配置初始化完整的遥测栈：
/// - **Console 日志**: 始终启用，输出到标准输出
/// - **OTLP Tracing**: 如果配置了 OTLP 端点，将 span 导出到 Jaeger/Collector
/// - **Prometheus Metrics**: 如果启用，提供 `/metrics` 端点数据
///
/// # Arguments
///
/// * `config` - 遥测配置
///
/// # Returns
///
/// 返回 `TelemetryGuard`，持有遥测资源的生命周期。
///
/// # Example
///
/// ```no_run
/// use rcoder_telemetry::{TelemetryConfig, init};
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let config = TelemetryConfig::new("my-service")
///         .with_otlp_endpoint("http://jaeger:4317")
///         .with_prometheus();
///
///     let telemetry = init(config).await?;
///
///     // 应用逻辑...
///
///     Ok(())
/// }
/// ```
pub async fn init(mut config: TelemetryConfig) -> Result<TelemetryGuard> {
    // 设置全局传播器（在初始化 subscriber 之前）
    set_global_propagator();

    // 初始化 OTLP（如果配置了）
    let tracer_provider = if let Some(ref otlp_config) = config.otlp {
        let provider = otlp::init_tracer_provider(otlp_config, &config.service_name).await?;
        otlp::set_global_tracer_provider(provider.clone());
        Some(provider)
    } else {
        None
    };

    // Initializing Prometheus（如果配置了）
    let prometheus_handle = if config.prometheus.is_some() {
        Some(prometheus::init_prometheus()?)
    } else {
        None
    };

    // 🆕 初始化 tracing subscriber（包括控制台、文件、OpenTelemetry、额外 layer）
    let has_extra_layer = config.extra_layer.is_some(); // take 前记，供下方启动日志使用
    let extra_layer = config.extra_layer.take();
    // tokio-console 观测层（config.console_layer）——注意与上方 fmt 终端输出层
    // 变量 console_layer 命名区分；独立于 extra_layer 槽（file-server 嵌入占用）
    let has_tokio_console = config.console_layer.is_some(); // take 前记，供启动日志使用
    let tokio_console_layer = config.console_layer.take();
    // tracing-flame 火焰图配置（take 前记，供启动日志使用）
    let flame_config = config.flame.take();
    // span 耗时→直方图规则（SpanMetricsLayer；调用点 #[instrument] 零计时代码）
    let span_metrics = std::mem::take(&mut config.span_metrics);
    let _flame_guard = subscriber::init_tracing_subscriber(
        &config.service_name,
        tracer_provider.as_ref(),
        config.file_log.as_ref(),
        extra_layer,
        tokio_console_layer,
        flame_config.as_ref(),
        span_metrics,
    )?;

    info!(
        "[Telemetry] Initializing telemetry system: {}",
        config.service_name
    );
    info!(
        "✅ [Telemetry] Telemetry system initialization completed: OTLP={}, Prometheus={}, FileLog={}, ExtraLayer={}, TokioConsole={}",
        tracer_provider.is_some(),
        prometheus_handle.is_some(),
        config.file_log.is_some(),
        has_extra_layer,
        has_tokio_console
    );

    Ok(TelemetryGuard {
        tracer_provider,
        prometheus_handle,
        service_name: config.service_name,
        _extra_layer_guard: config.extra_layer_guard.take(),
        // Mutex 槽包装：flush_flame() 显式 take→drop 落盘（见 guard.rs 注释）
        #[cfg(feature = "flame")]
        _flame_guard: _flame_guard.map(|g| std::sync::Mutex::new(Some(g))),
    })
}

/// 仅Initializing Prometheus（不初始化 OTLP 和 tracing）
///
/// 适用于只需要 metrics 不需要 tracing 的场景。
/// **注意**：此函数不会初始化 tracing subscriber，调用方需要自行初始化。
pub fn init_prometheus_only(service_name: impl Into<String>) -> Result<TelemetryGuard> {
    let service_name = service_name.into();
    info!("[Telemetry] Initializing Prometheus: {}", service_name);

    let prometheus_handle = prometheus::init_prometheus()?;

    Ok(TelemetryGuard {
        tracer_provider: None,
        prometheus_handle: Some(prometheus_handle),
        service_name,
        _extra_layer_guard: None,
        #[cfg(feature = "flame")]
        _flame_guard: None,
    })
}
