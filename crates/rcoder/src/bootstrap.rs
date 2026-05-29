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

    let cli_args = CliArgs::parse();
    let config = load_config_with_args(cli_args)?;

    let api_key_config = Arc::new(ArcSwap::from_pointee(config.api_key_auth.clone()));

    let file_log_config = FileLogConfig::new("logs", "rcoder")
        .with_max_files(config.cleanup_config.log_cleanup.log_retention_days as usize);

    let telemetry_config = TelemetryConfig::from_env("rcoder").with_file_log_config(file_log_config);
    let telemetry: TelemetryGuard = rcoder_telemetry::init(telemetry_config).await?;
    let telemetry = Arc::new(telemetry);

    info!("Starting rcoder - AI-powered development platform");
    info!("📦 rcoder version: {}", env!("CARGO_PKG_VERSION"));
    info!(
        "📋 Log config: keeping log files for {} days",
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
