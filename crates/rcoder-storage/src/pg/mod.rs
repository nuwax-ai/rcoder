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

// 两业务域目录化(开闭原则:改一个域不碰另一个域的文件):
// - project_store/  主服务域(ProjectStore 契约的 PG 后端实现全部)
// - userapp/        Userapp 业务域(activity/metadata)
mod project_store;

#[cfg(test)]
pub(crate) mod test_support;

// 对外面保持原路径不变(rcoder 消费 pg::sync / pg::leader_selection / PersistWriter):
pub use project_store::sync;
pub use project_store::writer::PersistWriter;
pub mod leader_selection {
    pub use super::project_store::leader::PgLeaderElection;
}

use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Duration, Instant};

use moka::sync::Cache;
use tokio::sync::mpsc;
use tracing::info;

use shared_types::{ContainerLookup, ProjectAndContainerInfo, ServiceType};

use self::project_store::persist_ops::PersistOp;
use crate::adapter::{ProjectAdapter, container_entry_key};
use crate::config::PostgresConfig;

/// Touch 类 op 的入队节流窗口（enqueue 前按 key 判断，零 PG 开销）
const TOUCH_THROTTLE: Duration = Duration::from_secs(5);
/// 节流表条目的闲置过期：clear_session/容器删除拿不到已消亡的 key（session id
/// 集合已清空），靠 TTI 自动回收防泄漏；热 key 读取即续期，节流语义不变
const TOUCH_THROTTLE_TTI: Duration = Duration::from_secs(3600);

/// PostgreSQL 持久化后端
pub struct PgStore {
    /// Serializes local mirror mutation and immutable operation registration; never spans await.
    registration: std::sync::Mutex<std::collections::HashMap<String, String>>,
    container_registration: std::sync::Mutex<
        std::collections::HashMap<String, shared_types::persistence::ContainerPersistenceIdentity>,
    >,
    closing: std::sync::atomic::AtomicBool,
    active_writes: std::sync::atomic::AtomicUsize,
    write_finished: tokio::sync::Notify,
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
    database: crate::db::owner::DatabaseOwner,
    postgres_config: PostgresConfig,
}

