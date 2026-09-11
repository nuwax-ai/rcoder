//! tracing subscriber 组装：EnvFilter + 终端/文件日志 + OTLP + 外部注入层。
//!
//! boxed layer（[`BoxedLayer`]）只能直接挂 Registry 顶层——外部注入层
//! （file-server 嵌入、tokio-console 观测）经 `stack_boxed_layers` 叠加。

use anyhow::Result;
use opentelemetry_sdk::trace::SdkTracerProvider;
use serde_json::{Map, Value, json};
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
        let meta = event.metadata();
        let mut obj = Map::new();

        // === 与标准 Format<Json> 逐字段对齐（timestamp 含微秒精度）===
        let timestamp = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Micros, false);
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

        // === span 上下文（完整 span 对象 + span 链数组，对齐标准 formatter）===
        // 标准 Format<Json> 输出 "span":{...fields..., "name":"..."} 和
        // "spans":[{...},...]（display_current_span/display_span_list 默认 true）
        Self::insert_span_context(&mut obj, ctx);

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

    /// 插入 `"span":{...}` 和 `"spans":[...]`——与标准 Format<Json> 对齐。
    ///
    /// 从每个 span 的 `FormattedFields<N>` 读取已格式化的字段（JSON 键值对），
    /// 解析为 JSON Value 后加 `"name"` 字段。span 链按 current → parent 顺序。
    fn insert_span_context<S, N>(obj: &mut Map<String, Value>, ctx: &fmt::FmtContext<'_, S, N>)
    where
        S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
        N: for<'a> fmt::format::FormatFields<'a> + 'static,
    {
        use tracing_subscriber::fmt::FormattedFields;

        let mut spans_array = Vec::new();
        let mut current = ctx.lookup_current();

        while let Some(span) = current {
            let mut span_obj = Map::new();
            // 读取 FormattedFields（span 的已格式化字段——method/uri/trace_id 等）
            let span_fields = span
                .extensions()
                .get::<FormattedFields<N>>()
                .and_then(|ff| serde_json::from_str::<Value>(&ff.fields).ok())
                .and_then(|v| match v {
                    Value::Object(map) => Some(map),
                    _ => None,
                });
            if let Some(fields) = span_fields {
                for (k, v) in fields {
                    span_obj.insert(k, v);
                }
            }
            span_obj.insert("name".to_string(), json!(span.metadata().name()));

            // 第一个（current）写入 "span"，全部写入 "spans"
            if spans_array.is_empty() {
                obj.insert("span".to_string(), Value::Object(span_obj.clone()));
            }
            spans_array.push(Value::Object(span_obj));

            current = span.parent();
        }

        if !spans_array.is_empty() {
            obj.insert("spans".to_string(), Value::Array(spans_array));
        }
    }
}

/// subscriber 组装参数（init_tracing_subscriber 入参结构体——
/// 多入参收拢为单一事实源，调用方构造清晰、扩展只加字段不改签名）
pub(crate) struct SubscriberParams<'a> {
    /// 服务名称（EnvFilter 默认指令 / tracer 标识）
    pub service_name: &'a str,
    /// OTLP TracerProvider（None 时装 no-op provider，保留 span context 基础设施）
    pub tracer_provider: Option<&'a SdkTracerProvider>,
    /// 文件日志配置（None 则无文件层）
    pub file_log: Option<&'a FileLogConfig>,
    /// 额外 boxed layer（如 file-server 独立日志）
    pub extra_layer: Option<BoxedLayer>,
    /// tokio-console 观测 layer（本地开发 feature 注入）
    pub tokio_console_layer: Option<BoxedLayer>,
    /// span 耗时→直方图规则（SpanMetricsLayer）
    pub span_metrics: Vec<crate::span_metrics::SpanMetricRule>,
    /// 控制台（stdout）日志 JSON 化（`TELEMETRY_CONSOLE_JSON`；默认 false=ANSI 文本）
    pub console_json: bool,
}

