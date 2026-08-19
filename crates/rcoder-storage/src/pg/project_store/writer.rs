//! write-behind writer：消费 PersistOp 队列，批量落 PostgreSQL
//!
//! - **批处理**：先 recv 阻塞等一条，再 try_recv 聚合（单批上限 200），单事务提交
//! - **保序**：FIFO；同 project 的操作顺序与本地镜像应用顺序一致
//! - **重试**：整批失败（PG 抖动）指数退避后重试整批——所有语句均为幂等
//!   upsert/delete，重放安全；结构性 op 永不丢弃
//! - **超深丢弃**：队列深度超 10k 时丢弃 Touch 类幂等 op（保结构、舍精度）
//! - **优雅关停**：cancel 后排空剩余队列再退出（flush_and_stop 有界等待）

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use sqlx::{PgPool, Transaction};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::persist_ops::PersistOp;
use super::repo;

/// 单批最大 op 数（一批一事务）
const BATCH_MAX: usize = 200;
/// 队列深度超过此值开始丢弃幂等 op（结构性 op 永不丢）
const DROP_DEPTH_THRESHOLD: usize = 10_000;
/// 重试退避上限
const MAX_BACKOFF: Duration = Duration::from_secs(10);
/// 优雅关停的排空等待上限（结构性 op 尽力落盘）
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// writer 句柄（PgStore 持有；rcoder 优雅关停时调用 flush_and_stop）
pub struct PersistWriter {
    cancel: CancellationToken,
    /// JoinHandle 一次性取出（flush_and_stop 取 &self；锁不跨 await）
    handle: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// 队列深度采样（writer task 每批后更新，监控用；非精确实时值）
    depth: Arc<AtomicUsize>,
}

impl PersistWriter {
    /// 启动后台 writer task（pending：在途计数，提交后自减——sync 排空屏障用）
    pub fn spawn(
        pool: PgPool,
        rx: mpsc::UnboundedReceiver<PersistOp>,
        pending: Arc<AtomicI64>,
    ) -> Self {
        let cancel = CancellationToken::new();
        let depth = Arc::new(AtomicUsize::new(0));
        let handle = tokio::spawn(run(pool, rx, cancel.clone(), Arc::clone(&depth), pending));
        Self {
            cancel,
            handle: std::sync::Mutex::new(Some(handle)),
            depth,
        }
    }

    /// 当前队列深度采样（监控/告警用，非精确实时值）
    pub fn queue_depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// 优雅关停：通知退出并等待队列排空（有界）。返回 true=全部落盘。
    /// 幂等：重复调用/已停止返回 true。
    pub async fn flush_and_stop(&self, timeout: Duration) -> bool {
        self.cancel.cancel();
        let handle = {
            // 毒化（持锁 panic）时从 PoisonError 取回 guard——关停路径不因历史 panic 卡死
            let mut guard = self
                .handle
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            guard.take()
        };
        let Some(handle) = handle else {
            return true; // 已停止
        };
        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(())) => true,
            Ok(Err(e)) => {
                warn!("[STORAGE_PG] writer task panicked: {e}");
                false
            }
            Err(_) => {
                warn!("[STORAGE_PG] writer flush timeout after {timeout:?}");
                false
            }
        }
    }
}

