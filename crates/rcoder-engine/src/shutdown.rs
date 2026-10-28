//! 信号处理与优雅关闭

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use container_runtime_api::ContainerRuntime;
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

/// One shutdown deadline covers producers, recovery, and admitted operations.
/// Expiry never authorizes closing storage underneath unfinished writers.
pub const SHUTDOWN_BUDGET: std::time::Duration = std::time::Duration::from_secs(65);

/// Owned handles needed to stop UserApp writers before closing their storage.
pub struct UserAppShutdown {
    pub store: Arc<dyn rcoder_storage::userapp_lifecycle::UserAppStoreControl>,
    pub operations: Option<Arc<crate::userapp_builder::shutdown_gate::OperationFlightGate>>,
    pub recovery: Option<tokio::task::JoinHandle<()>>,
}

pub async fn graceful_shutdown(
    deadline: tokio::time::Instant,
    config: crate::config::AppConfig,
    _runtime: Arc<dyn ContainerRuntime>,
    projects: Option<Arc<ProjectStoreBackend>>,
    activity: Arc<app_manager::AppActivityRegistry>,
    userapp: UserAppShutdown,
) -> anyhow::Result<()> {
    let UserAppShutdown {
        store: userapp_store_control,
        operations: userapp_op_flight,
        recovery: userapp_recovery,
    } = userapp;
    info!("starting graceful shutdown...");

    // R02 关机顺序：先停业务生产者，等在途协调任务有界收束，最后关库。
    // 排空数据库队列 ≠ 业务已结束——协调任务可能正在容器操作之间，
    // 稍后要提交终态；先关库会把可提交的终态变成失败。

    if let Some(gate) = &userapp_op_flight {
        gate.close();
    }
    if let Some(recovery) = userapp_recovery {
        tokio::time::timeout_at(deadline, recovery)
            .await
            .map_err(|_| {
                anyhow::anyhow!("recovery shutdown deadline exceeded; storage remains owned")
            })??;
    }
    if let Some(gate) = userapp_op_flight {
        let remaining = gate
            .wait_idle(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await;
        anyhow::ensure!(
            remaining == 0,
            "{remaining} operations still in flight; storage remains owned"
        );
    }

    // Producers, background flush and operation completion have all drained.
    // Flush their last lifecycle-bound timestamps before closing the shared DB.
    tokio::time::timeout_at(deadline, activity.flush_pending())
        .await
        .map_err(|_| {
            anyhow::anyhow!("activity final flush deadline exceeded; storage remains owned")
        })??;

    // 3) UserApp 控制存储关机（trait-design §6）：停接单 → 排空已接收
    // 事务 → 关连接 → 释放独占锁。失败不能报告关机成功。
    tokio::time::timeout_at(deadline, userapp_store_control.shutdown())
        .await
        .map_err(|_| anyhow::anyhow!("control storage shutdown deadline exceeded"))??;

    // A management-process shutdown preserves compute in every storage mode.
    // The PG project's write-behind queue still drains under the same deadline.
    if config.storage.backend == crate::config::StorageBackend::Postgres
        && let Some(backend) = &projects
    {
        let outcome = tokio::time::timeout_at(
            deadline,
            backend.shutdown_flush_outcome(
                deadline.saturating_duration_since(tokio::time::Instant::now()),
            ),
        )
        .await
        .map_err(|_| anyhow::anyhow!("project storage flush deadline exceeded"))?;
        if outcome.is_complete() {
            info!("[STORAGE_PG] shutdown flush completed");
        } else {
            error!(?outcome, "[STORAGE_PG] shutdown flush incomplete");
            anyhow::bail!("project storage shutdown flush incomplete");
        }
    }

    info!("Management shutdown preserves existing Agent and UserApp compute");
    info!(" RCoder graceful shutdown completed");
    Ok(())
}