pub(crate) fn init_tracing_subscriber(params: SubscriberParams<'_>) -> Result<()> {
    use opentelemetry::trace::TracerProvider;

    let SubscriberParams {
        service_name,
        tracer_provider,
        file_log: file_log_config,
        extra_layer,
        tokio_console_layer,
        span_metrics,
        console_json,
    } = params;

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

    // 控制台日志层（JSON 模式复用文件层同款 formatter + OTel 噪声过滤，
    // 文本模式保持原行为；见 build_console_layer）
    let console_layer = build_console_layer(console_json, deny_tokio, std::io::stdout);

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
    // （no-op 仍安装 OpenTelemetryLayer——提供 span context 存储基础设施）。
    // EnvFilter 为 console 放行的 tokio/runtime trace 级 span（每秒上万）必须
    // 在此 deny：OTel 层漏拦会把 BatchSpanProcessor 队列持续打满强制导出，
    // 导出 gRPC 本身又 spawn 任务生成新的 runtime span，正反馈下 RSS 以
    // ~30MB/s 爬升（compose 实测 10 分钟涨至 25GB）
    let otel_layer = {
        static NOOP_PROVIDER: std::sync::OnceLock<SdkTracerProvider> = std::sync::OnceLock::new();
        let tracer = match tracer_provider {
            Some(provider) => provider.tracer(service_name.to_string()),
            None => {
                let provider = NOOP_PROVIDER.get_or_init(|| SdkTracerProvider::builder().build());
                provider.tracer(service_name.to_string())
            }
        };
        tracing_opentelemetry::layer()
            .with_tracer(tracer)
            .with_filter(filter_fn(make_deny_filter(deny_tokio)))
    };

    // 组装 subscriber 链
    let has_tokio_console = tokio_console_layer.is_some();
    // 顺序约束：stack_boxed_layers（BoxedLayer 只支持 Registry 顶层）最先；
    // TraceIdExtractor 是泛型 Layer<S>，可挂任意层之后
    let registry = tracing_subscriber::registry()
        .with(stack_boxed_layers(extra_layer, tokio_console_layer))
        .with(TraceIdExtractor)
        .with(crate::span_metrics::SpanMetricsLayer::new(span_metrics))
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .with(otel_layer);
    registry.init();
    if has_tokio_console {
        info!("[Telemetry] tokio-console observation layer enabled");
    }

    Ok(())
}

/// 控制台日志层构建器（`TELEMETRY_CONSOLE_JSON` 开关的两分支）。
///
/// - `console_json=true`：与文件 JSON 层同款 [`TraceIdJsonFormat`]（单行 JSON +
///   root `trace_id`）+ `with_ansi(false)`（ANSI 转义序列会污染结构化流）+
///   [`make_json_console_filter`]（额外拦两种 OTel 噪声拼写）。面向 stdout 被
///   重定向进 `rcoder.log`、由日志采集器消费的场景。
/// - `console_json=false`：原文本行为逐字段不变（ANSI + target，无 thread/file）。
///
/// S 泛型 + 返回 `Box<dyn Layer<S>>`：boxed 对象只能挂在类型精确匹配的
/// subscriber 链位置（S 由调用点的 `.with()` 位置推断，与文件层同一模式）。
/// writer 参数化仅为测试注入（生产传 `std::io::stdout`）。
fn build_console_layer<S, W>(
    console_json: bool,
    deny_tokio: bool,
    writer: W,
) -> Box<dyn Layer<S> + Send + Sync + 'static>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    W: for<'a> fmt::MakeWriter<'a> + Send + Sync + 'static,
{
    if console_json {
        fmt::layer()
            .json()
            .event_format(TraceIdJsonFormat)
            .with_ansi(false)
            .with_writer(writer)
            .with_filter(filter_fn(make_json_console_filter(deny_tokio)))
            .boxed()
    } else {
        fmt::layer()
            .with_target(true)
            .with_ansi(true)
            .with_thread_ids(false)
            .with_file(false)
            .with_line_number(false)
            .with_writer(writer)
            .with_filter(filter_fn(make_deny_filter(deny_tokio)))
            .boxed()
    }
}

