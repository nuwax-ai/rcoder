//! 流量唤醒实现（从 activity_registry.rs 拆出；AppWakeControl trait impl +
//! wake single-flight 合流 + 多副本 remote_stopped 兜底）。
//!
//! - [`AppWakeControl::ensure_running`]：hold-and-wait 拉起（scale→1 + 轮询
//!   Ready ≤ wake_timeout），并发请求经 watch channel 合流为一次 scale-up
//!   （Leader/Follower + WakeGuard RAII 广播）；
//! - [`AppWakeControl::remote_stopped`]：多副本兜底——内存 stopped 表不知情
//!   其他副本的 stop 时查集群真实 replicas（moka TTL 缓存节流）。

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use dashmap::DashMap;
use tokio::sync::watch;
use tokio::time::timeout;
use tracing::{debug, warn};

use shared_types::{AppWakeControl, WakeOutcome};

use super::AppActivityRegistry;

/// follower 等待 leader 的额外宽限(leader 的 WakeGuard drop 必先广播,follower 不应先超时)
const WAKE_FOLLOWER_GRACE: Duration = Duration::from_secs(10);
/// leader 异常退出(panic)时广播给 follower 的失败原因
const WAKE_LEADER_ABORTED: &str = "wake leader aborted";
/// 集群真实状态兜底缓存 TTL（`remote_stopped` 查询节流，过期自动重查）
pub(super) const REMOTE_STATE_TTL: Duration = Duration::from_secs(30);
/// 兜底缓存容量上限（防 app 海量时内存膨胀）
pub(super) const REMOTE_STATE_MAX_ENTRIES: u64 = 10_000;

/// `get_deployment_status` 的兜底判定快照（多副本 stopped 事实源 = 集群 replicas）。
/// 拍板 2026-09-23：手动 stop 与闲置回收统一——stopped 一律可被流量唤醒，
/// wake-on-traffic 注解不再参与档位区分。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RemoteState {
    pub(super) stopped: bool,
}

impl RemoteState {
    /// 已停、可被流量唤醒（含手动 stop 与闲置回收）
    pub(super) const STOPPED: Self = Self { stopped: true };
}

/// 进行中的唤醒句柄(leader 持有 `tx`,follower `subscribe` 后等结果)
pub(super) struct WakeHandle {
    pub(super) tx: watch::Sender<Option<WakeOutcome>>,
}

/// RAII 守卫:leader 路径持有。drop 时(含 panic unwind)做两件事:
/// 1. 向 follower 广播 outcome(leader 正常 → wake_leader 的结果;panic 未写入 → `Failed` 快速通知,
///    避免 follower 干等 dead-man 超时);
/// 2. 从 `waking` 移除条目(防泄漏)。
pub(super) struct WakeGuard {
    pub(super) map: Arc<DashMap<String, Arc<WakeHandle>>>,
    pub(super) key: String,
    pub(super) handle: Arc<WakeHandle>,
    /// leader 完成后写入 outcome；panic 时仍为 None → drop 发 `Failed`。
    pub(super) outcome: Option<WakeOutcome>,
}

impl Drop for WakeGuard {
    fn drop(&mut self) {
        let outcome = self
            .outcome
            .take()
            .unwrap_or_else(|| WakeOutcome::Failed(WAKE_LEADER_ABORTED.into()));
        // A follower may have cloned the handle without subscribing yet.
        // Retain the terminal result even when there are currently no receivers.
        self.handle.tx.send_replace(Some(outcome));
        if let dashmap::mapref::entry::Entry::Occupied(entry) = self.map.entry(self.key.clone())
            && Arc::ptr_eq(entry.get(), &self.handle)
        {
            entry.remove();
        }
    }
}

impl AppActivityRegistry {
    async fn probe_remote_state(&self, app_id: &str) -> Result<RemoteState, String> {
        if let Some(state) = self.remote_state.get(app_id) {
            return Ok(state);
        }
        let runtime = self
            .runtime
            .get()
            .ok_or_else(|| "Runtime is unavailable".to_owned())?;
        let status = runtime
            .get_deployment_status(app_id)
            .await
            .map_err(|error| format!("Read application runtime state: {error}"))?
            .ok_or_else(|| format!("Application runtime not found: {app_id}"))?;
        let state = RemoteState {
            stopped: status.replicas <= 0,
        };
        self.remote_state.insert(app_id.to_string(), state);
        Ok(state)
    }

    /// Called under the admitted lifecycle operation lock after checking the
    /// authoritative wake policy. Starting replicas are not readiness evidence:
    /// keep traffic routed through the coordinator until completion is confirmed.
    /// Only this pre-mutation boundary may clear an older stale local flag
    /// (e.g. a deletion fence resolved by reconciliation); late failure paths
    /// must not overwrite a newer control's local state.
    pub(crate) fn prepare_traffic_wake(&self, app_id: &str) {
        self.wake_blocked.remove(app_id);
        self.stopped.insert(app_id.to_string());
        self.remote_state.invalidate(app_id);
        self.note_dirty(app_id);
    }

    /// Traffic wake never clears a deletion fence (or a stop landing during
    /// completion). Manual stops no longer set `wake_blocked` (unified wake,
    /// 2026-09-23), so this guard only fences deletion and mid-wake stops.
    pub(crate) fn try_mark_woken(&self, app_id: &str) -> bool {
        if self.wake_blocked.contains(app_id) {
            return false;
        }
        self.stopped.remove(app_id);
        self.last_accessed.insert(app_id.to_string(), Utc::now());
        self.note_dirty(app_id);
        self.remote_state
            .insert(app_id.to_string(), RemoteState::default());
        true
    }

