//! OpenTelemetry 追踪模块
//!
//! 集成 `rcoder-telemetry` 提供完整的分布式追踪功能。

use opentelemetry::trace::Status;
use tracing::{Level, Span, error, info, span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// OpenTelemetry 追踪配置
#[derive(Debug, Clone)]
pub struct TraceConfig {
    /// 是否启用追踪
    pub enabled: bool,
    /// OTLP 导出端点（如 Jaeger、OTLP）
    pub exporter_endpoint: Option<String>,
    /// 是否启用 Prometheus 指标
    pub prometheus_enabled: bool,
    /// 服务名称
    pub service_name: String,
}

impl Default for TraceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            exporter_endpoint: None,
            prometheus_enabled: true,
            service_name: "agent-runner".to_string(),
        }
    }
}

/// 初始化 OpenTelemetry 追踪
///
/// 集成 `rcoder-telemetry` 提供完整的分布式追踪功能：
/// - OTLP 导出器（支持 Jaeger、Zipkin、OTLP Collector）
/// - Prometheus 指标
/// - Trace context 传播
/// - Console 日志
///
/// # Arguments
///
/// * `config` - 追踪配置
///
/// # Returns
///
/// 返回遥测系统 Guard，需要在应用运行期间保持存活
///
/// # Environment Variables
///
/// 支持以下环境变量：
/// - `OTEL_EXPORTER_OTLP_ENDPOINT`: OTLP 端点（如 http://jaeger:4317）
/// - `OTEL_TRACES_SAMPLER_ARG`: 采样率 (0.0-1.0，默认 1.0)
/// - `TELEMETRY_PROMETHEUS_ENABLED`: 是否启用 Prometheus（默认 true）
pub async fn init_tracing(config: TraceConfig) -> anyhow::Result<rcoder_telemetry::TelemetryGuard> {
    if !config.enabled {
        info!("📍 [OTel] 追踪已禁用");
        // 即使追踪禁用，仍然Initializing Prometheus（如果启用）
        if config.prometheus_enabled {
            return rcoder_telemetry::init_prometheus_only(&config.service_name);
        }
        // 创建一个空的 guard（仅保留服务名称，用于后续渲染指标）
        // 使用空的 telemetry_config 创建一个没有任何功能的 guard
        let telemetry_config = rcoder_telemetry::TelemetryConfig::new(&config.service_name);
        return rcoder_telemetry::init(telemetry_config).await;
    }

    // 使用 from_env 从环境变量读取配置，然后覆盖必要字段
    let mut telemetry_config = rcoder_telemetry::TelemetryConfig::from_env(&config.service_name);

    // 如果配置中指定了端点，覆盖环境变量中的值
    if let Some(ref endpoint) = config.exporter_endpoint {
        telemetry_config = telemetry_config.with_otlp_endpoint(endpoint);
    }

    // 根据配置设置 Prometheus
    if config.prometheus_enabled {
        telemetry_config = telemetry_config.with_prometheus();
    } else {
        telemetry_config = telemetry_config.without_prometheus();
    }

    // Initializing telemetry system
    let guard = rcoder_telemetry::init(telemetry_config).await?;

    info!(
        "✅ [OTel] 追踪已初始化: OTLP={}, Prometheus={}",
        guard.is_otlp_enabled(),
        guard.is_prometheus_enabled()
    );

    Ok(guard)
}

impl TraceConfig {
    /// 从环境变量构建配置
    pub fn from_env() -> Self {
        Self {
            enabled: std::env::var("OTEL_TRACING_ENABLED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
            exporter_endpoint: std::env::var("OTEL_EXPORTER_OTLP_ENDPOINT").ok(),
            prometheus_enabled: std::env::var("TELEMETRY_PROMETHEUS_ENABLED")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(true),
            service_name: "agent-runner".to_string(),
        }
    }

    /// 设置 OTLP 端点
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.exporter_endpoint = Some(endpoint.into());
        self
    }

    /// 禁用 Prometheus
    pub fn without_prometheus(mut self) -> Self {
        self.prometheus_enabled = false;
        self
    }

    /// 禁用追踪
    pub fn disabled(mut self) -> Self {
        self.enabled = false;
        self
    }

    /// 设置服务名称
    pub fn with_service_name(mut self, name: impl Into<String>) -> Self {
        self.service_name = name.into();
        self
    }
}

