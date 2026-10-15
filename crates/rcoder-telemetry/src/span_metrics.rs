//! span→metrics 桥接层：将指定 span 的耗时自动记录为 Prometheus 直方图。
//!
//! 设计动机：`#[instrument]` 的 span 本身就在计时（创建→关闭 = 墙钟耗时），
//! 手动 `Instant::now()` + 出口记录是重复劳动且侵入业务代码。本 layer 与
//! `crate::subscriber::TraceIdExtractor` 同架构——通过 span extensions
//! 存开始时间，`on_close` 时按规则表记录直方图，调用点零计时代码。
//!
//! 规则表由 [`crate::config::TelemetryConfig::with_span_metric_rules`] 注入
//! （调用方 bootstrap 注册 span 名 → 指标名 + 标签的映射），本 crate 不感知
//! 业务 span 名。
//!
//! 注意：span 须为 INFO 级（`#[instrument]` 默认级别）才能穿过 EnvFilter。

use std::time::Instant;

use metrics::histogram;
use tracing::Subscriber;
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

/// span 耗时指标规则：span 名 → 直方图指标名 + 附加标签
#[derive(Debug, Clone)]
pub struct SpanMetricRule {
    /// 目标 span 名（如 `forward_chat`）
    pub span_name: &'static str,
    /// 直方图指标名（如 `grpc_request_duration_seconds`）
    pub metric: &'static str,
    /// 附加标签（如 `("method", "chat")`；用于同一指标族下区分 span 来源）
    pub label: (&'static str, &'static str),
}

/// span 开始时间（on_new_span 时存入 extensions，on_close 时消费；Copy 拷出避免借用 extensions 守卫）
#[derive(Clone, Copy)]
pub(crate) struct SpanStartExt(pub Instant);

/// 将规则表内 span 的耗时记录为直方图指标的 layer
pub struct SpanMetricsLayer {
    rules: Vec<SpanMetricRule>,
}

impl SpanMetricsLayer {
    pub(crate) fn new(rules: Vec<SpanMetricRule>) -> Self {
        Self { rules }
    }
}

impl<S> Layer<S> for SpanMetricsLayer
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    fn on_new_span(
        &self,
        attrs: &tracing::span::Attributes<'_>,
        id: &tracing::span::Id,
        ctx: Context<'_, S>,
    ) {
        let name = attrs.metadata().name();
        if self.rules.iter().any(|r| r.span_name == name)
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(SpanStartExt(Instant::now()));
        }
    }

    fn on_close(&self, id: tracing::span::Id, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(&id) else { return };
        let name = span.name();
        let Some(rule) = self.rules.iter().find(|r| r.span_name == name) else {
            return;
        };
        let Some(ext) = span.extensions().get::<SpanStartExt>().copied() else {
            return;
        };
        let elapsed = ext.0.elapsed().as_secs_f64();
        histogram!(rule.metric, rule.label.0 => rule.label.1).record(elapsed);
    }
}
