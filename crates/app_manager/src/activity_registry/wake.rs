//! 流量唤醒实现（从 activity_registry.rs 拆出；AppWakeControl trait impl +
//! wake single-flight 合流 + 多副本 remote_stopped 兜底）。
//!
//! - [`AppWakeControl::ensure_running`]：hold-and-wait 拉起（scale→1 + 轮询
//!   Ready ≤ wake_timeout），并发请求经 watch channel 合流为一次 scale-up
//!   （Leader/Follower + WakeGuard RAII 广播）；
//! - [`AppWakeControl::remote_stopped`]：多副本兜底——内存 stopped 表不知情
//!   其他副本的 stop 时查集群真实 replicas（moka TTL 缓存节流）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::Utc;
use dashmap::DashMap;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use container_runtime_api::UserAppRuntime;
use shared_types::{AppWakeControl, WakeOutcome};

use super::AppActivityRegistry;

/// wake 轮询 `get_deployment_status` 的间隔
const WAKE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// follower 等待 leader 的额外宽限(leader 的 WakeGuard drop 必先广播,follower 不应先超时)
const WAKE_FOLLOWER_GRACE: Duration = Duration::from_secs(10);
/// leader 异常退出(panic)时广播给 follower 的失败原因
const WAKE_LEADER_ABORTED: &str = "wake leader aborted";
/// 集群真实状态兜底缓存 TTL（`remote_stopped` 查询节流，过期自动重查）
pub(super) const REMOTE_STATE_TTL: Duration = Duration::from_secs(30);
/// 兜底缓存容量上限（防 app 海量时内存膨胀）
pub(super) const REMOTE_STATE_MAX_ENTRIES: u64 = 10_000;

/// `get_deployment_status` 的兜底判定快照（多副本 stopped 事实源 = 集群 replicas）。
#[derive(Debug, Clone, Copy, Default)]
pub(super) struct RemoteState {
    pub(super) stopped: bool,
    /// K8s `wake_on_traffic==Some(false)` 注解：手动停档（回填 `wake_blocked` 而非 `stopped`，
    /// 对齐 rebuild_stopped_apps 档位区分；Docker 形态恒 false）
    pub(super) manual_stop: bool,
}

impl RemoteState {
    /// 已停、可被流量唤醒（闲置回收/软停档）
    pub(super) const WAKEABLE_STOPPED: Self = Self {
        stopped: true,
        manual_stop: false,
    };
    /// 手动停止档（K8s wake-on-traffic 注解为 false）
    pub(super) const MANUAL_STOPPED: Self = Self {
        stopped: true,
        manual_stop: true,
    };
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
        if let Err(e) = self.handle.tx.send(Some(outcome)) {
            warn!("activity outcome send failed (no follower): {e}");
        }
        if let dashmap::mapref::entry::Entry::Occupied(entry) = self.map.entry(self.key.clone())
            && Arc::ptr_eq(entry.get(), &self.handle)
        {
            entry.remove();
        }
    }
}

impl AppActivityRegistry {
    /// 仅供流量唤醒成功路径使用（测试直调，pub(super)）。
    /// `preexisting_block`：唤醒启动前 app 已手动 stop（wake_blocked）——请求即
    /// 授权覆盖历史 stop，成功时一并解除阻断。唤醒**过程中**新到的手动 stop
    /// （在途 scale1 可能覆盖 stop_app 的 scale0）仍需尊重 → 返回 false，
    /// 由调用方补偿 scale0（时间后到者赢）。
    pub(super) fn try_mark_woken(&self, app_id: &str, preexisting_block: bool) -> bool {
        if !preexisting_block && self.wake_blocked.contains(app_id) {
            return false;
        }
        self.stopped.remove(app_id);
        self.wake_blocked.remove(app_id);
        self.last_accessed.insert(app_id.to_string(), Utc::now());
        self.note_dirty(app_id);
        self.remote_state
            .insert(app_id.to_string(), RemoteState::default());
        true
    }

    /// 集群快照为 stopped 时回填内存标记（幂等；manual_stop 档区分，对齐
    /// rebuild_stopped_apps 语义）。不 note_dirty：多副本 PG 行本就存在
    /// flush 互覆盖窗口，事实源已转集群；启动恢复由 rebuild_stopped_apps
    /// 从集群注解重建，无需依赖本回填落库。
    fn backfill_remote_state(&self, app_id: &str, state: RemoteState) -> bool {
        if !state.stopped {
            return false;
        }
        if state.manual_stop {
            self.stopped.remove(app_id);
            self.wake_blocked.insert(app_id.to_string());
        } else {
            self.wake_blocked.remove(app_id);
            self.stopped.insert(app_id.to_string());
        }
        debug!(
            "[ACTIVITY] remote stopped backfilled: app_id={app_id}, manual_stop={}",
            state.manual_stop
        );
        true
    }

    async fn keep_intentionally_stopped(
        runtime: &Arc<dyn UserAppRuntime>,
        app_id: &str,
    ) -> WakeOutcome {
        if let Err(error) = runtime.scale_deployment(app_id, 0).await {
            warn!(app_id, %error, "failed to restore scale0 after wake raced with stop");
        }
        WakeOutcome::Failed("app is intentionally stopped".into())
    }