/// writer 主循环
async fn run(
    pool: PgPool,
    mut rx: mpsc::UnboundedReceiver<PersistOp>,
    cancel: CancellationToken,
    depth: Arc<AtomicUsize>,
    pending: Arc<AtomicI64>,
) {
    info!("[STORAGE_PG] persist writer started");
    let mut backoff = Duration::from_secs(1);
    // 被 cancel 打断的未落盘批次（跨 loop 迭代累积，关停排空时统一尽力落盘）
    let mut interrupted: Vec<PersistOp> = Vec::new();
    loop {
        // 阻塞等第一条（允许关停打断）
        let first = tokio::select! {
            biased;
            _ = cancel.cancelled() => break,
            op = rx.recv() => match op {
                Some(op) => op,
                None => break, // 生产端全部 drop
            },
        };
        let mut batch = Vec::with_capacity(BATCH_MAX);
        batch.push(first);
        // 聚合 + 超深丢弃幂等 op
        let mut dropped = 0usize;
        while batch.len() < BATCH_MAX {
            match rx.try_recv() {
                Ok(op) => {
                    if !op.is_structural() && rx.len() > DROP_DEPTH_THRESHOLD {
                        dropped += 1;
                        // 丢弃的 op 在 enqueue 时已计数——同步归还，防 pending 泄漏
                        // （wait_drained 屏障会永久误判"在途"）
                        pending.fetch_sub(1, Ordering::AcqRel);
                        continue;
                    }
                    batch.push(op);
                }
                Err(_) => break,
            }
        }
        if dropped > 0 {
            warn!(
                "[STORAGE_PG] queue over {DROP_DEPTH_THRESHOLD}, dropped {dropped} idempotent ops"
            );
        }

        // 整批重试（cancel 可打断：未落盘批次并入关停排空集合，尽力最后一次）
        loop {
            match execute_batch(&pool, &batch).await {
                Ok(size) => {
                    debug!("[STORAGE_PG] batch committed: {size} ops (dropped={dropped})");
                    pending.fetch_sub(size as i64, Ordering::AcqRel);
                    backoff = Duration::from_secs(1);
                    depth.store(rx.len(), Ordering::Relaxed);
                    break;
                }
                Err(e) => {
                    error!(
                        "[STORAGE_PG] batch failed ({} ops, retry in {:?}): {e:#}",
                        batch.len(),
                        backoff
                    );
                    depth.store(rx.len(), Ordering::Relaxed);
                    let cancelled = tokio::select! {
                        biased;
                        _ = cancel.cancelled() => true,
                        _ = tokio::time::sleep(backoff) => false,
                    };
                    if cancelled {
                        interrupted.extend(batch);
                        break;
                    }
                    backoff = (backoff * 2).min(MAX_BACKOFF);
                }
            }
        }
    }

    // 关停排空：被 cancel 打断的未落盘批次 + 剩余结构性 op 尽力落盘（有界）
    if !interrupted.is_empty() {
        warn!(
            "[STORAGE_PG] retry interrupted by shutdown: {} ops deferred to drain",
            interrupted.len()
        );
    }
    let mut remaining = interrupted;
    let drain = async {
        let mut dropped = 0usize;
        while let Ok(op) = rx.try_recv() {
            if op.is_structural() {
                remaining.push(op);
            } else {
                // 与超深丢弃对称：幂等 op 关停丢弃也须归还 pending（wait_drained
                // 的屏障语义依赖计数准确），否则关停后 barrier 永久超时
                dropped += 1;
                pending.fetch_sub(1, Ordering::AcqRel);
            }
        }
        if dropped > 0 {
            tracing::debug!("[STORAGE_PG] shutdown drain dropped {dropped} idempotent ops");
        }
    };
    let _ = tokio::time::timeout(DRAIN_TIMEOUT, drain).await;
    if !remaining.is_empty() {
        match execute_batch(&pool, &remaining).await {
            Ok(size) => {
                info!("[STORAGE_PG] shutdown drain committed {size} ops");
                pending.fetch_sub(size as i64, Ordering::AcqRel);
            }
            Err(e) => error!(
                "[STORAGE_PG] shutdown drain failed ({} ops): {e:#}",
                remaining.len()
            ),
        }
    }
    info!("[STORAGE_PG] persist writer stopped");
}

/// 单事务执行一批 op
async fn execute_batch(pool: &PgPool, batch: &[PersistOp]) -> anyhow::Result<usize> {
    let mut tx: Transaction<'_, sqlx::Postgres> = pool.begin().await?;
    for op in batch {
        execute_op(&mut tx, op).await?;
    }
    tx.commit().await?;
    Ok(batch.len())
}

/// 单 op → repo 调用（全部幂等：upsert / delete，重放安全）。
/// 事务内执行器解引用传参（官方 transaction 示例范式）。
pub(in crate::pg) async fn execute_op(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    op: &PersistOp,
) -> anyhow::Result<()> {
    use PersistOp as Op;
    let db = &mut **tx;
    match op {
        Op::UpsertContainer(c) => repo::upsert_container(db, c).await?,
        Op::UpsertProject(p) => repo::upsert_project(db, p).await?,
        Op::RemoveProject { project_id } => repo::remove_project(db, project_id).await?,
        Op::AddSession {
            project_id,
            session_id,
            container_name,
        } => repo::add_session(db, project_id, session_id, container_name.as_deref()).await?,
        Op::RemoveSession { session_id } => repo::remove_session(db, session_id).await?,
        Op::ClearSessions { project_id } => repo::clear_sessions(db, project_id).await?,
        Op::DeleteContainerWithProjects { container_id } => {
            repo::delete_container_with_projects(db, container_id).await?
        }
        Op::TouchProject {
            project_id,
            last_activity,
        } => repo::touch_project(db, project_id, *last_activity).await?,
        Op::TouchContainer {
            container_name,
            last_activity,
        } => repo::touch_container(db, container_name, *last_activity).await?,
        Op::TouchSession {
            session_id,
            last_seen_at,
        } => repo::touch_session(db, session_id, *last_seen_at).await?,
        Op::UpdateAgentStatus {
            project_id,
            agent_status,
        } => repo::update_agent_status(db, project_id, agent_status).await?,
    }
    Ok(())
}
