//! 后台任务编排

use std::sync::Arc;
use std::time::Duration;

/// 后台任务句柄（PG 模式占位/监督任务的统一类型）
type TaskHandle = Option<tokio::task::JoinHandle<()>>;

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
    pub cleanup_handle: TaskHandle,
    /// PG 模式下由 leader 监督任务持有（此处为占位 pending task）
    pub status_checker_handle: TaskHandle,
    pub container_sync_handle: TaskHandle,
    pub userapp_recycle_handle: TaskHandle,
    /// M5：PG 模式的 activity 影子 flusher + publish TTL 清理（合并单 task）
    pub pg_shadow_handle: Option<tokio::task::JoinHandle<()>>,
    /// P2-M1：跨副本镜像同步任务
    pub pg_sync_handle: Option<tokio::task::JoinHandle<()>>,
    /// P2-M3：单实例后台任务的 leader 监督任务（PG 模式；
    /// cleanup/status_checker/container_sync/userapp_recycle 由它按 leadership 拉起/停止）
    pub pg_leader_handle: Option<tokio::task::JoinHandle<()>>,
}

async fn spawn_single_instance_tasks(
    config: &AppConfig,
    cleanup_config: cleanup_task::CleanupConfig,
    state: &Arc<AppState>,
    shutdown_tx: &tokio::sync::broadcast::Sender<()>,
) -> anyhow::Result<(
    Option<tokio::task::JoinHandle<()>>,
    tokio::task::JoinHandle<()>,
    tokio::task::JoinHandle<()>,
    Option<tokio::task::JoinHandle<()>>,
)> {
    let cleanup_handle = if config.cleanup_config.enabled {
        Some(
            cleanup_task::start_cleanup_task(cleanup_config, state.clone(), shutdown_tx.clone())
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
    Ok((
        cleanup_handle,
        status_checker_handle,
        container_sync_handle,
        userapp_recycle_handle,
    ))
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

    // P2-M3：单实例语义任务的拉起收敛为函数——memory 模式直接拉起，
    // PG 模式由 leader 监督任务按 leadership 代际拉起/停止。
    // （activity flush 与镜像 sync 是"每副本自身状态"任务，不在 leader 门控内。）
    let (
        cleanup_handle,
        status_checker_handle,
        container_sync_handle,
        userapp_recycle_handle,
        pg_leader_handle,
    ): (TaskHandle, TaskHandle, TaskHandle, TaskHandle, TaskHandle) =
        if state.projects.is_postgres() {
            #[cfg(feature = "rcoder-pg")]
            {
                let leader_store = state.projects.postgres().expect("is_postgres 为真");
                let election = Arc::new(
                    rcoder_storage::pg::leader_selection::PgLeaderElection::spawn(
                        leader_store.pool().clone(),
                        shutdown_tx.subscribe(),
                    ),
                );
                let config = config.clone();
                let state = Arc::clone(&state);
                let shutdown_tx = shutdown_tx.clone();
                let leader_handle = tokio::spawn(async move {
                    run_leader_supervisor(election, config, state, shutdown_tx).await;
                });
                // 监督任务异步拉起子任务，直接句柄由 supervisor 持有
                (
                    None,
                    Some(tokio::spawn(std::future::pending())),
                    Some(tokio::spawn(std::future::pending())),
                    None,
                    Some(leader_handle),
                )
            }
            #[cfg(not(feature = "rcoder-pg"))]
            {
                unreachable!("is_postgres 为真但未编译 rcoder-pg")
            }
        } else {
            let (c, s1, s2, u) =
                spawn_single_instance_tasks(config, cleanup_config, &state, &shutdown_tx).await?;
            (c, Some(s1), Some(s2), u, None)
        };

    // VNC 后端解析:pingora handle_vnc_upstream 优先查 ContainerLookupService(动态查项目
    // 存储,与 ttyd 一致、runtime 无关),回退 vnc_backends 显式注册(chat / pod_ensure /
    // pod_restart / pod_vnc_status 创建容器时 add_vnc_backend)。无定时同步任务。

    // M5：PG 模式的影子持久化后台任务——activity 脏行 5s 批量 flush + forget 删除；
    // publish 终态行 TTL 清理（1h 周期）。
    // 计时器用 interval 而非每轮重建的 sleep：biased select 中重建的 sleep(5s)
    // 会永久抢占低频 arm（missed tick 不累积、purge 永远饿死）；interval 的
    // missed tick 会累积补偿。purge arm 置于 flush 前：同时到期时低频任务优先。
    // P2-M1：跨副本镜像同步（5s 全量 diff；ClientIP affinity 下常规流量无感知，
    // 故障切换陈旧窗口 ≤ 同步周期）。持有 AppState 同源的 Arc<ProjectStoreBackend>，
    // 每 tick 经 postgres() 取借用——无 unsafe、无 Arc 循环。
    let pg_sync_handle = if state.projects.is_postgres() {
        #[cfg(feature = "rcoder-pg")]
        {
            let projects = Arc::clone(&state.projects);
            let shutdown_rx = shutdown_tx.subscribe();
            Some(tokio::spawn(async move {
                rcoder_storage::pg::sync::run_sync_loop(projects, shutdown_rx).await;
            }))
        }
        #[cfg(not(feature = "rcoder-pg"))]
        {
            None
        }
    } else {
        None
    };
    let pg_shadow_handle = if state.projects.is_postgres() {
        let state = Arc::clone(&state);
        let mut shutdown_rx = shutdown_tx.subscribe();
        Some(tokio::spawn(async move {
            let mut flush_tick = tokio::time::interval(Duration::from_secs(5));
            flush_tick.tick().await; // 首个 tick 立即返回，跳过
            let mut purge_tick = tokio::time::interval(Duration::from_secs(3600));
            purge_tick.tick().await; // 同上
            loop {
                tokio::select! {
                    biased;
                    _ = shutdown_rx.recv() => break,
                    _ = purge_tick.tick() => {
                        if let Some(repo) = state.publish_tasks.repo() {
                            match repo
                                .purge_expired(crate::userapp_publish::store::TERMINAL_TASK_TTL_SECS)
                                .await
                            {
                                Ok(n) if n > 0 => {
                                    tracing::info!("[STORAGE_PG] purged {n} expired publish tasks")
                                }
                                Ok(_) => {}
                                Err(e) => {
                                    tracing::error!("[STORAGE_PG] publish purge failed: {e:#}")
                                }
                            }
                        }
                    },
                    _ = flush_tick.tick() => {
                        let Some(persistence) = state.activity.persistence() else {
                            continue;
                        };
                        // 删除先行（forget_app 后同 id 重建不被旧行复活）；失败重登队列
                        let deleted = state.activity.drain_deleted();
                        for app_id in &deleted {
                            if let Err(e) = persistence.delete(app_id).await {
                                tracing::error!("[STORAGE_PG] activity delete {app_id} failed: {e:#}");
                                state.activity.re_delete(&deleted);
                                break;
                            }
                        }
                        // flush 失败重标脏（否则该批数据在下次变更前不会再落盘）
                        let rows = state.activity.collect_dirty();
                        if !rows.is_empty()
                            && let Err(e) = persistence.flush_batch(rows.clone()).await
                        {
                            tracing::error!("[STORAGE_PG] activity flush failed: {e:#}");
                            let ids: Vec<String> =
                                rows.iter().map(|r| r.app_id.clone()).collect();
                            state.activity.re_dirty(&ids);
                        }
                    },
                }
            }
        }))
    } else {
        None
    };

    Ok(BackgroundTaskHandles {
        cleanup_handle,
        status_checker_handle,
        container_sync_handle,
        userapp_recycle_handle,
        pg_shadow_handle,
        pg_sync_handle,
        pg_leader_handle,
    })
}

/// P2-M3：leader 监督——持有 leadership 期间拉起 4 个单实例任务，让位即停。
///
/// 代际 channel：每次获主创建独立 broadcast，失主时 drop 该代 sender → 子任务
/// 的 shutdown_rx.recv() 返回 Err → 循环退出（与优雅关停同一条退出路径）。
/// 重新获主重新拉起，无需重启进程。
#[cfg(feature = "rcoder-pg")]
async fn run_leader_supervisor(
    election: Arc<rcoder_storage::pg::leader_selection::PgLeaderElection>,
    config: AppConfig,
    state: Arc<AppState>,
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
) {
    let mut root_shutdown = shutdown_tx.subscribe();
    let mut current_gen: Option<tokio::sync::broadcast::Sender<()>> = None;
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    tick.tick().await; // 首个立即返回
    loop {
        tokio::select! {
            biased;
            _ = root_shutdown.recv() => break,
            _ = tick.tick() => {
                let is_leader = election.is_leader();
                if is_leader && current_gen.is_none() {
                    // 获主：拉起代际
                    let (gen_tx, _) = tokio::sync::broadcast::channel(1);
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
                    match spawn_single_instance_tasks(&config, cleanup_config, &state, &gen_tx).await {
                        Ok(handles) => {
                            let (c, s1, s2, u) = handles;
                            // 保活句柄防止任务被提前回收（JoinHandle 被 drop 不取消
                            // task，但保持引用便于调试器/控制台观察）
                            tokio::spawn(async move {
                                // 显式消费 JoinHandle（带 Drop 语义，避免 let _ 静默丢弃）
                                if let Some(h) = c { let _r = h.await; }
                                let _r1 = s1.await;
                                let _r2 = s2.await;
                                if let Some(h) = u { let _r3 = h.await; }
                            });
                            current_gen = Some(gen_tx);
                            info!("[STORAGE_PG] leader: single-instance background tasks started");
                        }
                        Err(e) => {
                            tracing::error!("[STORAGE_PG] leader task spawn failed: {e:#}");
                        }
                    }
                } else if !is_leader && let Some(gen_tx) = current_gen.take() {
                    // 让位：drop 代际 sender → 子任务退出
                    drop(gen_tx);
                    info!("[STORAGE_PG] leader stepped down: single-instance background tasks stopped");
                }
            }
        }
    }
    if let Some(gen_tx) = current_gen.take() {
        drop(gen_tx);
    }
    info!("[STORAGE_PG] leader supervisor stopped");
}
