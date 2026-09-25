//! Userapp 活动状态注册表（核心状态域）。
//!
//! [`AppActivityRegistry`] 是「闲置自动回收 + 流量唤醒」特性的共享状态中心(in-memory,
//! rcoder 单实例)——本目录按域拆分：
//! - 本文件（mod.rs）：struct 定义 + mark 系列状态转移 + 访问追踪
//!   ([`AppAccessTracker`] touch,5s 节流) + 回收过渡协调([`RecycleTransition`]);
//! - [`wake`]:流量唤醒（[`shared_types::AppWakeControl`] trait impl——hold-and-wait
//!   拉起 + 并发合流 + 多副本远端状态兜底）;
//! - [`persistence_ops`]:影子持久化（PG 影子行 flush/加载/脏行收集）。
//!
//! 构造顺序:rcoder 启动早期(init_proxy 之前)独立构造为 `Arc`,注入 Pingora(访问/唤醒);
//! runtime 构建后(RuntimeManager::get)经 [`AppActivityRegistry::set_runtime`] 注入(OnceLock 延迟)。
//! wake 只在 `is_stopped` 真时触发,而 `stopped` 表要到 `AppService::new` 才填充——此时 OnceLock 早已 set。

mod persistence_ops;
mod wake;

#[cfg(test)]
mod tests;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use tokio::sync::Notify;
use tracing::warn;

use container_runtime_api::UserAppRuntime;
use shared_types::{AppAccessTracker, WakeOutcome};

use wake::{REMOTE_STATE_MAX_ENTRIES, REMOTE_STATE_TTL, RemoteState, WakeHandle};

/// `touch` 节流粒度:同一 app 在此窗口内多次访问只写一次 `last_accessed`(降低 DashMap 锁竞争)
const TOUCH_THROTTLE: Duration = Duration::from_secs(5);

/// 闲置回收过渡守卫。守卫存活期内，新流量会等待 scale-to-zero 完成后再唤醒，
/// 避免扫描器在活跃请求中途停掉应用。
pub struct RecycleTransition {
    map: Arc<DashMap<String, Arc<Notify>>>,
    key: String,
    signal: Arc<Notify>,
}

impl Drop for RecycleTransition {
    fn drop(&mut self) {
        if let dashmap::mapref::entry::Entry::Occupied(entry) = self.map.entry(self.key.clone())
            && Arc::ptr_eq(entry.get(), &self.signal)
        {
            entry.remove();
        }
        self.signal.notify_waiters();
    }
}

/// Userapp 活动状态注册表(in-memory,rcoder 单实例共享)
///
/// M5 起支持影子持久化：注入 [`shared_types::ActivityPersistence`] 后，
/// 状态变更标脏，由 rcoder 侧 flusher 周期批量落 PG；启动时 `apply_loaded`
/// 恢复跨重启的 last_accessed；stopped/wake_blocked 从运行态与生命周期策略重建（wake single-flight 等进程内
/// 协调机制不持久化）。`last_accessed` 用 wall-clock（`DateTime<Utc>`）而非
/// `Instant`（单调钟不可序列化）；节流与 epoch 复核语义不变。
pub struct AppActivityRegistry {
    /// app_id → 最近一次真实 HTTP 访问时刻(节流更新;wall-clock,可持久化)
    pub(super) identities: std::sync::Mutex<std::collections::HashMap<String, (String, i64)>>,
    /// Versioned cache keys make in-flight lookups from before a local delete
    /// unreachable. Access this map only while holding `identities` first.
    identity_versions: std::sync::Mutex<std::collections::HashMap<String, u64>>,
    access_identities:
        moka::future::Cache<(String, u64), Arc<shared_types::UserAppLifecycleRecord>>,
    pub(super) last_accessed: DashMap<String, DateTime<Utc>>,
    /// app_id → 已 stopped(scale0)标记;stop/start/wake/重启重建 共同维护
    pub(super) stopped: DashSet<String>,
    /// app_id → 删除围栏中的应用（待清理/对账）；流量不得唤醒。
    /// 拍板 2026-09-23 统一唤醒语义后，手动停止不再进入本集合。
    pub(super) wake_blocked: DashSet<String>,
    /// app_id → 待持久化脏行(flusher 周期 collect_dirty 落库)
    pub(super) dirty: DashSet<String>,
    /// app_id → 待删除行(forget_app 后由 flusher 清 PG)
    pub(super) deleted: DashSet<(String, String)>,
    /// 影子持久化(延迟注入;PG 模式 main 设置,内存模式保持 None)
    ///
    /// 字段 `pub(super)`：供 `persistence_ops.rs` 子模块（同类型 extension-impl）访问。
    pub(super) persistence: OnceLock<Arc<dyn shared_types::ActivityPersistence>>,
    /// app_id → 进行中的唤醒句目(并发合流)
    waking: Arc<DashMap<String, Arc<WakeHandle>>>,
    /// app_id → 正在执行的闲置回收过渡。
    recycling: Arc<DashMap<String, Arc<Notify>>>,
    /// runtime 延迟注入(wake 需要 scale + 查 status;启动早期拿不到,故 OnceLock)
    runtime: OnceLock<Arc<dyn UserAppRuntime>>,
    /// Weak reference avoids a service/registry ownership cycle. Runtime access
    /// remains read-only here; all wake mutations go through the coordinator.
    coordinator: OnceLock<std::sync::Weak<crate::service::AppService>>,
    /// 集群真实状态兜底缓存(app_id → 快照;TTL 过期自动失效)。
    /// 多副本下本进程内存表可能不知情其他副本的 stop,集群 replicas 是权威事实源。
    /// sync 版 Cache:get/insert 均同步——mark_* 状态写点(同步方法)可直接刷新。
    remote_state: moka::sync::Cache<String, RemoteState>,
    /// 唤醒 hold-and-wait 上限
    wake_timeout: Duration,
    /// touch 节流(可配,便于测试)
    throttle: Duration,
}