/// 请求追踪 Guard（自动管理 span 生命周期）
///
/// 使用 `OpenTelemetrySpanExt` 支持动态属性设置
///
/// 注意：此结构体实现 Send + Sync，可在 tokio::spawn 中安全使用
pub struct RequestSpan {
    /// 底层 tracing span（用于 OpenTelemetrySpanExt 方法）
    span: Span,
}

impl RequestSpan {
    /// 创建新的请求 span
    ///
    /// # Arguments
    ///
    /// * `project_id` - 项目 ID
    /// * `request_id` - 请求 ID
    /// * `operation` - 操作名称（如 "process_prompt"）
    pub fn new(project_id: &str, request_id: &str, operation: &str) -> Self {
        let span = span!(
            Level::INFO,
            "agent_request",
            project_id = %project_id,
            request_id = %request_id,
            operation = %operation,
        );

        info!(
            "📍 [OTel] Span 已创建: project_id={}, request_id={}, operation={}",
            project_id, request_id, operation
        );

        Self {
            span,
        }
    }

    /// 设置动态属性（使用 OpenTelemetrySpanExt）
    ///
    /// # Arguments
    ///
    /// * `key` - 属性键
    /// * `value` - 属性值
    ///
    /// # Example
    ///
    /// ```rust
    /// use agent_runner::RequestSpan;
    ///
    /// let span = RequestSpan::new("proj", "req", "op");
    /// span.set_attribute("http.status_code", 200);
    /// span.set_attribute("user.id", "user-123");
    /// ```
    pub fn set_attribute<K, V>(&self, key: K, value: V)
    where
        K: Into<opentelemetry::Key>,
        V: Into<opentelemetry::Value>,
    {
        self.span.set_attribute(key, value);
    }

    /// 批量设置动态属性
    ///
    /// # Arguments
    ///
    /// * `attributes` - 属性列表 (key, value)
    pub fn set_attributes(&self, attributes: &[(&str, &str)]) {
        for (key, value) in attributes {
            // 需要转换为 String 以满足 'static 生命周期要求
            self.span.set_attribute(key.to_string(), value.to_string());
        }
    }

    /// 设置 span 状态为成功
    pub fn set_ok(&self) {
        self.span.set_status(Status::Ok);
    }

    /// 设置 span 状态为错误
    ///
    /// # Arguments
    ///
    /// * `description` - 错误描述
    pub fn set_error(&self, description: impl Into<std::borrow::Cow<'static, str>>) {
        self.span.set_status(Status::error(description));
    }

    /// 添加事件到当前 span（使用 OpenTelemetrySpanExt）
    ///
    /// # Arguments
    ///
    /// * `name` - 事件名称
    /// * `attributes` - 事件属性
    ///
    /// # Example
    ///
    /// ```rust
    /// use agent_runner::RequestSpan;
    /// use opentelemetry::KeyValue;
    ///
    /// let span = RequestSpan::new("proj", "req", "op");
    /// span.add_event("cache_hit", vec![
    ///     KeyValue::new("cache.key", "user:123"),
    ///     KeyValue::new("cache.ttl", 300),
    /// ]);
    /// ```
    pub fn add_event(&self, name: impl Into<std::borrow::Cow<'static, str>>, attributes: Vec<opentelemetry::KeyValue>) {
        self.span.add_event(name, attributes);
    }

