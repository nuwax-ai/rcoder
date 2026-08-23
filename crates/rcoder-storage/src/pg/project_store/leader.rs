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

use sqlx::{PgConnection, PgPool, Row};
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
}

impl PgLeaderElection {
    /// 启动选举任务（独立连接持锁）
    pub fn spawn(pool: PgPool, mut shutdown_rx: broadcast::Receiver<()>) -> Self {
        let is_leader = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let is_leader_task = Arc::clone(&is_leader);
        let cancel_task = cancel.clone();
        tokio::spawn(async move {
            run(pool, is_leader_task, cancel_task, &mut shutdown_rx).await;
        });
        Self {
            is_leader,
            _cancel: cancel,
        }
    }

    /// 当前是否 leader（原子读；连接保活失败自动翻 false）
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::Acquire)
    }
}

async fn run(
    pool: PgPool,
    is_leader: Arc<AtomicBool>,
    cancel: CancellationToken,
    shutdown_rx: &mut broadcast::Receiver<()>,
) {
    let mut lock_conn: Option<PgConnection> = None;
    info!("[STORAGE_PG] leader election started (key={LEADER_LOCK_KEY:#x})");
    loop {
        tokio::select! {
            biased;
            _ = shutdown_rx.recv() => break,
            _ = cancel.cancelled() => break,
            _ = tokio::time::sleep(POLL_INTERVAL) => {}
        }

        if is_leader.load(Ordering::Acquire) {
            // 保活探测：连接死亡（网络分区/PG 重启）→ 让位（服务端锁随连接释放）。
            // 应用层超时兜底：TCP 半开（交换机静默丢包）下语句到不了服务器，
            // statement_timeout 不生效，裸等会挂到内核 TCP 超时（分钟级）——
            // 期间 PG 侧锁已随连接死亡释放、他副本已抢主，形成双主窗口。
            // 超时按 POLL_INTERVAL 判死让位，双主窗口收敛到轮询量级。
            let Some(conn) = lock_conn.as_mut() else {
                // 不变式破坏（leader 必有持锁连接）：防御性复位
                warn!("[STORAGE_PG] leader invariant broken (no lock conn), stepping down");
                is_leader.store(false, Ordering::Release);
                continue;
            };
            let probe = sqlx::query("SELECT 1").execute(&mut *conn);
            match tokio::time::timeout(POLL_INTERVAL, probe).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    warn!("[STORAGE_PG] leader lock connection lost ({e}), stepping down");
                    is_leader.store(false, Ordering::Release);
                    lock_conn = None;
                }
                Err(_) => {
                    warn!("[STORAGE_PG] leader probe timed out (half-open conn?), stepping down");
                    is_leader.store(false, Ordering::Release);
                    lock_conn = None;
                }
            }
        } else {
            // 抢锁：独立连接 + session 级 advisory lock（连接存活期间持有）
            let Ok(mut conn) = pool.acquire().await else {
                continue; // PG 故障：下轮再试
            };
            let Ok(row) = sqlx::query("SELECT pg_try_advisory_lock($1) AS ok")
                .bind(LEADER_LOCK_KEY)
                .fetch_one(&mut *conn)
                .await
            else {
                continue;
            };
            let acquired: bool = row.get("ok");
            if acquired {
                // detach 连接出池（持锁连接的生命周期 = leadership）
                lock_conn = Some(conn.detach());
                is_leader.store(true, Ordering::Release);
                info!("[STORAGE_PG] leadership acquired");
            }
        }
    }
    // 关停：drop 连接 → PG 自动释放锁
    drop(lock_conn);
    is_leader.store(false, Ordering::Release);
    info!("[STORAGE_PG] leader election stopped (lock released)");
}