/// JSON 控制台模式专用 deny 过滤器：[`make_deny_filter`] 的全量语义（file_server /
/// tokio-console 时的 tokio+runtime）+ 额外拦截 OTel 导出噪声的**两种拼写**
/// （`opentelemetry-otlp` 连字符 / `opentelemetry_sdk` 下划线——B0 基线两者并存，
/// 仅拦一种漏 56%，合计占当日日志 20.3%）。
///
/// 仅 JSON 模式启用：JSON 面向采集器的结构化流，噪声行污染 Loki 检索；文本模式
/// （本地开发）保持原行为，便于排查 OTLP 本身的问题。
/// 不并入 [`make_deny_filter`]：该函数被 OTel/文件层共用，是 OOM 敏感路径，
/// 语义任何变化都可能改变生产导出行为。
fn make_json_console_filter(
    deny_tokio: bool,
) -> impl Fn(&tracing::Metadata<'_>) -> bool + Send + Sync + 'static {
    move |meta: &tracing::Metadata<'_>| {
        let target = meta.target();
        let deny = target.starts_with("file_server")
            || target.starts_with("opentelemetry-otlp")
            || target.starts_with("opentelemetry_sdk")
            || (deny_tokio && (target.starts_with("tokio") || target.starts_with("runtime")));
        !deny
    }
}

/// per-layer deny 过滤器工厂：deny `file_server` target（独立日志域），
/// `deny_tokio`（console 开启）时额外 deny `tokio`/`runtime` target——
/// EnvFilter 为 console 放行的这两类 trace 级 span 每秒上万，fmt/文件/OTel
/// 三个消费层都必须各自拦截（`filter_fn` 要求 `'static`，闭包经参数化工厂生成）。
fn make_deny_filter(
    deny_tokio: bool,
) -> impl Fn(&tracing::Metadata<'_>) -> bool + Send + Sync + 'static {
    move |meta: &tracing::Metadata<'_>| {
        let deny = meta.target().starts_with("file_server")
            || (deny_tokio
                && (meta.target().starts_with("tokio") || meta.target().starts_with("runtime")));
        !deny
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
struct JsonFieldVisitor<'a>(&'a mut Map<String, Value>);

impl tracing::field::Visit for JsonFieldVisitor<'_> {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .insert(field.name().to_string(), json!(format!("{value:?}")));
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        self.0.insert(field.name().to_string(), json!(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), json!(value));
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

#[cfg(test)]
mod otel_deny_tests {
    use super::*;

    /// 复现生产装配的 OTel 层 deny 行为：EnvFilter 为 tokio-console 放行
    /// `runtime=trace, tokio=trace` 后，这两类 target 的 span（console 场景
    /// 每秒上万）不得进入 OTel 导出——漏拦会把 BatchSpanProcessor 队列持续
    /// 打满强制导出，导出 gRPC 又 spawn 新任务生成 runtime span，正反馈下
    /// RSS 以 ~30MB/s 爬升（compose 实测 10 分钟涨至 25GB）。
    #[test]
    fn otel_layer_excludes_tokio_runtime_spans_under_console_env_filter() {
        use opentelemetry::trace::TracerProvider as _;
        use opentelemetry_sdk::testing::trace::new_test_exporter;

        let (exporter, mut rx_export, _rx_shutdown) = new_test_exporter();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter)
            .build();

        // 与 init_tracing_subscriber 同构：console 开启 → EnvFilter 放行
        // trace 级 tokio/runtime，otel 层挂 make_deny_filter(true)
        let env_filter = EnvFilter::new("debug,runtime=trace,tokio=trace");
        let otel = tracing_opentelemetry::layer()
            .with_tracer(provider.tracer("otel-deny-test"))
            .with_filter(filter_fn(make_deny_filter(true)));

        let subscriber = tracing_subscriber::registry().with(env_filter).with(otel);

        tracing::subscriber::with_default(subscriber, || {
            let _s1 = tracing::trace_span!(target: "runtime::tokio", "runtime.spawn").entered();
            let _s2 = tracing::trace_span!(target: "tokio::task", "task.spawn").entered();
            let _s3 = tracing::info_span!(target: "rcoder::handler", "http_request").entered();
        });
        drop(provider.force_flush());

        let mut names = Vec::new();
        while let Ok(span) = rx_export.try_recv() {
            names.push(span.name);
        }
        assert_eq!(
            names,
            ["http_request"],
            "runtime/tokio target 的 span 漏进了 OTel 导出: {names:?}"
        );
    }
}