    /// 集群快照为 stopped 时回填内存标记（幂等）。拍板 2026-09-23：手动
    /// stop 与闲置回收统一——stopped 一律可被流量唤醒，不再区分注解档位。
    /// 不 note_dirty：多副本 PG 行本就存在 flush 互覆盖窗口，事实源已转
    /// 集群；启动恢复由 rebuild_stopped_apps 从集群重建，无需依赖本回填落库。
    fn backfill_remote_state(&self, app_id: &str, state: RemoteState) -> bool {
        if !state.stopped {
            return false;
        }
        self.wake_blocked.remove(app_id);
        self.stopped.insert(app_id.to_string());
        debug!("[ACTIVITY] remote stopped backfilled: app_id={app_id}");
        true
    }

    /// The registry only merges callers. Durable admission, runtime identity and
    /// wake policy are checked by the lifecycle coordinator under its operation lock.
    async fn wake_leader(&self, app_id: &str) -> WakeOutcome {
        let Some(service) = self.coordinator.get().and_then(std::sync::Weak::upgrade) else {
            return WakeOutcome::Failed("Activity lifecycle coordinator is unavailable".into());
        };
        match service.wake_app_on_traffic(app_id, self.wake_timeout).await {
            Ok(outcome) => outcome,
            Err(crate::models::AppOperationError::ConflictBlocked { message, blocker }) => {
                WakeOutcome::Blocked { message, blocker }
            }
            Err(error) => WakeOutcome::Failed(error.to_string()),
        }
    }

    /// Leader 路径:result cell + WakeGuard 保证退出时(含 panic)必广播 outcome 并移除 waking 条目。
    async fn become_leader(&self, app_id: &str, handle: Arc<WakeHandle>) -> WakeOutcome {
        let mut guard = WakeGuard {
            map: self.waking.clone(),
            key: app_id.to_string(),
            handle: handle.clone(),
            outcome: None,
        };
        let r = self.wake_leader(app_id).await;
        // 写入 outcome；guard 在函数返回/panic unwind 时 drop → 广播给 follower + 移除 waking 条目。
        guard.outcome = Some(r.clone());
        r
    }

    /// Follower 路径:subscribe + 等 leader 广播(WakeGuard drop 必 send 一次)。
    async fn join_as_follower(&self, handle: Arc<WakeHandle>) -> WakeOutcome {
        let mut rx = handle.tx.subscribe();
        // leader 可能已 finished(borrow 拿到 Some)
        if let Some(outcome) = rx.borrow().clone() {
            return outcome;
        }
        match timeout(self.wake_timeout + WAKE_FOLLOWER_GRACE, rx.changed()).await {
            Ok(Ok(())) => rx
                .borrow()
                .clone()
                .unwrap_or(WakeOutcome::Failed("no outcome".into())),
            Ok(Err(_)) => WakeOutcome::Failed(WAKE_LEADER_ABORTED.into()),
            Err(_) => WakeOutcome::Failed("wake join timeout".into()),
        }
    }
}

#[async_trait::async_trait]
impl AppWakeControl for AppActivityRegistry {
    fn is_stopped(&self, app_id: &str) -> bool {
        self.stopped.contains(app_id)
            || self.wake_blocked.contains(app_id)
            || self.recycling.contains_key(app_id)
    }

    /// The advisory proxy trait only carries a boolean. Errors are not cached;
    /// the control entry below uses the fallible probe and never reports success
    /// from a failed runtime query.
    async fn remote_stopped(&self, app_id: &str) -> bool {
        match self.probe_remote_state(app_id).await {
            Ok(state) => self.backfill_remote_state(app_id, state),
            Err(error) => {
                warn!(%app_id, %error, "Remote application state probe failed");
                false
            }
        }
    }

    async fn ensure_running(&self, app_id: &str) -> WakeOutcome {
        self.ensure_running_inner(app_id).await
    }
}

impl AppActivityRegistry {
    /// 单一入口。拍板 2026-09-23：被动流量与显式动作（pod/ensure）语义统一
    /// ——有请求即唤醒，不再有手动停档区分。
    async fn ensure_running_inner(&self, app_id: &str) -> WakeOutcome {
        // 回收过渡期的请求必须等 scale0 完成,再由唤醒 single-flight scale1。
        self.wait_for_recycle_transition(app_id).await;
        // Local stop flags may outlive an operation completed by another replica.
        // The coordinator validates durable state and live policy before mutation.
        if !self.stopped.contains(app_id) && !self.wake_blocked.contains(app_id) {
            // 多副本兜底：内存无记录不代表集群在跑（其他副本 stop 后本副本
            // 不知情；本副本重启后未覆盖）。查集群真实 replicas（TTL 缓存
            // 节流），查到 stopped 会回填内存标记，继续走下方唤醒流程。
            match self.probe_remote_state(app_id).await {
                Ok(state) => {
                    self.backfill_remote_state(app_id, state);
                }
                Err(error) => return WakeOutcome::Failed(error),
            }
        }
        // A cached replica count is only a routing hint. Even a running workload
        // must pass durable lifecycle admission and physical identity validation
        // before this control entry point reports success.
        // 只在同步作用域内持有 DashMap entry guard，禁止 shard 锁跨越 await。
        let role = match self.waking.entry(app_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(e) => WakeRole::Follower(e.get().clone()),
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let (tx, _rx) = watch::channel(None::<WakeOutcome>);
                let handle = Arc::new(WakeHandle { tx });
                e.insert(handle.clone());
                WakeRole::Leader(handle)
            }
        };
        match role {
            WakeRole::Follower(handle) => self.join_as_follower(handle).await,
            WakeRole::Leader(handle) => self.become_leader(app_id, handle).await,
        }
    }
}

enum WakeRole {
    Leader(Arc<WakeHandle>),
    Follower(Arc<WakeHandle>),
}
