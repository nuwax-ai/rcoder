//! PgStore：PostgreSQL 持久化后端（内存镜像 + write-behind）
//!
//! 架构（Phase 1 单副本即生效；Phase 2 解锁多副本）：
//! - **读**：全部走内层 [`ProjectAdapter`] 内存镜像（同步、O(1)，满足
//!   ContainerLookup 同步 trait 与每消息级热路径）
//! - **写**：先应用镜像（复用内存实现唯一一份业务逻辑：引用计数/索引/RAII），
//!   成功后同步 enqueue [`PersistOp`]（微秒级、非阻塞），后台 writer 批量落 PG
//! - **启动**：connect → migrate → 全量 load 重建镜像（经 inner 直写，天然旁路持久化）
//! - **崩溃窗口**：结构性 op 顺序持久化、毫秒级典型延迟；kill -9 丢尾部 Touch
//!   （idle 判据秒级误差，可接受）
//!
//! PG 为跨重启/跨副本的真源；容器运行态真源仍在 K8s/Docker API（label + 确定性命名）。

mod leader;
mod load;
mod persist_ops;
mod repo;
pub mod leader_selection {
    pub use super::leader::PgLeaderElection;
}
mod store_impl;
pub mod sync;
mod writer;

#[cfg(test)]
mod tests;

pub mod activity;
pub mod publish;

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use moka::sync::Cache;
use sqlx::postgres::PgPoolOptions;
use tokio::sync::mpsc;
use tracing::info;

use shared_types::{ContainerLookup, ProjectAndContainerInfo, ServiceType};

use self::persist_ops::{ContainerSnapshot, PersistOp, ProjectSnapshot};
use crate::adapter::{ProjectAdapter, container_entry_key};
use crate::config::PostgresConfig;

pub use writer::PersistWriter;

/// Touch 类 op 的入队节流窗口（enqueue 前按 key 判断，零 PG 开销）
const TOUCH_THROTTLE: Duration = Duration::from_secs(5);
/// 节流表条目的闲置过期：clear_session/容器删除拿不到已消亡的 key（session id
/// 集合已清空），靠 TTI 自动回收防泄漏；热 key 读取即续期，节流语义不变
const TOUCH_THROTTLE_TTI: Duration = Duration::from_secs(3600);

/// PostgreSQL 持久化后端
pub struct PgStore {
    /// 内存镜像（读写共用；启动时由 load 模块全量重建）
    inner: ProjectAdapter,
    /// write-behind 队列生产端（消费端在 PersistWriter）
    ops_tx: mpsc::UnboundedSender<PersistOp>,
    /// Touch 节流表：key（"p:{id}"/"c:{name}"/"s:{sid}"）→ 上次入队时刻
    /// （moka TTI 缓存：死 key 一小时后自动回收）
    touch_throttled: Cache<String, Instant>,
    /// writer 句柄（flush_and_stop 由 rcoder 优雅关停调用）
    writer: PersistWriter,
    /// 在途 op 计数（enqueue 自增 / writer 提交后自减）。
    /// sync 任务用它做排空屏障：归零后读 PG 即包含本副本全部已提交写。
    pending_ops: Arc<AtomicI64>,
    /// 连接池（activity/publish 等兄弟持久化组件共用）
    pool: sqlx::PgPool,
}