#[cfg(test)]
mod extra_layer_tests {
    use super::*;
    use tracing_appender::non_blocking;
    use tracing_subscriber::filter::Targets;

    /// 复现生产装配：extra_layer（file-server 独立层, per-layer Targets）挂在 registry
    /// 最内层、EnvFilter（RUST_LOG=debug,...）在外层——rcoder 主容器的真实拓扑。
    /// 若该装配下 file_server 事件写不进文件, /app/logs/file-server 将全空（生产症状）。
    #[test]
    fn extra_layer_receives_file_server_events_under_global_debug_env_filter() {
        let dir = std::env::temp_dir().join(format!("rcoder-tel-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let appender = tracing_appender::rolling::Builder::new()
            .rotation(Rotation::NEVER)
            .filename_prefix("fs-test.log")
            .build(&dir)
            .unwrap();
        let (writer, guard) = non_blocking(appender);
        let extra_layer = fmt::layer()
            .with_writer(writer)
            .with_ansi(false)
            .with_filter(
                Targets::new()
                    .with_target("file_server", tracing::Level::INFO)
                    .with_default(tracing_subscriber::filter::LevelFilter::OFF),
            )
            .boxed();

        // 生产 compose 的 RUST_LOG（裸 debug 全局默认 + 三方收敛）
        let env_filter = EnvFilter::new("debug,bollard=info,h2=info,hyper=info,tonic=info");

        // 与 init_tracing_subscriber 同序: extra_layer 最内, env_filter 在外
        let subscriber = tracing_subscriber::registry()
            .with(Some(extra_layer))
            .with(env_filter)
            .with(fmt::layer().with_writer(std::io::sink));

        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(target: "file_server::http", "hello from file server");
            tracing::info!(target: "rcoder::bootstrap", "hello from rcoder");
        });
        drop(guard); // flush non_blocking 缓冲

        let written = std::fs::read_to_string(dir.join("fs-test.log")).unwrap_or_default();
        std::fs::remove_dir_all(&dir).ok();
        assert!(
            written.contains("hello from file server"),
            "file_server 事件未写入 extra_layer 文件, 实际内容: {written:?}"
        );
    }
}

#[cfg(test)]
mod trace_id_json_format_tests {
    use super::*;
    use tracing_appender::rolling::{Builder, Rotation};

    const VALID_TRACE_ID: &str = "0123456789abcdef0123456789abcdef";

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rcoder-b1-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 与生产文件层同构的最小装配：TraceIdExtractor（extensions 截获）+
    /// EnvFilter + JSON formatter（TraceIdJsonFormat）写盘。
    fn write_one_event(dir: &std::path::Path, record_trace_id: bool) -> String {
        let appender = Builder::new()
            .rotation(Rotation::NEVER)
            .filename_prefix("json-test.log")
            .build(dir)
            .unwrap();
        let subscriber = tracing_subscriber::registry()
            .with(TraceIdExtractor)
            .with(EnvFilter::new("debug"))
            .with(
                fmt::layer()
                    .json()
                    .event_format(TraceIdJsonFormat)
                    .with_writer(appender)
                    .with_ansi(false),
            );
        tracing::subscriber::with_default(subscriber, || {
            // 与 make_span_with_trace_parent 同款声明：field::Empty 先占位再 record
            let span = tracing::info_span!(
                "http_request",
                method = "GET",
                uri = "/health",
                trace_id = tracing::field::Empty
            );
            let _guard = span.enter();
            if record_trace_id {
                span.record("trace_id", tracing::field::display(VALID_TRACE_ID));
            }
            tracing::info!(target: "rcoder::b1", user = "tester", "Server starting on port 8086");
        });
        std::fs::read_to_string(dir.join("json-test.log")).unwrap()
    }