impl AppActivityRegistry {
    /// 创建注册表(生产构造,throttle 用默认 5s)
    pub fn new(wake_timeout: Duration) -> Self {
        Self::new_with(wake_timeout, TOUCH_THROTTLE)
    }

    /// 创建注册表(指定 throttle,测试用)
    fn new_with(wake_timeout: Duration, throttle: Duration) -> Self {
        Self {
            identities: std::sync::Mutex::new(Default::default()),
            identity_versions: std::sync::Mutex::new(Default::default()),
            access_identities: moka::future::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(Duration::from_secs(5))
                .build(),
            last_accessed: DashMap::new(),
            stopped: DashSet::new(),
            wake_blocked: DashSet::new(),
            dirty: DashSet::new(),
            deleted: DashSet::new(),
            persistence: OnceLock::new(),
            waking: Arc::new(DashMap::new()),
            recycling: Arc::new(DashMap::new()),
            runtime: OnceLock::new(),
            coordinator: OnceLock::new(),
            remote_state: moka::sync::Cache::builder()
                .time_to_live(REMOTE_STATE_TTL)
                .max_capacity(REMOTE_STATE_MAX_ENTRIES)
                .build(),
            wake_timeout,
            throttle,
        }
    }

    /// 注入 runtime(幂等;重复 set 告警不覆盖)。main.rs 在 runtime 构建后调用。
    pub fn set_runtime(&self, rt: Arc<dyn UserAppRuntime>) {
        if self.runtime.set(rt).is_err() {
            warn!("[ACTIVITY] set_runtime called twice; keeping existing runtime");
        }
    }

    pub(crate) fn set_coordinator(
        &self,
        service: std::sync::Weak<crate::service::AppService>,
    ) -> crate::models::AppResult<()> {
        self.coordinator.set(service).map_err(|_| {
            crate::models::AppOperationError::InvalidState(
                "Activity lifecycle coordinator is already initialized".into(),
            )
        })
    }

    /// 标记 app 为 stopped(scale0，可被流量唤醒)。手动 stop 与闲置回收
    /// 统一使用本档（拍板 2026-09-23）。
    pub fn mark_stopped(&self, app_id: &str) {
        self.wake_blocked.remove(app_id);
        self.stopped.insert(app_id.to_string());
        self.note_dirty(app_id);
        self.remote_state
            .insert(app_id.to_string(), RemoteState::STOPPED);
    }

