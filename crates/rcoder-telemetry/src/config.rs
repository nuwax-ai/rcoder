//! 遥测配置模块
//!
//! 提供统一的遥测配置，支持从环境变量读取。

use std::env;
use std::path::PathBuf;

/// 遥测系统统一配置
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// 服务名称（用于 trace 和 metrics 标识）
    pub service_name: String,
    /// OTLP 配置（可选）
    pub otlp: Option<OtlpConfig>,
    /// Prometheus 配置（可选）
    pub prometheus: Option<PrometheusConfig>,
    /// 文件日志配置（可选）
    pub file_log: Option<FileLogConfig>,
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

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            service_name: "unknown-service".to_string(),
            otlp: None,
            prometheus: Some(PrometheusConfig::default()),
            file_log: None,
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
