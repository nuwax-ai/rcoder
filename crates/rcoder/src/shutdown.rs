//! 信号处理与优雅关闭

use std::sync::atomic::{AtomicBool, Ordering};

use docker_manager::container_stop;
use docker_manager::runtime_selection::RuntimeType;
use tracing::{error, info, warn};

pub fn setup_signal_handlers() -> tokio::sync::broadcast::Sender<()> {
    let (shutdown_tx, _) = tokio::sync::broadcast::channel(1);

    static SHUTDOWN_INITIATED: AtomicBool = AtomicBool::new(false);

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let shutdown_tx_clone = shutdown_tx.clone();
        tokio::spawn(async move {
            let sigint_result = signal(SignalKind::interrupt());
            let sigterm_result = signal(SignalKind::terminate());

            match (sigint_result, sigterm_result) {
                (Ok(mut sigint), Ok(mut sigterm)) => {
                    tokio::select! {
                        _ = sigint.recv() => {
                            if !SHUTDOWN_INITIATED.swap(true, Ordering::SeqCst) {
                                info!("received SIGINT (Ctrl+C), starting graceful shutdown...");
                                let _ = shutdown_tx_clone.send(());
                            }
                        }
                        _ = sigterm.recv() => {
                            if !SHUTDOWN_INITIATED.swap(true, Ordering::SeqCst) {
                                info!("received SIGTERM, starting graceful shutdown...");
                                let _ = shutdown_tx_clone.send(());
                            }
                        }
                    }
                }
                (Err(e), _) | (_, Err(e)) => {
                    warn!(
                        "unix signal handler failed: {}, shutdown may not be graceful",
                        e
                    );
                }
            }
        });
    }

    #[cfg(not(unix))]
    {
        let shutdown_tx_clone = shutdown_tx.clone();
        tokio::spawn(async move {
            use tokio::signal;

            if let Ok(()) = signal::ctrl_c().await {
                if !SHUTDOWN_INITIATED.swap(true, Ordering::SeqCst) {
                    info!("received Ctrl+C, starting graceful shutdown...");
                    let _ = shutdown_tx_clone.send(());
                }
            }
        });
    }

    shutdown_tx
}

pub async fn graceful_shutdown(
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    config: crate::config::AppConfig,
) {
    let _ = shutdown_rx.recv().await;

    info!("starting graceful shutdown...");

    if let Err(e) = cleanup_all_containers(&config).await {
        error!("container cleanup failed: {}", e);
    } else {
        info!("container cleanup completed");
    }

    info!("🛑 RCoder graceful shutdown completed");
}

async fn cleanup_all_containers(config: &crate::config::AppConfig) -> anyhow::Result<()> {
    info!("🧹 starting cleanup of dynamically created containers...");

    match docker_manager::runtime::RuntimeManager::runtime_type() {
        RuntimeType::Docker => {
            let docker_manager = docker_manager::global::get_global_docker_manager()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get global DockerManager: {}", e))?;

            let multi_image_config = if let Some(docker_config) = &config.docker_config {
                docker_config.get_multi_image_config()
            } else {
                shared_types::create_default_multi_image_config()
            };

            match container_stop::startup_cleanup_all_enabled_services(
                &docker_manager,
                &multi_image_config,
            )
            .await
            {
                Ok(result) => {
                    if result.successfully_removed > 0 {
                        info!(
                            "🧹 Cleaned up {} containers (all enabled services)",
                            result.successfully_removed
                        );
                    }

                    if result.failed_removals > 0 {
                        warn!(
                            "container cleanup failed: failed count={}",
                            result.failed_removals
                        );
                    }
                }
                Err(e) => {
                    warn!("container cleanup error: {}", e);
                }
            }
        }
        RuntimeType::Kubernetes => {
            let runtime = docker_manager::runtime::RuntimeManager::get()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to get runtime: {}", e))?;
            runtime
                .cleanup_all()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to cleanup runtime resources: {}", e))?;
        }
    }

    Ok(())
}
