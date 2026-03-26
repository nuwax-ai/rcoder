//! RCoder 遥测模块
//!
//! 提供统一的遥测功能，包括：
//! - **OTLP Tracing**: 分布式追踪，支持 Jaeger/OTLP Collector
//! - **Prometheus Metrics**: HTTP/gRPC 请求指标、业务指标
//! - **Trace Propagation**: 跨服务 trace context 传播
//! - **Console & File Logging**: 控制台和文件日志输出
//!
//! # 快速开始
//!
//! ```no_run
//! use rcoder_telemetry::{TelemetryConfig, init};
//!
//! #[tokio::main]
//! async fn main() -> anyhow::Result<()> {
//!     // 从环境变量初始化配置
//!     let config = TelemetryConfig::from_env("my-service");
//!
//!     // Initializing telemetry system（包含 console 日志、OTLP 追踪、Prometheus 指标）
//!     let telemetry = init(config).await?;
//!
//!     // 在应用中使用 telemetry.render_metrics() 暴露 /metrics 端点
//!
//!     Ok(())
//! }
//! ```
//!
//! # 环境变量
//!
//! | 变量 | 说明 | 默认值 |
//! |-----|------|-------|
//! | `OTEL_EXPORTER_OTLP_ENDPOINT` | OTLP 端点 | - |
//! | `OTEL_SERVICE_NAME` | 服务名称 | 代码指定 |
//! | `OTEL_TRACES_SAMPLER_ARG` | 采样率 | `1.0` |
//! | `OTEL_EXPORTER_OTLP_PROTOCOL` | 协议 (grpc/http) | `grpc` |
//! | `TELEMETRY_PROMETHEUS_ENABLED` | 启用 Prometheus | `true` |
//! | `RUST_LOG` | 日志级别过滤 | `info` |

pub mod config;
pub mod middleware;
pub mod otlp;
pub mod prometheus;
pub mod propagation;

// Re-exports
pub use config::{FileLogConfig, OtlpConfig, PrometheusConfig, TelemetryConfig};
pub use middleware::{GrpcMetricsInterceptor, HttpMetricsLayer};
pub use prometheus::{
    dec_active_tasks, inc_active_tasks, record_agent_task, record_agent_task_duration,
    record_grpc_duration, record_grpc_request, record_http_duration, record_http_request,
    set_active_tasks,
};
pub use propagation::{
    extract_context, extract_context_http, inject_context, inject_context_http,
    set_global_propagator,
};

use anyhow::Result;
use metrics_exporter_prometheus::PrometheusHandle;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::info;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// 遥测系统 Guard
///
/// 持有遥测资源的生命周期，Drop 时自动清理。
/// 同时提供 Prometheus 指标渲染功能。
pub struct TelemetryGuard {
    /// OTLP TracerProvider（可选）
    tracer_provider: Option<SdkTracerProvider>,
    /// Prometheus Handle（可选）
    prometheus_handle: Option<PrometheusHandle>,
    /// 服务名称
    service_name: String,
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
pub async fn init(config: TelemetryConfig) -> Result<TelemetryGuard> {
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

    // 🆕 初始化 tracing subscriber（包括控制台、文件、OpenTelemetry）
    init_tracing_subscriber(
        &config.service_name,
        tracer_provider.as_ref(),
        config.file_log.as_ref(),
    )?;

    info!(
        "[Telemetry] Initializing telemetry system: {}",
        config.service_name
    );
    info!(
        "✅ [Telemetry] 遥测系统初始化完成: OTLP={}, Prometheus={}, FileLog={}, Console=true",
        tracer_provider.is_some(),
        prometheus_handle.is_some(),
        config.file_log.is_some()
    );

    Ok(TelemetryGuard {
        tracer_provider,
        prometheus_handle,
        service_name: config.service_name,
    })
}

/// 初始化 tracing subscriber
///
/// 配置以下层：
/// - EnvFilter: 基于 RUST_LOG 环境变量的日志级别过滤
/// - Console Layer: 控制台日志输出
/// - File Layer: 可选的文件日志输出（JSON 格式，按天滚动）
/// - OpenTelemetry Layer: 如果提供了 TracerProvider，将 span 发送到 OTLP
fn init_tracing_subscriber(
    service_name: &str,
    tracer_provider: Option<&SdkTracerProvider>,
    file_log_config: Option<&config::FileLogConfig>,
) -> Result<()> {
    use opentelemetry::trace::TracerProvider;

    // 创建 EnvFilter（支持 RUST_LOG 环境变量）
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // 默认日志级别
        format!(
            "{}=debug,tower_http=debug,axum=info,hyper=info,tonic=info",
            service_name.replace('-', "_")
        )
        .into()
    });

    // 创建控制台日志层
    let console_layer = fmt::layer()
        .with_target(true)
        .with_ansi(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false);

    // 创建文件日志层（如果配置了）
    let file_layer = if let Some(file_config) = file_log_config {
        // 创建日志目录
        if !file_config.directory.exists() {
            std::fs::create_dir_all(&file_config.directory)?;
        }

        // 创建按天滚动的 appender
        let file_appender = tracing_appender::rolling::Builder::new()
            .rotation(Rotation::DAILY)
            .filename_prefix(&file_config.filename_prefix)
            .max_log_files(file_config.max_log_files)
            .build(&file_config.directory)?;

        if file_config.json_format {
            // JSON 格式文件日志
            Some(
                fmt::layer()
                    .json()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
                    .boxed(),
            )
        } else {
            // 纯文本格式文件日志
            Some(
                fmt::layer()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .boxed(),
            )
        }
    } else {
        None
    };

    // 根据是否有 TracerProvider 和 FileLayer 决定如何初始化
    match (tracer_provider, file_layer) {
        (Some(provider), Some(file)) => {
            // 有 OTLP + 文件日志
            let tracer = provider.tracer(service_name.to_string());
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(console_layer)
                .with(file)
                .with(otel_layer)
                .init();
        }
        (Some(provider), None) => {
            // 只有 OTLP
            let tracer = provider.tracer(service_name.to_string());
            let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);

            tracing_subscriber::registry()
                .with(env_filter)
                .with(console_layer)
                .with(otel_layer)
                .init();
        }
        (None, Some(file)) => {
            // 只有文件日志
            tracing_subscriber::registry()
                .with(env_filter)
                .with(console_layer)
                .with(file)
                .init();
        }
        (None, None) => {
            // 仅控制台
            tracing_subscriber::registry()
                .with(env_filter)
                .with(console_layer)
                .init();
        }
    }

    Ok(())
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_from_env() {
        let config = TelemetryConfig::from_env("test-service");
        assert_eq!(config.service_name, "test-service");
    }

    #[test]
    fn test_config_builder() {
        let config = TelemetryConfig::new("my-service")
            .with_otlp_endpoint("http://localhost:4317")
            .with_prometheus();

        assert_eq!(config.service_name, "my-service");
        assert!(config.otlp.is_some());
        assert!(config.prometheus.is_some());
    }
}
