//! 遥测配置模块
//!
//! 提供统一的遥测配置，支持从环境变量读取。

use std::env;
use std::path::PathBuf;

use tracing_appender::non_blocking::WorkerGuard;

use crate::BoxedLayer;

/// 遥测系统统一配置
pub struct TelemetryConfig {
    /// 服务名称（用于 trace 和 metrics 标识）
    pub service_name: String,
    /// OTLP 配置（可选）
    pub otlp: Option<OtlpConfig>,
    /// Prometheus 配置（可选）
    pub prometheus: Option<PrometheusConfig>,
    /// 文件日志配置（可选）
    pub file_log: Option<FileLogConfig>,
    /// 额外 tracing layer（嵌入式场景，如 file-server 独立日志）
    pub extra_layer: Option<BoxedLayer>,
    /// 额外 layer 关联的 WorkerGuard（必须存活到进程结束）
    pub extra_layer_guard: Option<WorkerGuard>,
    /// tokio-console 观测 layer（本地开发 feature；独立于 extra_layer 槽——
    /// extra_layer 已被 file-server 嵌入场景占用。无 guard：console 服务器
    /// 任务自管理生命周期）
    pub console_layer: Option<BoxedLayer>,
    /// span 耗时→直方图指标规则（SpanMetricsLayer；空=不桥接）
    pub span_metrics: Vec<crate::span_metrics::SpanMetricRule>,
}

/// OTLP 导出器配置
#[derive(Debug, Clone)]
pub struct OtlpConfig {
    /// OTLP 端点地址
    /// 默认: `http://localhost:4317`（gRPC）或 `http://localhost:4318`（HTTP）
    pub endpoint: String,
    /// 采样率 (0.0 - 1.0)
    /// 默认: 1.0（100% 采样）
    pub sample_rate: f64,
    /// 是否使用 gRPC 协议
    /// 默认: true
    pub use_grpc: bool,
}

/// Prometheus 指标配置
#[derive(Debug, Clone)]
pub struct PrometheusConfig {
    /// 是否启用 Prometheus 指标
    pub enabled: bool,
}

/// 文件日志配置
#[derive(Debug, Clone)]
pub struct FileLogConfig {
    /// 日志目录
    pub directory: PathBuf,
    /// 文件名前缀
    pub filename_prefix: String,
    /// 保留的日志文件数量
    pub max_log_files: usize,
    /// 使用 JSON 格式
    pub json_format: bool,
}

impl std::fmt::Debug for TelemetryConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TelemetryConfig")
            .field("service_name", &self.service_name)
            .field("otlp", &self.otlp)
            .field("prometheus", &self.prometheus)
            .field("file_log", &self.file_log)
            .field("extra_layer", &self.extra_layer.is_some())
            .field("extra_layer_guard", &self.extra_layer_guard.is_some())
            .field("console_layer", &self.console_layer.is_some())
            .finish()
    }
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "unknown-service".to_string(),
            otlp: None,
            prometheus: Some(PrometheusConfig::default()),
            file_log: None,
            extra_layer: None,
            extra_layer_guard: None,
            console_layer: None,
            span_metrics: Vec::new(),
        }
    }
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4317".to_string(),
            sample_rate: 1.0,
            use_grpc: true,
        }
    }
}

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl Default for FileLogConfig {
    fn default() -> Self {
        Self {
            directory: PathBuf::from("logs"),
            filename_prefix: "app".to_string(),
            max_log_files: 5,
            json_format: true,
        }
    }
}

impl FileLogConfig {
    /// 创建新的文件日志配置
    pub fn new(directory: impl Into<PathBuf>, filename_prefix: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            filename_prefix: filename_prefix.into(),
            ..Default::default()
        }
    }

    /// 设置保留的日志文件数量
    pub fn with_max_files(mut self, max_files: usize) -> Self {
        self.max_log_files = max_files;
        self
    }

    /// 禁用 JSON 格式（使用纯文本）
    pub fn with_text_format(mut self) -> Self {
        self.json_format = false;
        self
    }
}

impl TelemetryConfig {
    /// 创建新的配置
    pub fn new(service_name: impl Into<String>) -> Self {
        Self {
            service_name: service_name.into(),
            ..Default::default()
        }
    }

