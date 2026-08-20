//! gRPC 入口 trace context 提取（跨服务追踪：rcoder → agent_runner）
//!
//! rcoder 侧在 `new_request_with_locale` 统一注入 W3C traceparent（见
//! rcoder/src/grpc/locale_metadata.rs）；本模块在 gRPC handler 的 `#[instrument]`
//! span 内提取并 `set_parent`——agent span 挂到与 rcoder 同一 trace，
//! OTLP → Tempo 后 Grafana 可见全链路瀑布/火焰图。
//!
//! 同时把 trace_id 写为 span field：agent 侧 JSON 日志顶层自动带 trace_id
//! （与 rcoder 侧 `http_request` span 同款机制），跨服务日志检索统一。

use tonic::Request;
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// 在 gRPC handler 的当前 `#[instrument]` span 内提取 traceparent 并挂接。
///
/// 必须在 `#[instrument]` span 体内调用（`Span::current()` 即 handler span）。
/// 无 traceparent / 无效 context（如 rcoder OTLP 关闭）时为 no-op——
/// agent span 成为独立根 span。
pub(crate) fn attach_trace_parent<T>(request: &Request<T>) {
    use opentelemetry::trace::TraceContextExt;

    let remote_cx = rcoder_telemetry::propagation::extract_context(request.metadata());
    // SpanRef 借用自 remote_cx——语句内取值（is_valid/trace_id 均 owned 返回），
    // 不跨 set_parent(remote_cx) 的 move 存活
    if !remote_cx.span().span_context().is_valid() {
        return;
    }
    let trace_id = remote_cx.span().span_context().trace_id();
    let span = tracing::Span::current();
    // 与 rcoder 侧 make_span_with_trace_parent 同款双写：
    // ① span field（日志 JSON 顶层 trace_id 可见，不依赖 exporter）
    // ② set_parent（OTLP 开启时挂到远端 trace 全链路导出）
    span.record("trace_id", tracing::field::display(trace_id));
    if let Err(e) = span.set_parent(remote_cx) {
        tracing::debug!("[Propagation] attach remote trace {trace_id} failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attach_without_traceparent_is_noop() {
        // 无 metadata 的请求：不 panic、不改当前 span
        let request = Request::new(());
        attach_trace_parent(&request);
    }
}