    /// 记录事件（简化版本，兼容旧 API）
    pub fn event(&self, name: &str, attributes: &[(&str, String)]) {
        // 转换为 String 以满足 'static 生命周期要求
        let kv_attrs: Vec<opentelemetry::KeyValue> = attributes
            .iter()
            .map(|(k, v)| opentelemetry::KeyValue::new(k.to_string(), v.clone()))
            .collect();

        self.span.add_event(name.to_string(), kv_attrs);

        info!(
            "📍 [OTel] 事件: {}, 属性: {:?}",
            name,
            attributes
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    /// 记录错误到当前 span
    pub fn error(&self, err: &anyhow::Error) {
        self.set_error(err.to_string());
        error!(
            error = %err,
            "📍 [OTel] 请求错误"
        );
    }

    /// 获取 OpenTelemetry 上下文（用于跨服务传播）
    pub fn context(&self) -> opentelemetry::Context {
        self.span.context()
    }

    /// 完成 span（手动关闭）
    pub fn finish(self) {
        self.set_ok();
        // span 在 drop 时会自动关闭
    }
}

impl Drop for RequestSpan {
    fn drop(&mut self) {
        info!("📍 [OTel] Span 已关闭");
    }
}

/// 创建子 span（用于追踪子操作）
///
/// # Arguments
///
/// * `_parent` - 父 span 的引用（子 span 会自动继承父 span 的上下文）
/// * `name` - 子 span 名称
/// * `attributes` - 附加属性
///
/// # Example
///
/// ```rust
/// use agent_runner::{RequestSpan, child_span};
///
/// let parent = RequestSpan::new("proj", "req", "parent_op");
/// let child = child_span(&parent, "child_op", &[("key", "value".to_string())]);
/// child.finish();
/// parent.finish();
/// ```
pub fn child_span(_parent: &RequestSpan, name: &str, attributes: &[(&str, String)]) -> RequestSpan {
    let span = span!(
        Level::INFO,
        "child_operation",
        otel.name = %name,
    );

    // 使用 OpenTelemetrySpanExt 设置动态属性
    // 需要转换为 String 以满足 'static 生命周期要求
    for (key, value) in attributes {
        span.set_attribute(key.to_string(), value.clone());
    }

    info!("📍 [OTel] 子 Span 已创建: {}", name);

    RequestSpan {
        span,
    }
}

/// 从上下文中提取当前 span（用于跨线程传递）
pub fn current_span() -> RequestSpan {
    let span = Span::current();

    RequestSpan {
        span,
    }
}

/// 创建带属性的 span
///
/// # Example
///
/// ```rust
/// use agent_runner::otel_tracing::span_with_attributes;
///
/// let span = span_with_attributes(
///     "process_attachment",
///     &[
///         ("file_name", "test.pdf".to_string()),
///         ("file_size", "1024".to_string()),
///     ]
/// );
/// ```
pub fn span_with_attributes(name: &str, attributes: &[(&str, String)]) -> RequestSpan {
    let span = span!(
        Level::INFO,
        "custom_operation",
        otel.name = %name,
    );

    // 使用 OpenTelemetrySpanExt 设置动态属性
    // 需要转换为 String 以满足 'static 生命周期要求
    for (key, value) in attributes {
        span.set_attribute(key.to_string(), value.clone());
    }

    RequestSpan {
        span,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trace_config_default() {
        let config = TraceConfig::default();
        assert!(config.enabled);
        assert!(config.exporter_endpoint.is_none());
        assert!(config.prometheus_enabled);
    }

    #[test]
    fn test_request_span_creation() {
        let span = RequestSpan::new("test_project", "test_request", "test_operation");
        span.event("test_event", &[("key", "value".to_string())]);
        span.finish();
    }

    #[test]
    fn test_request_span_with_attributes() {
        let span = RequestSpan::new("test_project", "test_request", "test_operation");

        // 使用 OpenTelemetrySpanExt 设置动态属性
        span.set_attribute("http.method", "GET");
        span.set_attribute("http.status_code", 200i64);
        span.set_attribute("user.id", "user-123");

        span.set_ok();
        span.finish();
    }

    #[test]
    fn test_request_span_with_error() {
        let span = RequestSpan::new("test_project", "test_request", "test_operation");
        span.set_error("Connection timeout");
        // span 会在 drop 时自动关闭
    }

    #[test]
    fn test_request_span_auto_drop() {
        let _span = RequestSpan::new("test_project", "test_request", "test_operation");
        // Span 会在 drop 时自动关闭
    }

    #[test]
    fn test_child_span() {
        let parent = RequestSpan::new("test_project", "test_request", "parent_operation");
        let child = child_span(&parent, "child_operation", &[("attr", "value".to_string())]);
        child.finish();
        parent.finish();
    }

    #[test]
    fn test_span_with_attributes() {
        let span = span_with_attributes(
            "custom_op",
            &[
                ("file_name", "test.pdf".to_string()),
                ("file_size", "1024".to_string()),
            ],
        );
        span.finish();
    }

    #[test]
    fn test_add_event() {
        let span = RequestSpan::new("test_project", "test_request", "test_operation");

        span.add_event(
            "cache_hit",
            vec![
                opentelemetry::KeyValue::new("cache.key", "user:123"),
                opentelemetry::KeyValue::new("cache.ttl", 300i64),
            ],
        );

        span.finish();
    }

    #[tokio::test]
    async fn test_init_tracing() {
        let config = TraceConfig::default();
        let result = init_tracing(config).await;
        assert!(result.is_ok());
    }
}
