//! Trace Context 传播模块
//!
//! 提供跨服务的 trace context 传播功能，支持 gRPC 和 HTTP。

use opentelemetry::Context;
use opentelemetry::propagation::{Extractor, Injector, TextMapPropagator};
use opentelemetry_sdk::propagation::TraceContextPropagator;
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tracing::debug;

/// gRPC MetadataMap 的 Injector 实现
struct MetadataMapInjector<'a>(&'a mut MetadataMap);

impl Injector for MetadataMapInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(key) = MetadataKey::from_bytes(key.as_bytes())
            && let Ok(value) = MetadataValue::try_from(&value)
        {
            self.0.insert(key, value);
        }
    }
}

/// gRPC MetadataMap 的 Extractor 实现
struct MetadataMapExtractor<'a>(&'a MetadataMap);

impl Extractor for MetadataMapExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
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
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let propagator = TraceContextPropagator::new();
    // 必须取当前 tracing span 的 otel context（存于 span extensions）——
    // 纯 otel 的 Context::current() 在 tracing 世界是空 context（无 scope
    // guard），注入会静默跳过 traceparent（零调用方时期潜伏的 bug）
    let cx = tracing::Span::current().context();
    let mut injector = MetadataMapInjector(metadata);
    propagator.inject_context(&cx, &mut injector);

    let tp = injector.0.get("traceparent").and_then(|v| v.to_str().ok());
    debug!("[Propagation] gRPC metadata traceparent = {:?}", tp);
}

/// Extracting trace context from gRPC metadata
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

    debug!("[Propagation] Extracting trace context from gRPC metadata");

    cx
}

/// HTTP Headers 的 Injector 实现
pub struct HttpHeaderInjector<'a>(pub &'a mut http::HeaderMap);

impl Injector for HttpHeaderInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        if let Ok(name) = http::header::HeaderName::from_bytes(key.as_bytes())
            && let Ok(value) = http::header::HeaderValue::from_str(&value)
        {
            self.0.insert(name, value);
        }
    }
}

/// HTTP Headers 的 Extractor 实现
pub struct HttpHeaderExtractor<'a>(pub &'a http::HeaderMap);

impl Extractor for HttpHeaderExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|value| value.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        self.0.keys().map(|key| key.as_str()).collect()
    }
}

/// 注入 trace context 到 HTTP headers
///
/// # Arguments
///
/// * `headers` - HTTP headers
pub fn inject_context_http(headers: &mut http::HeaderMap) {
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let propagator = TraceContextPropagator::new();
    // 同 inject_context：取 tracing span 的 otel context 而非空 scope context
    let cx = tracing::Span::current().context();
    let mut injector = HttpHeaderInjector(headers);
    propagator.inject_context(&cx, &mut injector);

    debug!("[Propagation] Trace context injected into HTTP headers");
}

/// Extracting trace context from HTTP headers
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

    debug!("[Propagation] Extracting trace context from HTTP headers");

    cx
}

/// tower_http TraceLayer 的 make_span_with 用 span 构造器：请求 span 继承
/// 入站 W3C traceparent 指定的远端父上下文（e2e/上游注入 trace 贯通）；
/// 无 header 或格式非法时退化为根 span（与原 TraceLayer 行为一致）。
///
/// 关联：OTLP 开启时全链路同一 trace；HttpResult 的 tid 经
/// `Span::current().context()` 读取（子 span 关联需 OTLP layer 安装）。
pub fn make_span_with_trace_parent<B>(req: &http::Request<B>) -> tracing::Span {
    use opentelemetry::trace::TraceContextExt;
    use tracing_opentelemetry::OpenTelemetrySpanExt;

    let span = tracing::info_span!(
        "http_request",
        method = %req.method(),
        uri = %req.uri(),
        // trace_id 直接作为 tracing span field（JSON 日志自动可见；无需
        // OTel layer——OTLP 关闭时 set_parent 的 context 不生效，但 span
        // field 始终可读）。OTLP 开启时额外 set_parent 做全链路 export。
        trace_id = tracing::field::Empty,
    );
    let remote_cx = extract_context_http(req.headers());
    let valid = remote_cx.span().span_context().is_valid();
    if valid {
        let otel_span = remote_cx.span();
        let sc = otel_span.span_context();
        let trace_id = sc.trace_id();
        // ① 直接写入 tracing span field（日志 JSON 可靠可见）
        span.record("trace_id", tracing::field::display(trace_id));
        // ② set_parent（OTLP 开启时全链路同一 trace export；失败仅降级）
        if let Err(e) = span.set_parent(remote_cx) {
            tracing::debug!("[Propagation] attach remote trace {trace_id} failed: {e}");
        } else {
            tracing::debug!("[Propagation] request span attached to remote trace: {trace_id}");
        }
    }
    span
}

/// 设置全局 text map 传播器
///
/// 应该在应用启动时调用一次。
pub fn set_global_propagator() {
    opentelemetry::global::set_text_map_propagator(TraceContextPropagator::new());
    debug!("[Propagation] Global TraceContextPropagator set");
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::TraceContextExt;

    #[test]
    fn test_metadata_injector_extractor() {
        let mut metadata = MetadataMap::new();

        // 手动设置一些 metadata
        metadata.insert(
            "traceparent",
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );

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
            "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01"
                .parse()
                .unwrap(),
        );

        // 提取 context
        let cx = extract_context_http(&headers);
        assert!(!cx.span().span_context().trace_id().to_string().is_empty());
    }
}

#[cfg(test)]
mod inject_tests {
    use super::inject_context;
    use opentelemetry::trace::TracerProvider;
    use tracing_subscriber::prelude::*;

    /// inject_context 必须取当前 tracing span 的 otel context（span extensions），
    /// 而非纯 otel scope 的 Context::current()（tracing 世界恒为空 → 静默跳过
    /// traceparent —— 跨服务追踪断链的潜伏 bug 回归测试）
    #[test]
    fn inject_context_writes_traceparent_of_current_tracing_span() {
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("propagation-test");
        let subscriber =
            tracing_subscriber::registry().with(tracing_opentelemetry::layer().with_tracer(tracer));

        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!("test_inject_span");
            let _guard = span.enter();
            let mut metadata = tonic::metadata::MetadataMap::new();
            inject_context(&mut metadata);
            let tp = metadata
                .get("traceparent")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default();
            // W3C: 00-{32 hex trace id}-{16 hex span id}-01
            assert_eq!(tp.len(), 55, "traceparent 应写入有效值，实际: {tp:?}");
        });
    }
}

