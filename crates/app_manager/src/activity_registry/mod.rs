//! Userapp 活动状态注册表（核心状态域）。
//!
//! [`AppActivityRegistry`] 是「闲置自动回收 + 流量唤醒」特性的共享状态中心(in-memory,
//! rcoder 单实例)——本目录按域拆分：
//! - 本文件（mod.rs）：struct 定义 + mark 系列状态转移 + 访问追踪
//!   ([`AppAccessTracker`] touch,5s 节流) + 回收过渡协调([`RecycleTransition`]);
//! - [`wake`]:流量唤醒（[`shared_types::AppWakeControl`] trait impl——hold-and-wait
//!   拉起 + 并发合流 + 多副本 remote_stopped 兜底）;
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
/// 恢复跨重启的 last_accessed/stopped/wake_blocked（wake single-flight 等进程内
/// 协调机制不持久化）。`last_accessed` 用 wall-clock（`DateTime<Utc>`）而非
/// `Instant`（单调钟不可序列化）；节流与 epoch 复核语义不变。
pub struct AppActivityRegistry {
    /// app_id → 最近一次真实 HTTP 访问时刻(节流更新;wall-clock,可持久化)
    pub(super) last_accessed: DashMap<String, DateTime<Utc>>,
    /// app_id → 已 stopped(scale0)标记;stop/start/wake/重启重建 共同维护
    pub(super) stopped: DashSet<String>,
    /// app_id → 用户主动停止或发布切换中的应用；流量不得自动唤醒。
    pub(super) wake_blocked: DashSet<String>,
    /// app_id → 待持久化脏行(flusher 周期 collect_dirty 落库)
    pub(super) dirty: DashSet<String>,
    /// app_id → 待删除行(forget_app 后由 flusher 清 PG)
    pub(super) deleted: DashSet<String>,
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
            last_accessed: DashMap::new(),
            stopped: DashSet::new(),
            wake_blocked: DashSet::new(),
            dirty: DashSet::new(),
            deleted: DashSet::new(),
            persistence: OnceLock::new(),
            waking: Arc::new(DashMap::new()),
            recycling: Arc::new(DashMap::new()),
            runtime: OnceLock::new(),
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

    /// 标记 app 为 stopped(scale0)。AppService::stop_app / 回收扫描器调用。
    pub fn mark_stopped(&self, app_id: &str) {
        self.wake_blocked.remove(app_id);
        self.stopped.insert(app_id.to_string());
        self.note_dirty(app_id);
        self.remote_state
            .insert(app_id.to_string(), RemoteState::WAKEABLE_STOPPED);
    }

    /// 主动停止/发布切换：记录 scale0，但禁止流量自动拉起。
    pub fn mark_wake_blocked(&self, app_id: &str) {
        self.stopped.remove(app_id);
        self.wake_blocked.insert(app_id.to_string());
        self.note_dirty(app_id);
        self.remote_state
            .insert(app_id.to_string(), RemoteState::MANUAL_STOPPED);
    }

    /// 闲置回收完成后把停止状态转换为可由流量唤醒。
    pub fn mark_recycled(&self, app_id: &str) {
        self.wake_blocked.remove(app_id);
        self.stopped.insert(app_id.to_string());
        self.note_dirty(app_id);
        self.remote_state
            .insert(app_id.to_string(), RemoteState::WAKEABLE_STOPPED);
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
    pub fn forget_app(&self, app_id: &str) {
        self.last_accessed.remove(app_id);
        self.stopped.remove(app_id);
        self.dirty.remove(app_id);
        self.deleted.insert(app_id.to_string());
        self.wake_blocked.remove(app_id);
        self.remote_state.invalidate(app_id);
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.waking.entry(app_id.to_string())
        {
            let handle = entry.remove();
            if let Err(e) = handle
                .tx
                .send(Some(WakeOutcome::Failed("app was deleted".into())))
            {
                warn!("activity outcome send failed (no follower): {e}");
            }
        }
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.recycling.entry(app_id.to_string())
        {
            entry.remove().notify_waiters();
        }
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
        // guard 物化到独立语句（scrutinee 临时值存活到 match 结束——match 内
        // insert 会与持存的 read guard 抢同 shard 写锁，自死锁）
        let cur = self.last_accessed.get(app_id).map(|r| *r);
        match cur {
            Some(c) if c >= t => c,
            _ => {
                self.last_accessed.insert(app_id.to_string(), t);
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
        self.remote_state
            .insert(app_id.to_string(), RemoteState::default());
    }
}

impl AppAccessTracker for AppActivityRegistry {
    fn touch(&self, app_id: &str) {
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
