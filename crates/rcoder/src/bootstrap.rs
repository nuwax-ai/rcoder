//! 启动引导：Rustls 初始化、CLI 解析、配置加载、遥测初始化

use std::sync::Arc;

use arc_swap::ArcSwap;
use clap::Parser;
use rcoder_telemetry::{FileLogConfig, TelemetryConfig, TelemetryGuard};
use tracing::info;

use crate::config::{CliArgs, load_config_with_args};

pub struct BootstrapResult {
    pub config: crate::config::AppConfig,
    pub api_key_config: Arc<ArcSwap<shared_types::ApiKeyAuthConfig>>,
    pub telemetry: Arc<TelemetryGuard>,
    pub config_file_path: std::path::PathBuf,
    pub config_watcher_enabled: bool,
}

pub async fn bootstrap() -> anyhow::Result<BootstrapResult> {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    let mut cli_args = CliArgs::parse();

    // 管理子命令模式 (rcoder file-server stop 等): 不启动服务, 不初始化 telemetry,
    // 仅加载同源配置 (端口 + api key) 后 HTTP 调运行中的 rcoder 进程, 完成即退出。
    if let Some(command) = cli_args.command.take() {
        let cli_port = cli_args.port;
        let config = load_config_with_args(cli_args)?;
        let (action, fs_port) = match command {
            crate::config::AdminCommand::FileServer { action } => match action {
                crate::config::FileServerAction::Start { port } => ("start", port),
                crate::config::FileServerAction::Stop => ("stop", None),
                crate::config::FileServerAction::Restart { port } => ("restart", port),
                crate::config::FileServerAction::Status => ("status", None),
            },
        };
        let port = cli_port.unwrap_or(config.port);
        let api_key = if config.api_key_auth.enabled {
            Some(config.api_key_auth.api_key.clone())
        } else {
            None
        };
        rcoder::file_server_admin::run_cli_command(action, port, fs_port, api_key.as_deref())
            .await?;
        std::process::exit(0);
    }

    let config = load_config_with_args(cli_args)?;

    let api_key_config = Arc::new(ArcSwap::from_pointee(config.api_key_auth.clone()));

    let file_log_config = FileLogConfig::new("logs", "rcoder")
        .with_max_files(config.cleanup_config.log_cleanup.log_retention_days as usize);

    let mut telemetry_config =
        TelemetryConfig::from_env("rcoder").with_file_log_config(file_log_config);

    // 嵌入式 file-server：构建独立日志 layer + guard，注入到 rcoder 的 tracing subscriber
    if shared_types::FeatureFlags::get().embed_file_server {
        match file_server::Config::load() {
            Ok(fs_config) => match file_server::logging::build_file_layer(&fs_config) {
                Ok((layer, guard)) => {
                    // tracing 尚未 init（下方才 init），用 eprintln 保证可见（与 main.rs 一致）
                    eprintln!(
                        "[BOOTSTRAP] file-server independent log layer injected: dir={}",
                        fs_config.service_log_dir.display()
                    );
                    telemetry_config = telemetry_config.with_extra_layer(layer, guard);
                }
                Err(e) => {
                    eprintln!(
                        "[BOOTSTRAP] failed to build file-server log layer (falling back to rcoder.log): {e}"
                    );
                }
            },
            Err(e) => {
                eprintln!(
                    "[BOOTSTRAP] failed to load file-server config (file-server logs will mix into rcoder.log): {e}"
                );
            }
        }
    }

    // tokio-console 观测（console feature；shadowing 绑定——无 feature 时零代码）
    #[cfg(feature = "console")]
    let telemetry_config = crate::console_obs::attach(telemetry_config);

    // span 耗时→直方图指标规则：调用点只写 #[instrument]，耗时指标由
    // SpanMetricsLayer 自动记录（span 即计时事实源，零 Instant 侵入）
    let telemetry_config = telemetry_config.with_span_metric_rules(vec![
        rcoder_telemetry::SpanMetricRule {
            span_name: "forward_chat",
            metric: rcoder_telemetry::prometheus::GRPC_REQUEST_DURATION_SECONDS,
            label: ("method", "chat"),
        },
        rcoder_telemetry::SpanMetricRule {
            span_name: "grpc_dial",
            metric: rcoder_telemetry::prometheus::GRPC_REQUEST_DURATION_SECONDS,
            label: ("method", "dial"),
        },
        rcoder_telemetry::SpanMetricRule {
            span_name: "ensure_container_ready",
            metric: rcoder_telemetry::prometheus::CONTAINER_ENSURE_DURATION_SECONDS,
            label: ("op", "ensure"),
        },
        rcoder_telemetry::SpanMetricRule {
            span_name: "sse_subscribe",
            metric: rcoder_telemetry::prometheus::SSE_SUBSCRIPTION_DURATION_SECONDS,
            label: ("kind", "client"),
        },
    ]);

    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let telemetry = Arc::new(telemetry);

    info!("Starting rcoder - AI-powered development platform");
    info!(" rcoder version: {}", env!("CARGO_PKG_VERSION"));
    info!(
        " Log config: keeping log files for {} days",
        config.cleanup_config.log_cleanup.log_retention_days
    );

    tokio::fs::create_dir_all(&config.projects_dir)
        .await
        .map_err(|e| anyhow::anyhow!("Failed to create projects directory: {}", e))?;
    info!("Projects directory: {:?}", config.projects_dir);

    let config_file_path = std::path::PathBuf::from(crate::config::CONFIG_FILE);
    let config_watcher_enabled = config_file_path.exists();

    Ok(BootstrapResult {
        config,
        api_key_config,
        telemetry,
        config_file_path,
        config_watcher_enabled,
    })
}