    /// span.record(trace_id) 后：root 级 trace_id 存在且值等于记录值
    /// （B4 console JSON 分支复用同一 formatter，此契约即两条通道的共同保证）。
    #[test]
    fn root_trace_id_present_when_recorded() {
        let dir = temp_dir("present");
        let written = write_one_event(&dir, true);
        let obj: Value = serde_json::from_str(written.trim_end()).unwrap();
        assert_eq!(obj["trace_id"], json!(VALID_TRACE_ID));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 未 record 时 root 级 trace_id 缺席（被动继承模型的现状行为）。
    #[test]
    fn root_trace_id_absent_without_record() {
        let dir = temp_dir("absent");
        let written = write_one_event(&dir, false);
        let obj: Value = serde_json::from_str(written.trim_end()).unwrap();
        assert!(obj.get("trace_id").is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// span{}/spans[] 形态：current span 对象含 name 与已格式化字段。
    #[test]
    fn span_and_spans_shape() {
        let dir = temp_dir("spans");
        let written = write_one_event(&dir, true);
        let obj: Value = serde_json::from_str(written.trim_end()).unwrap();
        let span = &obj["span"];
        assert_eq!(span["name"], json!("http_request"));
        assert_eq!(span["method"], json!("GET"));
        assert_eq!(span["uri"], json!("/health"));
        let spans = obj["spans"].as_array().unwrap();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0]["name"], json!("http_request"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 标准 JSON 字段齐全（与标准 Format<Json> 逐字段对齐的契约）。
    #[test]
    fn standard_fields_present() {
        let dir = temp_dir("fields");
        let written = write_one_event(&dir, false);
        let obj: Value = serde_json::from_str(written.trim_end()).unwrap();
        let ts = obj["timestamp"].as_str().unwrap();
        assert!(
            ts.contains('T') && ts.len() >= 20,
            "timestamp 非 rfc3339: {ts}"
        );
        assert_eq!(obj["level"], json!("INFO"));
        assert_eq!(obj["target"], json!("rcoder::b1"));
        assert_eq!(obj["fields"]["user"], json!("tester"));
        assert!(obj["filename"].as_str().unwrap().ends_with("subscriber.rs"));
        assert!(obj["line_number"].as_u64().unwrap() > 0);
        assert!(obj["threadId"].as_str().is_some());
        assert!(obj["threadName"].as_str().is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 单行输出且整体可被 serde_json 解析（Loki `| json` 的前提）。
    #[test]
    fn single_line_parseable_json() {
        let dir = temp_dir("single");
        let written = write_one_event(&dir, true);
        let trimmed = written.trim_end_matches('\n');
        assert!(!trimmed.contains('\n'), "输出不是单行: {trimmed:?}");
        assert_eq!(trimmed.lines().count(), 1);
        assert!(serde_json::from_str::<Value>(trimmed).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// grep 契约（start-services.sh:77 依赖）：message 子串在 JSON 转义后逐字保留。
    /// serde_json 只转义 `"`/`\`/控制字符，普通子串不受影响——锁定此性质。
    #[test]
    fn message_substring_survives_json_escaping() {
        let dir = temp_dir("grep");
        let written = write_one_event(&dir, true);
        assert!(
            written.contains("Server starting on port 8086"),
            "message 子串被转义破坏: {written}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// is_valid_hex 边界：32 位 hex 通过；31/33 位与非 hex 拒绝。
    #[test]
    fn is_valid_hex_rejects_malformed() {
        assert!(TraceIdVisitor::is_valid_hex(VALID_TRACE_ID));
        assert!(
            !TraceIdVisitor::is_valid_hex(&VALID_TRACE_ID[1..]),
            "31 hex 应拒绝"
        );
        assert!(
            !TraceIdVisitor::is_valid_hex(&format!("{VALID_TRACE_ID}f")),
            "33 hex 应拒绝"
        );
        assert!(
            !TraceIdVisitor::is_valid_hex("zzzz456789abcdef0123456789abcdef"),
            "非 hex 应拒绝"
        );
        assert!(!TraceIdVisitor::is_valid_hex(""), "空串应拒绝");
    }
}

#[cfg(test)]
mod console_layer_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const VALID_TRACE_ID: &str = "0123456789abcdef0123456789abcdef";

    /// 测试用 writer：捕获 fmt layer 输出（生产传 std::io::stdout，
    /// 经 build_console_layer 的 writer 参数注入）。
    #[derive(Clone, Default)]
    struct CaptureWriter(Arc<Mutex<Vec<u8>>>);

    impl std::io::Write for CaptureWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> fmt::MakeWriter<'a> for CaptureWriter {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    fn captured_string(writer: &CaptureWriter) -> String {
        String::from_utf8(writer.0.lock().unwrap().clone()).unwrap()
    }

    /// 用 build_console_layer 的指定分支装配 registry 并发出事件（含 trace_id）。
    fn emit_events(console_json: bool, writer: &CaptureWriter) {
        let layer = build_console_layer(console_json, false, writer.clone());
        let subscriber = tracing_subscriber::registry()
            .with(TraceIdExtractor)
            .with(EnvFilter::new("trace"))
            .with(layer);
        tracing::subscriber::with_default(subscriber, || {
            let span = tracing::info_span!(
                "http_request",
                method = "GET",
                uri = "/health",
                trace_id = tracing::field::Empty
            );
            let _guard = span.enter();
            span.record("trace_id", tracing::field::display(VALID_TRACE_ID));
            tracing::info!(target: "rcoder::console", "Server starting on port 8086");
            // B0 基线的两种 OTel 噪声拼写
            tracing::debug!(target: "opentelemetry-otlp::exporter", retry = 3, "noise hyphen");
            tracing::debug!(target: "opentelemetry_sdk::trace", "noise underscore");
            // file_server 独立日志域（两模式都必须 deny）
            tracing::debug!(target: "file_server::router", "noise file_server");
        });
    }

    /// JSON 分支：输出单行 JSON、root trace_id 存在、grep 契约保留。
    #[test]
    fn json_branch_writes_single_line_json_with_root_trace_id() {
        let writer = CaptureWriter::default();
        emit_events(true, &writer);
        let out = captured_string(&writer);
        let line = out.trim_end();
        assert_eq!(line.lines().count(), 1, "应为单行 JSON: {out:?}");
        let obj: Value = serde_json::from_str(line).expect("JSON 分支输出应为合法 JSON");
        assert_eq!(obj["trace_id"], json!(VALID_TRACE_ID));
        assert!(
            line.contains("Server starting on port 8086"),
            "grep 契约破坏: {line}"
        );
    }

    /// JSON 分支：两种 OTel 噪声拼写 + file_server 全部被 deny，
    /// 业务 target（rcoder::console）正常通过。
    #[test]
    fn json_branch_denies_otel_noise_both_spellings_and_file_server() {
        let writer = CaptureWriter::default();
        emit_events(true, &writer);
        let out = captured_string(&writer);
        assert!(
            !out.contains("noise hyphen"),
            "opentelemetry-otlp 漏拦: {out:?}"
        );
        assert!(
            !out.contains("noise underscore"),
            "opentelemetry_sdk 漏拦: {out:?}"
        );
        assert!(
            !out.contains("noise file_server"),
            "file_server 漏拦: {out:?}"
        );
        assert!(
            out.contains("rcoder::console"),
            "业务 target 不应被拦: {out:?}"
        );
    }

    /// 文本分支：行为不变——非 JSON、无 root trace_id、OTel 噪声不拦（便于本地排查）。
    #[test]
    fn text_branch_keeps_original_shape_and_allows_otel_noise() {
        let writer = CaptureWriter::default();
        emit_events(false, &writer);
        let out = captured_string(&writer);
        assert!(
            !out.trim_start().starts_with('{'),
            "文本分支不应输出 JSON: {out:?}"
        );
        assert!(out.contains("Server starting on port 8086"));
        assert!(
            out.contains("noise hyphen"),
            "文本模式不应拦 opentelemetry-otlp: {out:?}"
        );
        assert!(
            out.contains("noise underscore"),
            "文本模式不应拦 opentelemetry_sdk: {out:?}"
        );
        assert!(
            !out.contains("noise file_server"),
            "file_server 两模式都必须拦: {out:?}"
        );
    }
}
