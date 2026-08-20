//! tracing subscriber 组装：EnvFilter + 终端/文件日志 + OTLP + 外部注入层。
//!
//! boxed layer（[`BoxedLayer`]）只能直接挂 Registry 顶层——外部注入层
//! （file-server 嵌入、tokio-console 观测）经 [`stack_boxed_layers`] 叠加。

use anyhow::Result;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::info;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{
    EnvFilter, Layer, filter::filter_fn, fmt, layer::SubscriberExt, registry::Registry,
    util::SubscriberInitExt,
};

use crate::config::FileLogConfig;

/// 类型擦除的 tracing layer（用于跨 crate 注入额外日志层）。
pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

// ============================================================
// trace_id 类型化存储 + 自动提取（Layer + extensions 模式）
// ============================================================

/// trace_id 的 span extension 存储类型。
///
/// 由 [`TraceIdExtractor`] 在 `span.record("trace_id", ...)` 时自动截获并存入
/// span extensions——后续 [`TraceIdJsonFormat`] 通过 `extensions().get::<TraceIdExt>()`
/// 类型化读取（O(1) 查找），不做字符串解析。
#[derive(Debug, Clone)]
pub(crate) struct TraceIdExt(pub String);

/// 拦截 `span.record("trace_id", ...)` 并存入 extensions 的 Layer。
///
/// `make_span_with_trace_parent` 调用 `span.record("trace_id", display(tid))` 时，
/// tracing 会分发到所有 Layer 的 `on_record`——本 Layer 用 `Visit` 截获该字段值
/// 并 `extensions_mut().insert(TraceIdExt(tid))`。与 tracing-opentelemetry 的
/// `OtelDataLock` extensions 模式同构（结构化数据走 extensions，不走 fmt 字符串）。
pub(crate) struct TraceIdExtractor;

impl<S> Layer<S> for TraceIdExtractor
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    fn on_record(
        &self,
        id: &tracing::span::Id,
        values: &tracing::span::Record<'_>,
        ctx: tracing_subscriber::layer::Context<'_, S>,
    ) {
        let mut visitor = TraceIdVisitor::default();
        values.record(&mut visitor);
        if let Some(tid) = visitor.0
            && let Some(span) = ctx.span(id)
        {
            span.extensions_mut().insert(TraceIdExt(tid));
        }
    }
}

/// `Visit` 实现：识别 `trace_id` 字段（32 位 hex）。
#[derive(Default)]
struct TraceIdVisitor(Option<String>);

impl tracing::field::Visit for TraceIdVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if Self::is_trace_id(field, value) {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // `tracing::field::display(tid)` 走 record_debug（Display 的 Debug 包装）
        if field.name() == "trace_id" {
            let s = format!("{value:?}");
            let s = s.trim_matches('"');
            if Self::is_valid_hex(s) {
                self.0 = Some(s.to_owned());
            }
        }
    }
}

impl TraceIdVisitor {
    fn is_trace_id(field: &tracing::field::Field, value: &str) -> bool {
        field.name() == "trace_id" && Self::is_valid_hex(value)
    }

    /// W3C TraceId 格式：32 字符 hex
    fn is_valid_hex(s: &str) -> bool {
        s.len() == 32 && s.chars().all(|c| c.is_ascii_hexdigit())
    }
}

// ============================================================
// 自定义 JSON 格式化：与标准 Format<Json> 同款内部实现 + 顶层 trace_id
// ============================================================

/// JSON 文件日志格式化器。
///
/// 内部实现与 `tracing_subscriber::fmt::format::Format<Json>` 完全同款
/// （`serde_json::Serializer` + `serialize_map`），额外在 JSON root 注入
/// `trace_id` 字段——这是 `FormatEvent` trait 的设计用途（官方扩展点）。
pub(crate) struct TraceIdJsonFormat;

