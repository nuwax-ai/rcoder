use super::wake::{WakeGuard, WakeHandle};
use super::*;
use Arc;
use std::sync::Mutex as StdMutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use tokio::sync::watch;
use tokio::time::sleep;

use container_runtime_api::{
    ContainerRuntimeError, ContainerRuntimeResult, DeploymentStatus, UserAppDeploymentRuntime,
    WorkspaceRuntime,
};
use shared_types::AppWakeControl;

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
    // get_deployment_status 调用计数(remote_stopped TTL 缓存断言用)
    status_calls: AtomicU32,
    // 返回的 wake_on_traffic 注解值(manual_stop 档断言用)
    wake_on_traffic: StdMutex<Option<bool>>,
    // true 时 get_deployment_status 返回 Err(瞬断注入,验证 Err 不缓存)
    fail_status: AtomicBool,
}

impl MockRuntime {
    fn new(running_after_scale: bool) -> Self {
        Self {
            scale_calls: Arc::new(AtomicU32::new(0)),
            running_after_scale,
            panic_on_nth: AtomicU32::new(0),
            phase: StdMutex::new("Starting".to_string()),
            status_calls: AtomicU32::new(0),
            wake_on_traffic: StdMutex::new(None),
            fail_status: AtomicBool::new(false),
        }
    }
}

#[async_trait::async_trait]
impl WorkspaceRuntime for MockRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for MockRuntime {
    async fn scale_deployment(&self, _app_id: &str, _replicas: i32) -> ContainerRuntimeResult<()> {
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
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        if self.fail_status.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(format!(
                "injected transient failure for {app_id}"
            )));
        }
        let phase = self.phase.lock().unwrap().clone();
        let mut status = mk_status(app_id, &phase);
        // 对齐集群语义:replicas==0 即 Stopped(remote_stopped 按 replicas 判定)
        if phase == "Stopped" {
            status.replicas = 0;
        }
        status.wake_on_traffic = *self.wake_on_traffic.lock().unwrap();
        Ok(Some(status))
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
    let reg = AppActivityRegistry::new_with(Duration::from_millis(300), Duration::from_millis(50));
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
    let reg = AppActivityRegistry::new_with(Duration::from_millis(50), Duration::from_millis(1));
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
    let registry = AppActivityRegistry::new_with(Duration::from_secs(2), Duration::from_millis(0));
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

// ── remote_stopped 多副本兜底（集群 replicas 为 stopped 事实源）──

/// 内存无任何标记（模拟其他副本 stop 后本副本不知情）+ 集群 replicas=0：
/// ensure_running 经兜底回填后正常唤醒。
#[tokio::test]
async fn remote_stopped_backfills_and_wakes_when_cluster_says_stopped() {
    let rt = Arc::new(MockRuntime::new(true)); // scale 后转 Running
    *rt.phase.lock().unwrap() = "Stopped".to_string();
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());
    assert!(!reg.is_stopped("app-x"), "前置：内存视图无记录");

    let outcome = reg.ensure_running("app-x").await;
    assert!(matches!(outcome, WakeOutcome::Ready), "got {outcome:?}");
    assert_eq!(rt.scale_calls.load(Ordering::SeqCst), 1);
    assert!(!reg.is_stopped("app-x"), "唤醒成功后内存标记清除");
}

/// Running app 的兜底查询负缓存：TTL 内重复判定零额外集群查询。
#[tokio::test]
async fn remote_stopped_negative_cache_avoids_extra_queries() {
    let rt = Arc::new(MockRuntime::new(true)); // phase Starting → replicas 1
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());

    assert!(!reg.remote_stopped("app-run").await);
    assert!(!reg.remote_stopped("app-run").await);
    assert_eq!(
        rt.status_calls.load(Ordering::SeqCst),
        1,
        "TTL 内第二次零额外查询"
    );

    // 内存也无记录 → ensure_running 走兜底（缓存命中 false，无 IO）
    assert_eq!(
        reg.ensure_running("app-run").await,
        WakeOutcome::AlreadyRunning
    );
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 1);
}

/// 查询瞬断（Err）不缓存：下次调用重查，恢复后返回真实值。
#[tokio::test]
async fn remote_stopped_err_not_cached_retries_next_call() {
    let rt = Arc::new(MockRuntime::new(true));
    rt.fail_status.store(true, Ordering::SeqCst);
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());

    assert!(!reg.remote_stopped("app-e").await, "瞬断退化为 false");
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 1);

    rt.fail_status.store(false, Ordering::SeqCst);
    assert!(
        !reg.remote_stopped("app-e").await,
        "恢复后重查（Err 未缓存）"
    );
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 2);
}

/// K8s wake_on_traffic==Some(false) 注解：回填 wake_blocked 档（非 stopped 档）。
#[tokio::test]
async fn remote_stopped_manual_stop_backfills_wake_blocked_tier() {
    let rt = Arc::new(MockRuntime::new(true));
    *rt.phase.lock().unwrap() = "Stopped".to_string();
    *rt.wake_on_traffic.lock().unwrap() = Some(false);
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt);

    assert!(reg.remote_stopped("app-m").await);
    assert!(
        reg.is_wake_blocked("app-m"),
        "manual_stop 档回填 wake_blocked"
    );
}

/// 本副本状态写点即时刷新兜底缓存（防 TTL 窗口旧值）。
#[tokio::test]
async fn mark_writes_refresh_remote_cache_immediately() {
    let rt = Arc::new(MockRuntime::new(true));
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());
    assert!(!reg.remote_stopped("app-c").await); // 查一次（Running）缓存 false
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 1);

    reg.mark_wake_blocked("app-c"); // 本副本 stop → 缓存即时刷新
    assert!(
        reg.remote_stopped("app-c").await,
        "mark 后缓存立即为 stopped"
    );
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 1, "零额外集群查询");
}

/// 跨副本访问时间合并：PG 较新覆盖内存（并回写，保 epoch 复核），较旧保内存值。
#[test]
fn merge_accessed_takes_max_and_backfills_memory() {
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(1));
    let old_t = Utc::now() - chrono::Duration::hours(10);
    let new_t = Utc::now() - chrono::Duration::minutes(1);
    reg.last_accessed.insert("app-g".to_string(), old_t);

    // PG 较新 → 覆盖内存并返回新值
    assert_eq!(reg.merge_accessed("app-g", new_t), new_t);
    assert_eq!(reg.last_accessed_at("app-g"), Some(new_t), "新值已回写内存");

    // PG 较旧 → 保内存新值（返回值仍为较新者）
    let stale_t = old_t - chrono::Duration::hours(1);
    assert_eq!(reg.merge_accessed("app-g", stale_t), new_t);
    assert_eq!(reg.last_accessed_at("app-g"), Some(new_t), "旧值不覆盖");
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
