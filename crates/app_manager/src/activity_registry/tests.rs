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
    replace_after_start: AtomicBool,
    lease_held: Arc<AtomicBool>,
    fail_scale: AtomicBool,
    pause_scale: AtomicBool,
    scale_entered: Notify,
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
            replace_after_start: AtomicBool::new(false),
            lease_held: Arc::new(AtomicBool::new(false)),
            fail_scale: AtomicBool::new(false),
            pause_scale: AtomicBool::new(false),
            scale_entered: Notify::new(),
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

struct TestWakeLease(Arc<AtomicBool>);
#[async_trait::async_trait]
impl shared_types::AppOperationLease for TestWakeLease {
    async fn release(self: Box<Self>) -> Result<(), String> {
        self.0.store(false, Ordering::SeqCst);
        Ok(())
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for MockRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for MockRuntime {
    async fn acquire_app_operation(
        &self,
        _app_id: &str,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        if self.lease_held.swap(true, Ordering::SeqCst) {
            return Err(ContainerRuntimeError::Conflict(
                "operation still owned".into(),
            ));
        }
        Ok(Some(Box::new(TestWakeLease(self.lease_held.clone()))))
    }

    async fn capture_app_mutation_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
        _expected_version: Option<&str>,
    ) -> ContainerRuntimeResult<shared_types::UserAppMutationTarget> {
        context
            .validate_identity(&context.app_id)
            .map_err(ContainerRuntimeError::ConfigurationError)?;
        Ok(shared_types::UserAppMutationTarget {
            context: context.clone(),
            resource: shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::Deployment,
                name: format!("rcoder-app-{}", context.app_id),
                uid: if self.replace_after_start.load(Ordering::SeqCst)
                    && self.scale_calls.load(Ordering::SeqCst) > 0
                {
                    format!("replacement-{}", context.app_id)
                } else {
                    format!("uid-{}", context.app_id)
                },
                resource_version: Some("1".into()),
            },
        })
    }

    async fn start_app_target(
        &self,
        target: &shared_types::UserAppMutationTarget,
    ) -> ContainerRuntimeResult<()> {
        assert_eq!(
            target.resource.uid,
            format!("uid-{}", target.context.app_id)
        );
        self.scale_deployment(&target.context.app_id, 1).await
    }

    async fn scale_deployment(&self, _app_id: &str, _replicas: i32) -> ContainerRuntimeResult<()> {
        let n = self.scale_calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.scale_entered.notify_one();
        if self.pause_scale.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        if self.fail_scale.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "uncertain scale result".into(),
            ));
        }
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

// Real SQLite admission with an infrastructure runtime adapter. Keep both the
// coordinator and its database directory alive until all wake tasks have joined.
async fn attach_coordinator(
    registry: &Arc<AppActivityRegistry>,
    runtime: Arc<MockRuntime>,
    app_id: &str,
) -> (Arc<crate::service::AppService>, tempfile::TempDir) {
    let directory = tempfile::tempdir().expect("wake database directory");
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("wake.turso.db"),
        )
        .await
        .expect("wake database"),
    );
    use shared_types::UserAppLifecycleStore as _;
    store
        .ensure_identity(app_id)
        .await
        .expect("application identity");
    let service = Arc::new(crate::service::AppService {
        config: crate::config::AppManagerConfig {
            access_mode: crate::config::AppAccessMode::Kubernetes,
            ..Default::default()
        },
        runtime,
        activity: registry.clone(),
        pingora: None,
        pingora_ports: DashMap::new(),
        release_locks: DashMap::new(),
        metadata: crate::runtime::metadata::AppMetadataStore::new(store),
        dev_cleanup: std::sync::RwLock::new(None),
        dev_locator: std::sync::RwLock::new(None),
        builder_recovery: std::sync::RwLock::new(None),
        deploy_list_cache: tokio::sync::Mutex::new(None),
    });
    service
        .attach_activity_coordinator()
        .expect("attach coordinator");
    (service, directory)
}