impl PgStore {
    /// Stop admission before draining accepted direct writes and the fallback queue.
    pub async fn shutdown_flush_outcome(&self, timeout: Duration) -> shared_types::FlushOutcome {
        let deadline = tokio::time::Instant::now() + timeout;
        {
            let _registration = self.registration.lock().unwrap_or_else(|p| p.into_inner());
            self.closing.store(true, Ordering::Release);
        }
        loop {
            let done = self.write_finished.notified();
            if self.active_writes.load(Ordering::Acquire) == 0 {
                break;
            }
            if tokio::time::timeout_at(deadline, done).await.is_err() {
                return shared_types::FlushOutcome::TimedOut {
                    pending: self.pending_ops.load(Ordering::Acquire).max(0) as usize,
                };
            }
        }
        let outcome = self
            .writer
            .flush_outcome(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .await;
        if !outcome.is_complete() {
            return outcome;
        }
        match tokio::time::timeout_at(deadline, self.database.shutdown()).await {
            Ok(Ok(())) => outcome,
            Ok(Err(error)) => shared_types::FlushOutcome::Incomplete {
                pending: 0,
                reason: error.to_string(),
            },
            Err(_) => shared_types::FlushOutcome::TimedOut { pending: 0 },
        }
    }

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
        let database =
            crate::db::postgres::open(config, vec![crate::db::schema::Component::Project]).await?;

        let (inner, cleanup_rx) = ProjectAdapter::new(namespace, cluster_domain);
        let container_registration = project_store::load::load_all(&database, &inner).await?;
        let (ops_tx, ops_rx) = mpsc::unbounded_channel();
        let pending_ops = Arc::new(AtomicI64::new(0));
        let writer = PersistWriter::spawn(database.clone(), ops_rx, Arc::clone(&pending_ops));

        let store = Self {
            registration: std::sync::Mutex::new(std::collections::HashMap::new()),
            container_registration: std::sync::Mutex::new(container_registration),
            closing: std::sync::atomic::AtomicBool::new(false),
            active_writes: std::sync::atomic::AtomicUsize::new(0),
            write_finished: tokio::sync::Notify::new(),
            inner,
            ops_tx,
            touch_throttled: Cache::builder().time_to_idle(TOUCH_THROTTLE_TTI).build(),
            writer,
            pending_ops,
            database,
            postgres_config: config.clone(),
        };
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

    pub fn postgres_config(&self) -> &PostgresConfig {
        &self.postgres_config
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

    /// 非阻塞 enqueue（结构性 op）。队列无界，写入即返回。
    fn enqueue_structural(&self, op: PersistOp) {
        self.register_container_intent(&op);
        self.pending_ops.fetch_add(1, Ordering::AcqRel);
        if let Err(op) = self.ops_tx.send(op) {
            // Retain the pending count: a closed queue is not durable success.
            // writer 已停止（仅发生在关停期）：丢弃并告警
            tracing::warn!(
                "[STORAGE_PG] persist queue closed, dropped operation: kind={}",
                op.0.kind()
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

    fn prepare_info(
        &self,
        mut info: Arc<ProjectAndContainerInfo>,
        retired: &std::collections::HashMap<String, String>,
    ) -> anyhow::Result<Arc<ProjectAndContainerInfo>> {
        let supplied = info.persistence_identity().clone();
        // 契约三行3/§1.2 换代门输入：当前注册的 workload UID（同名替换判定）。
        let mut registered_workload_uid: Option<String> = None;
        let mut identity = if let Some(existing) = self.inner.get(info.project_id()) {
            let current = existing.persistence_identity();
            registered_workload_uid = existing
                .container_info()
                .and_then(|basic| basic.workload_uid);
            anyhow::ensure!(
                supplied.revision == 0
                    || (supplied.generation == current.generation
                        && supplied.revision == current.revision),
                "Project registration changed; stale snapshot cannot be rebased"
            );
            current.clone()
        } else if let Some(previous) = retired.get(info.project_id()) {
            let mut identity = shared_types::persistence::ProjectPersistenceIdentity::default();
            for sid in info.sessions() {
                identity.sessions.insert(sid.clone(), uuid_generation());
            }
            identity.predecessor = Some(previous.clone());
            identity
        } else {
            supplied.clone()
        };
        identity.revision = identity
            .revision
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("Project registration revision exhausted"))?;
        identity.container = if let Some(basic) = info.container_info() {
            let key = container_entry_key(&info);
            let registrations = self
                .container_registration
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let current = registrations
                .get(&key)
                .cloned()
                .or_else(|| identity.container.clone());
            let physical_uid = (!basic.container_id.is_empty()).then(|| basic.container_id.clone());
            if let (Some(previous), Some(supplied)) = (&current, &supplied.container) {
                anyhow::ensure!(
                    supplied.generation == previous.generation
                        && supplied.revision == previous.revision,
                    "Container registration changed; stale snapshot cannot be rebased"
                );
            }
            let next = match current {
                Some(mut previous)
                    if previous.physical_uid == physical_uid || previous.physical_uid.is_none() =>
                {
                    previous.revision = previous.revision.checked_add(1).ok_or_else(|| {
                        anyhow::anyhow!("Container registration revision exhausted")
                    })?;
                    previous.physical_uid = physical_uid;
                    previous
                }
                Some(previous) => {
                    anyhow::ensure!(
                        physical_uid.is_some(),
                        "A placeholder cannot clear a bound container identity"
                    );
                    // 契约三行3/§1.2：同名 workload 对象已被替换（注册与观察的
                    // workload UID 均在且不同）→ 禁止自动换代重绑——自动接管
                    // 会把 projects/sessions 绑到别人的 workload 上；需显式
                    // 恢复路径处置旧绑定。任一方缺 workload UID（Docker/
                    // bare-pod/Deployment 族）不在此门内，按既有换代事务走。
                    if let (Some(registered), Some(observed)) = (
                        registered_workload_uid.as_deref(),
                        basic.workload_uid.as_deref(),
                    ) {
                        anyhow::ensure!(
                            registered == observed,
                            "Container workload was replaced ({registered} -> {observed}); \
                             stop the project to retire the stale binding before re-ensuring"
                        );
                    }
                    shared_types::persistence::ContainerPersistenceIdentity {
                        generation: uuid_generation(),
                        revision: 1,
                        physical_uid,
                        predecessor: Some(previous.generation),
                        predecessor_revision: Some(previous.revision),
                    }
                }
                None => shared_types::persistence::ContainerPersistenceIdentity {
                    generation: uuid_generation(),
                    revision: 1,
                    physical_uid,
                    predecessor: None,
                    predecessor_revision: None,
                },
            };
            Some(next)
        } else {
            None
        };
        Arc::make_mut(&mut info).set_persistence_identity(identity);
        Ok(info)
    }

    /// Caller holds registration, matching the prepare/sync lock order.
    fn container_deletion_targets(&self, uid: &str) -> Vec<(String, String)> {
        let rows = self
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let mut targets: Vec<_> = rows
            .iter()
            .filter(|(_, identity)| identity.physical_uid.as_deref() == Some(uid))
            .map(|(name, identity)| (name.clone(), identity.generation.clone()))
            .collect();
        targets.sort();
        targets
    }

    /// Register accepted immutable intents. A retry of an old queued batch must
    /// never roll the local registration cursor back to an older generation.
    fn register_container_intent(&self, op: &PersistOp) {
        let snapshot = match op {
            #[cfg(test)]
            PersistOp::UpsertContainer(snapshot) => snapshot,
            PersistOp::RegisterProject {
                container: Some(snapshot),
                ..
            } => snapshot,
            _ => return,
        };
        let mut rows = self
            .container_registration
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let replace = rows.get(&snapshot.container_name).is_none_or(|current| {
            (current.generation == snapshot.generation
                && current.revision <= snapshot.expected_revision)
                || (snapshot.predecessor.as_ref() == Some(&current.generation)
                    && snapshot.predecessor_revision == Some(current.revision))
        });
        if replace {
            rows.insert(
                snapshot.container_name.clone(),
                shared_types::persistence::ContainerPersistenceIdentity {
                    generation: snapshot.generation.clone(),
                    revision: snapshot.expected_revision.saturating_add(1),
                    physical_uid: snapshot.container_id.clone(),
                    predecessor: snapshot.predecessor.clone(),
                    predecessor_revision: snapshot.predecessor_revision,
                },
            );
        }
    }

    /// 从 info 构造并按 FK 顺序（先容器后 project）入队快照。
    fn persist_upsert(&self, info: &ProjectAndContainerInfo) -> anyhow::Result<()> {
        self.enqueue_structural(project_store::persist_ops::registration_for_info(info)?);
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

    fn find_app_runtime_addr(&self, app_id: &str) -> Option<String> {
        self.inner.find_app_runtime_addr(app_id)
    }

    fn find_project_scope(
        &self,
        project_id: &str,
        service_type: &ServiceType,
    ) -> Option<shared_types::ProjectScope> {
        self.inner.find_project_scope(project_id, service_type)
    }
}

fn uuid_generation() -> String {
    shared_types::persistence::ProjectPersistenceIdentity::default().generation
}