impl<S, N> fmt::format::FormatEvent<S, N> for TraceIdJsonFormat
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> fmt::format::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &fmt::FmtContext<'_, S, N>,
        mut writer: fmt::format::Writer<'_>,
        event: &tracing::Event<'_>,
    ) -> std::fmt::Result {
        use serde_json::{Map, Value, json};

        let meta = event.metadata();
        let mut obj = Map::new();

        // === 与标准 Format<Json> 逐字段对齐 ===
        let timestamp = chrono::Local::now().to_rfc3339();
        obj.insert("timestamp".to_string(), json!(timestamp));

        obj.insert("level".to_string(), json!(meta.level().as_str()));

        // event 字段：用 Visit 收集到 JSON map（同标准 "fields" 对象）
        let mut fields = Map::new();
        event.record(&mut JsonFieldVisitor(&mut fields));
        obj.insert("fields".to_string(), Value::Object(fields));

        if let Some(filename) = meta.file() {
            obj.insert("filename".to_string(), json!(filename));
        }
        if let Some(line) = meta.line() {
            obj.insert("line_number".to_string(), json!(line));
        }
        obj.insert("target".to_string(), json!(meta.target()));
        obj.insert(
            "threadId".to_string(),
            json!(format!("{:?}", std::thread::current().id())),
        );
        obj.insert(
            "threadName".to_string(),
            json!(std::thread::current().name().unwrap_or("unnamed")),
        );

        // === span 上下文（当前 span 名）===
        if let Some(span) = ctx.lookup_current() {
            obj.insert("span".to_string(), json!(span.metadata().name()));
        }

        // === ★ trace_id 在 JSON ROOT（从 extensions 类型化读取）===
        if let Some(tid) = Self::extract_trace_id(ctx) {
            obj.insert("trace_id".to_string(), json!(tid));
        }

        // 序列化输出（单行 JSON）
        let output = serde_json::to_string(&obj).map_err(|_| std::fmt::Error)?;
        writeln!(writer, "{output}")
    }
}

