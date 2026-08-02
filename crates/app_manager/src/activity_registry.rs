//! UserApp 活动状态注册表 + 访问追踪/流量唤醒实现
//!
//! [`AppActivityRegistry`] 是「闲置自动回收 + 流量唤醒」特性的共享状态中心(in-memory,
//! rcoder 单实例):
//! - [`AppAccessTracker`](shared_types::AppAccessTracker):Pingora 热路径 `touch` 记录最近访问(5s 节流);
//! - [`AppWakeControl`](shared_types::AppWakeControl):stopped app 收到请求时 hold-and-wait 拉起,
//!   并发请求经 `tokio::sync::watch` 合流为一次 scale-up。
//!
//! 构造顺序:rcoder 启动早期(main.rs ~:80)独立构造为 `Arc`,注入 Pingora(访问/唤醒);
//! runtime 在 main.rs:122 构建后经 [`AppActivityRegistry::set_runtime`] 注入(OnceLock 延迟)。
//! wake 只在 `is_stopped` 真时触发,而 `stopped` 表要到 `AppService::new` 才填充——此时 OnceLock 早已 set。

use std::sync::OnceLock;
use std::time::{Duration, Instant};

use ::tokio::sync::watch;
use ::tokio::time::sleep;
use dashmap::DashMap;
use tracing::{debug, warn};

use container_runtime_api::UserAppRuntime;
use shared_types::{AppAccessTracker, AppWakeControl, WakeOutcome};

/// `touch` 节流粒度:同一 app 在此窗口内多次访问只写一次 `last_accessed`(降低 DashMap 锁竞争)
const TOUCH_THROTTLE: Duration = Duration::from_secs(5);
/// wake 轮询 `get_deployment_status` 的间隔
const WAKE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// 进行中的唤醒句柄(leader 持有 `tx`,follower `subscribe` 后等结果)
struct WakeHandle {
    tx: watch::Sender<Option<WakeOutcome>>,
}

/// RAII 守卫:leader 路径持有。drop 时(含 panic unwind)做两件事:
/// 1. 向 follower 广播 outcome(leader 正常 → wake_leader 的结果;panic 未写入 → `Failed` 快速通知,
///    避免 follower 干等 dead-man 超时);
/// 2. 从 `waking` 移除条目(防泄漏)。
struct WakeGuard {
    map: std::sync::Arc<DashMap<String, std::sync::Arc<WakeHandle>>>,
    key: String,
    handle: std::sync::Arc<WakeHandle>,
    /// leader 完成后写入 outcome;panic 时仍为 None → drop 发 `Failed`。
    result: std::sync::Arc<std::sync::Mutex<Option<WakeOutcome>>>,
}

impl Drop for WakeGuard {
    fn drop(&mut self) {
        // unwrap_or_else(into_inner):即使 result 锁被毒化(leader 持锁时 panic,极罕见)也能取值,不二次 panic
        let outcome = self
            .result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| WakeOutcome::Failed("wake leader aborted".into()));
        let _ = self.handle.tx.send(Some(outcome));
        self.map.remove(&self.key);
    }
}

