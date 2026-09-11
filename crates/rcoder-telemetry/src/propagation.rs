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
    // 无上游 traceparent 时合成（见 remote_context_or_synthesized）——
    // 日志 field、OTel span、注入的 traceparent、Tempo 四处同一 id。
    let parent_cx = remote_context_or_synthesized(extract_context_http(req.headers()));
    let trace_id = parent_cx.span().span_context().trace_id();
    span.record("trace_id", tracing::field::display(trace_id));
    if let Err(e) = span.set_parent(parent_cx) {
        tracing::debug!("[Propagation] request span attach trace {trace_id} failed: {e}");
    } else {
        tracing::debug!("[Propagation] request span attached to trace: {trace_id}");
    }
    span
}

/// 入站 context 有效则原样返回；否则合成一个 remote parent context。
///
/// K8s（无 OTLP exporter）下 subscriber 装的是 no-op SDK provider（带
/// RandomIdGenerator 的真实 SDK）——`Span::current().context()` 本就有
/// 随机 trace_id，但请求 span 自己不挂父，日志的 trace_id 字段恒空。
/// 直接「随机 id + record」会让日志与注入出去的 traceparent / Tempo 分叉
/// （两个不同 id，无法 join）。因此把随机 id 装成**合成的 remote parent**
/// 再走既有 record + set_parent 路径，四处保持同一 id。
///
/// 采样语义：`TraceFlags::SAMPLED` 使无上游请求恒采样（SDK 默认
/// ParentBased 下 remote parent 采样即继承）；默认采样率 1.0 无差异，
/// 调低 `OTEL_TRACES_SAMPLER_ARG` 前需重新评估。
pub fn remote_context_or_synthesized(extracted: Context) -> Context {
    use opentelemetry::trace::{
        SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState,
    };

    if extracted.span().span_context().is_valid() {
        return extracted;
    }
    let synthesized = SpanContext::new(
        TraceId::from(rand::random::<u128>()),
        SpanId::from(rand::random::<u64>()),
        TraceFlags::SAMPLED,
        /* is_remote = */ true,
        TraceState::NONE,
    );
    Context::current().with_remote_span_context(synthesized)
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

/// gRPC 服务端入口 span 构造（traceparent 创建期挂接）——宏形式：
/// tracing 的 span 名仅接受字面量（callsite 缓存），函数无法传动态名。
///
/// 与 HTTP 版 [`make_span_with_trace_parent`] 同语义：span 处于 Builder
/// 状态时 set_parent（started 后会被 tracing-opentelemetry 拒绝——
/// `SetParentError::AlreadyStarted`，intentional design），并把 trace_id
/// 写为 span field（日志 JSON 顶层可见）。
///
/// 用法（tonic handler，替代 `#[instrument]`）：
/// `let span = grpc_span!("chat", request.metadata());`
/// `chat::chat(&self.app_state, request).instrument(span).await`
#[macro_export]
macro_rules! grpc_span {
    ($name:literal, $metadata:expr) => {{
        // 宏卫生：trait 导入必须置于块首（if 条件里的 .span() 也依赖之）
        use opentelemetry::trace::TraceContextExt as _;
        use tracing_opentelemetry::OpenTelemetrySpanExt as _;
        let span = tracing::info_span!($name, trace_id = tracing::field::Empty);
        // 与 HTTP 版同语义：无上游时合成（remote_context_or_synthesized），
        // 日志 field / OTel span / 注入 traceparent 四处同一 id
        let parent_cx = $crate::propagation::remote_context_or_synthesized(
            $crate::propagation::extract_context($metadata),
        );
        let trace_id = parent_cx.span().span_context().trace_id();
        span.record("trace_id", tracing::field::display(trace_id));
        if let Err(e) = span.set_parent(parent_cx) {
            tracing::debug!("[Propagation] gRPC span attach trace {trace_id} failed: {e}");
        }
        span
    }};
}

#[cfg(test)]
mod synth_trace_tests {
    use super::*;
    use crate::subscriber::{TraceIdExtractor, TraceIdJsonFormat};
    use opentelemetry::trace::TraceContextExt as _;
    use opentelemetry::trace::TracerProvider;
    use opentelemetry_sdk::trace::SdkTracerProvider;
    use tracing_appender::rolling::{Builder, Rotation};
    use tracing_subscriber::{EnvFilter, layer::SubscriberExt};

    const INBOUND_TRACE_ID: &str = "0af7651916cd43dd8448eb211c80319c";

    /// 无上游 traceparent ⇒ 合成 remote parent：context 有效、trace_id 为 32 位 hex。
    #[test]
    fn synthesizes_when_no_upstream() {
        let cx = remote_context_or_synthesized(Context::new());
        let span = cx.span();
        let sc = span.span_context();
        assert!(sc.is_valid(), "合成 context 应有效");
        let tid = sc.trace_id().to_string();
        assert_eq!(tid.len(), 32);
        assert!(tid.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// 有上游 traceparent ⇒ 原样透传（id 等于入站值，无回归）。
    #[test]
    fn passes_through_valid_upstream() {
        let mut metadata = MetadataMap::new();
        metadata.insert(
            "traceparent",
            format!("00-{INBOUND_TRACE_ID}-b7ad6b7169203331-01")
                .parse()
                .unwrap(),
        );
        let cx = remote_context_or_synthesized(extract_context(&metadata));
        assert_eq!(
            cx.span().span_context().trace_id().to_string(),
            INBOUND_TRACE_ID
        );
    }

    /// 两次调用产生不同 id（随机性）。
    #[test]
    fn distinct_ids_across_calls() {
        let a = remote_context_or_synthesized(Context::new());
        let b = remote_context_or_synthesized(Context::new());
        assert_ne!(
            a.span().span_context().trace_id(),
            b.span().span_context().trace_id()
        );
    }

    /// 防分叉关键测试：无上游请求下，注入出去的 traceparent 与写进日志
    /// JSON root 的 trace_id 是**同一个 id**（装成 remote parent 的意义所在）。
    /// 装配与生产同构：no-op SDK tracer（K8s 形态）+ TraceIdExtractor + JSON 层。
    #[test]
    fn injected_traceparent_matches_logged_trace_id_without_upstream() {
        let dir = std::env::temp_dir().join(format!("rcoder-b2-synth-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let appender = Builder::new()
            .rotation(Rotation::NEVER)
            .filename_prefix("synth-test.log")
            .build(&dir)
            .unwrap();

        let provider = SdkTracerProvider::builder().build();
        let tracer = provider.tracer("synth-test");
        let subscriber = tracing_subscriber::registry()
            .with(TraceIdExtractor)
            .with(EnvFilter::new("debug"))
            .with(
                tracing_subscriber::fmt::layer()
                    .json()
                    .event_format(TraceIdJsonFormat)
                    .with_writer(appender)
                    .with_ansi(false),
            )
            .with(tracing_opentelemetry::layer().with_tracer(tracer));

        let tp = tracing::subscriber::with_default(subscriber, || {
            let req = http::Request::builder().uri("/health").body(()).unwrap();
            let span = make_span_with_trace_parent(&req);
            let _guard = span.enter();
            tracing::warn!(target: "rcoder::b2", "synth probe");
            let mut metadata = MetadataMap::new();
            inject_context(&mut metadata);
            metadata
                .get("traceparent")
                .and_then(|v| v.to_str().ok())
                .unwrap_or_default()
                .to_string()
        });

        let written = std::fs::read_to_string(dir.join("synth-test.log")).unwrap();
        std::fs::remove_dir_all(&dir).ok();
        // 文件含多条 JSON 行（Propagation debug 行等）；取探针事件所在行
        let probe_line = written
            .lines()
            .find(|l| l.contains("synth probe"))
            .expect("探针事件未写入日志");
        let obj: serde_json::Value = serde_json::from_str(probe_line).unwrap();
        let logged = obj["trace_id"].as_str().expect("日志应有 root trace_id");
        // W3C: 00-{32 hex trace id}-{16 hex span id}-{flags}
        assert_eq!(tp.len(), 55, "traceparent 应写入有效值: {tp:?}");
        assert_eq!(&tp[3..35], logged, "注入 traceparent 与日志 trace_id 分叉");
    }
}
