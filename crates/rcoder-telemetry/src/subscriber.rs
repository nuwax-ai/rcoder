//! tracing subscriber 组装：EnvFilter + 终端/文件日志 + OTLP + 外部注入层。
//!
//! boxed layer（[`BoxedLayer`]）只能直接挂 Registry 顶层——外部注入层
//! （file-server 嵌入、tokio-console 观测）经 [`stack_boxed_layers`] 叠加。

use anyhow::Result;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::info;
use tracing_appender::rolling::Rotation;
use tracing_subscriber::{
    EnvFilter, Layer, filter::filter_fn, fmt, fmt::format::Format, layer::SubscriberExt,
    registry::Registry, util::SubscriberInitExt,
};

use crate::config::FileLogConfig;

/// 类型擦除的 tracing layer（用于跨 crate 注入额外日志层）。
pub type BoxedLayer = Box<dyn Layer<Registry> + Send + Sync>;

/// JSON 日志格式化包装器：在标准 JSON 输出末尾追加 OTel trace_id/span_id。
///
/// OTLP layer 安装且当前 span 有 valid trace context 时（e2e 注入
/// traceparent 或 OTLP exporter 创建的 span），自动追加两个字段：
/// `"trace_id":"...","span_id":"..."`；无 OTel context 时不追加——
/// 日志行为与裸 `Format<Json>` 完全一致（零破坏）。
struct TraceIdFormat {
    inner: Format<fmt::format::Json, fmt::time::SystemTime>,
}

impl<S, N> fmt::format::FormatEvent<S, N> for TraceIdFormat
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
        // 缓冲标准 JSON 输出（内部会写完整 JSON + 换行）
        let mut buf = String::new();
        {
            let mut inner_writer = fmt::format::Writer::new(&mut buf);
            <Format<fmt::format::Json, fmt::time::SystemTime> as fmt::format::FormatEvent<S, N>>::format_event(
                &self.inner,
                ctx,
                inner_writer.by_ref(),
                event,
            )?;
        }
        // 去掉尾部换行，在 `}` 前插入 trace_id
        let json = buf.trim_end();
        match (trace_id_from_span_chain(ctx), json.rfind('}')) {
            (Some(tid), Some(pos)) => {
                write!(
                    writer,
                    "{},\"trace_id\":\"{}\"{}",
                    &json[..pos],
                    tid,
                    &json[pos..]
                )?;
            }
            _ => write!(writer, "{}", json)?,
        }
        writeln!(writer)
    }
}