    /// 删除围栏：应用进入删除/待清理流程，阻止流量唤醒路由与回收扫描。
    pub fn mark_wake_blocked(&self, app_id: &str) {
        self.stopped.remove(app_id);
        self.wake_blocked.insert(app_id.to_string());
        self.note_dirty(app_id);
        self.remote_state
            .insert(app_id.to_string(), RemoteState::STOPPED);
    }

    /// 是否有进行中的唤醒(回收扫描器据此跳过,避免与 in-flight wake 竞态)
    pub fn is_waking(&self, app_id: &str) -> bool {
        self.waking.contains_key(app_id)
    }

    pub fn is_wake_blocked(&self, app_id: &str) -> bool {
        self.wake_blocked.contains(app_id)
    }

    /// 应用删除后清理所有内存态。正在等待的唤醒/回收请求会立即收到终止信号；
    /// RAII 守卫使用指针比对移除条目，不会误删同 ID 重建后的新状态。
    #[cfg(test)]
    pub fn forget_app(&self, app_id: &str) {
        self.forget_matching_lifecycle(app_id, None);
    }

    pub fn forget_lifecycle(&self, app_id: &str, lifecycle_id: &str) -> bool {
        self.forget_matching_lifecycle(app_id, Some(lifecycle_id))
    }

    fn forget_matching_lifecycle(&self, app_id: &str, expected: Option<&str>) -> bool {
        let mut identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(expected) = expected {
            if identities
                .get(app_id)
                .is_some_and(|(current, _)| current != expected)
            {
                return false;
            }
            self.deleted
                .insert((app_id.to_owned(), expected.to_owned()));
        }
        self.invalidate_access_identity(app_id);
        if let Some((lifecycle, _)) = identities.remove(app_id) {
            self.deleted.insert((app_id.to_owned(), lifecycle));
        }
        self.last_accessed.remove(app_id);
        self.stopped.remove(app_id);
        self.dirty.remove(app_id);
        self.wake_blocked.remove(app_id);
        self.remote_state.invalidate(app_id);
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.waking.entry(app_id.to_string())
        {
            let handle = entry.remove();
            handle
                .tx
                .send_replace(Some(WakeOutcome::Failed("Application was deleted".into())));
        }
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.recycling.entry(app_id.to_string())
        {
            entry.remove().notify_waiters();
        }
        true
    }

    /// 仅当最近访问时间仍等于扫描器观测值时，原子登记回收过渡。
    pub fn try_begin_recycle(
        &self,
        app_id: &str,
        observed_access: DateTime<Utc>,
    ) -> Option<RecycleTransition> {
        let signal = Arc::new(Notify::new());
        match self.recycling.entry(app_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(_) => return None,
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(signal.clone());
            }
        }
        let transition = RecycleTransition {
            map: self.recycling.clone(),
            key: app_id.to_string(),
            signal,
        };
        if self.last_accessed_at(app_id) != Some(observed_access) {
            return None;
        }
        Some(transition)
    }

    async fn wait_for_recycle_transition(&self, app_id: &str) {
        loop {
            let Some(signal_ref) = self.recycling.get(app_id) else {
                return;
            };
            let signal = signal_ref.value().clone();
            drop(signal_ref);
            let notified = signal.notified();
            tokio::pin!(notified);
            // 先把 waiter 注册进 Notify，再复查 map。否则 transition 恰好在
            // `still_current` 与 `.await` 之间 drop 时，notify_waiters 可能丢失。
            notified.as_mut().enable();
            let still_current = self
                .recycling
                .get(app_id)
                .map(|current| Arc::ptr_eq(current.value(), &signal))
                .unwrap_or(false);
            if !still_current {
                continue;
            }
            notified.as_mut().await;
        }
    }

    /// 给 Running app 种入 last_accessed=now(rebuild_stopped_apps / 外部 start 用)
    pub fn seed_accessed(&self, app_id: &str) {
        self.last_accessed.insert(app_id.to_string(), Utc::now());
        self.note_dirty(app_id);
    }

    /// 返回上次访问时刻,供回收扫描器计算闲置时长;None=从未被访问(应视为 grace,不回收)。
    pub fn last_accessed_at(&self, app_id: &str) -> Option<DateTime<Utc>> {
        self.last_accessed.get(app_id).map(|r| *r)
    }

    /// 合并跨副本访问时间（多副本回收判定用）：仅当 `t` 比本进程内存新才覆盖，
    /// 返回合并后的有效值。不标脏（值来自 PG 影子行，无需回写）。
    /// 覆盖内存是必须的：`try_begin_recycle` 按"内存值 == 判定时观测值"做
    /// epoch 复核——不回写则 PG 较新时复核恒失败，app 永远无法回收。
    pub fn merge_accessed(&self, app_id: &str, t: DateTime<Utc>) -> DateTime<Utc> {
        match self.last_accessed.entry(app_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                let newest = (*entry.get()).max(t);
                entry.insert(newest);
                newest
            }
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(t);
                t
            }
        }
    }

    /// 标记 app 为 Running(唤醒成功 / start_app / 外部 start 后调用,清 stopped 态 + 刷新访问时间)。
    pub fn mark_running(&self, app_id: &str) {
        self.stopped.remove(app_id);
        self.wake_blocked.remove(app_id);
        self.last_accessed.insert(app_id.to_string(), Utc::now());
        self.note_dirty(app_id);
        // A successful create/start write does not prove K8s readiness yet.
        // Force the next proxy request to observe the actual replica count.
        self.remote_state.invalidate(app_id);
    }
}

