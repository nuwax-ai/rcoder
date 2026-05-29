mod background_tasks;
mod bootstrap;
mod config;
mod config_watcher;
mod handler;
mod cleanup_task;
mod middleware;
mod proxy_init;
mod docker_init;
mod router;
mod server;
mod service;
mod shutdown;
mod utils;

use std::sync::Arc;

use tracing::{info, warn};

use rcoder::*;

use router::AppState;

use docker_manager::runtime_selection::RuntimeType;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let bootstrap_result = bootstrap::bootstrap().await?;

    let runtime_type = RuntimeType::from_env();
    info!("Runtime type: {:?}", runtime_type);

    docker_init::init_path_resolver(runtime_type).await?;
    docker_init::init_docker_manager(&bootstrap_result.config).await?;
    docker_init::startup_cleanup(&bootstrap_result.config).await;

    let proxy_result =
        proxy_init::init_proxy(&bootstrap_result.config, Arc::clone(&bootstrap_result.api_key_config)).await;
    proxy_init::log_proxy_info(&bootstrap_result.config);

    let shutdown_tx = shutdown::setup_signal_handlers();

    let _config_watcher = if bootstrap_result.config_watcher_enabled {
        match config_watcher::ConfigWatcher::new(
            bootstrap_result.config_file_path.clone(),
            Arc::clone(&bootstrap_result.api_key_config),
        ) {
            Ok(watcher) => {
                info!("📁 Config file watcher started: {:?}", bootstrap_result.config_file_path);
                Some(watcher)
            }
            Err(e) => {
                warn!("config file watcher start failed: {}, API Key updated", e);
                None
            }
        }
    } else {
        None
    };

    let (container_prefix_rcoder, container_prefix_computer) =
        docker_init::get_container_prefixes(&bootstrap_result.config)?;

    let state = Arc::new(AppState::new(
        bootstrap_result.config.clone(),
        proxy_result.pingora_service.clone(),
        bootstrap_result.api_key_config,
        container_prefix_rcoder,
        container_prefix_computer,
    )?);

    let _bg_handles = background_tasks::start_all_background_tasks(&bootstrap_result.config, state.clone()).await?;

    let app = router::create_router(state, Some(bootstrap_result.telemetry));
    let server_handle = server::start_http_server(app, bootstrap_result.config.port, shutdown_tx.clone()).await?;

    shutdown::graceful_shutdown(shutdown_tx.subscribe(), bootstrap_result.config.clone()).await;
    server_handle.abort();

    if let Some(pingora_shutdown_tx) = proxy_result.pingora_shutdown_tx {
        let _ = pingora_shutdown_tx.send(());
    }
    if let Some(proxy_handle) = proxy_result.proxy_handle {
        let _ = proxy_handle.await;
    }

    Ok(())
}
