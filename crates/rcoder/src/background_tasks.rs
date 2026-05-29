//! 后台任务编排

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::config::AppConfig;
use crate::router::AppState;
use crate::service::{
    ContainerStatusCheckerConfig, ContainerSyncConfig, VncSyncConfig,
    start_container_status_checker, start_container_sync_task, start_vnc_sync_task,
};
use crate::cleanup_task;

#[allow(dead_code)]
pub struct BackgroundTaskHandles {
    pub cleanup_handle: Option<tokio::task::JoinHandle<()>>,
    pub status_checker_handle: tokio::task::JoinHandle<()>,
    pub container_sync_handle: tokio::task::JoinHandle<()>,
    pub vnc_sync_handle: Option<tokio::task::JoinHandle<()>>,
}

pub async fn start_all_background_tasks(
    config: &AppConfig,
    state: Arc<AppState>,
) -> anyhow::Result<BackgroundTaskHandles> {
    let cleanup_config = cleanup_task::CleanupConfig {
        idle_timeout: Duration::from_secs(config.cleanup_config.idle_timeout_seconds),
        cleanup_interval: Duration::from_secs(config.cleanup_config.cleanup_interval_seconds),
        docker_stop_timeout: Duration::from_secs(config.cleanup_config.docker_stop_timeout_seconds),
        container_protection_duration: Duration::from_secs(
            config.cleanup_config.container_protection_seconds,
        ),
        active_window: Duration::from_secs(5 * 60),
        log_dir: config.cleanup_config.log_cleanup.log_dir.clone(),
        log_retention_duration: Duration::from_secs(
            config.cleanup_config.log_cleanup.log_retention_days * 24 * 60 * 60,
        ),
    };
    info!(
        "🧹 Cleanup config: idle_timeout={}s, cleanup_interval={}s, docker_stop_timeout={}s, container_protection={}s, log_dir={}, log_retention={}days",
        config.cleanup_config.idle_timeout_seconds,
        config.cleanup_config.cleanup_interval_seconds,
        config.cleanup_config.docker_stop_timeout_seconds,
        config.cleanup_config.container_protection_seconds,
        config.cleanup_config.log_cleanup.log_dir,
        config.cleanup_config.log_cleanup.log_retention_days
    );

    let cleanup_handle = if config.cleanup_config.enabled {
        let cleanup_config_clone = cleanup_config.clone();
        let state_for_cleanup = state.clone();
        Some(
            cleanup_task::start_cleanup_task(cleanup_config_clone, state_for_cleanup)
                .await?,
        )
    } else {
        info!("Container cleanup task already started (cleanup_config.enabled=false)");
        None
    };

    let status_checker_config = ContainerStatusCheckerConfig {
        check_interval: Duration::from_secs(30),
        query_timeout: Duration::from_secs(5),
        failure_threshold: 3,
        skip_duration: Duration::from_secs(5 * 60),
        health_reset_interval: Duration::from_secs(30 * 60),
    };
    let status_checker_handle = start_container_status_checker(status_checker_config, state.clone());
    info!("Container status checker already started (interval: 30s, will skip Docker on failure)");

    let container_sync_config = ContainerSyncConfig {
        sync_interval: Duration::from_secs(60),
    };
    let container_sync_handle =
        start_container_sync_task(container_sync_config, state.grpc_pool.clone());
    info!("Container status sync already started (interval: 60s, detect container)");

    let vnc_sync_handle = if let Some(ref pingora_service) = state.pingora_service {
        let vnc_sync_config = VncSyncConfig {
            sync_interval: Duration::from_secs(5),
        };
        let handle = start_vnc_sync_task(
            pingora_service.clone(),
            vnc_sync_config,
            state.container_prefix_rcoder.clone(),
            state.container_prefix_computer.clone(),
        );
        info!("VNC sync already started (interval: 5s, sync Docker container IP)");
        Some(handle)
    } else {
        None
    };

    Ok(BackgroundTaskHandles {
        cleanup_handle,
        status_checker_handle,
        container_sync_handle,
        vnc_sync_handle,
    })
}
