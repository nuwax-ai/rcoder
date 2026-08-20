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
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        // 默认日志级别
        format!(
            "{}=debug,tower_http=debug,axum=info,hyper=info,tonic=info",
            service_name.replace('-', "_")
        )
        .into()
    });

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
            // JSON 格式文件日志
            Some(
                fmt::layer()
                    .json()
                    .with_writer(file_appender)
                    .with_ansi(false)
                    .with_target(true)
                    .with_thread_ids(true)
                    .with_thread_names(true)
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

    // OTLP layer（可选）
    let otel_layer = tracer_provider.map(|provider| {
        let tracer = provider.tracer(service_name.to_string());
        tracing_opentelemetry::layer().with_tracer(tracer)
    });

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
