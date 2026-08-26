//! UserApp 活动状态注册表 + 访问追踪/流量唤醒实现
//!
//! [`AppActivityRegistry`] 是「闲置自动回收 + 流量唤醒」特性的共享状态中心(in-memory,
//! rcoder 单实例):
//! - [`AppAccessTracker`](shared_types::AppAccessTracker):Pingora 热路径 `touch` 记录最近访问(5s 节流);
//! - [`AppWakeControl`](shared_types::AppWakeControl):stopped app 收到请求时 hold-and-wait 拉起,
//!   并发请求经 `tokio::sync::watch` 合流为一次 scale-up。
//!
//! 构造顺序:rcoder 启动早期(init_proxy 之前)独立构造为 `Arc`,注入 Pingora(访问/唤醒);
//! runtime 构建后(RuntimeManager::get)经 [`AppActivityRegistry::set_runtime`] 注入(OnceLock 延迟)。
//! wake 只在 `is_stopped` 真时触发,而 `stopped` 表要到 `AppService::new` 才填充——此时 OnceLock 早已 set。

use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use dashmap::{DashMap, DashSet};
use tokio::sync::{Notify, watch};
use tokio::time::{sleep, timeout};
use tracing::{debug, warn};

use container_runtime_api::UserAppRuntime;
use shared_types::{AppAccessTracker, AppWakeControl, WakeOutcome};

/// `touch` 节流粒度:同一 app 在此窗口内多次访问只写一次 `last_accessed`(降低 DashMap 锁竞争)
const TOUCH_THROTTLE: Duration = Duration::from_secs(5);
/// wake 轮询 `get_deployment_status` 的间隔
const WAKE_POLL_INTERVAL: Duration = Duration::from_secs(1);
/// follower 等待 leader 的额外宽限(leader 的 WakeGuard drop 必先广播,follower 不应先超时)
const WAKE_FOLLOWER_GRACE: Duration = Duration::from_secs(10);
/// leader 异常退出(panic)时广播给 follower 的失败原因
const WAKE_LEADER_ABORTED: &str = "wake leader aborted";

/// 进行中的唤醒句柄(leader 持有 `tx`,follower `subscribe` 后等结果)
struct WakeHandle {
    tx: watch::Sender<Option<WakeOutcome>>,
}

/// RAII 守卫:leader 路径持有。drop 时(含 panic unwind)做两件事:
/// 1. 向 follower 广播 outcome(leader 正常 → wake_leader 的结果;panic 未写入 → `Failed` 快速通知,
///    避免 follower 干等 dead-man 超时);
/// 2. 从 `waking` 移除条目(防泄漏)。
struct WakeGuard {
    map: Arc<DashMap<String, Arc<WakeHandle>>>,
    key: String,
    handle: Arc<WakeHandle>,
    /// leader 完成后写入 outcome；panic 时仍为 None → drop 发 `Failed`。
    outcome: Option<WakeOutcome>,
}

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