    /// 从环境变量读取配置
    ///
    /// 支持的环境变量：
    /// - `OTEL_SERVICE_NAME` - 服务名称（如果未指定则使用参数值）
    /// - `OTEL_EXPORTER_OTLP_ENDPOINT` - OTLP 端点
    /// - `OTEL_TRACES_SAMPLER_ARG` - 采样率
    /// - `OTEL_EXPORTER_OTLP_PROTOCOL` - 协议（grpc/http）
    /// - `TELEMETRY_PROMETHEUS_ENABLED` - 是否启用 Prometheus（true/false）
    pub fn from_env(default_service_name: impl Into<String>) -> Self {
        let service_name =
            env::var("OTEL_SERVICE_NAME").unwrap_or_else(|_| default_service_name.into());

        // OTLP 配置
        let otlp_endpoint = env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok();
        let otlp = otlp_endpoint.map(|endpoint| {
            let sample_rate = env::var("OTEL_TRACES_SAMPLER_ARG")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(1.0);

            let use_grpc = env::var("OTEL_EXPORTER_OTLP_PROTOCOL")
                .map(|p| p.to_lowercase() != "http")
                .unwrap_or(true);

            OtlpConfig {
                endpoint,
                sample_rate,
                use_grpc,
            }
        });

        // Prometheus 配置
        let prometheus_enabled = env::var("TELEMETRY_PROMETHEUS_ENABLED")
            .map(|v| v.to_lowercase() != "false" && v != "0")
            .unwrap_or(true);

        let prometheus = if prometheus_enabled {
            Some(PrometheusConfig { enabled: true })
        } else {
            None
        };

        Self {
            service_name,
            otlp,
            prometheus,
            file_log: None,
            extra_layer: None,
            extra_layer_guard: None,
            console_layer: None,
            span_metrics: Vec::new(),
        }
    }

    /// 启用 OTLP（使用默认配置）
    pub fn with_otlp(mut self) -> Self {
        self.otlp = Some(OtlpConfig::default());
        self
    }

    /// 启用 OTLP（使用指定端点）
    pub fn with_otlp_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.otlp = Some(OtlpConfig {
            endpoint: endpoint.into(),
            ..Default::default()
        });
        self
    }

    /// 设置 OTLP 配置
    pub fn with_otlp_config(mut self, config: OtlpConfig) -> Self {
        self.otlp = Some(config);
        self
    }

    /// 禁用 Prometheus
    pub fn without_prometheus(mut self) -> Self {
        self.prometheus = None;
        self
    }

    /// 启用 Prometheus
    pub fn with_prometheus(mut self) -> Self {
        self.prometheus = Some(PrometheusConfig::default());
        self
    }

    /// 启用文件日志（使用默认配置）
    pub fn with_file_log(mut self, filename_prefix: impl Into<String>) -> Self {
        self.file_log = Some(FileLogConfig {
            filename_prefix: filename_prefix.into(),
            ..Default::default()
        });
        self
    }

    /// 启用文件日志（使用自定义配置）
    pub fn with_file_log_config(mut self, config: FileLogConfig) -> Self {
        self.file_log = Some(config);
        self
    }

    /// 禁用文件日志
    pub fn without_file_log(mut self) -> Self {
        self.file_log = None;
        self
    }

    /// 注入额外的 tracing layer 及其关联的 WorkerGuard（嵌入式场景，如 file-server 独立日志）。
    ///
    /// extra_layer 通过 per-layer filter 独立过滤，不受全局 EnvFilter 影响。
    /// extra_layer_guard 必须存活到进程结束（由 [`crate::TelemetryGuard`] 持有）。
    pub fn with_extra_layer(mut self, layer: BoxedLayer, guard: WorkerGuard) -> Self {
        self.extra_layer = Some(layer);
        self.extra_layer_guard = Some(guard);
        self
    }

    /// 注入 tokio-console 观测 layer（rcoder/agent_runner 的 `console` feature
    /// 构造后传入；telemetry 自身不依赖 console-subscriber，经 BoxedLayer 泛化承载）。
    pub fn with_console_layer(mut self, layer: BoxedLayer) -> Self {
        self.console_layer = Some(layer);
        self
    }

    /// 注册 span 耗时→直方图指标规则（见 [`crate::span_metrics`]）。
    ///
    /// 调用点只需 `#[instrument]`，耗时指标由 SpanMetricsLayer 自动记录。
    pub fn with_span_metric_rules(
        mut self,
        rules: Vec<crate::span_metrics::SpanMetricRule>,
    ) -> Self {
        self.span_metrics = rules;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = TelemetryConfig::default();
        assert_eq!(config.service_name, "unknown-service");
        assert!(config.otlp.is_none());
        assert!(config.prometheus.is_some());
        assert!(config.file_log.is_none());
    }

    #[test]
    fn test_new_config() {
        let config = TelemetryConfig::new("my-service");
        assert_eq!(config.service_name, "my-service");
    }

    #[test]
    fn test_with_otlp() {
        let config = TelemetryConfig::new("test").with_otlp_endpoint("http://jaeger:4317");

        assert!(config.otlp.is_some());
        let otlp = config.otlp.unwrap();
        assert_eq!(otlp.endpoint, "http://jaeger:4317");
        assert!(otlp.use_grpc);
    }

    #[test]
    fn test_without_prometheus() {
        let config = TelemetryConfig::new("test").without_prometheus();
        assert!(config.prometheus.is_none());
    }

    #[test]
    fn test_with_file_log() {
        let config = TelemetryConfig::new("test").with_file_log("my-service");

        assert!(config.file_log.is_some());
        let file_log = config.file_log.unwrap();
        assert_eq!(file_log.filename_prefix, "my-service");
        assert_eq!(file_log.directory, PathBuf::from("logs"));
    }
}