/// UserApp 活动状态注册表(in-memory,rcoder 单实例共享)
pub struct AppActivityRegistry {
    /// app_id → 最近一次真实 HTTP 访问时刻(节流更新)
    last_accessed: DashMap<String, Instant>,
    /// app_id → 已 stopped(scale0)标记;stop/start/wake/重启重建 共同维护
    stopped: DashMap<String, ()>,
    /// app_id → 进行中的唤醒句目(并发合流)
    waking: std::sync::Arc<DashMap<String, std::sync::Arc<WakeHandle>>>,
    /// runtime 延迟注入(wake 需要 scale + 查 status;启动早期拿不到,故 OnceLock)
    runtime: OnceLock<std::sync::Arc<dyn UserAppRuntime>>,
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
            stopped: DashMap::new(),
            waking: std::sync::Arc::new(DashMap::new()),
            runtime: OnceLock::new(),
            wake_timeout,
            throttle,
        }
    }

    /// 注入 runtime(幂等;重复 set 告警不覆盖)。main.rs 在 runtime 构建后调用。
    pub fn set_runtime(&self, rt: std::sync::Arc<dyn UserAppRuntime>) {
        if self.runtime.set(rt).is_err() {
            warn!("[ACTIVITY] set_runtime called twice; keeping existing runtime");
        }
    }

    /// 标记 app 为 stopped(scale0)。AppService::stop_app / 回收扫描器调用。
    pub fn mark_stopped(&self, app_id: &str) {
        self.stopped.insert(app_id.to_string(), ());
    }

    /// 是否有进行中的唤醒(回收扫描器据此跳过,避免与 in-flight wake 竞态)
    pub fn is_waking(&self, app_id: &str) -> bool {
        self.waking.contains_key(app_id)
    }

    /// 给 Running app 种入 last_accessed=now(rebuild_stopped_apps / 外部 start 用)
    pub fn seed_accessed(&self, app_id: &str) {
        self.last_accessed
            .insert(app_id.to_string(), Instant::now());
    }

    /// 返回上次访问时刻,供回收扫描器计算闲置时长;None=从未被访问(应视为 grace,不回收)。
    pub fn last_accessed_at(&self, app_id: &str) -> Option<Instant> {
        self.last_accessed.get(app_id).map(|r| *r)
    }

    /// 标记 app 为 Running(唤醒成功 / start_app / 外部 start 后调用,清 stopped 态 + 刷新访问时间)。
    pub fn mark_running(&self, app_id: &str) {
        self.stopped.remove(app_id);
        self.last_accessed
            .insert(app_id.to_string(), Instant::now());
    }

    /// leader 实际执行唤醒:scale→1 + 轮询直到 Running/Error/超时
    async fn wake_leader(&self, app_id: &str) -> WakeOutcome {
        // double-check:进入 leader 前可能已被别处拉起
        if !self.stopped.contains_key(app_id) {
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
        // 轮询 get_deployment_status 直到 Running / Error / 超时
        let deadline = Instant::now() + self.wake_timeout;
        loop {
            match rt.get_deployment_status(app_id).await {
                Ok(Some(s)) if s.phase == "Running" => {
                    // 唤醒成功:清 stopped + 种 last_accessed(避免扫描器立刻又回收)
                    self.mark_running(app_id);
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
}

impl AppAccessTracker for AppActivityRegistry {
    fn touch(&self, app_id: &str) {
        let now = Instant::now();
        // entry API:同一 shard 一次锁;Vacant 直接插,Occupied 仅超节流窗口才写
        match self.last_accessed.entry(app_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(mut e) => {
                let prev = *e.get();
                if now.saturating_duration_since(prev) >= self.throttle {
                    e.insert(now);
                }
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                e.insert(now);
            }
        }
    }
}

#[async_trait::async_trait]
impl AppWakeControl for AppActivityRegistry {
    fn is_stopped(&self, app_id: &str) -> bool {
        self.stopped.contains_key(app_id)
    }

    async fn ensure_running(&self, app_id: &str) -> WakeOutcome {
        // Single-flight:DashMap::entry 原子 insert-or-join 选主
        let handle = match self.waking.entry(app_id.to_string()) {
            dashmap::mapref::entry::Entry::Occupied(e) => {
                // FOLLOWER:克隆句柄,释放 entry guard 后等结果
                e.get().clone()
            }
            dashmap::mapref::entry::Entry::Vacant(e) => {
                // LEADER:建 channel、插入句柄;result cell + WakeGuard 保证退出时
                // (含 panic)必广播一次 outcome 并移除条目。
                let (tx, _rx) = watch::channel(None::<WakeOutcome>);
                let h = std::sync::Arc::new(WakeHandle { tx });
                e.insert(h.clone()); // VacantEntry::insert 消耗 e,语句结束即释放 entry shard 锁
                let result = std::sync::Arc::new(std::sync::Mutex::new(None::<WakeOutcome>));
                let outcome = {
                    let _guard = WakeGuard {
                        map: self.waking.clone(),
                        key: app_id.to_string(),
                        handle: h.clone(),
                        result: result.clone(),
                    };
                    let r = self.wake_leader(app_id).await;
                    // 正常完成:写入 result,guard drop 时取出 Some(r) 广播给 follower。
                    // unwrap_or_else(into_inner):锁毒化(理论不会发生)也不 panic,与 WakeGuard::drop 一致。
                    *result.lock().unwrap_or_else(|e| e.into_inner()) = Some(r.clone());
                    r
                    // _guard 在此 drop:
                    //   - 正常:取 result(=Some),send 给 follower,移除条目
                    //   - panic(wake_leader 中途 unwound):result 仍 None → 发 Failed("aborted"),
                    //     follower 立即收到(不再干等 dead-man 超时),移除条目
                };
                return outcome;
            }
        };

        // —— FOLLOWER 路径 ——
        let mut rx = handle.tx.subscribe();
        // leader 可能已 finished(borrow 拿到 Some)
        if let Some(outcome) = rx.borrow().clone() {
            return outcome;
        }
        // 等 leader 广播(WakeGuard drop 必 send 一次)。changed() Err = leader 的所有 tx 句柄全 drop
        // (理论不会发生:guard + follower 各持一份 Arc<WakeHandle>);dead-man 开关兜底极端情况。
        match ::tokio::time::timeout(self.wake_timeout + Duration::from_secs(10), rx.changed())
            .await
        {
            Ok(Ok(())) => rx
                .borrow()
                .clone()
                .unwrap_or(WakeOutcome::Failed("no outcome".into())),
            Ok(Err(_)) => WakeOutcome::Failed("wake leader aborted".into()),
            Err(_) => WakeOutcome::Failed("wake join timeout".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
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

    fn registry_stopped(reg: &AppActivityRegistry, app_id: &str) {
        reg.mark_stopped(app_id);
    }

    #[tokio::test]
    async fn touch_throttle_collapses_writes_within_window() {
        let reg = AppActivityRegistry::new_with(Duration::from_secs(1), Duration::from_millis(100));
        reg.touch("app-a");
        let t0 = reg.last_accessed_at("app-a").expect("first touch recorded");

        // 窗口内多次 touch 不更新
        for _ in 0..10 {
            reg.touch("app-a");
            sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(reg.last_accessed_at("app-a"), Some(t0), "throttled");

        // 超过窗口后更新
        sleep(Duration::from_millis(120)).await;
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
        registry_stopped(&reg, "app-x");

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
        registry_stopped(&reg, "app-t");

        let outcome = reg.ensure_running("app-t").await;
        assert_eq!(outcome, WakeOutcome::Timeout);
        // 超时后保持 stopped(下次请求重新唤醒)
        assert!(reg.is_stopped("app-t"));
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
        registry_stopped(&reg, "app-p");

        // 第一次唤醒(leader panic)
        let jh = tokio::spawn({
            let r = reg.clone();
            async move { r.ensure_running("app-p").await }
        });
        let _ = jh.await; // task panic → JoinError,吞掉

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
        let map: std::sync::Arc<DashMap<String, std::sync::Arc<WakeHandle>>> =
            std::sync::Arc::new(DashMap::new());
        let (tx, _) = watch::channel(None::<WakeOutcome>);
        let handle = std::sync::Arc::new(WakeHandle { tx });
        map.insert("app-g".to_string(), handle.clone());

        // follower 先 subscribe(模拟并发请求 join 到 leader)
        let rx = handle.tx.subscribe();
        assert!(
            !rx.has_changed().unwrap_or(false),
            "leader 尚未完成,channel 仍为初始 None"
        );

        // leader 中途 panic:result 仍为 None,guard drop
        let result: std::sync::Arc<std::sync::Mutex<Option<WakeOutcome>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        {
            let _guard = WakeGuard {
                map: map.clone(),
                key: "app-g".to_string(),
                handle: handle.clone(),
                result,
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
}