    /// leader 实际执行唤醒:scale→1 + 轮询直到 Running/Error/超时
    async fn wake_leader(&self, app_id: &str) -> WakeOutcome {
        // 唤醒启动前已手动 stop（wake_blocked）：请求即授权覆盖历史 stop
        //（有请求即唤醒语义）；记录基线，唤醒**过程中**新到的 stop 才触发
        // 竞争补偿——时间后到者赢，并发 stop 语义不破。
        let preexisting_block = self.wake_blocked.contains(app_id);
        if !preexisting_block && !self.stopped.contains(app_id) {
            return WakeOutcome::AlreadyRunning;
        }
        let rt = match self.runtime.get() {
            Some(rt) => rt.clone(),
            None => {
                return WakeOutcome::Failed("runtime not initialized".into());
            }
        };
        // scale→1(幂等:已是 1 也无害)
        if let Err(e) = rt.scale_deployment(app_id, 1).await {
            return WakeOutcome::Failed(format!("scale_deployment: {e}"));
        }
        if !preexisting_block && self.wake_blocked.contains(app_id) {
            return Self::keep_intentionally_stopped(&rt, app_id).await;
        }
        // 轮询 get_deployment_status 直到 Running / Error / 超时
        let deadline = Instant::now() + self.wake_timeout;
        loop {
            if !preexisting_block && self.wake_blocked.contains(app_id) {
                return Self::keep_intentionally_stopped(&rt, app_id).await;
            }
            match rt.get_deployment_status(app_id).await {
                Ok(Some(s)) if s.phase == "Running" => {
                    // 唤醒成功不能覆盖**并发**手动 stop；若竞争失败，补偿 scale0。
                    if !self.try_mark_woken(app_id, preexisting_block) {
                        return Self::keep_intentionally_stopped(&rt, app_id).await;
                    }
                    debug!("[ACTIVITY] app {} woken (Ready)", app_id);
                    return WakeOutcome::Ready;
                }
                Ok(Some(s)) if s.phase == "Error" => {
                    return WakeOutcome::Failed(s.message.unwrap_or_else(|| "app error".into()));
                }
                _ => {}
            }
            if Instant::now() >= deadline {
                // 超时:app 仍在后台启动,保持 stopped 态,下次请求重新发起 wake(幂等 scale)
                warn!(
                    "[ACTIVITY] wake timeout for app {} (left starting in background)",
                    app_id
                );
                return WakeOutcome::Timeout;
            }
            sleep(WAKE_POLL_INTERVAL).await;
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

    /// 兜底判定：TTL 缓存命中零 IO；miss 查一次 `get_deployment_status`。
    /// 查到 stopped 回填内存标记（manual_stop 档对齐 rebuild_stopped_apps），
    /// 让本副本后续请求走内存快路，并使 `ensure_running` 的兜底分支自然
    /// 衔接到 wake_leader。查询失败（API 瞬断）不缓存，行为退化同旧（仅
    /// 内存视图）。
    async fn remote_stopped(&self, app_id: &str) -> bool {
        if let Some(state) = self.remote_state.get(app_id) {
            return self.backfill_remote_state(app_id, state);
        }
        let Some(rt) = self.runtime.get().cloned() else {
            return false;
        };
        let state = match rt.get_deployment_status(app_id).await {
            // replicas==0 即停（K8s derive_phase 恒 Stopped；Docker stop 后同为 Stopped）
            Ok(Some(s)) => RemoteState {
                stopped: s.replicas <= 0,
                manual_stop: s.wake_on_traffic == Some(false),
            },
            // app 不存在：非 stopped 负缓存（防幻报 app 每请求白查）
            Ok(None) => RemoteState::default(),
            Err(e) => {
                warn!(
                    "[ACTIVITY] remote state probe failed (fallback to memory view): app_id={app_id}: {e}"
                );
                return false;
            }
        };
        self.remote_state.insert(app_id.to_string(), state);
        self.backfill_remote_state(app_id, state)
    }

    async fn ensure_running(&self, app_id: &str) -> WakeOutcome {
        // 回收过渡期的请求必须等 scale0 完成，再由唤醒 single-flight scale1。
        self.wait_for_recycle_transition(app_id).await;
        // 有请求即唤醒（2026-08 拍板）：手动 stop（wake_blocked）不再拒绝——
        // 请求本身就是把 app 拉起来的授权；唤醒过程中新到的 stop 由
        // wake_leader 的竞争保护尊重（时间后到者赢）。
        if !self.stopped.contains(app_id) && !self.wake_blocked.contains(app_id) {
            // 多副本兜底：内存无记录不代表集群在跑（其他副本 stop 后本副本
            // 不知情；本副本重启后未覆盖）。查集群真实 replicas（TTL 缓存
            // 节流），查到 stopped 会回填内存标记，继续走下方唤醒流程。
            if !self.remote_stopped(app_id).await {
                return WakeOutcome::AlreadyRunning;
            }
        }
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
