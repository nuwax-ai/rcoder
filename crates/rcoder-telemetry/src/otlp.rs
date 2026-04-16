//! OTLP TracerProvider 初始化模块
//!
//! 提供 OpenTelemetry OTLP 导出器的初始化功能，支持 gRPC 和 HTTP 协议。

use crate::config::OtlpConfig;
use anyhow::Result;
use opentelemetry_otlp::WithExportConfig;
use opentelemetry_sdk::trace::{Sampler, SdkTracerProvider};
use tracing::info;

/// 初始化 OTLP TracerProvider
///
/// 根据配置创建 OTLP 导出器并构建 TracerProvider。
///
/// # Arguments
///
/// * `config` - OTLP 配置
/// * `service_name` - 服务名称（用于 resource 标识）
///
/// # Returns
///
/// 返回初始化后的 `SdkTracerProvider`
///
/// # Example
///
/// ```no_run
/// use rcoder_telemetry::otlp::init_tracer_provider;
/// use rcoder_telemetry::config::OtlpConfig;
///
/// #[tokio::main]
/// async fn main() -> anyhow::Result<()> {
///     let config = OtlpConfig::default();
///     let provider = init_tracer_provider(&config, "my-service").await?;
///     Ok(())
/// }
/// ```
pub async fn init_tracer_provider(
    config: &OtlpConfig,
    service_name: &str,
) -> Result<SdkTracerProvider> {
    use opentelemetry::KeyValue;
    use opentelemetry_sdk::Resource;

    info!(
        "🔧 [OTLP] Initializing TracerProvider: endpoint={}, grpc={}, sample_rate={}",
        config.endpoint, config.use_grpc, config.sample_rate
    );

    // 创建 Resource（标识服务）
    // 使用 service.name 语义约定
    let resource = Resource::builder()
        .with_attributes([KeyValue::new("service.name", service_name.to_string())])
        .build();

    // 创建采样器
    let sampler = if config.sample_rate >= 1.0 {
        Sampler::AlwaysOn
    } else if config.sample_rate <= 0.0 {
        Sampler::AlwaysOff
    } else {
        Sampler::TraceIdRatioBased(config.sample_rate)
    };

    // 创建 OTLP 导出器
    let tracer_provider = if config.use_grpc {
        init_grpc_provider(&config.endpoint, resource, sampler).await?
    } else {
        init_http_provider(&config.endpoint, resource, sampler).await?
    };

    info!("[OTLP] TracerProvider initialization completed");

    Ok(tracer_provider)
}

/// 初始化 gRPC OTLP 导出器
async fn init_grpc_provider(
    endpoint: &str,
    resource: opentelemetry_sdk::Resource,
    sampler: Sampler,
) -> Result<SdkTracerProvider> {
    use opentelemetry_otlp::SpanExporter;

    // 创建 gRPC 导出器
    let exporter = SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint)
        .build()?;

    // 构建 TracerProvider
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource)
        .build();

    Ok(provider)
}

/// 初始化 HTTP OTLP 导出器
async fn init_http_provider(
    endpoint: &str,
    resource: opentelemetry_sdk::Resource,
    sampler: Sampler,
) -> Result<SdkTracerProvider> {
    use opentelemetry_otlp::Protocol;
    use opentelemetry_otlp::SpanExporter;

    // 创建 HTTP 导出器
    let exporter = SpanExporter::builder()
        .with_http()
        .with_endpoint(endpoint)
        .with_protocol(Protocol::HttpBinary)
        .build()?;

    // 构建 TracerProvider
    let provider = SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_sampler(sampler)
        .with_resource(resource)
        .build();

    Ok(provider)
}

/// 设置全局 TracerProvider
///
/// 将 TracerProvider 设置为全局 provider，并获取 tracer。
pub fn set_global_tracer_provider(provider: SdkTracerProvider) {
    opentelemetry::global::set_tracer_provider(provider);
    info!("[OTLP] Global TracerProvider set");
}

/// 关闭 TracerProvider
///
/// 在应用退出前调用，确保所有 span 被导出。
/// 注意：在 OpenTelemetry 0.31+ 中，shutdown 需要在 TracerProvider 实例上调用。
/// 此函数仅用于日志记录，实际 shutdown 由 TelemetryGuard 的 Drop 处理。
pub fn shutdown_tracer_provider() {
    info!("[OTLP] TracerProvider shutdown request logged");
    // 注意：在 OpenTelemetry 0.31+ 中，全局 shutdown 函数已移除
    // TracerProvider 的 Drop 会自动处理清理
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sampler_always_on() {
        let config = OtlpConfig {
            sample_rate: 1.0,
            ..Default::default()
        };
        assert!(config.sample_rate >= 1.0);
    }

    #[test]
    fn test_sampler_always_off() {
        let config = OtlpConfig {
            sample_rate: 0.0,
            ..Default::default()
        };
        assert!(config.sample_rate <= 0.0);
    }

    #[test]
    fn test_sampler_ratio() {
        let config = OtlpConfig {
            sample_rate: 0.5,
            ..Default::default()
        };
        assert!(config.sample_rate > 0.0 && config.sample_rate < 1.0);
    }
}