impl TraceIdJsonFormat {
    /// 从当前 span 链的 extensions 提取 trace_id（泛型 N 版本）。
    fn extract_trace_id<S, N>(ctx: &fmt::FmtContext<'_, S, N>) -> Option<String>
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
        N: for<'a> fmt::format::FormatFields<'a> + 'static,
    {
        let mut span = ctx.lookup_current()?;
        loop {
            if let Some(ext) = span.extensions().get::<TraceIdExt>() {
                return Some(ext.0.clone());
            }
            span = span.parent()?;
        }
    }
}

/// flame feature 的 FlushGuard 类型别名（feature off 时退化为 unit）。
#[cfg(feature = "flame")]
pub(crate) type FlameGuard = tracing_flame::FlushGuard<std::io::BufWriter<std::fs::File>>;
#[cfg(not(feature = "flame"))]
pub(crate) type FlameGuard = ();

#[allow(clippy::too_many_arguments)]
pub(crate) fn init_tracing_subscriber(
    service_name: &str,
    tracer_provider: Option<&SdkTracerProvider>,
    file_log_config: Option<&FileLogConfig>,
    extra_layer: Option<BoxedLayer>,
    tokio_console_layer: Option<BoxedLayer>,
    flame_config: Option<&crate::config::FlameConfig>,
) -> Result<Option<FlameGuard>> {
    use opentelemetry::trace::TracerProvider;

    // 创建 EnvFilter（支持 RUST_LOG 环境变量）
    let mut env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        format!(
            "{}=debug,tower_http=debug,axum=info,hyper=info,tonic=info",
            service_name.replace('-', "_")
        )
        .into()
    });
    // tokio-console 观测开启时必须放行 tokio/runtime target 的 trace 级事件。
    // 放行会让 fmt/文件层也收到（海量）——deny 过滤在 console/file layer 处理。
    if tokio_console_layer.is_some() {
        for directive in ["tokio=trace", "runtime=trace"] {
            if let Ok(d) = directive.parse() {
                env_filter = env_filter.add_directive(d);
            }
        }
    }
    let deny_tokio = tokio_console_layer.is_some();
    let make_deny_filter = |deny_tokio: bool| {
        move |meta: &tracing::Metadata<'_>| {
            let deny = meta.target().starts_with("file_server")
                || (deny_tokio
                    && (meta.target().starts_with("tokio")
                        || meta.target().starts_with("runtime")));
            !deny
        }
    };

    // 控制台日志层（deny file_server + console 开启时额外 deny tokio/runtime）
    let console_layer = fmt::layer()
        .with_target(true)
        .with_ansi(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_filter(filter_fn(make_deny_filter(deny_tokio)));

    // 文件日志层
    let file_layer = if let Some(file_config) = file_log_config {
        if !file_config.directory.exists() {
            std::fs::create_dir_all(&file_config.directory)?;
        }
        let file_appender = tracing_appender::rolling::Builder::new()
            .rotation(Rotation::DAILY)
            .filename_prefix(&file_config.filename_prefix)
            .max_log_files(file_config.max_log_files)
            .build(&file_config.directory)?;
        let deny_fs = filter_fn(make_deny_filter(deny_tokio));

        if file_config.json_format {
            // JSON 格式：自定义 formatter + 顶层 trace_id
            Some(
                fmt::layer()
                    .json()
                    .event_format(TraceIdJsonFormat)
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_filter(deny_fs)
                    .boxed(),
            )
        } else {
            Some(
                fmt::layer()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .with_filter(deny_fs)
                    .boxed(),
            )
        }
    } else {
        None
    };

    // OTLP layer：有 exporter 用真实 provider；无 exporter 用全局 no-op
    // （no-op 仍安装 OpenTelemetryLayer——提供 span context 存储基础设施）
    let otel_layer = {
        static NOOP_PROVIDER: std::sync::OnceLock<SdkTracerProvider> = std::sync::OnceLock::new();
        let tracer = match tracer_provider {
            Some(provider) => provider.tracer(service_name.to_string()),
            None => {
                let provider = NOOP_PROVIDER.get_or_init(|| SdkTracerProvider::builder().build());
                provider.tracer(service_name.to_string())
            }
        };
        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    };

    // tracing-flame 火焰图 layer（flame feature）
    #[cfg(feature = "flame")]
    let (flame_layer, flame_guard) = match flame_config {
        Some(fc) => {
            use tracing_flame::FlameLayer;
            if let Some(dir) = fc.output_path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let (layer, guard) = FlameLayer::with_file(&fc.output_path)?;
            (
                Some(layer.with_threads_collapsed(fc.collapse_threads)),
                Some(guard),
            )
        }
        None => (None, None),
    };
    #[cfg(not(feature = "flame"))]
    let _ = flame_config;

    // 组装 subscriber 链
    let has_tokio_console = tokio_console_layer.is_some();
    // TraceIdExtractor 是具体类型（非 BoxedLayer），直接挂 Registry——但必须在
    // stack_boxed_layers 之前（stack 期望 Registry 顶层），类型上 Layer<Registry> 满足
    // TraceIdExtractor 是泛型 Layer<S>（可挂任意层之后）；stack_boxed_layers
    // 的 BoxedLayer 只支持 Registry——必须最先挂
    #[cfg(feature = "flame")]
    let registry = tracing_subscriber::registry()
        .with(stack_boxed_layers(extra_layer, tokio_console_layer))
        .with(TraceIdExtractor)
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .with(flame_layer)
        .with(otel_layer);
    #[cfg(not(feature = "flame"))]
    let registry = tracing_subscriber::registry()
        .with(stack_boxed_layers(extra_layer, tokio_console_layer))
        .with(TraceIdExtractor)
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .with(otel_layer);
    registry.init();
    if has_tokio_console {
        info!("[Telemetry] tokio-console observation layer enabled");
    }
    #[cfg(feature = "flame")]
    if let Some(fc) = flame_config {
        info!(
            "[Telemetry] tracing-flame enabled (output: {:?})",
            fc.output_path
        );
    }

    #[cfg(feature = "flame")]
    {
        Ok(flame_guard)
    }
    #[cfg(not(feature = "flame"))]
    {
        Ok(None)
    }
}

/// 两个 boxed layer（Option 包装，None 为 no-op）叠加为单层。
fn stack_boxed_layers(
    a: Option<BoxedLayer>,
    b: Option<BoxedLayer>,
) -> tracing_subscriber::layer::Layered<Option<BoxedLayer>, Option<BoxedLayer>, Registry> {
    <Option<BoxedLayer> as Layer<Registry>>::and_then(a, b)
}

/// event 字段的 JSON Visit 收集器（把 event.record() 转为 JSON map）。
struct JsonFieldVisitor<'a>(&'a mut serde_json::Map<String, serde_json::Value>);

impl tracing::field::Visit for JsonFieldVisitor<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(
            field.name().to_string(),
            serde_json::json!(format!("{value:?}")),
        );
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0
            .insert(field.name().to_string(), serde_json::json!(value));
    }

    fn record_error(
        &mut self,
        field: &tracing::field::Field,
        value: &(dyn std::error::Error + 'static),
    ) {
        self.0.insert(
            field.name().to_string(),
            serde_json::json!(value.to_string()),
        );
    }
}
