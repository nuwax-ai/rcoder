//! 信号处理与优雅关闭

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use container_runtime_api::ContainerRuntime;
use docker_manager::container_stop;
use docker_manager::runtime_selection::RuntimeType;
use rcoder_storage::ProjectStoreBackend;
use tracing::{error, info, warn};

/// 全局 panic hook：panic 信息（含 `file:line:column` 位置）进 tracing
/// 结构化日志（文件日志按天滚动——排障主通道）。
///
/// 背景：catch_unwind 的兜底（publish/build 编排、SSE 转发 task 等）只能从
/// payload 拿到消息文本，**位置只在默认 hook 的 stderr 里**——容器 stderr
/// 与 tracing 文件日志是两路，按日志排查定位不到代码行。此处统一补齐，
/// 并保留默认 hook 的 stderr 输出（Docker/K8s 容器日志兼容）。
pub fn set_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown>".to_string());
        let message = if let Some(s) = info.payload().downcast_ref::<&str>() {
            (*s).to_string()
        } else if let Some(s) = info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "<non-string panic payload>".to_string()
        };
        error!(target: "panic", location = %location, "💥 panic: {message}");
        default_hook(info);
    }));
}

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

/// R02：在途协调任务收束预算。预算耗尽时记录未完成数量后继续关库
/// （不无限等待，也不强行清理不确定资源——留待重启恢复兜底）。
const COORDINATION_DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);
/// 恢复扫描器自身的收束预算（recovery.rs RECOVERY_DRAIN_BUDGET）加余量。
const RECOVERY_EXIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(35);

pub async fn graceful_shutdown(
    mut shutdown_rx: tokio::sync::broadcast::Receiver<()>,
    config: crate::config::AppConfig,
    runtime: Arc<dyn ContainerRuntime>,
    projects: Option<Arc<ProjectStoreBackend>>,
    userapp_store_control: Arc<dyn rcoder_storage::userapp_lifecycle::UserAppStoreControl>,
    userapp_op_flight: Option<Arc<crate::userapp_builder::shutdown_gate::OperationFlightGate>>,
    userapp_recovery: Option<tokio::task::JoinHandle<()>>,
) {
    let _ = shutdown_rx.recv().await;

    info!("starting graceful shutdown...");

    // R02 关机顺序：先停业务生产者，等在途协调任务有界收束，最后关库。
    // 排空数据库队列 ≠ 业务已结束——协调任务可能正在容器操作之间，
    // 稍后要提交终态；先关库会把可提交的终态变成失败。

    // 1) 恢复扫描器：停止发现新工作，等待其有界收束并退出
    if let Some(recovery) = userapp_recovery {
        match tokio::time::timeout(RECOVERY_EXIT_BUDGET, recovery).await {
            Ok(_) => info!("[USERAPP_SHUTDOWN] recovery scanner exited"),
            Err(_) => error!(
                budget_secs = RECOVERY_EXIT_BUDGET.as_secs(),
                "recovery scanner did not exit within budget; proceeding to close store"
            ),
        }
    }

    // 2) 在途协调任务（builder 创建/停止/重启等 spawn 工作器）有界收束
    if let Some(gate) = userapp_op_flight {
        let remaining = gate.wait_idle(COORDINATION_DRAIN_BUDGET).await;
        if remaining > 0 {
            error!(
                remaining,
                budget_secs = COORDINATION_DRAIN_BUDGET.as_secs(),
                "userApp coordination tasks still in flight; closing store anyway (uncertain outcomes protected by restart quarantine)"
            );
        } else {
            info!("[USERAPP_SHUTDOWN] coordination tasks drained");
        }
    }

    // 3) UserApp 控制存储关机（trait-design §6）：停接单 → 排空已接收
    // 事务 → 关连接 → 释放独占锁。失败不阻断其余清理，但必须显式记录。
    if let Err(error) = userapp_store_control.shutdown().await {
        error!(?error, "userApp control store shutdown incomplete");
    }

    // PG 模式：用户容器跨重启存活（多副本目标形态），跳过全量清删；
    // 先 flush write-behind 队列（有界 5s），保证结构性 op 落盘后退出
    if config.storage.backend == crate::config::StorageBackend::Postgres {
        info!(
            "[STORAGE_PG] container cleanup skipped (postgres mode: agent containers survive restarts)"
        );
        if let Some(backend) = &projects {
            let outcome = backend
                .shutdown_flush_outcome(std::time::Duration::from_secs(5))
                .await;
            if outcome.is_complete() {
                info!("[STORAGE_PG] shutdown flush completed");
            } else {
                error!(?outcome, "[STORAGE_PG] shutdown flush incomplete");
            }
        }
        info!(" RCoder graceful shutdown completed");
        return;
    }

    if let Err(e) = cleanup_all_containers(&config, &runtime).await {
        error!("container cleanup failed: {}", e);
    } else {
        info!("container cleanup completed");
    }

    info!(" RCoder graceful shutdown completed");
}

async fn cleanup_all_containers(
    config: &crate::config::AppConfig,
    runtime: &Arc<dyn ContainerRuntime>,
) -> anyhow::Result<()> {
    info!(" starting cleanup of dynamically created containers...");

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
                            " Cleaned up {} containers (all enabled services)",
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
            runtime
                .cleanup_all()
                .await
                .map_err(|e| anyhow::anyhow!("Failed to cleanup runtime resources: {}", e))?;
        }
    }

    Ok(())
}
