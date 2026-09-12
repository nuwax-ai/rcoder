//! write-behind writer：消费 PersistOp 队列，批量落 PostgreSQL
//!
//! - **批处理**：先 recv 阻塞等一条，再 try_recv 聚合（单批上限 200），单事务提交
//! - **顺序**：队列自身 FIFO；与 durable 直写交错由身份条件和墓碑保护，不能假设调用时序等于提交时序。
//! - **重试**：整批失败（PG 抖动）指数退避后重试整批——所有语句均为幂等
//!   upsert/delete，重放安全；结构性 op 永不丢弃
//! - **超深丢弃**：队列深度超 10k 时丢弃 Touch 类幂等 op（保结构、舍精度）
//! - **优雅关停**：cancel 后排空剩余队列再退出（flush_and_stop 有界等待）

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use shared_types::{FlushOutcome, persistence::PersistenceOperationOutcome};
use sqlx::{PgPool, Transaction};
use tokio::sync::mpsc;
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
/// 整批重试上限：超过后拆单定位坏 op（防单条非瞬态错误永久堵塞队列头——
/// 后续全部结构性 op 永不落库，PG 与镜像永久分叉直到重启）
const MAX_BATCH_RETRIES: u32 = 5;
/// 优雅关停的排空等待上限（结构性 op 尽力落盘）
const DRAIN_TIMEOUT: Duration = Duration::from_secs(5);

