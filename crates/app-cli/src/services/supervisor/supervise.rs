use super::*;

// ── supervise（信号 + 任一退出 → kill all → return）─────────────────────────────

/// 优雅停机：所有受管子进程并发停止（信号 → 共享宽限 deadline → 整树强杀），
/// 任何一个进程树收束未确认即整体失败（R01：停止完成要求整个受管树收束，
/// 不能只等直接 Child）。
pub(super) async fn supervise(
    mut children: ManagedChildren,
    shutdown_timeout_seconds: u64,
    cancel: Option<tokio_util::sync::CancellationToken>,
    known_failed: Vec<String>,
) -> Result<()> {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            info!("📡 received SIGINT, shutting down");
        }
        _ = wait_sigterm() => {
            info!("📡 received SIGTERM, shutting down");
        }
        // server 形态的外部取消（热部署切换 / 容器停服级联）：与信号同路径优雅停
        () = async {
            match cancel {
                Some(token) => token.cancelled().await,
                None => std::future::pending().await,
            }
        } => {
            info!("📡 orchestration cancelled (hot deploy / shutdown), stopping services");
        }
        exited = poll_any_exit(&mut children, &known_failed) => {
            if let Some(name) = exited {
                error!("❌ {name} exited — shutting down (supervisor will restart)");
            }
        }
    }
    // P1/V2-03：会话收束前先 standby 摘流（常驻入口不再把请求转发到即将
    // 停止的业务服务——裸 502 窗口）。发布/确认失败记警告不阻塞停机：
    // 业务停止本身仍有 shutdown_all 的强收束证据；入口路由停留在旧配置
    // 的窗口由下个会话的发布收敛。直跑形态槽空为 no-op。
    if let Err(error) = super::resident::publish_standby_if_serving().await {
        tracing::warn!("standby drain before session shutdown failed: {error:#}");
    }
    shutdown_all(children, shutdown_timeout_seconds).await
}

/// A deployment cannot publish a terminal status until shutdown is confirmed.
#[derive(Debug)]
pub(crate) struct ShutdownUnconfirmed(pub String);
impl std::fmt::Display for ShutdownUnconfirmed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "shutdown not confirmed: {}", self.0)
    }
}
impl std::error::Error for ShutdownUnconfirmed {}

pub(super) async fn shutdown_all(
    children: ManagedChildren,
    shutdown_timeout_seconds: u64,
) -> Result<()> {
    // 并发停止：每个 stop 的首个 poll 即发出信号，宽限窗口共享同一时刻起算
    //（与旧实现"先全体 TERM、再并行等宽限、超时全体 KILL"等时序）。
    let grace =
        Duration::from_secs(shutdown_timeout_seconds.min(crate::supervision::STOP_GRACE_SECONDS));
    let results =
        futures::future::join_all(children.into_iter().map(|(name, mut child)| async move {
            let outcome = child.stop(grace).await;
            (name, outcome)
        }))
        .await;
    let unconfirmed: Vec<String> = results
        .into_iter()
        .filter_map(|(name, outcome)| match outcome {
            StopOutcome::Unconfirmed => Some(name),
            _ => None,
        })
        .collect();
    if unconfirmed.is_empty() {
        Ok(())
    } else {
        Err(ShutdownUnconfirmed(format!(
            "process tree(s) remain after termination deadline: {}",
            unconfirmed.join(", ")
        ))
        .into())
    }
}

/// Detect an unexpected child exit without discarding handles or tree
/// identities: final shutdown still has to confirm every original tree stopped.
/// root 语义（孙进程存活不掩盖根进程死亡，见 [`ManagedChild::try_wait_root`]）。
pub(super) async fn poll_any_exit(
    children: &mut [(String, ManagedChild)],
    known_failed: &[String],
) -> Option<String> {
    loop {
        for (name, child) in children.iter_mut() {
            if matches!(child.try_wait_root(), Ok(Some(_)) | Err(_))
                && !known_failed.iter().any(|failed| failed == name)
            {
                return Some(name.clone());
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// 等 SIGTERM（Unix 专属）。handler 安装失败时降级（不 panic），
/// 由 [`tokio::signal::ctrl_c`] / `poll_any_exit` 兜底触发关闭。
#[cfg(unix)]
pub(super) async fn wait_sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    let mut sig = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(e) => {
            warn!("install SIGTERM handler failed: {e} — SIGTERM 不会被捕获");
            return;
        }
    };
    sig.recv().await;
}

#[cfg(not(unix))]
pub(super) async fn wait_sigterm() {
    std::future::pending::<()>().await;
}