/// UserApp 活动状态注册表(in-memory,rcoder 单实例共享)
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
    /// 字段 `pub(super)`：供 `activity_persistence_ops.rs`（同类型 extension-impl 拆分）访问。
    pub(super) persistence: OnceLock<Arc<dyn shared_types::ActivityPersistence>>,
    /// app_id → 进行中的唤醒句目(并发合流)
    waking: Arc<DashMap<String, Arc<WakeHandle>>>,
    /// app_id → 正在执行的闲置回收过渡。
    recycling: Arc<DashMap<String, Arc<Notify>>>,
    /// runtime 延迟注入(wake 需要 scale + 查 status;启动早期拿不到,故 OnceLock)
    runtime: OnceLock<Arc<dyn UserAppRuntime>>,
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
    }

    /// 主动停止/发布切换：记录 scale0，但禁止流量自动拉起。
    pub fn mark_wake_blocked(&self, app_id: &str) {
        self.stopped.remove(app_id);
        self.wake_blocked.insert(app_id.to_string());
        self.note_dirty(app_id);
    }

    /// 闲置回收完成后把停止状态转换为可由流量唤醒。
    pub fn mark_recycled(&self, app_id: &str) {
        self.wake_blocked.remove(app_id);
        self.stopped.insert(app_id.to_string());
        self.note_dirty(app_id);
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

    /// 标记 app 为 Running(唤醒成功 / start_app / 外部 start 后调用,清 stopped 态 + 刷新访问时间)。
    pub fn mark_running(&self, app_id: &str) {
        self.stopped.remove(app_id);
        self.wake_blocked.remove(app_id);
        self.last_accessed.insert(app_id.to_string(), Utc::now());
        self.note_dirty(app_id);
    }

    /// 仅供流量唤醒成功路径使用。
    /// `preexisting_block`：唤醒启动前 app 已手动 stop（wake_blocked）——请求即
    /// 授权覆盖历史 stop，成功时一并解除阻断。唤醒**过程中**新到的手动 stop
    /// （在途 scale1 可能覆盖 stop_app 的 scale0）仍需尊重 → 返回 false，
    /// 由调用方补偿 scale0（时间后到者赢）。
    fn try_mark_woken(&self, app_id: &str, preexisting_block: bool) -> bool {
        if !preexisting_block && self.wake_blocked.contains(app_id) {
            return false;
        }
        self.stopped.remove(app_id);
        self.wake_blocked.remove(app_id);
        self.last_accessed.insert(app_id.to_string(), Utc::now());
        self.note_dirty(app_id);
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

#[async_trait::async_trait]
impl AppWakeControl for AppActivityRegistry {
    fn is_stopped(&self, app_id: &str) -> bool {
        self.stopped.contains(app_id)
            || self.wake_blocked.contains(app_id)
            || self.recycling.contains_key(app_id)
    }

    async fn ensure_running(&self, app_id: &str) -> WakeOutcome {
        // 回收过渡期的请求必须等 scale0 完成，再由唤醒 single-flight scale1。
        self.wait_for_recycle_transition(app_id).await;
        // 有请求即唤醒（2026-08 拍板）：手动 stop（wake_blocked）不再拒绝——
        // 请求本身就是把 app 拉起来的授权；唤醒过程中新到的 stop 由
        // wake_leader 的竞争保护尊重（时间后到者赢）。
        if !self.stopped.contains(app_id) && !self.wake_blocked.contains(app_id) {
            return WakeOutcome::AlreadyRunning;
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

#[cfg(test)]
mod tests {
    use super::*;
    use Arc;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicU32, Ordering};

    use container_runtime_api::{
        ContainerRuntimeResult, DeploymentStatus, UserAppDeploymentRuntime, WorkspaceRuntime,
    };

    /// 构造一个 DeploymentStatus(仅填测试关心字段)
    fn mk_status(app_id: &str, phase: &str) -> DeploymentStatus {
        DeploymentStatus {
            app_id: app_id.to_string(),
            replicas: 1,
            ready_replicas: if phase == "Running" { 1 } else { 0 },
            phase: phase.to_string(),
            ..Default::default()
        }
    }

    /// mock runtime:计数 scale 调用,可配置 status 相位与 scale 行为(panic/err)
    struct MockRuntime {
        scale_calls: Arc<AtomicU32>,
        // 返回的相位;首次 scale 后切到 running_after_scale
        running_after_scale: bool,
        // >0 时第 N 次(1-based)scale panic;0=永不 panic
        panic_on_nth: AtomicU32,
        // 互斥保护相位切换
        phase: StdMutex<String>,
    }

    impl MockRuntime {
        fn new(running_after_scale: bool) -> Self {
            Self {
                scale_calls: Arc::new(AtomicU32::new(0)),
                running_after_scale,
                panic_on_nth: AtomicU32::new(0),
                phase: StdMutex::new("Starting".to_string()),
            }
        }
    }

    #[async_trait::async_trait]
    impl WorkspaceRuntime for MockRuntime {}
    #[async_trait::async_trait]
    impl UserAppDeploymentRuntime for MockRuntime {
        async fn scale_deployment(
            &self,
            _app_id: &str,
            _replicas: i32,
        ) -> ContainerRuntimeResult<()> {
            let n = self.scale_calls.fetch_add(1, Ordering::SeqCst) + 1;
            let panic_nth = self.panic_on_nth.load(Ordering::SeqCst);
            if panic_nth > 0 && n == panic_nth {
                panic!("mock scale panic #{}", n);
            }
            if self.running_after_scale {
                *self.phase.lock().unwrap() = "Running".to_string();
            }
            Ok(())
        }
        async fn get_deployment_status(
            &self,
            app_id: &str,
        ) -> ContainerRuntimeResult<Option<DeploymentStatus>> {
            let phase = self.phase.lock().unwrap().clone();
            Ok(Some(mk_status(app_id, &phase)))
        }
    }

    #[tokio::test]
    async fn touch_throttle_collapses_writes_within_window() {
        // 窗口取 500ms（循环名义 50ms 留 10 倍余量）：此前 100ms 窗口下负载漂移
        // 可使循环实际超窗（实测 ~110ms），第一段断言高频 flaky
        let reg = AppActivityRegistry::new_with(Duration::from_secs(1), Duration::from_millis(500));
        reg.touch("app-a");
        let t0 = reg.last_accessed_at("app-a").expect("first touch recorded");

        // 窗口内多次 touch 不更新
        for _ in 0..10 {
            reg.touch("app-a");
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(reg.last_accessed_at("app-a"), Some(t0), "throttled");

        // 超过窗口后更新
        sleep(Duration::from_millis(600)).await;
        reg.touch("app-a");
        assert!(
            reg.last_accessed_at("app-a").unwrap() > t0,
            "updated after window"
        );
    }

    #[tokio::test]
    async fn wake_concurrent_dedup_scales_once() {
        let rt = Arc::new(MockRuntime::new(true)); // scale 后转 Running
        let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
        reg.set_runtime(rt.clone());
        reg.mark_stopped("app-x");

        // 5 并发唤醒:无论时序如何都只 scale 一次——
        // leader 拉起后 mark_running 清 stopped,后到者走 leader 路径时 is_stopped=false → AlreadyRunning(不再 scale);
        // 或 join 到同一 leader 的 follower 经 channel 拿到 Ready。两种路径 scale 都只发生一次。
        let reg = Arc::new(reg);
        let mut handles = vec![];
        for _ in 0..5 {
            let r = reg.clone();
            handles.push(tokio::spawn(async move { r.ensure_running("app-x").await }));
        }
        let outcomes: Vec<WakeOutcome> = futures_util::future::join_all(handles)
            .await
            .into_iter()
            .map(|r| r.unwrap())
            .collect();

        assert!(
            outcomes
                .iter()
                .all(|o| matches!(o, WakeOutcome::Ready | WakeOutcome::AlreadyRunning)),
            "outcomes: {:?}",
            outcomes
        );
        assert_eq!(
            rt.scale_calls.load(Ordering::SeqCst),
            1,
            "scale called exactly once"
        );
    }

    #[tokio::test]
    async fn wake_timeout_when_never_ready() {
        let rt = Arc::new(MockRuntime::new(false)); // scale 后仍 Starting,永不 Running
        let reg =
            AppActivityRegistry::new_with(Duration::from_millis(300), Duration::from_millis(50));
        reg.set_runtime(rt.clone());
        reg.mark_stopped("app-t");

        let outcome = reg.ensure_running("app-t").await;
        assert_eq!(outcome, WakeOutcome::Timeout);
        // 超时后保持 stopped(下次请求重新唤醒)
        assert!(reg.is_stopped("app-t"));
    }

    #[tokio::test]
    async fn manually_stopped_app_is_woken_by_traffic() {
        // 有请求即唤醒（2026-08 拍板）：手动 stop（wake_blocked）不再拒绝——
        // 请求即授权拉起；成功后阻断标志一并解除（is_stopped 归 false）
        let reg =
            AppActivityRegistry::new_with(Duration::from_millis(50), Duration::from_millis(1));
        let runtime = Arc::new(MockRuntime::new(true));
        let scale_calls = runtime.scale_calls.clone();
        reg.set_runtime(runtime);
        reg.mark_wake_blocked("app-manual");

        let outcome = reg.ensure_running("app-manual").await;

        assert!(matches!(outcome, WakeOutcome::Ready), "got {outcome:?}");
        assert_eq!(scale_calls.load(Ordering::SeqCst), 1);
        assert!(!reg.is_wake_blocked("app-manual"));
        assert!(!reg.is_stopped("app-manual"));
    }

    #[tokio::test]
    async fn recycle_transition_waits_for_stop_then_wakes_once() {
        let runtime = Arc::new(MockRuntime::new(true));
        let registry = Arc::new(AppActivityRegistry::new_with(
            Duration::from_secs(2),
            Duration::from_millis(10),
        ));
        registry.set_runtime(runtime.clone());
        registry.seed_accessed("app-r");
        let observed = registry
            .last_accessed_at("app-r")
            .expect("seeded access timestamp");
        let transition = registry
            .try_begin_recycle("app-r", observed)
            .expect("unchanged app may enter recycle transition");

        let wake = tokio::spawn({
            let registry = registry.clone();
            async move { registry.ensure_running("app-r").await }
        });
        tokio::task::yield_now().await;
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);

        registry.mark_stopped("app-r");
        drop(transition);
        let outcome = wake.await.expect("wake task");
        assert_eq!(outcome, WakeOutcome::Ready);
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn recycle_transition_rejects_stale_access_observation() {
        let registry =
            AppActivityRegistry::new_with(Duration::from_secs(2), Duration::from_millis(0));
        registry.seed_accessed("app-r");
        let observed = registry
            .last_accessed_at("app-r")
            .expect("seeded access timestamp");
        sleep(Duration::from_millis(1)).await;
        registry.touch("app-r");

        assert!(registry.try_begin_recycle("app-r", observed).is_none());
        assert!(!registry.is_stopped("app-r"));
    }

    #[tokio::test]
    async fn wake_leader_panic_cleans_entry_and_follower_recovers() {
        // 首次 scale panic,第二次成功
        let rt = Arc::new(MockRuntime::new(true));
        rt.panic_on_nth.store(1, Ordering::SeqCst); // panic_on_nth 同模块可访问
        let reg = Arc::new(AppActivityRegistry::new_with(
            Duration::from_secs(2),
            Duration::from_millis(50),
        ));
        reg.set_runtime(rt.clone());
        reg.mark_stopped("app-p");

        // 第一次唤醒(leader panic)
        let jh = tokio::spawn({
            let r = reg.clone();
            async move { r.ensure_running("app-p").await }
        });
        drop(jh.await); // task panic → JoinError,吞掉

        // 守卫应已移除 waking 条目(未泄漏)
        assert!(
            !reg.is_waking("app-p"),
            "waking entry must be cleaned after leader panic"
        );

        // 第二次唤醒:新 leader,scale 成功 → Ready
        let outcome = reg.ensure_running("app-p").await;
        assert!(
            matches!(outcome, WakeOutcome::Ready | WakeOutcome::AlreadyRunning),
            "second wake should succeed: {:?}",
            outcome
        );
        assert!(!reg.is_stopped("app-p"));
    }

    /// 验证 Fix3:leader 中途 panic(result 未写入)时,`WakeGuard` drop 必须广播 `Failed`,
    /// 让已 subscribe 的 follower 立即收到(而非干等 dead-man 超时)。
    #[tokio::test]
    async fn guard_broadcasts_failed_on_drop_when_leader_did_not_finish() {
        let map: Arc<DashMap<String, Arc<WakeHandle>>> = Arc::new(DashMap::new());
        let (tx, _) = watch::channel(None::<WakeOutcome>);
        let handle = Arc::new(WakeHandle { tx });
        map.insert("app-g".to_string(), handle.clone());

        // follower 先 subscribe(模拟并发请求 join 到 leader)
        let rx = handle.tx.subscribe();
        assert!(
            !rx.has_changed().unwrap_or(false),
            "leader 尚未完成,channel 仍为初始 None"
        );

        // leader 中途 panic:result 仍为 None,guard drop
        {
            let _guard = WakeGuard {
                map: map.clone(),
                key: "app-g".to_string(),
                handle: handle.clone(),
                outcome: None,
            };
            // _guard 在此 drop(模拟 leader task 终止):result=None → 广播 Failed + 移除条目
        }

        // follower 应立即收到 Failed(不等待 wake_timeout)
        assert!(
            rx.has_changed().unwrap_or(false),
            "follower must be notified on guard drop"
        );
        match (*rx.borrow()).clone() {
            Some(WakeOutcome::Failed(_)) => {}
            other => panic!("expected Failed, got {:?}", other),
        }
        assert!(
            !map.contains_key("app-g"),
            "entry must be removed on guard drop"
        );
    }

    #[test]
    fn wake_completion_respects_concurrent_but_not_preexisting_stop() {
        let registry = AppActivityRegistry::new(Duration::from_secs(2));
        // 唤醒过程中新到的手动 stop（preexisting_block=false）：唤醒不得覆盖 → false
        registry.mark_stopped("app-stop-race");
        registry.mark_wake_blocked("app-stop-race");
        assert!(!registry.try_mark_woken("app-stop-race", false));
        assert!(registry.is_wake_blocked("app-stop-race"));
        assert!(registry.is_stopped("app-stop-race"));

        // 唤醒启动前已手动 stop（preexisting_block=true）：请求即授权 → true 并清两表
        let registry2 = AppActivityRegistry::new(Duration::from_secs(2));
        registry2.mark_wake_blocked("app-stop-old");
        assert!(registry2.try_mark_woken("app-stop-old", true));
        assert!(!registry2.is_wake_blocked("app-stop-old"));
        assert!(!registry2.is_stopped("app-stop-old"));
    }

    #[test]
    fn forget_app_clears_deleted_app_state() {
        let registry = AppActivityRegistry::new(Duration::from_secs(2));
        registry.seed_accessed("app-deleted");
        registry.mark_wake_blocked("app-deleted");

        registry.forget_app("app-deleted");

        assert!(!registry.is_stopped("app-deleted"));
        assert!(!registry.is_wake_blocked("app-deleted"));
        assert_eq!(registry.last_accessed_at("app-deleted"), None);
    }
}