/// writer 句柄（PgStore 持有；rcoder 优雅关停时调用 flush_and_stop）
pub struct PersistWriter {
    #[cfg(test)]
    drain_count: Arc<AtomicUsize>,
    cancel: CancellationToken,
    /// Every caller observes the same terminal result; a caller timeout does not detach it.
    completion: tokio::sync::watch::Receiver<Option<FlushOutcome>>,
    pending: Arc<AtomicI64>,
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
        let drain_count = Arc::new(AtomicUsize::new(0));
        let (completion_tx, completion) = tokio::sync::watch::channel(None);
        let handle = tokio::spawn(run(
            pool,
            rx,
            cancel.clone(),
            Arc::clone(&depth),
            pending.clone(),
            drain_count.clone(),
        ));
        let task_pending = pending.clone();
        tokio::spawn(async move {
            let outcome = match handle.await {
                Ok(outcome) => outcome,
                Err(error) => FlushOutcome::Incomplete {
                    pending: task_pending.load(Ordering::Acquire).max(0) as usize,
                    reason: format!("Persist writer task failed: {error}"),
                },
            };
            completion_tx.send_replace(Some(outcome));
        });
        Self {
            #[cfg(test)]
            drain_count,
            cancel,
            completion,
            pending,
            depth,
        }
    }

    #[cfg(test)]
    pub(crate) fn drain_count(&self) -> usize {
        self.drain_count.load(Ordering::Acquire)
    }

    /// 当前队列深度采样（监控/告警用，非精确实时值）
    pub fn queue_depth(&self) -> usize {
        self.depth.load(Ordering::Relaxed)
    }

    /// Compatibility predicate; detailed callers should consume `flush_outcome`.
    pub async fn flush_and_stop(&self, timeout: Duration) -> bool {
        self.flush_outcome(timeout).await.is_complete()
    }

    /// All shutdown callers wait for the same drain and retain its final result.
    pub async fn flush_outcome(&self, timeout: Duration) -> FlushOutcome {
        self.cancel.cancel();
        let mut completion = self.completion.clone();
        let wait = async {
            loop {
                if let Some(outcome) = completion.borrow_and_update().clone() {
                    return outcome;
                }
                if completion.changed().await.is_err() {
                    return FlushOutcome::Incomplete {
                        pending: self.pending.load(Ordering::Acquire).max(0) as usize,
                        reason: "Persist writer completion channel closed".into(),
                    };
                }
            }
        };
        match tokio::time::timeout(timeout, wait).await {
            Ok(outcome) => outcome,
            Err(_) => FlushOutcome::TimedOut {
                pending: self.pending.load(Ordering::Acquire).max(0) as usize,
            },
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
    drain_count: Arc<AtomicUsize>,
) -> FlushOutcome {
    info!("[STORAGE_PG] persist writer started");
    let mut backoff = Duration::from_secs(1);
    let mut rejected = 0usize;
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
        let mut retries: u32 = 0;
        loop {
            match execute_batch(&pool, &batch).await {
                Ok(size) => {
                    debug!("[STORAGE_PG] batch resolved: {size} ops (dropped={dropped})");
                    pending.fetch_sub(size as i64, Ordering::AcqRel);
                    backoff = Duration::from_secs(1);
                    depth.store(rx.len(), Ordering::Relaxed);
                    break;
                }
                Err(e) => {
                    retries += 1;
                    error!(
                        "[STORAGE_PG] batch failed ({} ops, attempt {retries}, retry in {:?}): {e:#}",
                        batch.len(),
                        backoff
                    );
                    depth.store(rx.len(), Ordering::Relaxed);
                    // 瞬态错误（PG 抖动）整批重放安全；持续失败到界说明批内混有
                    // 确定性坏 op（典型：跨副本交错导致 AddSession 的 FK violation
                    // ——project 行已被清理 leader 删除，重放必败）——拆单执行定位：
                    // 确定性错误丢弃告警，瞬态失败的 op 及其后未执行者**并回重试**
                    // （保数据：sync 是 PG→镜像单向，丢弃瞬态 op 会让 PG 恢复后
                    // 反向清掉镜像条目——session 双双消失）
                    if retries >= MAX_BATCH_RETRIES {
                        warn!(
                            "[STORAGE_PG] batch still failing after {retries} attempts, isolating per-op"
                        );
                        let (committed, quarantined, remaining) =
                            isolate_poison_ops(&pool, &batch, &cancel).await;
                        pending.fetch_sub(committed as i64, Ordering::AcqRel);
                        rejected += quarantined.len();
                        for (op, err) in quarantined {
                            error!(
                                "[STORAGE_PG] dropped deterministic-error op (kind={}, mirror/PG may diverge): {err:#}",
                                op.kind()
                            );
                            pending.fetch_sub(1, Ordering::AcqRel);
                        }
                        if remaining.is_empty() {
                            backoff = Duration::from_secs(1);
                            break;
                        }
                        // 瞬态残余：换批继续退避重试（队列头已被确定性坏 op 的
                        // 剔除解开）；重置计数给新批满退避窗口
                        batch = remaining;
                        retries = 0;
                    }
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

    drain_count.fetch_add(1, Ordering::AcqRel);
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
                info!("[STORAGE_PG] shutdown drain resolved {size} ops");
                pending.fetch_sub(size as i64, Ordering::AcqRel);
            }
            Err(e) => error!(
                "[STORAGE_PG] shutdown drain failed ({} ops): {e:#}",
                remaining.len()
            ),
        }
    }
    info!("[STORAGE_PG] persist writer stopped");
    let remaining = pending.load(Ordering::Acquire).max(0) as usize;
    if remaining == 0 && rejected == 0 {
        FlushOutcome::Complete
    } else {
        FlushOutcome::Incomplete {
            pending: remaining,
            reason: format!(
                "Persistence incomplete: {remaining} pending, {rejected} rejected operations"
            ),
        }
    }
}

/// 单事务执行一批 op
async fn execute_batch(pool: &PgPool, batch: &[PersistOp]) -> anyhow::Result<usize> {
    let mut tx: Transaction<'_, sqlx::Postgres> = pool.begin().await?;
    lock_ops(&mut tx, batch).await?;
    let mut superseded = 0usize;
    for op in batch {
        if execute_op(&mut tx, op).await? == PersistenceOperationOutcome::Superseded {
            superseded += 1;
        }
    }
    tx.commit().await?;
    info!(
        committed = batch.len() - superseded,
        superseded, "[STORAGE_PG] persistence batch resolved"
    );
    Ok(batch.len())
}

/// 拆单定位坏 op：逐条独立事务执行。
/// - 成功 → 照常落库（committed）；
/// - **确定性错误**（SQLSTATE 23xxx 完整性冲突，重放必败——如 FK violation：
///   project 已被清理 leader 删除，AddSession 永远无法满足依赖）→ 隔离清单
///   丢弃并告警（quarantined），解开队列头堵塞；
/// - **瞬态错误**（连接类，PG 不可达）→ 当前 op 及其后未执行者并回重试集
///   （remaining）——丢弃会让 PG 恢复后 sync 反向清掉镜像（数据丢失），
///   维持退避重试直到 PG 恢复。
///
/// cancel 时剩余 op 同样进 remaining（关停排空 flush_and_stop 会再尝试）。
async fn isolate_poison_ops(
    pool: &PgPool,
    batch: &[PersistOp],
    cancel: &CancellationToken,
) -> (usize, Vec<(PersistOp, anyhow::Error)>, Vec<PersistOp>) {
    let mut quarantined = Vec::new();
    let mut committed = 0usize;
    let mut remaining: Vec<PersistOp> = Vec::new();
    for (idx, op) in batch.iter().enumerate() {
        if cancel.is_cancelled() {
            remaining.extend(batch[idx..].iter().cloned());
            break;
        }
        match execute_batch(pool, std::slice::from_ref(op)).await {
            Ok(_) => committed += 1,
            Err(e) if is_deterministic_pg_error(&e) => quarantined.push((op.clone(), e)),
            Err(e) => {
                debug!("[STORAGE_PG] transient error during isolation, deferring rest: {e:#}");
                remaining.extend(batch[idx..].iter().cloned());
                break;
            }
        }
    }
    (committed, quarantined, remaining)
}

/// PG 确定性错误判定：仅 SQLSTATE **23xxx**（完整性约束：FK 23503 / unique
/// 23505 / check 23514）。刻意不含 42xxx（语法/undefined_table 等_schema
/// 漂移类）：漏迁移会让**每个 op** 都被判"确定性"而整批清空，比堵住队列头
/// （保住数据可人工恢复）危害大得多——schema 类错误保留在瞬态重试路径，
/// 由持续 error 日志暴露人工介入。
fn is_deterministic_pg_error(e: &anyhow::Error) -> bool {
    for cause in e.chain() {
        if let Some(db_err) = cause.downcast_ref::<sqlx::Error>()
            && let sqlx::Error::Database(db) = db_err
            && let Some(code) = db.code()
        {
            return code.to_string().starts_with("23");
        }
        // 连接池/IO 类 sqlx 错误 → 瞬态
    }
    false
}

/// Acquire a stable sorted key set once per transaction, preventing lock-order cycles.
pub(in crate::pg) async fn lock_ops(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    ops: &[PersistOp],
) -> anyhow::Result<()> {
    let mut keys = std::collections::BTreeSet::new();
    for op in ops {
        match op {
            PersistOp::UpsertProject(p) => {
                keys.insert(format!("project:{}", p.project_id));
            }
            PersistOp::RemoveProject { project_id, .. }
            | PersistOp::TouchProject { project_id, .. }
            | PersistOp::UpdateAgentStatus { project_id, .. } => {
                keys.insert(format!("project:{project_id}"));
            }
            PersistOp::RemoveProjectForContainer {
                project_id,
                container_id,
                container_name,
                ..
            } => {
                keys.insert(format!("project:{project_id}"));
                keys.insert(format!("container:{container_id}"));
                keys.insert(format!("container-name:{container_name}"));
            }
            PersistOp::AddSession {
                project_id,
                session_id,
                ..
            } => {
                keys.insert(format!("project:{project_id}"));
                keys.insert(format!("session:{session_id}"));
            }
            PersistOp::RemoveSession { session_id, .. }
            | PersistOp::TouchSession { session_id, .. } => {
                keys.insert(format!("session:{session_id}"));
            }
            PersistOp::ClearSessions {
                project_id,
                sessions,
                ..
            } => {
                keys.insert(format!("project:{project_id}"));
                for (id, _) in sessions {
                    keys.insert(format!("session:{id}"));
                }
            }
            PersistOp::DeleteContainerWithProjects {
                container_id,
                projects,
            } => {
                keys.insert(format!("container:{container_id}"));
                for (id, _) in projects {
                    keys.insert(format!("project:{id}"));
                }
            }
            PersistOp::UpsertContainer(c) => {
                keys.insert(format!("container-name:{}", c.container_name));
                if let Some(id) = &c.container_id {
                    keys.insert(format!("container:{id}"));
                }
            }
            PersistOp::TouchContainer { .. } => {}
        }
    }
    for key in keys {
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 719324))")
            .bind(key)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

/// Single operation result: Committed and Superseded are distinct successful SQL outcomes.
/// 事务内执行器解引用传参（官方 transaction 示例范式）。
pub(in crate::pg) async fn execute_op(
    tx: &mut Transaction<'_, sqlx::Postgres>,
    op: &PersistOp,
) -> anyhow::Result<PersistenceOperationOutcome> {
    use PersistOp as Op;
    let db = &mut **tx;
    let outcome = match op {
        Op::UpsertContainer(c) => repo::upsert_container(db, c).await?,
        Op::UpsertProject(p) => repo::upsert_project(db, p).await?,
        Op::RemoveProject {
            project_id,
            generation,
        } => repo::remove_project(db, project_id, generation).await?,
        Op::RemoveProjectForContainer {
            project_id,
            generation,
            container_id,
            ..
        } => repo::remove_project_for_container(db, project_id, generation, container_id).await?,
        Op::AddSession {
            project_id,
            session_id,
            container_name,
            project_generation,
            generation,
            predecessor,
        } => {
            repo::add_session(
                db,
                project_id,
                session_id,
                container_name.as_deref(),
                project_generation,
                generation,
                predecessor.as_deref(),
            )
            .await?
        }
        Op::RemoveSession {
            session_id,
            generation,
        } => repo::remove_session(db, session_id, generation).await?,
        Op::ClearSessions {
            project_id,
            generation,
            sessions,
        } => repo::clear_sessions(db, project_id, generation, sessions).await?,
        Op::DeleteContainerWithProjects {
            container_id,
            projects,
        } => repo::delete_container_with_projects(db, container_id, projects).await?,
        Op::TouchProject {
            project_id,
            last_activity,
        } => {
            repo::touch_project(db, project_id, *last_activity).await?;
            PersistenceOperationOutcome::Committed
        }
        Op::TouchContainer {
            container_name,
            last_activity,
        } => {
            repo::touch_container(db, container_name, *last_activity).await?;
            PersistenceOperationOutcome::Committed
        }
        Op::TouchSession {
            session_id,
            last_seen_at,
        } => {
            repo::touch_session(db, session_id, *last_seen_at).await?;
            PersistenceOperationOutcome::Committed
        }
        Op::UpdateAgentStatus {
            project_id,
            agent_status,
        } => {
            repo::update_agent_status(db, project_id, agent_status).await?;
            PersistenceOperationOutcome::Committed
        }
    };
    Ok(outcome)
}
