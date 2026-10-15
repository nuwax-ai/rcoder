//! gRPC 入口 trace context 挂接（跨服务追踪：rcoder → agent_runner）
//!
//! rcoder 侧在 `new_request_with_locale` 统一注入 W3C traceparent；本模块在
//! span **创建期**提取并 set_parent（tracing-opentelemetry 的 set_parent 仅在
//! span 处于 Builder 状态时生效——started 后返回 `AlreadyStarted` 拒绝改父，
//! 故不能用 `#[instrument]` + 事后 set_parent 的组合，须在构造 span 时挂接，
//! body 用 `.instrument(span)` 包裹）。
//!
//! span 挂到与 rcoder 同一 trace 后（OTLP → Tempo），Grafana 可见全链路
//! 瀑布/火焰图；trace_id 同时写为 span field——agent 侧日志 JSON 顶层自动
//! 带 trace_id，跨服务日志检索统一。
//!
//! 用法见 [`grpc_span!`]（rcoder_telemetry 导出的构造宏）。

#[cfg(test)]
mod tests {
    /// 无 traceparent 的请求：grpc_span! 构造独立根 span，body 正常执行
    #[tokio::test]
    async fn grpc_span_without_traceparent_runs_body() {
        use tracing::Instrument;

        let request = tonic::Request::new(());
        let span = rcoder_telemetry::grpc_span!("test_handler", request.metadata());
        let out = async { 42 }.instrument(span).await;
        assert_eq!(out, 42);
    }
}