impl PgStore {
    /// 连接 + 迁移 + 全量加载，构造 PG 后端。
    ///
    /// 返回 `(store, cleanup_rx)`：与 ProjectAdapter::new 同形，CleanupRequest
    /// 队列语义完全复用（ResourceReaper 无感知切换）。
    ///
    /// # Errors
    /// 连接失败/迁移失败/加载解码失败均 fail fast（PG 模式下绝不静默降级内存）。
    pub async fn connect(
        config: &PostgresConfig,
        namespace: String,
        cluster_domain: String,
    ) -> anyhow::Result<(Self, mpsc::Receiver<shared_types::CleanupRequest>)> {
        let dsn = config.to_dsn().map_err(anyhow::Error::msg)?;
        // 语句超时在 acquire 后的会话级设置（防单条慢查询拖死连接）
        let statement_timeout_ms = config.statement_timeout_secs() * 1000;
        let pool = PgPoolOptions::new()
            .max_connections(config.max_connections())
            .acquire_timeout(Duration::from_secs(config.connect_timeout_secs()))
            .after_connect(move |conn, _meta| {
                Box::pin(async move {
                    // SET 语句不支持参数占位符；set_config 是等价的标准做法且可参数化
                    // （sqlx 新版 SqlSafeStr 约束也禁止 format! 动态拼 SQL）
                    sqlx::query("SELECT set_config('statement_timeout', $1, false)")
                        .bind(statement_timeout_ms.to_string())
                        .execute(&mut *conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(&dsn)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "PG connect failed ({}/{}): {e}",
                    config.host.as_deref().unwrap_or("?"),
                    config.database.as_deref().unwrap_or("?")
                )
            })?;
        info!(
            "[STORAGE_PG] connected: {} (password not logged)",
            config.describe()
        );

        // 迁移：sqlx migrate 自带 advisory lock，多副本并发启动安全
        sqlx::migrate!("./migrations")
            .run(&pool)
            .await
            .map_err(|e| anyhow::anyhow!("PG migrate failed: {e}"))?;

        let (inner, cleanup_rx) = ProjectAdapter::new(namespace, cluster_domain);
        let (ops_tx, ops_rx) = mpsc::unbounded_channel();
        let pending_ops = Arc::new(AtomicI64::new(0));
        let writer = PersistWriter::spawn(pool.clone(), ops_rx, Arc::clone(&pending_ops));

        let store = Self {
            inner,
            ops_tx,
            touch_throttled: Cache::builder().time_to_idle(TOUCH_THROTTLE_TTI).build(),
            writer,
            pending_ops,
            pool: pool.clone(),
        };
        load::load_all(&pool, &store.inner).await?;
        let stats = store.inner.get_stats();
        info!(
            "[STORAGE_PG] boot load complete: projects={} containers={} sessions={}",
            stats.total_projects, stats.total_containers, stats.active_sessions
        );
        Ok((store, cleanup_rx))
    }

    /// 内存镜像访问（backend 装配/快照构造等特有能力）
    pub fn inner(&self) -> &ProjectAdapter {
        &self.inner
    }

    /// writer 句柄（优雅关停 flush 用）
    pub fn writer(&self) -> &PersistWriter {
        &self.writer
    }

    /// 连接池（activity/publish 等兄弟持久化组件构造用）
    pub fn pool(&self) -> &sqlx::PgPool {
        &self.pool
    }

    /// 等待在途 op 全部落库（sync 任务的排空屏障；二次确认防批处理间隙误判）
    pub async fn wait_drained(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let first = self.pending_ops.load(Ordering::Acquire);
            if first == 0 {
                tokio::time::sleep(Duration::from_millis(50)).await;
                if self.pending_ops.load(Ordering::Acquire) == 0 {
                    return true;
                }
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// 结构性写的事务直写超时：正常毫秒级完成；超时降级走 write-behind。
    const DURABLE_COMMIT_TIMEOUT: Duration = Duration::from_millis(600);

    /// 会话创建的结构性 op **事务直写**（durable 路径）：
    /// 内存镜像更新 + container/project/session 于同一 sqlx 事务提交——
    /// 方法返回（Ok）即 PG 主库已提交，chat 侧据此保证"session_id 交到
    /// 前端手上时任何副本回源直查必命中"。
    ///
    /// 提交超时/失败降级：事务丢弃（drop=rollback），改入 write-behind
    /// 队列（现有异步路径）——chat 不失败（内存真源），可见性窗口仅在
    /// PG 故障态退化。降级路径 op 会入队，成功路径不入队（无双写）。
    pub async fn insert_with_session_durable(
        &self,
        project_id: String,
        info: Arc<ProjectAndContainerInfo>,
        session_id: &str,
    ) -> anyhow::Result<()> {
        // 1. 内存镜像（与 insert_with_session 的内存部分一致）
        self.inner
            .insert_with_session(project_id, Arc::clone(&info), Some(session_id))?;

        // 2. 事务直写（与 persist_upsert 相同的 op 集：容器 + 项目 + 会话）
        let session_project = info.project_id().to_string();
        let container_name = info.container_info().map(|_| container_entry_key(&info));
        let snapshot = ProjectSnapshot::from_info(&info)?;
        let durable = async {
            let mut tx = self.pool.begin().await?;
            if let Some(basic) = info.container_info()
                && let Some(st) = info.service_type()
            {
                repo::upsert_container(
                    &mut *tx,
                    &ContainerSnapshot::from_info(&container_entry_key(&info), &basic, &st),
                )
                .await?;
            }
            repo::upsert_project(&mut *tx, &snapshot).await?;
            repo::add_session(
                &mut *tx,
                &session_project,
                session_id,
                container_name.as_deref(),
            )
            .await?;
            tx.commit().await
        };
        match tokio::time::timeout(Self::DURABLE_COMMIT_TIMEOUT, durable).await {
            Ok(Ok(())) => {
                tracing::debug!(
                    "[STORAGE_PG] durable commit ok: project_id={}, session_id={}",
                    session_project,
                    session_id
                );
                Ok(())
            }
            outcome => {
                let reason = match outcome {
                    Ok(Ok(())) => unreachable!("covered by first arm"),
                    Ok(Err(e)) => format!("sql error: {e}"),
                    Err(_) => "timeout".to_string(),
                };
                tracing::warn!(
                    "[STORAGE_PG] durable commit failed ({reason}), falling back to write-behind: project_id={}, session_id={}",
                    session_project,
                    session_id
                );
                // 降级：与 insert_with_session 的入队路径完全一致（幂等，writer 重放安全）
                tracing::warn!(
                    "[STORAGE_PG] durable commit failed, falling back to write-behind: project_id={}, session_id={}",
                    session_project,
                    session_id
                );
                self.persist_upsert(&info)?;
                self.enqueue_structural(PersistOp::AddSession {
                    project_id: session_project,
                    session_id: session_id.to_string(),
                    container_name,
                });
                Ok(())
            }
        }
    }

    /// 按 session_id 读：内存镜像 hit 直接返回；miss 回源直查主库一次
    /// （所有副本连 `-rw` 主库，无复制延迟——正常路径 durable 提交后必中），
    /// 命中则 hydrate 进本地镜像（旁路持久化，此后走内存）。
    pub async fn get_by_session_id_with_fetch(
        &self,
        session_id: &str,
    ) -> Option<Arc<ProjectAndContainerInfo>> {
        if let Some(hit) = self.inner.get_by_session_id(session_id) {
            return Some(hit);
        }
        let fetched = repo::fetch_project_by_session(&self.pool, session_id).await?;
        let hydrated = self.hydrate_fetched(fetched);
        tracing::info!(
            "[STORAGE_PG] session miss backfilled from PG: session_id={}, project_id={}",
            session_id,
            hydrated.project_id()
        );
        Some(hydrated)
    }

    /// 回源行组装为 info 并旁路写入内存镜像（数据本就来自 PG，回写不走持久化，
    /// 与 sync.rs 的 hydrate 同模式——复用 load 的组装逻辑）
    fn hydrate_fetched(
        &self,
        (project_row, container_row): (repo::rows::ProjectRow, Option<repo::rows::ContainerRow>),
    ) -> Arc<ProjectAndContainerInfo> {
        let mut container_by_name = std::collections::HashMap::new();
        if let Some(row) = container_row {
            let name = row.container_name.clone();
            let basic = load::container_row_to_basic(&row);
            container_by_name.insert(name, basic);
        }
        // 解析失败（枚举演进/缺 service_type）→ 兜底空 map：项目记录仍返回
        //（SSE 只需要 project_id/container_name 路由信息）
        let info = load::hydrate_project(&project_row, &container_by_name)
            .unwrap_or_else(|| ProjectAndContainerInfo::new(project_row.project_id.clone()));
        let info = Arc::new(info);
        // 旁路写入镜像（数据来自 PG，不走持久化）
        let pid = info.project_id().to_string();
        self.inner
            .insert_with_session(pid, Arc::clone(&info), None)
            .ok();
        // 回源键是 sessions 表的 session_id（可能与 latest_session 不同）——
        // 显式补齐该映射，让下次同 session 查询走内存命中
        if let Some(sid) = project_row.latest_session.as_deref() {
            self.inner.add_session_to_project(info.project_id(), sid);
        }
        info
    }

    /// 非阻塞 enqueue（结构性 op）。队列无界，写入即返回。
    fn enqueue_structural(&self, op: PersistOp) {
        self.pending_ops.fetch_add(1, Ordering::AcqRel);
        if let Err(op) = self.ops_tx.send(op) {
            self.pending_ops.fetch_sub(1, Ordering::AcqRel);
            // writer 已停止（仅发生在关停期）：丢弃并告警
            tracing::warn!(
                "[STORAGE_PG] persist queue closed, dropped {}: {:?}",
                op.0.kind(),
                op.0
            );
        }
    }

    /// 节流 enqueue（Touch/UpdateAgentStatus 类）。同 key 在窗口内只入队一次。
    ///
    /// get/insert 两步存在良性竞态（并发首触可能各发一次）：Touch 类幂等，
    /// 多一条 op 只是多一次无害 UPDATE。
    fn enqueue_throttled(&self, key: &str, op: PersistOp) {
        let now = Instant::now();
        let should_send = match self.touch_throttled.get(key) {
            Some(last) if now.duration_since(last) < TOUCH_THROTTLE => false,
            _ => {
                self.touch_throttled.insert(key.to_string(), now);
                true
            }
        };
        if should_send {
            self.enqueue_structural(op);
        }
    }

    /// 从 info 构造并按 FK 顺序（先容器后 project）入队快照。
    fn persist_upsert(&self, info: &ProjectAndContainerInfo) -> anyhow::Result<()> {
        if let Some(basic) = info.container_info()
            && let Some(st) = info.service_type()
        {
            let snapshot = ContainerSnapshot::from_info(&container_entry_key(info), &basic, &st);
            self.enqueue_structural(PersistOp::UpsertContainer(Box::new(snapshot)));
        }
        self.enqueue_structural(PersistOp::UpsertProject(Box::new(
            ProjectSnapshot::from_info(info)?,
        )));
        Ok(())
    }
}

impl ContainerLookup for PgStore {
    fn find_by_user_id(&self, user_id: &str, service_type: &ServiceType) -> Option<String> {
        self.inner.find_by_user_id(user_id, service_type)
    }

    fn find_by_project_id(&self, project_id: &str, service_type: &ServiceType) -> Option<String> {
        self.inner.find_by_project_id(project_id, service_type)
    }

    fn find_by_pod_id(&self, pod_id: &str, service_type: &ServiceType) -> Option<String> {
        self.inner.find_by_pod_id(pod_id, service_type)
    }

    fn find_project_scope(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> Option<shared_types::ProjectScope> {
        self.inner.find_project_scope(project_id, service_type)
    }
}
