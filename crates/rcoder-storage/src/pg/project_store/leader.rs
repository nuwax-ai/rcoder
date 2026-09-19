//! PG advisory lock 心跳式 leader election（P2-M3）
//!
//! 多副本下"单实例语义"的后台任务（清理/巡检/回收）不能各副本重复执行。
//! 本实现用 **session 级 advisory lock** 选主：
//! - 持锁连接存活 = leadership 存活（连接断开 PG 自动释放锁，无需显式交还）
//! - follower 以 [`POLL_INTERVAL`] 周期 `pg_try_advisory_lock` 抢锁 → 故障切换
//!   时延 = poll 间隔 + 连接死亡检测
//! - `is_leader()` 原子读，供任务装配层做 per-tick 门控
//!
//! 锁 key 为全集群约定的常量（见 [`LEADER_LOCK_KEY`]）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::config::PostgresConfig;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

/// 抢锁/保活轮询间隔（故障切换时延上限 ≈ 本间隔 + 连接超时）
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// 全集群 leader 锁 key（"rcoder" ASCII 常量，跨版本稳定——改它会让滚动升级期间双主）
pub const LEADER_LOCK_KEY: i64 = 0x7263_6f64_6572;

/// Leader 选举句柄
pub struct PgLeaderElection {
    is_leader: Arc<AtomicBool>,
    _cancel: CancellationToken,
    finished: tokio::sync::watch::Receiver<Option<Result<(), String>>>,
}

impl PgLeaderElection {
    /// 启动选举任务（独立连接持锁）
    pub fn spawn(config: PostgresConfig, mut shutdown_rx: broadcast::Receiver<()>) -> Self {
        let is_leader = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let is_leader_task = Arc::clone(&is_leader);
        let cancel_task = cancel.clone();
        let (finished_tx, finished) = tokio::sync::watch::channel(None);
        tokio::spawn(async move {
            let result = run(config, is_leader_task, cancel_task, &mut shutdown_rx).await;
            finished_tx.send_replace(Some(result.map_err(|error| error.to_string())));
        });
        Self {
            is_leader,
            _cancel: cancel,
            finished,
        }
    }

    /// 当前是否 leader（原子读；连接保活失败自动翻 false）
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Acquire)
    }

    /// Wait for the owned election task, including dropping its dedicated
    /// connection. Cancelling this waiter never cancels the close itself.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self._cancel.cancel();
        let mut finished = self.finished.clone();
        loop {
            if let Some(result) = finished.borrow_and_update().clone() {
                return result.map_err(anyhow::Error::msg);
            }
            finished.changed().await.map_err(|_| {
                anyhow::anyhow!("PG leader task exited without shutdown confirmation")
            })?;
        }
    }
}

impl Drop for PgLeaderElection {
    fn drop(&mut self) {
        self._cancel.cancel();
    }
}

async fn run(
    mut config: PostgresConfig,
    is_leader: Arc<AtomicBool>,
    cancel: CancellationToken,
    shutdown_rx: &mut broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    // This owner/pool is never shared with application transactions. Every
    // uncertain probe ends the whole owner runtime before attempting a new
    // election; a potentially locked session cannot reenter a business pool.
    config.max_connections = Some(1);
    config.min_connections = Some(0);
    loop {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            _ = shutdown_rx.recv() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }
        let owner =
            match crate::db::postgres::open(&config, vec![crate::db::schema::Component::Project])
                .await
            {
                Ok(owner) => owner,
                Err(error) => {
                    warn!(%error, "Leader connection unavailable");
                    continue;
                }
            };
        let job_cancel = cancel.child_token();
        let stop_job = job_cancel.clone();
        let flag = is_leader.clone();
        let mut job = Box::pin(owner.execute(move |db| async move {
            let mut connection = db.connection().await?;
            loop {
                let query = async {
                    if flag.load(Ordering::Acquire) {
                        toasty::sql::query("SELECT 1").exec(&mut connection).await?;
                    } else {
                        let rows = toasty::sql::query("SELECT pg_try_advisory_lock($1)")
                            .bind(LEADER_LOCK_KEY)
                            .exec(&mut connection)
                            .await?;
                        let acquired = match rows.as_slice() {
                            [toasty_core::stmt::Value::Record(record)] => {
                                match record.fields.as_slice() {
                                    [toasty_core::stmt::Value::Bool(value)] => *value,
                                    _ => anyhow::bail!("Invalid leader lock result type"),
                                }
                            }
                            _ => anyhow::bail!("Invalid leader lock result shape"),
                        };
                        flag.store(acquired, Ordering::Release);
                    }
                    Ok::<_, anyhow::Error>(())
                };
                tokio::select! {
                    biased;
                    _ = job_cancel.cancelled() => break,
                    result = tokio::time::timeout(POLL_INTERVAL, query) => { result??; }
                }
                tokio::select! {
                    biased;
                    _ = job_cancel.cancelled() => break,
                    _ = tokio::time::sleep(POLL_INTERVAL) => {}
                }
            }
            Ok(())
        }));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => { stop_job.cancel(); drop(job.as_mut().await); }
            _ = shutdown_rx.recv() => { cancel.cancel(); stop_job.cancel(); drop(job.as_mut().await); }
            result = job.as_mut() => { if let Err(error) = result { warn!(%error, "Leader session ended"); } }
        }
        is_leader.store(false, Ordering::Release);
        drop(job);
        owner.shutdown().await?;
    }
    is_leader.store(false, Ordering::Release);
    info!("[STORAGE_PG] leader election stopped after dedicated runtime close");
    Ok(())
}
