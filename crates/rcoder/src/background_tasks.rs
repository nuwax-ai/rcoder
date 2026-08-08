//! 后台任务编排

use std::sync::Arc;
use std::time::Duration;

use tracing::info;

use crate::cleanup_task;
use crate::config::AppConfig;
use crate::router::AppState;
use crate::service::{
    ContainerStatusCheckerConfig, ContainerSyncConfig, start_container_status_checker,
    start_container_sync_task,
};
use crate::userapp_recycle;

#[allow(dead_code)]
pub struct BackgroundTaskHandles {
    pub cleanup_handle: Option<tokio::task::JoinHandle<()>>,
    pub status_checker_handle: tokio::task::JoinHandle<()>,
    pub container_sync_handle: tokio::task::JoinHandle<()>,
    pub userapp_recycle_handle: Option<tokio::task::JoinHandle<()>>,
}

pub async fn start_all_background_tasks(
    config: &AppConfig,
    state: Arc<AppState>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) -> anyhow::Result<BackgroundTaskHandles> {
    let cleanup_config = cleanup_task::CleanupConfig {
        idle_timeout: Duration::from_secs(config.cleanup_config.idle_timeout_seconds),
        cleanup_interval: Duration::from_secs(config.cleanup_config.cleanup_interval_seconds),
        docker_stop_timeout: Duration::from_secs(config.cleanup_config.docker_stop_timeout_seconds),
        container_protection_duration: Duration::from_secs(
            config.cleanup_config.container_protection_seconds,
        ),
        long_idle_timeout: Duration::from_secs(config.cleanup_config.long_idle_timeout_seconds),
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
            cleanup_task::start_cleanup_task(
                cleanup_config_clone,
                state_for_cleanup,
                shutdown_tx.clone(),
            )
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
    let status_checker_handle =
        start_container_status_checker(status_checker_config, state.clone(), shutdown_tx.clone());
    info!("Container status checker already started (interval: 30s, will skip Docker on failure)");

    let container_sync_config = ContainerSyncConfig {
        sync_interval: Duration::from_secs(60),
    };
    let container_sync_handle = start_container_sync_task(
        container_sync_config,
        state.grpc_pool.clone(),
        state.runtime().clone(),
        shutdown_tx.clone(),
    );
    info!("Container status sync already started (interval: 60s, detect container)");

    // UserApp 闲置自动回收扫描器(config.userapp_recycle.enabled 开关,默认 true=免费用户自动回收)。
    // 闲置超阈值 → scale0 回收;付费 app 注解 recycle-enabled=false opt-out;流量唤醒由 pingora 负责。
    let userapp_recycle_handle = if config.userapp_recycle.enabled {
        let recycle_cfg = userapp_recycle::UserAppRecycleRuntimeConfig {
            idle_timeout: Duration::from_secs(config.userapp_recycle.idle_timeout_seconds),
            scan_interval: Duration::from_secs(config.userapp_recycle.scan_interval_seconds),
            protection: Duration::from_secs(config.userapp_recycle.protection_seconds),
        };
        info!(
            "[USERAPP_RECYCLE] enabled: idle_timeout={}s, scan_interval={}s, protection={}s",
            config.userapp_recycle.idle_timeout_seconds,
            config.userapp_recycle.scan_interval_seconds,
            config.userapp_recycle.protection_seconds,
        );
        Some(
            userapp_recycle::start_userapp_recycle_task(
                recycle_cfg,
                state.clone(),
                shutdown_tx.clone(),
            )
            .await?,
        )
    } else {
        info!("[USERAPP_RECYCLE] disabled (userapp_recycle.enabled=false)");
        None
    };

    // VNC 后端解析:pingora handle_vnc_upstream 优先查 ContainerLookupService(动态查项目
    // 存储,与 ttyd 一致、runtime 无关),回退 vnc_backends 显式注册(chat / pod_ensure /
    // pod_restart / pod_vnc_status 创建容器时 add_vnc_backend)。无定时同步任务。

    Ok(BackgroundTaskHandles {
        cleanup_handle,
        status_checker_handle,
        container_sync_handle,
        userapp_recycle_handle,
    })
}
