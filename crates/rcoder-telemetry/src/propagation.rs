//! Trace Context 传播模块
//!
//! 提供跨服务的 trace context 传播功能，支持 gRPC 和 HTTP。

use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry::Context;
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tracing::debug;

/// gRPC MetadataMap 的 Injector 实现
struct MetadataMapInjector<'a>(&'a mut MetadataMap);

impl Injector for MetadataMapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(key) = MetadataKey::from_bytes(key.as_bytes()) {
            if let Ok(value) = MetadataValue::try_from(&value) {
                self.0.insert(key, value);
            }
        }
    }
}

/// gRPC MetadataMap 的 Extractor 实现
struct MetadataMapExtractor<'a>(&'a MetadataMap);

impl Extractor for MetadataMapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .filter_map(|key| {
                if let tonic::metadata::KeyRef::Ascii(k) = key {
                    Some(k.as_str())
                } else {
                    None
                }
            })
            .collect()
    }
}

/// 注入 trace context 到 gRPC metadata
///
/// 将当前 span 的 trace context 注入到 gRPC metadata 中，
/// 用于跨服务传播。
///
/// # Arguments
///
/// * `metadata` - gRPC metadata
///
/// # Example
///
/// ```no_run
/// use tonic::metadata::MetadataMap;
/// use rcoder_telemetry::propagation::inject_context;
///
/// let mut metadata = MetadataMap::new();
/// inject_context(&mut metadata);
/// // 现在 metadata 包含 traceparent 和 tracestate headers
/// ```
pub fn inject_context(metadata: &mut MetadataMap) {
    let propagator = TraceContextPropagator::new();
    let cx = Context::current();
    let mut injector = MetadataMapInjector(metadata);
    propagator.inject_context(&cx, &mut injector);

    debug!("📤 [Propagation] Trace context 已注入到 gRPC metadata");
}

/// 从 gRPC metadata 提取 trace context
///
/// 从 gRPC metadata 中提取 trace context，
/// 用于继续跨服务的 trace。
///
/// # Arguments
///
/// * `metadata` - gRPC metadata
///
/// # Returns
///
/// 返回提取的 `Context`，如果没有找到则返回当前 context。
///
/// # Example
///
/// ```no_run
/// use tonic::metadata::MetadataMap;
/// use rcoder_telemetry::propagation::extract_context;
///
/// let metadata = MetadataMap::new();
/// let context = extract_context(&metadata);
/// // 使用 context 创建新的 span
/// ```
pub fn extract_context(metadata: &MetadataMap) -> Context {
    let propagator = TraceContextPropagator::new();
    let extractor = MetadataMapExtractor(metadata);
    let cx = propagator.extract(&extractor);

    debug!("📥 [Propagation] 从 gRPC metadata 提取 trace context");

    cx
}

/// HTTP Headers 的 Injector 实现
pub struct HttpHeaderInjector<'a>(pub &'a mut http::HeaderMap);

impl Injector for HttpHeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(name) = http::header::HeaderName::from_bytes(key.as_bytes()) {
            if let Ok(value) = http::header::HeaderValue::from_str(&value) {
                self.0.insert(name, value);
            }
        }
    }
}

/// HTTP Headers 的 Extractor 实现
pub struct HttpHeaderExtractor<'a>(pub &'a http::HeaderMap);

impl Extractor for HttpHeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0
            .keys()
            .map(|key| key.as_str())
            .collect()
    }
}

/// 注入 trace context 到 HTTP headers
///
/// # Arguments
///
/// * `headers` - HTTP headers
pub fn inject_context_http(headers: &mut http::HeaderMap) {
    let propagator = TraceContextPropagator::new();
    let cx = Context::current();
    let mut injector = HttpHeaderInjector(headers);
    propagator.inject_context(&cx, &mut injector);

    debug!("📤 [Propagation] Trace context 已注入到 HTTP headers");
}

/// 从 HTTP headers 提取 trace context
///
/// # Arguments
///
/// * `headers` - HTTP headers
///
/// # Returns
///
/// 返回提取的 `Context`
pub fn extract_context_http(headers: &http::HeaderMap) -> Context {
    let propagator = TraceContextPropagator::new();
    let extractor = HttpHeaderExtractor(headers);
    let cx = propagator.extract(&extractor);

    debug!("📥 [Propagation] 从 HTTP headers 提取 trace context");

    cx
}

/// 设置全局 text map 传播器
///
/// 应该在应用启动时调用一次。
pub fn set_global_propagator() {
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    debug!("✅ [Propagation] 全局 TraceContextPropagator 已设置");
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TraceContextExt;

    #[test]
    fn test_metadata_injector_extractor() {
        let mut metadata = MetadataMap::new();

        // 手动设置一些 metadata
        metadata.insert("traceparent", "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".parse().unwrap());

        // 提取 context
        let cx = extract_context(&metadata);
        assert!(!cx.span().span_context().trace_id().to_string().is_empty());
    }

    #[test]
    fn test_http_header_injector_extractor() {
        let mut headers = http::HeaderMap::new();

        // 手动设置 traceparent header
        headers.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".parse().unwrap(),
        );

        // 提取 context
        let cx = extract_context_http(&headers);
        assert!(!cx.span().span_context().trace_id().to_string().is_empty());
    }
}