impl AppActivityRegistry {
    fn record_local_access(&self, app_id: &str) {
        let now = Utc::now();
        // entry API:同一 shard 一次锁;Vacant 直接插,Occupied 仅超节流窗口才写
        match self.last_accessed.entry(app_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut e) => {
                let prev = *e.get();
                if now.signed_duration_since(prev)
                    >= chrono::Duration::from_std(self.throttle).unwrap_or_default()
                {
                    e.insert(now);
                    self.note_dirty(app_id);
                }
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                e.insert(now);
                self.note_dirty(app_id);
            }
        }
    }
}

#[async_trait::async_trait]
impl AppAccessTracker for AppActivityRegistry {
    async fn touch(&self, app_id: &str) -> Option<DateTime<Utc>> {
        let service = self.coordinator.get().and_then(std::sync::Weak::upgrade)?;
        // A cache hit performs no database I/O. Concurrent misses for the same
        // app/version share one query; errors are observable and not cached.
        let version = {
            let _identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
            self.access_identity_version(app_id)
        };
        if version == u64::MAX {
            return None;
        }
        let identity = match self
            .access_identities
            .try_get_with((app_id.to_owned(), version), async {
                tracing::debug!(%app_id, "Refreshing activity lifecycle identity");
                service
                    .metadata
                    .store
                    .get_application(app_id)
                    .await?
                    .filter(|row| row.state == shared_types::UserAppLifecycleState::Active)
                    .map(Arc::new)
                    .ok_or(shared_types::UserAppStoreError::NotFound)
            })
            .await
        {
            Ok(identity) => identity,
            Err(error) => {
                tracing::warn!(%app_id, %error, "Activity identity unavailable; access was not rebound");
                return None;
            }
        };
        self.record_verified_access(app_id, version, &identity)
    }
}

impl AppActivityRegistry {
    fn access_identity_version(&self, app_id: &str) -> u64 {
        self.identity_versions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(app_id)
            .copied()
            .unwrap_or(0)
    }

    // Caller holds identities, so changing the cache namespace and registration
    // is atomic relative to recording a touch. Retain tombstones to avoid ABA.
    fn invalidate_access_identity(&self, app_id: &str) {
        let mut versions = self
            .identity_versions
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let revision = versions.entry(app_id.to_owned()).or_default();
        *revision = revision.checked_add(1).unwrap_or(u64::MAX);
    }

    fn record_verified_access(
        &self,
        app_id: &str,
        version: u64,
        identity: &shared_types::UserAppLifecycleRecord,
    ) -> Option<DateTime<Utc>> {
        let mut identities = self.identities.lock().unwrap_or_else(|p| p.into_inner());
        if identity.app_id != app_id
            || identity.state != shared_types::UserAppLifecycleState::Active
            || self.access_identity_version(app_id) != version
            || version == u64::MAX
        {
            return None;
        }
        if !self.bind_lifecycle_locked(
            &mut identities,
            app_id,
            &identity.lifecycle_id,
            identity.lifecycle_epoch,
        ) {
            return None;
        }
        self.record_local_access(app_id);
        self.last_accessed_at(app_id)
    }
}