/// 从当前 span 链向上查找 `trace_id` 字段（跳过无该字段的中间层）。
///
/// trace_id 由 `make_span_with_trace_parent` 作为 tracing span field 写入
/// `http_request` span（不依赖 OTel layer——OTLP 关闭时也可见）。
/// 事件可能在子 span（如 handler instrument span）中，需沿父链向上查找。
fn trace_id_from_span_chain<S, N>(ctx: &fmt::FmtContext<'_, S, N>) -> Option<String>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> fmt::format::FormatFields<'a> + 'static,
{
    use tracing_subscriber::fmt::FormattedFields;
    // 显式使用 JsonFields（与 .json() 层的 fmt_fields 类型一致）——泛型 N
    // 在运行时可能不匹配 FormattedFields 的实际类型参数
    type JsonFmt = fmt::format::JsonFields;

    let mut span = ctx.lookup_current()?;
    loop {
        if let Some(ff) = span.extensions().get::<FormattedFields<JsonFmt>>() {
            let tid = serde_json::from_str::<serde_json::Value>(&ff.fields)
                .ok()
                .and_then(|v| v.get("trace_id")?.as_str().map(str::to_owned));
            if tid.is_some() {
                return tid;
            }
        }
        span = span.parent()?;
    }
}

/// 初始化 tracing subscriber
///
/// 配置以下层：
/// - EnvFilter: 基于 RUST_LOG 环境变量的日志级别过滤
/// - Console Layer: 控制台日志输出（deny `file_server` target，独立写入 file-server.log）
/// - File Layer: 可选的文件日志输出（JSON 格式，按天滚动，deny `file_server` target）
/// - OpenTelemetry Layer: 如果提供了 TracerProvider，将 span 发送到 OTLP
/// - Extra Layer: 可选的外部注入 layer（如 file-server 独立日志，per-layer filter 独立过滤）
/// - TokioConsole Layer: 可选的 tokio-console 观测 layer（本地开发 feature 注入）
pub(crate) fn init_tracing_subscriber(
    service_name: &str,
    tracer_provider: Option<&SdkTracerProvider>,
    file_log_config: Option<&FileLogConfig>,
    extra_layer: Option<BoxedLayer>,
    tokio_console_layer: Option<BoxedLayer>,
) -> Result<()> {
    use opentelemetry::trace::TracerProvider;

    // 创建 EnvFilter（支持 RUST_LOG 环境变量）
    let mut env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // 默认日志级别
        format!(
            "{}=debug,tower_http=debug,axum=info,hyper=info,tonic=info",
            service_name.replace('-', "_")
        )
        .into()
    });
    // tokio-console 观测开启时必须放行 tokio/runtime target 的 trace 级事件
    // （任务/waker 事件为 trace 级；EnvFilter 的全局压制会挡住 per-layer
    // 过滤的 console 层——实测矩阵：无放行 = 零任务）。与 RUST_LOG 叠加，
    // 不影响 fmt/文件层的输出（这两个 target 无业务日志输出）。
    if tokio_console_layer.is_some() {
        for directive in ["tokio=trace", "runtime=trace"] {
            if let Ok(d) = directive.parse() {
                env_filter = env_filter.add_directive(d);
            }
        }
    }

    // 创建控制台日志层（deny file_server → file-server 日志不进 rcoder console）
    let console_layer = fmt::layer()
        .with_target(true)
        .with_ansi(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .with_filter(filter_fn(|meta: &tracing::Metadata<'_>| {
            !meta.target().starts_with("file_server")
        }));

    // 创建文件日志层（deny file_server → file-server 日志不进 rcoder.log）
    let file_layer = if let Some(file_config) = file_log_config {
        // 创建日志目录
        if !file_config.directory.exists() {
            std::fs::create_dir_all(&file_config.directory)?;
        }

        // 创建按天滚动的 appender
        let file_appender = tracing_appender::rolling::Builder::new()
            .rotation(Rotation::DAILY)
            .filename_prefix(&file_config.filename_prefix)
            .max_log_files(file_config.max_log_files)
            .build(&file_config.directory)?;

        let deny_fs =
            filter_fn(|meta: &tracing::Metadata<'_>| !meta.target().starts_with("file_server"));

        if file_config.json_format {
            // JSON 格式文件日志 + trace_id/span_id 自动注入
            // （OTel context 存在时追加——e2e traceparent 或 OTLP 均适用）
            let json_event_format = fmt::format()
                .json()
                .with_target(true)
                .with_thread_ids(true)
                .with_thread_names(true);
            Some(
                fmt::layer()
                    .json() // JsonFields 格式化（span/fields 的 JSON 键值）
                    .event_format(TraceIdFormat {
                        inner: json_event_format,
                    })
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_filter(deny_fs)
                    .boxed(),
            )
        } else {
            // 纯文本格式文件日志
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

    // OTLP layer：有 exporter 用真实 provider；无 exporter 用 no-op
    // （no-op 仍安装 OpenTelemetryLayer——提供 span context 存储基础设施：
    // set_parent / span.context() / 日志 trace_id 注入依赖它；仅不 export）
    let otel_layer = {
        let tracer = match tracer_provider {
            Some(provider) => provider.tracer(service_name.to_string()),
            None => {
                let noop = SdkTracerProvider::builder().build();
                noop.tracer(service_name.to_string())
            }
        };
        Some(tracing_opentelemetry::layer().with_tracer(tracer))
    };

    // 构建完整 subscriber 链：
    // extra_layer 先注入到 Registry 上（Box<dyn Layer<Registry>> 直接匹配 Registry），
    // 然后 env_filter（全局过滤）、console/file（deny file_server）、otel、tokio-console
    // boxed layer 只能直接挂 Registry 顶层（Box<dyn Layer<Registry>>）——
    // extra_layer 与 tokio_console_layer 叠加为单层注入
    let has_tokio_console = tokio_console_layer.is_some();
    tracing_subscriber::registry()
        .with(stack_boxed_layers(extra_layer, tokio_console_layer))
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .with(otel_layer)
        .init();
    if has_tokio_console {
        info!("[Telemetry] tokio-console observation layer enabled");
    }

    Ok(())
}

/// 两个 boxed layer（Option 包装，None 为 no-op）叠加为单层。
/// UFCS 显式走 tracing_subscriber::Layer::and_then——避开 Option::and_then
/// 的方法解析歧义。
fn stack_boxed_layers(
    a: Option<BoxedLayer>,
    b: Option<BoxedLayer>,
) -> tracing_subscriber::layer::Layered<Option<BoxedLayer>, Option<BoxedLayer>, Registry> {
    <Option<BoxedLayer> as Layer<Registry>>::and_then(a, b)
}
