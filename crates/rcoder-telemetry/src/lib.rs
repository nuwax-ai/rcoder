//! RCoder 遥测模块
//!
//! 提供统一的遥测功能，包括：
//! - **OTLP Tracing**: 分布式追踪，支持 Jaeger/OTLP Collector
//! - **Prometheus Metrics**: HTTP/gRPC 请求指标、业务指标
//! - **Trace Propagation**: 跨服务 trace context 传播（含 W3C traceparent 入站提取）
//! - **Console & File Logging**: 控制台和文件日志输出
//! - **tokio-console 观测**: 本地开发 feature（`console`，使用方 crate 定义）
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
pub mod guard;
pub mod init;
pub mod middleware;
pub mod otlp;
pub mod prometheus;
pub mod propagation;
pub mod span_metrics;
pub mod subscriber;

// Re-exports（外部消费面统一走 crate 根路径）
pub use config::{FileLogConfig, OtlpConfig, PrometheusConfig, TelemetryConfig};
pub use guard::TelemetryGuard;
pub use init::{init, init_prometheus_only};
pub use middleware::{GrpcMetricsInterceptor, HttpMetricsLayer};
pub use prometheus::{
    dec_active_tasks, inc_active_tasks, record_agent_task, record_agent_task_duration,
    record_grpc_duration, record_grpc_request, record_http_duration, record_http_request,
    set_active_tasks,
};
pub use propagation::{
    extract_context, extract_context_http, inject_context, inject_context_http,
    make_span_with_trace_parent, set_global_propagator,
};
pub use span_metrics::SpanMetricRule;
pub use subscriber::BoxedLayer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reexports_complete() {
        // 门面完整性：核心类型经根路径可用（编译期检查）
        fn assert_types(_: &TelemetryConfig, _: Option<BoxedLayer>) {}
        let cfg = TelemetryConfig::from_env("facade-smoke");
        assert_types(&cfg, None);
        assert_eq!(cfg.service_name, "facade-smoke");
    }
}