#[tokio::test]
async fn touch_throttle_collapses_writes_within_window() {
    // 窗口取 500ms（循环名义 50ms 留 10 倍余量）：此前 100ms 窗口下负载漂移
    // 可使循环实际超窗（实测 ~110ms），第一段断言高频 flaky
    let reg = AppActivityRegistry::new_with(Duration::from_secs(1), Duration::from_millis(500));
    reg.touch("appa");
    let t0 = reg.last_accessed_at("appa").expect("first touch recorded");

    // 窗口内多次 touch 不更新
    for _ in 0..10 {
        reg.touch("appa");
        sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(reg.last_accessed_at("appa"), Some(t0), "throttled");

    // 超过窗口后更新
    sleep(Duration::from_millis(600)).await;
    reg.touch("appa");
    assert!(
        reg.last_accessed_at("appa").unwrap() > t0,
        "updated after window"
    );
}

#[tokio::test]
async fn wake_concurrent_dedup_scales_once() {
    let rt = Arc::new(MockRuntime::new(true)); // scale 后转 Running
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());
    let reg = Arc::new(reg);
    let _fixture = attach_coordinator(&reg, rt.clone(), "appx").await;
    reg.mark_stopped("appx");

    // 5 并发唤醒:无论时序如何都只 scale 一次——
    // leader 拉起后 mark_running 清 stopped,后到者走 leader 路径时 is_stopped=false → AlreadyRunning(不再 scale);
    // 或 join 到同一 leader 的 follower 经 channel 拿到 Ready。两种路径 scale 都只发生一次。
    let mut handles = vec![];
    for _ in 0..5 {
        let r = reg.clone();
        handles.push(tokio::spawn(async move { r.ensure_running("appx").await }));
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
    let reg = Arc::new(reg);
    let _fixture = attach_coordinator(&reg, rt.clone(), "appt").await;
    reg.mark_stopped("appt");

    let outcome = reg.ensure_running("appt").await;
    assert_eq!(outcome, WakeOutcome::Timeout);
    // Timeout retains the durable mutation; another request cannot start again.
    assert!(reg.is_stopped("appt"));
    assert!(matches!(
        reg.ensure_running("appt").await,
        WakeOutcome::Failed(_)
    ));
    assert_eq!(rt.scale_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn manually_stopped_app_rejects_traffic_without_runtime_write() {
    let reg = AppActivityRegistry::new_with(Duration::from_millis(50), Duration::from_millis(1));
    let runtime = Arc::new(MockRuntime::new(true));
    let scale_calls = runtime.scale_calls.clone();
    reg.set_runtime(runtime);
    reg.mark_wake_blocked("appmanual");

    let outcome = reg.ensure_running("appmanual").await;

    assert!(matches!(outcome, WakeOutcome::Failed(_)), "got {outcome:?}");
    assert_eq!(scale_calls.load(Ordering::SeqCst), 0);
    assert!(reg.is_wake_blocked("appmanual"));
}

#[tokio::test]
async fn recycle_transition_waits_for_stop_then_wakes_once() {
    let runtime = Arc::new(MockRuntime::new(true));
    let registry = Arc::new(AppActivityRegistry::new_with(
        Duration::from_secs(2),
        Duration::from_millis(10),
    ));
    registry.set_runtime(runtime.clone());
    let _fixture = attach_coordinator(&registry, runtime.clone(), "appr").await;
    registry.seed_accessed("appr");
    let observed = registry
        .last_accessed_at("appr")
        .expect("seeded access timestamp");
    let transition = registry
        .try_begin_recycle("appr", observed)
        .expect("unchanged app may enter recycle transition");

    let wake = tokio::spawn({
        let registry = registry.clone();
        async move { registry.ensure_running("appr").await }
    });
    tokio::task::yield_now().await;
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);

    registry.mark_stopped("appr");
    drop(transition);
    let outcome = wake.await.expect("wake task");
    assert_eq!(outcome, WakeOutcome::Ready);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn recycle_transition_rejects_stale_access_observation() {
    let registry = AppActivityRegistry::new_with(Duration::from_secs(2), Duration::from_millis(0));
    registry.seed_accessed("appr");
    let observed = registry
        .last_accessed_at("appr")
        .expect("seeded access timestamp");
    sleep(Duration::from_millis(1)).await;
    registry.touch("appr");

    assert!(registry.try_begin_recycle("appr", observed).is_none());
    assert!(!registry.is_stopped("appr"));
}

#[tokio::test]
async fn wake_leader_panic_cleans_flight_but_retains_uncertain_operation() {
    // Panic after starting a remote mutation cannot authorize a second writer.
    let rt = Arc::new(MockRuntime::new(true));
    rt.panic_on_nth.store(1, Ordering::SeqCst); // panic_on_nth 同模块可访问
    let reg = Arc::new(AppActivityRegistry::new_with(
        Duration::from_secs(2),
        Duration::from_millis(50),
    ));
    reg.set_runtime(rt.clone());
    let _fixture = attach_coordinator(&reg, rt.clone(), "appp").await;
    reg.mark_stopped("appp");

    // 第一次唤醒(leader panic)
    let jh = tokio::spawn({
        let r = reg.clone();
        async move { r.ensure_running("appp").await }
    });
    drop(jh.await); // task panic → JoinError,吞掉

    // 守卫应已移除 waking 条目(未泄漏)
    assert!(
        !reg.is_waking("appp"),
        "waking entry must be cleaned after leader panic"
    );

    // The flight is gone, but the durable mutation and lease remain fenced.
    let outcome = reg.ensure_running("appp").await;
    assert!(
        matches!(outcome, WakeOutcome::Failed(_)),
        "second wake must not retry an uncertain mutation: {:?}",
        outcome
    );
    assert!(reg.is_stopped("appp"));
    assert_eq!(rt.scale_calls.load(Ordering::SeqCst), 1);
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
    let reg = Arc::new(reg);
    let _fixture = attach_coordinator(&reg, rt.clone(), "appx").await;
    assert!(!reg.is_stopped("appx"), "前置：内存视图无记录");

    let outcome = reg.ensure_running("appx").await;
    assert!(matches!(outcome, WakeOutcome::Ready), "got {outcome:?}");
    assert_eq!(rt.scale_calls.load(Ordering::SeqCst), 1);
    assert!(!reg.is_stopped("appx"), "唤醒成功后内存标记清除");
}

/// Running app 的兜底查询负缓存：TTL 内重复判定零额外集群查询。
#[tokio::test]
async fn remote_stopped_negative_cache_avoids_extra_queries() {
    let rt = Arc::new(MockRuntime::new(true)); // phase Starting → replicas 1
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());

    assert!(!reg.remote_stopped("apprun").await);
    assert!(!reg.remote_stopped("apprun").await);
    assert_eq!(
        rt.status_calls.load(Ordering::SeqCst),
        1,
        "TTL 内第二次零额外查询"
    );

    // The negative probe cache must not authorize control success without a
    // coordinator that can check durable lifecycle and resource identity.
    assert!(matches!(
        reg.ensure_running("apprun").await,
        WakeOutcome::Failed(_)
    ));
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cached_running_state_cannot_bypass_a_remote_manual_stop() {
    let runtime = Arc::new(MockRuntime::new(true));
    *runtime.phase.lock().expect("phase") = "Running".into();
    let registry = Arc::new(AppActivityRegistry::new(Duration::from_secs(2)));
    registry.set_runtime(runtime.clone());
    let _fixture = attach_coordinator(&registry, runtime.clone(), "cachedstop").await;
    assert!(!registry.remote_stopped("cachedstop").await);
    assert_eq!(runtime.status_calls.load(Ordering::SeqCst), 1);

    // Another replica commits a manual stop while this replica retains its
    // negative probe cache and has no local stopped flag.
    *runtime.phase.lock().expect("phase") = "Stopped".into();
    *runtime.wake_on_traffic.lock().expect("policy") = Some(false);
    let outcome = registry.ensure_running("cachedstop").await;
    assert!(
        matches!(&outcome, WakeOutcome::Failed(message) if message.contains("intentionally stopped")),
        "{outcome:?}"
    );
    assert!(runtime.status_calls.load(Ordering::SeqCst) > 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert!(registry.waking.is_empty());
}

#[tokio::test]
async fn cached_running_state_requires_a_durable_application_identity() {
    let runtime = Arc::new(MockRuntime::new(true));
    *runtime.phase.lock().expect("phase") = "Running".into();
    let registry = Arc::new(AppActivityRegistry::new(Duration::from_secs(2)));
    registry.set_runtime(runtime.clone());
    let _fixture = attach_coordinator(&registry, runtime.clone(), "registeredapp").await;
    assert!(!registry.remote_stopped("unregisteredapp").await);

    let outcome = registry.ensure_running("unregisteredapp").await;
    assert!(
        matches!(&outcome, WakeOutcome::Failed(message) if message.contains("Application identity not found")),
        "{outcome:?}"
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert!(registry.waking.is_empty());
}

/// 查询瞬断（Err）不缓存：下次调用重查，恢复后返回真实值。
#[tokio::test]
async fn remote_stopped_err_not_cached_retries_next_call() {
    let rt = Arc::new(MockRuntime::new(true));
    rt.fail_status.store(true, Ordering::SeqCst);
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());

    assert!(!reg.remote_stopped("appe").await, "瞬断退化为 false");
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 1);

    rt.fail_status.store(false, Ordering::SeqCst);
    assert!(
        !reg.remote_stopped("appe").await,
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

    assert!(reg.remote_stopped("appm").await);
    assert!(
        reg.is_wake_blocked("appm"),
        "manual_stop 档回填 wake_blocked"
    );
}

/// 本副本状态写点即时刷新兜底缓存（防 TTL 窗口旧值）。
#[tokio::test]
async fn mark_writes_refresh_remote_cache_immediately() {
    let rt = Arc::new(MockRuntime::new(true));
    let reg = AppActivityRegistry::new_with(Duration::from_secs(5), Duration::from_millis(100));
    reg.set_runtime(rt.clone());
    assert!(!reg.remote_stopped("appc").await); // 查一次（Running）缓存 false
    assert_eq!(rt.status_calls.load(Ordering::SeqCst), 1);

    reg.mark_wake_blocked("appc"); // 本副本 stop → 缓存即时刷新
    assert!(
        reg.remote_stopped("appc").await,
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
    reg.last_accessed.insert("appg".to_string(), old_t);

    // PG 较新 → 覆盖内存并返回新值
    assert_eq!(reg.merge_accessed("appg", new_t), new_t);
    assert_eq!(reg.last_accessed_at("appg"), Some(new_t), "新值已回写内存");

    // PG 较旧 → 保内存新值（返回值仍为较新者）
    let stale_t = old_t - chrono::Duration::hours(1);
    assert_eq!(reg.merge_accessed("appg", stale_t), new_t);
    assert_eq!(reg.last_accessed_at("appg"), Some(new_t), "旧值不覆盖");
}

/// 验证 Fix3:leader 中途 panic(result 未写入)时,`WakeGuard` drop 必须广播 `Failed`,
/// 让已 subscribe 的 follower 立即收到(而非干等 dead-man 超时)。
#[tokio::test]
async fn guard_broadcasts_failed_on_drop_when_leader_did_not_finish() {
    let map: Arc<DashMap<String, Arc<WakeHandle>>> = Arc::new(DashMap::new());
    let (tx, _) = watch::channel(None::<WakeOutcome>);
    let handle = Arc::new(WakeHandle { tx });
    map.insert("appg".to_string(), handle.clone());

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
            key: "appg".to_string(),
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
        !map.contains_key("appg"),
        "entry must be removed on guard drop"
    );
}

#[test]
fn wake_completion_respects_all_manual_stops() {
    let registry = AppActivityRegistry::new(Duration::from_secs(2));
    // A stop arriving during wake cannot be cleared by completion.
    registry.mark_stopped("appstoprace");
    registry.mark_wake_blocked("appstoprace");
    assert!(!registry.try_mark_woken("appstoprace"));
    assert!(registry.is_wake_blocked("appstoprace"));
    assert!(registry.is_stopped("appstoprace"));

    // A preexisting manual stop is equally authoritative.
    let registry2 = AppActivityRegistry::new(Duration::from_secs(2));
    registry2.mark_wake_blocked("appstopold");
    assert!(!registry2.try_mark_woken("appstopold"));
    assert!(registry2.is_wake_blocked("appstopold"));
}

#[test]
fn completed_wake_is_retained_for_follower_that_has_not_subscribed_yet() {
    for outcome in [WakeOutcome::Ready, WakeOutcome::Failed("cancelled".into())] {
        let map = Arc::new(DashMap::new());
        let (tx, initial_rx) = watch::channel(None::<WakeOutcome>);
        drop(initial_rx);
        let handle = Arc::new(WakeHandle { tx });
        map.insert("latefollower".into(), handle.clone());
        // The follower selected its role but has not subscribed yet.
        let follower_handle = handle.clone();
        drop(WakeGuard {
            map,
            key: "latefollower".into(),
            handle,
            outcome: Some(outcome.clone()),
        });
        let receiver = follower_handle.tx.subscribe();
        assert_eq!(
            format!("{:?}", *receiver.borrow()),
            format!("{:?}", Some(outcome))
        );
    }
}

#[test]
fn deletion_outcome_is_retained_for_a_late_wake_subscriber() {
    let registry = AppActivityRegistry::new(Duration::from_secs(2));
    let (tx, receiver) = watch::channel(None);
    drop(receiver);
    let handle = Arc::new(WakeHandle { tx });
    registry
        .waking
        .insert("deletedlatesubscriber".into(), handle.clone());
    registry.forget_app("deletedlatesubscriber");
    let receiver = handle.tx.subscribe();
    assert!(
        matches!(&*receiver.borrow(), Some(WakeOutcome::Failed(message)) if message == "Application was deleted")
    );
    assert!(!registry.waking.contains_key("deletedlatesubscriber"));
}

#[test]
fn forget_app_clears_deleted_app_state() {
    let registry = AppActivityRegistry::new(Duration::from_secs(2));
    registry.seed_accessed("appdeleted");
    registry.mark_wake_blocked("appdeleted");

    registry.forget_app("appdeleted");

    assert!(!registry.is_stopped("appdeleted"));
    assert!(!registry.is_wake_blocked("appdeleted"));
    assert_eq!(registry.last_accessed_at("appdeleted"), None);
}

#[tokio::test]
async fn wake_failed_mutation_retains_lease_but_success_releases() {
    for fail in [false, true] {
        let rt = MockRuntime::new(true);
        rt.fail_scale.store(fail, Ordering::SeqCst);
        let rt = Arc::new(rt);
        let reg = AppActivityRegistry::new_with(Duration::from_secs(2), Duration::from_millis(1));
        reg.set_runtime(rt.clone());
        let reg = Arc::new(reg);
        let _fixture = attach_coordinator(&reg, rt.clone(), "leasewake").await;
        reg.mark_stopped("leasewake");
        let outcome = reg.ensure_running("leasewake").await;
        assert_eq!(matches!(outcome, WakeOutcome::Failed(_)), fail);
        assert_eq!(rt.lease_held.load(Ordering::SeqCst), fail);
        if fail {
            assert!(rt.acquire_app_operation("leasewake").await.is_err());
        }
    }
}

#[tokio::test]
async fn cancelled_wake_after_scale_started_retains_lease() {
    let rt = MockRuntime::new(true);
    rt.pause_scale.store(true, Ordering::SeqCst);
    let rt = Arc::new(rt);
    let reg = Arc::new(AppActivityRegistry::new_with(
        Duration::from_secs(2),
        Duration::from_millis(1),
    ));
    reg.set_runtime(rt.clone());
    let _fixture = attach_coordinator(&reg, rt.clone(), "cancelledwake").await;
    reg.mark_stopped("cancelledwake");
    let task = tokio::spawn(async move { reg.ensure_running("cancelledwake").await });
    tokio::time::timeout(Duration::from_secs(2), rt.scale_entered.notified())
        .await
        .expect("scale entered");
    task.abort();
    assert!(task.await.expect_err("cancelled").is_cancelled());
    assert!(rt.lease_held.load(Ordering::SeqCst));
    assert!(rt.acquire_app_operation("cancelledwake").await.is_err());
}

#[tokio::test]
async fn traffic_observing_remote_manual_stop_does_not_start_the_application() {
    let runtime = Arc::new(MockRuntime::new(true));
    *runtime.phase.lock().expect("phase") = "Stopped".into();
    *runtime.wake_on_traffic.lock().expect("wake policy") = Some(false);
    let registry = AppActivityRegistry::new_with(Duration::from_secs(1), Duration::from_millis(1));
    registry.set_runtime(runtime.clone());
    let outcome = registry.ensure_running("remotemanualstop").await;
    assert!(matches!(outcome, WakeOutcome::Failed(_)), "{outcome:?}");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert!(registry.is_wake_blocked("remotemanualstop"));
    assert!(
        registry.waking.is_empty(),
        "rejected traffic must not retain a wake flight"
    );
}

#[tokio::test]
async fn wake_runtime_query_failure_is_not_already_running() {
    let runtime = Arc::new(MockRuntime::new(true));
    runtime.fail_status.store(true, Ordering::SeqCst);
    let registry = AppActivityRegistry::new(Duration::from_secs(1));
    registry.set_runtime(runtime.clone());
    assert!(matches!(
        registry.ensure_running("queryfailure").await,
        WakeOutcome::Failed(_)
    ));
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert!(registry.remote_state.get("queryfailure").is_none());
}

#[tokio::test]
async fn wake_requires_an_attached_live_coordinator() {
    let runtime = Arc::new(MockRuntime::new(true));
    let registry = Arc::new(AppActivityRegistry::new(Duration::from_secs(1)));
    registry.set_runtime(runtime.clone());
    registry.mark_stopped("detached");
    assert!(matches!(
        registry.ensure_running("detached").await,
        WakeOutcome::Failed(_)
    ));
    let fixture = attach_coordinator(&registry, runtime.clone(), "detached").await;
    drop(fixture);
    assert!(matches!(
        registry.ensure_running("detached").await,
        WakeOutcome::Failed(_)
    ));
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn traffic_wake_does_not_confirm_a_replacement_running_resource() {
    let runtime = Arc::new(MockRuntime::new(true));
    runtime.replace_after_start.store(true, Ordering::SeqCst);
    let registry = Arc::new(AppActivityRegistry::new(Duration::from_secs(2)));
    registry.set_runtime(runtime.clone());
    let fixture = attach_coordinator(&registry, runtime.clone(), "replacedwake").await;
    registry.mark_stopped("replacedwake");
    let outcome = registry.ensure_running("replacedwake").await;
    assert!(
        matches!(outcome, WakeOutcome::Failed(ref message) if message.contains("replaced")),
        "{outcome:?}"
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    assert!(runtime.lease_held.load(Ordering::SeqCst));
    let operations = fixture
        .0
        .metadata
        .store
        .unfinished_operations(None, 10)
        .await
        .expect("persisted operations");
    assert_eq!(operations.len(), 1);
    assert_eq!(
        operations[0].state,
        shared_types::UserAppOperationState::RecoveryRequired
    );
    assert_eq!(
        operations[0].checkpoint["target"]["resource"]["uid"],
        "uid-replacedwake"
    );
    assert!(registry.is_stopped("replacedwake"));
}

#[tokio::test]
async fn traffic_wake_retains_confirmation_gate_while_start_is_in_flight() {
    let runtime = Arc::new(MockRuntime::new(true));
    runtime.pause_scale.store(true, Ordering::SeqCst);
    let registry = Arc::new(AppActivityRegistry::new(Duration::from_secs(2)));
    registry.set_runtime(runtime.clone());
    let _fixture = attach_coordinator(&registry, runtime.clone(), "pendingready").await;
    registry.mark_stopped("pendingready");
    let worker = tokio::spawn({
        let registry = registry.clone();
        async move { registry.ensure_running("pendingready").await }
    });
    tokio::time::timeout(Duration::from_secs(2), runtime.scale_entered.notified())
        .await
        .expect("start request entered");
    assert!(
        registry.is_stopped("pendingready"),
        "Starting replicas must not clear the confirmation gate"
    );
    assert!(registry.is_waking("pendingready"));
    assert!(runtime.lease_held.load(Ordering::SeqCst));
    let result = tokio::time::timeout(Duration::from_secs(4), worker)
        .await
        .expect("bounded observation")
        .expect("worker");
    assert_eq!(result, WakeOutcome::Timeout);
    assert!(registry.is_stopped("pendingready"));
    assert!(runtime.lease_held.load(Ordering::SeqCst));
    assert!(matches!(
        registry.ensure_running("pendingready").await,
        WakeOutcome::Failed(_)
    ));
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
}
