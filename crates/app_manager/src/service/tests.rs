use super::*;
use std::sync::atomic::Ordering;

use crate::models::ResourceLimits;
use crate::test_support::{MockRuntime, StubDevCleanup, release_lock, test_service};
use container_runtime_api::StorageResizeOutcome;

pub(crate) fn create_request(app_id: &str) -> CreateAppRequest {
    CreateAppRequest {
        app_id: Some(app_id.to_owned()),
        lifecycle_id: None,
        request_id: None,
        name: "r2app".into(),
        image: Some("registry.example/app-runtime:test".into()),
        command: None,
        env: None,
        secrets: None,
        resources: None,
        ports: None,
        health_check: None,
        tenant_id: None,
        space_id: None,
        recycle_enabled: None,
        idle_timeout_seconds: None,
    }
}

#[tokio::test]
async fn failed_update_restores_registered_ports_not_drifted_live_ports() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let mut service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .store
        .ensure_identity("portdrift")
        .await
        .expect("owner identity");
    service.pingora = Some(Arc::new(PingoraProxyService::new(
        rcoder_proxy::ProxyConfig::default(),
    )));
    runtime.deployments.insert(
        "portdrift".into(),
        DeploymentStatus {
            app_id: "portdrift".into(),
            phase: "Running".into(),
            pod_ip: Some("10.0.0.1".into()),
            ..Default::default()
        },
    );
    runtime.specs.insert(
        "portdrift".into(),
        container_runtime_api::ContainerSpecSnapshot {
            ports: Some(vec![container_runtime_api::AppPortSpec {
                name: "http".into(),
                port: 9081,
                expose_type: container_runtime_api::ExposeType::Http,
                strip_prefix: None,
            }]),
            ..Default::default()
        },
    );
    service
        .register_pingora_backends("portdrift", &[9080], "10.0.0.1")
        .await;
    runtime.create_fails.store(true, Ordering::SeqCst);
    let request = UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        image: Some("registry.example/app-runtime:test".into()),
        name: None,
        env: None,
        secrets: None,
        resources: None,
        tenant_id: None,
        space_id: None,
        recycle_enabled: None,
        idle_timeout_seconds: None,
        expected_resource_version: None,
    };
    assert!(service.update_app("portdrift", request).await.is_err());
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        1,
        "patch was attempted"
    );
    assert_eq!(service.registered_http_ports("portdrift"), vec![9080]);
}

/// 外部 delete 快失败 + 版本保护两段覆盖：
/// 1. 锁被进行中操作持有 → 立即 Conflict（不排队——旧"等锁后复查版本"场景
///    在新语义下不可达）；
/// 2. 释放锁后以过期 expected_resource_version 重新发起 → 仍 Conflict，
///    拿锁后复查版本防误删的保护不丢失（delete/PVC 销毁调用均为 0）。
#[tokio::test]
async fn busy_delete_fails_fast_and_stale_version_conflicts_after_release() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .store
        .ensure_identity("deleterace")
        .await
        .expect("owner identity");
    runtime.deployments.insert(
        "deleterace".into(),
        DeploymentStatus {
            app_id: "deleterace".into(),
            phase: "Running".into(),
            resource_version: Some("1".into()),
            ..Default::default()
        },
    );
    // 段 1：锁被占 → 有界时间内 Conflict（1s 是测试防挂预算，不是 HTTP SLA；
    // 等待语义回归时此处超时失败而非挂死）。
    let writer = service
        .acquire_process_release_lock("deleterace")
        .await
        .expect("writer lock");
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        service.delete_app("deleterace", true, Some("1")),
    )
    .await
    .expect("busy delete must not queue behind the held lock")
    .expect_err("held lock must reject delete");
    // M3 结构化 blocker：裸持锁（无 durable admission）窗口 → ConflictBlocked
    // 携带 scope=Prod 哨兵 blocker（operation_id 空——不伪造身份）
    assert!(
        matches!(
            &error,
            AppOperationError::ConflictBlocked { message, blocker }
                if message.contains("in progress")
                    && blocker.scope == shared_types::UserAppOperationScope::Prod
                    && blocker.operation_id.is_empty()
        ),
        "got: {error}"
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    // 版本在锁被占期间漂移（旧测试的竞争窗口，如今只能发生在拒绝之后）。
    runtime
        .deployments
        .get_mut("deleterace")
        .expect("deployment")
        .resource_version = Some("2".into());
    drop(writer);
    // 段 2：锁已释放，过期版本重新发起 → 版本复查仍拒绝，零物理副作用。
    let error = service
        .delete_app("deleterace", true, Some("1"))
        .await
        .expect_err("stale expected version must conflict after lock release");
    assert!(
        matches!(&error, AppOperationError::Conflict(_)),
        "got: {error}"
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn independent_services_share_application_file_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let first = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let second = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let held = first
        .acquire_process_release_lock("crossprocess")
        .await
        .expect("first lock");
    let contender = second.acquire_process_release_lock("crossprocess");
    tokio::pin!(contender);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), contender.as_mut())
            .await
            .is_err()
    );
    drop(held);
    let guard = tokio::time::timeout(std::time::Duration::from_secs(2), contender)
        .await
        .expect("released lock must progress")
        .expect("second lock");
    drop(guard);
}

/// 外部 stop 快失败：锁被进行中操作持有 → 有界时间内 Conflict，不排队、
/// 零副作用（无运行时变更/无本次操作持久记录/无唤醒围栏变化）；释放锁
/// 不会使被拒请求自行执行；释放后重新发起的新请求正常完成。
#[tokio::test]
async fn stop_conflicts_immediately_while_release_lock_held() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "stopbusy").await;
    let held = service
        .acquire_process_release_lock("stopbusy")
        .await
        .expect("held lock");
    let request = shared_types::UserAppControlRequest {
        lifecycle_id: None,
        request_id: Some("stopbusyreject".into()),
    };
    // 1s = 测试防挂预算，不是 HTTP SLA；等待语义回归时此处超时失败而非挂死。
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        service.stop_app_controlled("stopbusy", request.clone()),
    )
    .await
    .expect("stop must not queue behind the held lock")
    .expect_err("held lock must reject stop");
    // M3 结构化 blocker：裸持锁（无 durable admission）窗口 → ConflictBlocked
    // 携带 scope=Prod 哨兵 blocker（operation_id 空——不伪造身份）
    assert!(
        matches!(
            &error,
            AppOperationError::ConflictBlocked { message, blocker }
                if message.contains("in progress")
                    && blocker.scope == shared_types::UserAppOperationScope::Prod
                    && blocker.operation_id.is_empty()
        ),
        "got: {error}"
    );
    // 拒绝后零副作用
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime
            .deployments
            .get("stopbusy")
            .expect("deployment")
            .replicas,
        1,
        "rejected stop must not touch the running workload"
    );
    assert!(
        service
            .get_control_operation_by_request("stopbusy", "stopbusyreject")
            .await
            .expect("operation query")
            .is_none(),
        "rejected stop must leave no durable operation record"
    );
    assert!(!service.activity.is_wake_blocked("stopbusy"));
    // 释放锁不会使被拒请求自行执行（锁不排队 ≠ 延迟执行）
    drop(held);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        0,
        "rejected stop must not auto-execute after lock release"
    );
    // 释放后重新发起新请求（同一 request_id 亦证明拒绝未留下持久痕迹）→ 正常完成
    service
        .stop_app_controlled("stopbusy", request)
        .await
        .expect("fresh stop after release");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .deployments
            .get("stopbusy")
            .expect("deployment")
            .replicas,
        0
    );
    let operation = service
        .get_control_operation_by_request("stopbusy", "stopbusyreject")
        .await
        .expect("operation query")
        .expect("operation record");
    assert_eq!(operation.kind, shared_types::UserAppOperationKind::Stop);
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
}

/// 外部 restart（无 url 路径）快失败：与 stop 同款断言矩阵。
#[tokio::test]
async fn restart_conflicts_immediately_while_restart_lock_held() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "restartbusy").await;
    let held = service
        .acquire_process_release_lock("restartbusy")
        .await
        .expect("held lock");
    let request = shared_types::UserAppControlRequest {
        lifecycle_id: None,
        request_id: Some("restartbusyreject".into()),
    };
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        service.restart_app_controlled("restartbusy", request.clone()),
    )
    .await
    .expect("restart must not queue behind the held lock")
    .expect_err("held lock must reject restart");
    // M3 结构化 blocker：裸持锁（无 durable admission）窗口 → ConflictBlocked
    // 携带 scope=Prod 哨兵 blocker（operation_id 空——不伪造身份）
    assert!(
        matches!(
            &error,
            AppOperationError::ConflictBlocked { message, blocker }
                if message.contains("in progress")
                    && blocker.scope == shared_types::UserAppOperationScope::Prod
                    && blocker.operation_id.is_empty()
        ),
        "got: {error}"
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert!(
        service
            .get_control_operation_by_request("restartbusy", "restartbusyreject")
            .await
            .expect("operation query")
            .is_none(),
        "rejected restart must leave no durable operation record"
    );
    drop(held);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        0,
        "rejected restart must not auto-execute after lock release"
    );
    service
        .restart_app_controlled("restartbusy", request)
        .await
        .expect("fresh restart after release");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime
            .deployments
            .get("restartbusy")
            .expect("deployment")
            .phase,
        "Running"
    );
    let operation = service
        .get_control_operation_by_request("restartbusy", "restartbusyreject")
        .await
        .expect("operation query")
        .expect("operation record");
    assert_eq!(operation.kind, shared_types::UserAppOperationKind::Restart);
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
}

/// 外部 delete 快失败：锁被占立即 Conflict，身份/运行态/持久记录零变化；
/// 释放后被拒请求不自行执行；重新发起的新删除正常完成。
#[tokio::test]
async fn delete_conflicts_immediately_while_release_lock_held() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "deletebusy").await;
    let held = service
        .acquire_process_release_lock("deletebusy")
        .await
        .expect("held lock");
    let request = DeleteAppRequest {
        lifecycle_id: None,
        request_id: Some("deletebusyreject".into()),
        purge: Some(false),
        expected_resource_version: None,
    };
    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        service.delete_app_controlled("deletebusy", request.clone()),
    )
    .await
    .expect("delete must not queue behind the held lock")
    .expect_err("held lock must reject delete");
    // M3 结构化 blocker：裸持锁（无 durable admission）窗口 → ConflictBlocked
    // 携带 scope=Prod 哨兵 blocker（operation_id 空——不伪造身份）
    assert!(
        matches!(
            &error,
            AppOperationError::ConflictBlocked { message, blocker }
                if message.contains("in progress")
                    && blocker.scope == shared_types::UserAppOperationScope::Prod
                    && blocker.operation_id.is_empty()
        ),
        "got: {error}"
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    assert!(
        runtime.deployments.contains_key("deletebusy"),
        "rejected delete must keep the running workload"
    );
    assert!(
        service
            .metadata
            .store
            .get_application("deletebusy")
            .await
            .expect("identity query")
            .is_some(),
        "rejected delete must keep the lifecycle identity"
    );
    assert!(
        service
            .get_control_operation_by_request("deletebusy", "deletebusyreject")
            .await
            .expect("operation query")
            .is_none(),
        "rejected delete must leave no durable operation record"
    );
    drop(held);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(
        runtime.delete_calls.load(Ordering::SeqCst),
        0,
        "rejected delete must not auto-execute after lock release"
    );
    service
        .delete_app_controlled("deletebusy", request)
        .await
        .expect("fresh delete after release");
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    assert!(!runtime.deployments.contains_key("deletebusy"));
}

/// 等待语义保留：无 url 的外部 start 与内部回收器（recycle_app）在锁被占
/// 期间保持排队（有界窗口内不返回，更不返回 Conflict），释放锁后继续完成。
/// 与快失败测试互为对照——防止把 try 锁误扩散到等待型入口。
#[tokio::test]
async fn start_and_recycle_keep_waiting_for_the_held_lock() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "waitqueue").await;

    // 阶段 1：内部回收路径（recycle_app → wake_on_traffic=true）保持等待
    let held = service
        .acquire_process_release_lock("waitqueue")
        .await
        .expect("held lock");
    let recycle = service.recycle_app("waitqueue");
    tokio::pin!(recycle);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), recycle.as_mut())
            .await
            .is_err(),
        "internal recycler must keep waiting for the lock instead of failing fast"
    );
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(5), recycle)
        .await
        .expect("recycle proceeds after lock release")
        .expect("recycle succeeds");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    {
        let deployment = runtime.deployments.get("waitqueue").expect("deployment");
        assert_eq!(deployment.replicas, 0);
        assert_eq!(deployment.wake_on_traffic, Some(true));
    }

    // 阶段 2：无 url 的外部 start 保持等待
    let held = service
        .acquire_process_release_lock("waitqueue")
        .await
        .expect("held lock again");
    let start = service.start_app_controlled(
        "waitqueue",
        shared_types::UserAppControlRequest {
            lifecycle_id: None,
            request_id: Some("waitqueuestart".into()),
        },
    );
    tokio::pin!(start);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(200), start.as_mut())
            .await
            .is_err(),
        "external start (no url) must keep waiting for the lock instead of failing fast"
    );
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(5), start)
        .await
        .expect("start proceeds after lock release")
        .expect("start succeeds");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime
            .deployments
            .get("waitqueue")
            .expect("deployment")
            .replicas,
        1
    );
}

/// 跨实例文件锁层同样快失败：两个独立 AppService 共用锁根，第一实例持锁
/// （进程 Mutex + flock）时，第二实例的外部 stop/restart/delete 都必须在
/// 文件锁层立即 Conflict（不能只测同一实例的进程 Mutex）。
#[tokio::test]
async fn external_operations_fail_fast_across_service_file_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let first = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let second_runtime = Arc::new(MockRuntime::default());
    let second = test_service(root.path(), second_runtime.clone()).await;
    let held = first
        .acquire_process_release_lock("crosslock")
        .await
        .expect("first instance lock");

    let control = shared_types::UserAppControlRequest {
        lifecycle_id: None,
        request_id: Some("crosslockreject".into()),
    };

    // 三个外部入口返回类型不同（stop/restart=AppRuntimeInfo，delete=()），
    // 用泛型 helper 收敛"文件锁层快失败"断言。
    async fn expect_file_lock_conflict<T>(label: &str, future: impl Future<Output = AppResult<T>>) {
        let error = match tokio::time::timeout(std::time::Duration::from_secs(1), future).await {
            Ok(Err(error)) => error,
            Ok(Ok(_)) => panic!("{label} must be rejected by the cross-instance file lock"),
            Err(_) => {
                panic!("{label} must not queue behind the cross-instance file lock")
            }
        };
        assert!(
            matches!(&error, AppOperationError::Conflict(message) if message.contains("another process")),
            "{label} got: {error}"
        );
    }

    expect_file_lock_conflict(
        "stop",
        second.stop_app_controlled("crosslock", control.clone()),
    )
    .await;
    expect_file_lock_conflict(
        "restart",
        second.restart_app_controlled("crosslock", control.clone()),
    )
    .await;
    expect_file_lock_conflict(
        "delete",
        second.delete_app_controlled(
            "crosslock",
            DeleteAppRequest {
                lifecycle_id: None,
                request_id: Some("crosslockrejectdelete".into()),
                purge: Some(false),
                expected_resource_version: None,
            },
        ),
    )
    .await;
    assert_eq!(second_runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(second_runtime.delete_calls.load(Ordering::SeqCst), 0);

    // 释放后第二实例可正常取得文件锁（等待版语义未变，见
    // independent_services_share_application_file_lock）。
    drop(held);
    let guard = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        second.acquire_process_release_lock("crosslock"),
    )
    .await
    .expect("released file lock must progress")
    .expect("second instance lock");
    guard.finish().await.expect("finish guard");
}

/// 按指定 access_mode 直构造 AppService（K8s 分支测试用；test_support 的
/// test_service 固定 Docker，本任务六文件范围内不改 test_support）。
async fn test_service_with_mode(
    workspace_root: &std::path::Path,
    runtime: Arc<MockRuntime>,
    access_mode: AppAccessMode,
) -> AppService {
    tokio::fs::create_dir_all(workspace_root)
        .await
        .expect("test workspace");
    // Turso 实例独占目录：独立子目录避免同 root 多实例锁冲突
    let metadata_dir = workspace_root.join(format!("metadata-{}", uuid::Uuid::new_v4()));
    tokio::fs::create_dir_all(&metadata_dir)
        .await
        .expect("metadata directory");
    let store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
        &metadata_dir.join("userapp.turso.db"),
    )
    .await
    .expect("Turso metadata store");
    let config = AppManagerConfig {
        workspace_root: Some(workspace_root.to_string_lossy().into_owned()),
        operation_lock_root: workspace_root.to_string_lossy().into_owned(),
        access_mode,
        ..AppManagerConfig::default()
    };
    let store = Arc::new(store);
    AppService {
        runtime_configuration: store.clone(),
        operation_flight: Arc::default(),
        config,
        runtime: runtime as Arc<dyn UserAppRuntime>,
        activity: Arc::new(AppActivityRegistry::new(std::time::Duration::from_secs(
            300,
        ))),
        pingora: None,
        pingora_ports: DashMap::new(),
        release_locks: DashMap::new(),
        metadata: AppMetadataStore::new(store),
        dev_cleanup: std::sync::RwLock::new(None),
        dev_locator: std::sync::RwLock::new(None),
        builder_recovery: std::sync::RwLock::new(None),
        deploy_list_cache: tokio::sync::Mutex::new(None),
    }
}

/// K8s 模式租约冲突快失败：operation_guard 的 K8s 分支经 acquire_app_operation
/// 取得跨副本租约；受控 runtime 报告租约被占（MockRuntime 的 Conflict 与
/// OperationInProgress 在 map_runtime_error 同一映射臂）→ 外部 stop 立即
/// Conflict、不重试不排队、零运行时变更；租约释放后可正常取得。
#[tokio::test]
async fn kubernetes_waiting_acquire_polls_until_lease_released() {
    // K8s 模式等待语义：wait=true 时租约被占轮询直至持有者释放
    //（131 实测修复前 start 无 url 在忙锁下立即 ERR_CONFLICT）
    let directory = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    let service =
        test_service_with_mode(directory.path(), runtime.clone(), AppAccessMode::Kubernetes).await;
    service
        .metadata
        .store
        .ensure_identity("k8swait")
        .await
        .expect("identity");
    // 他者持有租约
    runtime.lease_held.store(true, Ordering::SeqCst);
    let process = service
        .release_locks
        .entry((
            "k8swait".to_owned(),
            shared_types::UserAppOperationScope::Prod,
        ))
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
        .lock_owned()
        .await;
    let waiting = service.operation_guard("k8swait", process, true);
    tokio::pin!(waiting);
    // 等待者保持排队（不立即失败）
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(400), &mut waiting)
            .await
            .is_err(),
        "wait=true 必须排队等待而非立即 Conflict"
    );
    // 释放租约 → 等待者在有界时间内获得
    runtime.lease_held.store(false, Ordering::SeqCst);
    let guard = tokio::time::timeout(std::time::Duration::from_secs(2), waiting)
        .await
        .expect("bounded wait")
        .expect("waiting acquire succeeds");
    assert!(runtime.lease_held.load(Ordering::SeqCst));
    drop(guard);
}

#[tokio::test]
async fn kubernetes_lease_conflict_fails_fast_without_queueing() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let service =
        test_service_with_mode(root.path(), runtime.clone(), AppAccessMode::Kubernetes).await;
    service
        .metadata
        .store
        .ensure_identity("k8sbusy")
        .await
        .expect("owner identity");
    // 模拟另一副本持有跨实例操作租约
    runtime.lease_held.store(true, Ordering::SeqCst);

    let error = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        service.stop_app_controlled(
            "k8sbusy",
            shared_types::UserAppControlRequest {
                lifecycle_id: None,
                request_id: Some("k8sbusyreject".into()),
            },
        ),
    )
    .await
    .expect("K8s lease conflict must fail fast without queueing")
    .expect_err("held lease must reject stop");
    assert!(
        matches!(&error, AppOperationError::Conflict(_)),
        "got: {error}"
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert!(
        service
            .get_control_operation_by_request("k8sbusy", "k8sbusyreject")
            .await
            .expect("operation query")
            .is_none(),
        "lease-rejected stop must leave no durable operation record"
    );

    // 租约释放后 try 版可正常取得（快失败不是永久拒绝）
    runtime.lease_held.store(false, Ordering::SeqCst);
    let guard = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        service.try_acquire_process_release_lock("k8sbusy"),
    )
    .await
    .expect("free lease must be acquired")
    .expect("guard");
    guard.finish().await.expect("finish releases the lease");
    assert!(!runtime.lease_held.load(Ordering::SeqCst));
}

/// 正式信封验收：忙锁 stop 经真实 handler + axum router + envelope_errors
/// 中间件 → HTTP 200、success=false、code=ERR_CONFLICT（不能只看 HTTP 状态）。
#[tokio::test]
async fn busy_stop_envelope_returns_http_200_with_err_conflict() {
    let directory = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    let service = Arc::new(test_service(directory.path(), runtime.clone()).await);
    let held = service
        .acquire_process_release_lock("envelopebusy")
        .await
        .expect("held lock");
    let router = axum::Router::new()
        .route(
            "/api/v1/userapp/{app_id}/stop",
            axum::routing::post(crate::handlers::ops::stop_app),
        )
        .layer(axum::middleware::from_fn(
            shared_types::userapp_http::envelope_errors,
        ))
        .with_state(Arc::new(crate::handlers::state::AppManagerState {
            app_service: service.clone(),
            http_client: reqwest::Client::new(),
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener");
    let address = listener.local_addr().expect("listener address");
    struct Server(tokio::task::JoinHandle<()>);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _server = Server(tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("HTTP test server");
    }));
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .expect("HTTP client");
    let response = client
        .post(format!("http://{address}/api/v1/userapp/envelopebusy/stop"))
        .send()
        .await
        .expect("HTTP response");
    // 业务信封：HTTP 200，失败语义在 body 的 success/code
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let envelope: serde_json::Value = response.json().await.expect("JSON envelope");
    assert_eq!(envelope["success"], false);
    assert_eq!(envelope["code"], shared_types::error_codes::ERR_CONFLICT);
    assert!(
        envelope["message"]
            .as_str()
            .expect("message")
            .contains("in progress"),
        "envelope: {envelope}"
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    drop(held);
}

/// R01：runtime 未返回创建凭据时，service 不得按名字补偿删除竞争赢家。
#[tokio::test]
pub(crate) async fn create_app_runtime_failure_does_not_delete_unowned_resources() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    runtime.create_fails.store(true, Ordering::SeqCst);
    let service = test_service(root.path(), runtime.clone()).await;
    // build_container_params 需 code/release.lock.toml，预铺现场
    let app_dir = root.path().join("appr2");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");

    let error = service
        .create_app(create_request("appr2"))
        .await
        .expect_err("create_app must fail");

    // 原始错误原样返回（create_deployment 失败的映射，未被清理逻辑覆盖）
    assert!(
        matches!(&error, AppOperationError::Backend(message) if message.contains("mock create_deployment failure")),
        "original error must be preserved, got: {error}"
    );
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        runtime.delete_calls.load(Ordering::SeqCst),
        0,
        "failed creation does not establish ownership of the named deployment"
    );
}

/// A′：创建链在 claim 阶段被确定性拒绝且仅保留幂等资源 → 操作落 Failed
/// （step=rejected_without_mutation），错误消息显式记录保留资源（不冒充
/// 零变更），围栏释放——后续创建可直接重新受理。
#[tokio::test]
pub(crate) async fn creation_safe_failure_settles_failed_and_releases_fence() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    *runtime.create_abort.lock().expect("injection") =
        Some((container_runtime_api::CreationStage::StorageClaim, true));
    let service = test_service(root.path(), runtime.clone()).await;
    let app_dir = root.path().join("appsafe");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");
    let mut request = create_request("appsafe");
    request.request_id = Some("safefailure1".into());

    let error = service
        .create_app(request)
        .await
        .expect_err("definitive claim rejection must fail creation");
    // wire 保真：source 是 Conflict → ERR_CONFLICT；安全注记进 message
    assert!(
        matches!(&error, AppOperationError::Conflict(message)
            if message.contains("safe failure")
                && message.contains("workspace pvc ensured")
                && message.contains("storage-claim annotations may persist")),
        "wire keeps conflict class with explicit retention note, got: {error}"
    );

    let operation = service
        .metadata
        .store
        .get_operation_by_request("appsafe", "safefailure1")
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(operation.state, shared_types::UserAppOperationState::Failed);
    assert_eq!(operation.step, "rejected_without_mutation");
    assert!(
        operation
            .error_message
            .as_deref()
            .is_some_and(|message| message.contains("safe failure")),
        "{operation:?}"
    );

    // 围栏已释放：同一 app 的下一次创建可直接受理并成功
    *runtime.create_abort.lock().expect("injection") = None;
    service
        .create_app(create_request("appsafe"))
        .await
        .expect("subsequent creation admits after safe failure");
}

/// A′ 对照：claim 阶段未知结果（超时类）不构成安全证明 → 操作保持
/// RecoveryRequired 围栏，后续创建被「操作进行中」拒绝（R02 保守语义）。
#[tokio::test]
pub(crate) async fn creation_unknown_outcome_keeps_recovery_fence() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    *runtime.create_abort.lock().expect("injection") =
        Some((container_runtime_api::CreationStage::StorageClaim, false));
    let service = test_service(root.path(), runtime.clone()).await;
    let app_dir = root.path().join("appunknown");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");
    let mut request = create_request("appunknown");
    request.request_id = Some("unknownoutcome1".into());

    let error = service
        .create_app(request)
        .await
        .expect_err("unknown claim outcome must fail creation");
    // source Timeout → Backend（wire 不变），且无安全注记
    assert!(
        matches!(&error, AppOperationError::Backend(message)
            if !message.contains("safe failure")),
        "unknown outcome keeps backend class without safe note, got: {error}"
    );

    let operation = service
        .metadata
        .store
        .get_operation_by_request("appunknown", "unknownoutcome1")
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::RecoveryRequired
    );

    // 围栏仍在：新创建被未完成变更围栏挡回
    let fenced = service
        .create_app(create_request("appunknown"))
        .await
        .expect_err("fence must block new operations");
    assert!(
        matches!(&fenced, AppOperationError::Conflict(message)
            if message.contains("requires operator recovery")),
        "expected recovery fence conflict, got: {fenced}"
    );
}

/// B′：未知围栏操作的 retry 拒绝必须携带只读观察证据且零状态迁移——
/// 观察不裁决（R02：查无≠安全），围栏原样保留。
#[tokio::test]
pub(crate) async fn uncertain_retry_refusal_carries_readonly_observation() {
    for observation in ["absent", "unavailable"] {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        *runtime.create_abort.lock().expect("injection") =
            Some((container_runtime_api::CreationStage::StorageClaim, false));
        let service = test_service(root.path(), runtime.clone()).await;
        let app_dir = root.path().join("appdiag");
        tokio::fs::create_dir_all(app_dir.join("code"))
            .await
            .expect("create code dir");
        tokio::fs::write(
            app_dir.join("code").join("release.lock.toml"),
            release_lock(),
        )
        .await
        .expect("write release lock");
        let mut request = create_request("appdiag");
        request.request_id = Some("diag-1".into());
        service
            .create_app(request)
            .await
            .expect_err("uncertain creation fails");
        let operation = service
            .metadata
            .store
            .get_operation_by_request("appdiag", "diag-1")
            .await
            .expect("read")
            .expect("operation");
        assert_eq!(
            operation.state,
            shared_types::UserAppOperationState::RecoveryRequired
        );

        // "unavailable" 形态在重试调用前注入（避免被 create 前置查询提前消费）
        if observation == "unavailable" {
            runtime.status_fails.store(1, Ordering::SeqCst);
        }
        let refusal = service
            .retry_control_operation(
                "appdiag",
                &operation.operation_id,
                shared_types::UserAppRetryRequest {
                    lifecycle_id: operation.lifecycle_id.clone(),
                    expected_revision: operation.revision,
                },
            )
            .await
            .expect_err("uncertain operations are not replayable");
        let expected_fragment = if observation == "absent" {
            "observed runtime deployment absent"
        } else {
            "runtime observation unavailable"
        };
        let proof_caveat_required = observation == "absent";
        assert!(
            matches!(&refusal, AppOperationError::InvalidState(message)
                if message.contains(expected_fragment)
                    && message.contains("manual reconciliation required")
                    && (!proof_caveat_required || message.contains("not proof of absence"))),
            "[{observation}] refusal carries read-only evidence, got: {refusal}"
        );
        // 零状态迁移：操作记录逐字节不变
        assert_eq!(
            service
                .metadata
                .store
                .get_operation("appdiag", &operation.operation_id)
                .await
                .expect("read"),
            Some(operation)
        );
    }
}

/// R2 对照：清理自身失败也不改变原始错误（只 warn）。
#[tokio::test]
pub(crate) async fn create_app_cleanup_failure_keeps_original_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    runtime.create_fails.store(true, Ordering::SeqCst);
    runtime.delete_fails.store(true, Ordering::SeqCst);
    let service = test_service(root.path(), runtime.clone()).await;
    let app_dir = root.path().join("appr2b");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");

    let error = service
        .create_app(create_request("appr2b"))
        .await
        .expect_err("create_app must fail");

    assert!(
        matches!(&error, AppOperationError::Backend(message) if message.contains("mock create_deployment failure")),
        "original error must not be masked by cleanup failure, got: {error}"
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

/// 回归（userapp_metadata）：update 不带 name（name 是"仅元数据"调用方常省略）
/// 不得清空已存业务名——否则 query name 过滤对该 app 永久失效。带 name 则覆盖。
#[tokio::test]
pub(crate) async fn update_app_without_name_keeps_metadata_name() {
    use crate::models::UpdateAppRequest;

    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime).await;
    // create_app 需要 code/release.lock.toml
    let app_dir = root.path().join("appmeta");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");

    let mut create = create_request("appmeta");
    create.name = "alpha".into();
    service.create_app(create).await.expect("create app");
    assert_eq!(
        service
            .metadata
            .lookup("appmeta")
            .await
            .expect("metadata query")
            .and_then(|m| m.name),
        Some("alpha".into()),
        "create records name"
    );

    let update_no_name = UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        name: None,
        image: Some("registry.example/app-runtime:v2".into()),
        env: None,
        secrets: None,
        resources: None,
        tenant_id: None,
        space_id: None,
        recycle_enabled: None,
        idle_timeout_seconds: None,
        expected_resource_version: None,
    };
    service
        .update_app("appmeta", update_no_name.clone())
        .await
        .expect("update without name");
    assert_eq!(
        service
            .metadata
            .lookup("appmeta")
            .await
            .expect("metadata query")
            .and_then(|m| m.name),
        Some("alpha".into()),
        "update without name must NOT clear recorded name"
    );

    let mut update_with_name = update_no_name;
    update_with_name.image = Some("registry.example/app-runtime:v3".into());
    update_with_name.name = Some("beta".into());
    service
        .update_app("appmeta", update_with_name)
        .await
        .expect("update with name");
    assert_eq!(
        service
            .metadata
            .lookup("appmeta")
            .await
            .expect("metadata query")
            .and_then(|m| m.name),
        Some("beta".into()),
        "explicit name overrides"
    );
}

/// update 与发布并发：发布锁被占 → 立即 409（不排队傻等 activate 的就绪窗口）。
#[tokio::test]
pub(crate) async fn update_app_conflicts_while_release_lock_held() {
    let root = tempfile::tempdir().expect("tempdir");
    let service = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let _publish_lock = service
        .acquire_process_release_lock("appbusy")
        .await
        .expect("operation lock");

    let request = UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        name: None,
        image: Some("registry.example/app-runtime:v2".into()),
        env: None,
        secrets: None,
        resources: None,
        tenant_id: None,
        space_id: None,
        recycle_enabled: None,
        idle_timeout_seconds: None,
        expected_resource_version: None,
    };
    let error = service
        .update_app("appbusy", request)
        .await
        .expect_err("update during publish must 409");
    assert!(
        matches!(error, AppOperationError::Conflict(_)),
        "got: {error}"
    );
}

/// update 前置：create 一个 running app（fetch_runtime_status 需要 Deployment
/// 存在），返回 service 与 runtime 句柄（resize/patch 调用断言用）。
async fn created_app_service(
    root: &std::path::Path,
    app_id: &str,
) -> (AppService, Arc<MockRuntime>) {
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root, runtime.clone()).await;
    let app_dir = root.join(app_id);
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");
    service
        .create_app(create_request(app_id))
        .await
        .expect("create app");
    (service, runtime)
}

fn update_request_with_storage(storage: Option<&str>) -> UpdateAppRequest {
    UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        name: None,
        image: Some("registry.example/app-runtime:v2".into()),
        env: None,
        secrets: None,
        resources: storage.map(|s| ResourceLimits {
            cpu: None,
            memory: None,
            storage: Some(s.to_string()),
            ephemeral_storage: None,
        }),
        tenant_id: None,
        space_id: None,
        recycle_enabled: None,
        idle_timeout_seconds: None,
        expected_resource_version: None,
    }
}

async fn admit_recovery_control(
    service: &AppService,
    app_id: &str,
    command: shared_types::UserAppControlCommand,
) -> shared_types::UserAppOperationRecord {
    let identity = service
        .metadata
        .store
        .get_application(app_id)
        .await
        .expect("identity query")
        .expect("identity");
    let operation_id = uuid::Uuid::new_v4().to_string();
    let admission = shared_types::UserAppAdmission {
        runtime_policy_on_success: None,
        app_id: app_id.into(),
        lifecycle_id: Some(identity.lifecycle_id),
        request_id: Some(operation_id.clone()),
        operation_id,
        request_fingerprint: "a".repeat(64),
        kind: command.kind(),
        command: Some(command),
        metadata: None,
    };
    match service
        .metadata
        .store
        .admit(&admission)
        .await
        .expect("pending admission")
    {
        shared_types::UserAppAdmissionOutcome::Accepted(record) => record,
        shared_types::UserAppAdmissionOutcome::Existing(_) => {
            panic!("fresh operation must be accepted")
        }
    }
}

#[tokio::test]
async fn pending_control_recovery_keeps_the_original_operation_and_executes_once() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recovercontrol").await;
    let pending = admit_recovery_control(
        &service,
        "recovercontrol",
        shared_types::UserAppControlCommand::Restart,
    )
    .await;
    assert!(
        service
            .resume_pending_control(&pending)
            .await
            .expect("recover pending")
    );
    assert!(
        !service
            .resume_pending_control(&pending)
            .await
            .expect("stale scan is ignored")
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    let completed = service
        .metadata
        .store
        .get_operation("recovercontrol", &pending.operation_id)
        .await
        .expect("operation query")
        .expect("operation");
    assert_eq!(
        completed.state,
        shared_types::UserAppOperationState::Succeeded
    );
    assert_eq!(completed.command, pending.command);
    assert_eq!(
        completed.checkpoint["target"]["context"]["operation_id"],
        pending.operation_id
    );
    assert_eq!(
        completed.checkpoint["target"]["context"]["lifecycle_id"],
        pending.lifecycle_id
    );
}

#[tokio::test]
async fn pending_control_recovery_does_not_take_over_a_running_executor() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recoverrunning").await;
    let pending = admit_recovery_control(
        &service,
        "recoverrunning",
        shared_types::UserAppControlCommand::Start { traffic: false },
    )
    .await;
    let claimed = OwnedOperation::claim_pending(service.metadata.store.clone(), pending.clone())
        .await
        .expect("claim")
        .expect("executor");
    assert!(
        !service
            .resume_pending_control(&pending)
            .await
            .expect("stale pending snapshot")
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    let running = service
        .metadata
        .store
        .get_operation("recoverrunning", &pending.operation_id)
        .await
        .expect("operation query")
        .expect("operation");
    assert_eq!(running.state, shared_types::UserAppOperationState::Running);
    assert_eq!(running.revision, pending.revision + 1);
    claimed
        .reject_without_mutation(&AppOperationError::Backend(
            "Test executor did not mutate runtime".into(),
        ))
        .await
        .expect("test cleanup");
}

#[tokio::test]
async fn traffic_rechecks_stale_local_block_but_respects_committed_manual_stop() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "stalewakeblock").await;
    let service = Arc::new(service);
    service.attach_activity_coordinator().expect("coordinator");
    service.activity.mark_wake_blocked("stalewakeblock");
    let result =
        shared_types::AppWakeControl::ensure_running(service.activity.as_ref(), "stalewakeblock")
            .await;
    assert_eq!(result, shared_types::WakeOutcome::AlreadyRunning);
    assert!(!service.activity.is_wake_blocked("stalewakeblock"));
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        0,
        "old local state does not restart a running application"
    );
    service
        .stop_app_controlled(
            "stalewakeblock",
            shared_types::UserAppControlRequest {
                lifecycle_id: None,
                request_id: Some("committed-manual-stop".into()),
            },
        )
        .await
        .expect("intentional stop");
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    let result =
        shared_types::AppWakeControl::ensure_running(service.activity.as_ref(), "stalewakeblock")
            .await;
    assert!(matches!(result, shared_types::WakeOutcome::Failed(_)));
    assert!(service.activity.is_wake_blocked("stalewakeblock"));
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        before,
        "committed manual stop is never overridden by traffic"
    );
}

#[tokio::test]
async fn pending_traffic_recovery_cannot_override_a_manual_stop() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recovermanual").await;
    service
        .stop_app_controlled(
            "recovermanual",
            shared_types::UserAppControlRequest {
                lifecycle_id: None,
                request_id: Some("manual-stop-before-recovery".into()),
            },
        )
        .await
        .expect("persist intentional stop");
    let prior_scale_calls = runtime.scale_calls.load(Ordering::SeqCst);
    let pending = admit_recovery_control(
        &service,
        "recovermanual",
        shared_types::UserAppControlCommand::Start { traffic: true },
    )
    .await;
    assert!(service.resume_pending_control(&pending).await.is_err());
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        prior_scale_calls,
        "traffic recovery does not start an intentionally stopped application"
    );
    let failed = service
        .metadata
        .store
        .get_operation("recovermanual", &pending.operation_id)
        .await
        .expect("operation query")
        .expect("operation");
    assert_eq!(failed.state, shared_types::UserAppOperationState::Failed);
    assert!(service.activity.is_wake_blocked("recovermanual"));
    assert!(failed.checkpoint.is_null());
}

#[tokio::test]
async fn stop_rejection_releases_ownership_but_uncertain_result_retains_it() {
    for status in [403, 408, 500] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "stopoutcome").await;
        runtime.stop_failure_status.store(status, Ordering::SeqCst);
        let request = shared_types::UserAppControlRequest {
            lifecycle_id: None,
            request_id: Some("stop-outcome-request".into()),
        };
        let error = service
            .stop_app_controlled("stopoutcome", request)
            .await
            .expect_err("injected failure");
        let rejected = status == 403;
        assert_eq!(
            matches!(error, AppOperationError::RuntimeRejected(_)),
            rejected
        );
        let record = service
            .metadata
            .store
            .get_operation_by_request("stopoutcome", "stop-outcome-request")
            .await
            .expect("operation query")
            .expect("operation");
        assert_eq!(
            record.state,
            if rejected {
                shared_types::UserAppOperationState::Failed
            } else {
                shared_types::UserAppOperationState::RecoveryRequired
            }
        );
        assert_eq!(service.activity.is_wake_blocked("stopoutcome"), !rejected);
        let next = service
            .try_acquire_process_release_lock("stopoutcome")
            .await;
        if rejected {
            next.expect("definitive rejection releases marker")
                .finish()
                .await
                .expect("release test lease");
        } else {
            assert!(
                next.is_err(),
                "uncertain stop cannot authorize another writer"
            );
        }
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn policy_control_is_durable_idempotent_and_does_not_restart_runtime() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "durablepolicy").await;
    let request = RecyclePolicyRequest {
        lifecycle_id: None,
        request_id: Some("policy-request".into()),
        recycle_enabled: Some(false),
        idle_timeout_seconds: Some(0),
        wake_on_traffic: Some(false),
    };
    service
        .set_recycle_policy("durablepolicy", request.clone())
        .await
        .expect("policy");
    service
        .set_recycle_policy("durablepolicy", request.clone())
        .await
        .expect("repeat");
    assert_eq!(runtime.policy_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
    let identity = service
        .metadata
        .store
        .get_application("durablepolicy")
        .await
        .expect("identity query")
        .expect("identity");
    assert_eq!(identity.runtime_policy.recycle_enabled, Some(false));
    let status = service
        .get_app("durablepolicy")
        .await
        .expect("policy readback");
    assert_eq!(status.recycle_enabled, Some(false));
    assert_eq!(status.idle_timeout_seconds, Some(0));
    assert_eq!(status.wake_on_traffic, Some(false));
    let mut changed = request;
    changed.wake_on_traffic = Some(true);
    assert!(
        service
            .set_recycle_policy("durablepolicy", changed)
            .await
            .is_err()
    );
    assert_eq!(runtime.policy_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn creation_commits_initial_runtime_policy() {
    let directory = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(directory.path(), runtime).await;
    let mut request = create_request("initialruntimepolicy");
    request.recycle_enabled = Some(false);
    request.idle_timeout_seconds = Some(1800);
    service.create_app(request).await.expect("create");
    let identity = service
        .metadata
        .store
        .get_application("initialruntimepolicy")
        .await
        .expect("read")
        .expect("identity");
    assert_eq!(
        identity.runtime_policy,
        shared_types::UserAppRuntimePolicy {
            recycle_enabled: Some(false),
            idle_timeout_seconds: Some(1800),
            wake_on_traffic: Some(true),
        }
    );
}

#[tokio::test]
async fn configuration_update_commits_new_policy_and_releases_operation_marker() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "updatepolicy").await;
    service
        .set_recycle_policy(
            "updatepolicy",
            RecyclePolicyRequest {
                lifecycle_id: None,
                request_id: Some("initial-policy".into()),
                recycle_enabled: Some(false),
                idle_timeout_seconds: Some(300),
                wake_on_traffic: Some(false),
            },
        )
        .await
        .expect("initial policy");
    let mut update = update_request_with_storage(None);
    update.request_id = Some("update-policy-request".into());
    update.recycle_enabled = Some(true);
    update.idle_timeout_seconds = Some(900);
    let result = service
        .update_app("updatepolicy", update.clone())
        .await
        .expect("configuration update");
    assert_eq!(result.recycle_enabled, Some(true));
    assert_eq!(result.idle_timeout_seconds, Some(900));
    assert_eq!(result.wake_on_traffic, Some(false));
    let calls = runtime.create_calls.load(Ordering::SeqCst);
    service
        .update_app("updatepolicy", update)
        .await
        .expect("exact replay after successful update");
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), calls);
    let next = service
        .acquire_process_release_lock("updatepolicy")
        .await
        .expect("successful update releases resource marker");
    next.finish().await.expect("release test lease");
}

#[tokio::test]
async fn pending_policy_recovery_applies_the_original_policy() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recoverpolicy").await;
    let policy = shared_types::UserAppRuntimePolicy {
        recycle_enabled: Some(false),
        idle_timeout_seconds: Some(450),
        wake_on_traffic: None,
    };
    let previous_wake = service
        .metadata
        .store
        .get_application("recoverpolicy")
        .await
        .expect("read previous policy")
        .expect("identity")
        .runtime_policy
        .wake_on_traffic;
    let pending = admit_recovery_control(
        &service,
        "recoverpolicy",
        shared_types::UserAppControlCommand::SetRecyclePolicy {
            policy: policy.clone(),
        },
    )
    .await;
    assert!(
        service
            .resume_pending_control(&pending)
            .await
            .expect("policy recovery")
    );
    assert_eq!(runtime.policy_calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        service
            .metadata
            .store
            .get_application("recoverpolicy")
            .await
            .expect("read")
            .expect("identity")
            .runtime_policy,
        shared_types::UserAppRuntimePolicy {
            wake_on_traffic: previous_wake,
            ..policy
        }
    );
}

/// update 带 resources.storage → resize_app_storage 收到扩容目标，update 整体成功。
#[tokio::test]
async fn update_commits_a_durable_operation_matching_runtime_context() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durableupdate").await;
    let mut request = update_request_with_storage(None);
    request.name = Some("updated-name".into());
    request.request_id = Some("update-request".into());
    service
        .update_app("durableupdate", request.clone())
        .await
        .expect("update");
    let calls = runtime.create_calls.load(Ordering::SeqCst);
    service
        .update_app("durableupdate", request.clone())
        .await
        .expect("exact retry");
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        calls,
        "completed retry must not reapply runtime configuration"
    );
    request.name = Some("different-intent".into());
    assert!(matches!(
        service.update_app("durableupdate", request).await,
        Err(AppOperationError::Conflict(_))
    ));
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), calls);
    let context = runtime
        .create_params_history
        .get("durableupdate")
        .and_then(|history| {
            history
                .last()
                .and_then(|params| params.execution_context.clone())
        })
        .expect("runtime received operation context");
    let target = runtime
        .create_params_history
        .get("durableupdate")
        .and_then(|history| {
            history
                .last()
                .and_then(|params| params.mutation_target.clone())
        })
        .expect("runtime received captured update identity");
    assert_eq!(target.context, context);
    let operation = service
        .metadata
        .store
        .get_operation("durableupdate", &context.operation_id)
        .await
        .expect("read operation")
        .expect("operation persisted");
    let stored_target: shared_types::UserAppMutationTarget =
        serde_json::from_value(operation.checkpoint["target"].clone())
            .expect("stored mutation identity");
    assert_eq!(stored_target, target);

    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
    assert_eq!(operation.lifecycle_id, context.lifecycle_id);
    assert_eq!(
        operation.executor_id.as_deref(),
        Some(context.executor_id.as_str())
    );
    assert_eq!(operation.kind, shared_types::UserAppOperationKind::Update);
    let queried = service
        .get_control_operation_by_request("durableupdate", "update-request")
        .await
        .expect("query by caller token")
        .expect("operation");
    assert_eq!(queried.operation_id, operation.operation_id);

    let app = service
        .metadata
        .store
        .get_application("durableupdate")
        .await
        .expect("read identity")
        .expect("identity");
    assert_eq!(app.name.as_deref(), Some("updated-name"));
    assert!(app.active_operations.is_empty());
}

#[tokio::test]
async fn explicit_stop_is_durable_idempotent_and_blocks_traffic_wake() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durablestop").await;
    let request = shared_types::UserAppControlRequest {
        lifecycle_id: None,
        request_id: Some("stop-request".into()),
    };
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    service
        .stop_app_controlled("durablestop", request.clone())
        .await
        .expect("stop");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert!(service.activity.is_wake_blocked("durablestop"));
    service
        .stop_app_controlled("durablestop", request.clone())
        .await
        .expect("exact retry");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    let operation = service
        .get_control_operation_by_request("durablestop", "stop-request")
        .await
        .expect("query")
        .expect("operation");
    assert_eq!(operation.kind, shared_types::UserAppOperationKind::Stop);
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
    let persisted = service
        .metadata
        .store
        .get_operation_by_request("durablestop", "stop-request")
        .await
        .expect("stored operation")
        .expect("receipt");
    let target: shared_types::UserAppMutationTarget =
        serde_json::from_value(persisted.checkpoint["target"].clone())
            .expect("persisted physical stop target");
    assert_eq!(target.context.operation_id, operation.operation_id);
    assert_eq!(target.context.lifecycle_id, persisted.lifecycle_id);
    assert!(!target.resource.uid.is_empty());
    assert_eq!(persisted.checkpoint["wake_on_traffic"], false);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn compute_delete_failure_keeps_wake_blocked_until_reconciliation() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "deletefence").await;
    runtime.delete_fails.store(true, Ordering::SeqCst);
    let result = service
        .delete_app_controlled(
            "deletefence",
            DeleteAppRequest {
                lifecycle_id: None,
                request_id: Some("delete-fence-request".into()),
                purge: Some(false),
                expected_resource_version: None,
            },
        )
        .await;
    assert!(result.is_err());
    assert!(service.activity.is_wake_blocked("deletefence"));
    assert!(runtime.deployments.contains_key("deletefence"));
    let operation = service
        .get_control_operation_by_request("deletefence", "delete-fence-request")
        .await
        .expect("query")
        .expect("operation");
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::RecoveryRequired
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    service.activity.forget_app("deletefence");
    service
        .rebuild_stopped_apps()
        .await
        .expect("restore activity from durable deletion");
    assert!(
        service.activity.is_wake_blocked("deletefence"),
        "a still-running old container cannot remove the deletion fence on restart"
    );
}

#[tokio::test]
async fn compute_delete_replays_after_runtime_disappears_and_preserves_lifecycle() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durabledelete").await;
    let before = service
        .metadata
        .store
        .get_application("durabledelete")
        .await
        .expect("read")
        .expect("identity");
    let request = DeleteAppRequest {
        request_id: Some("delete-request".into()),
        purge: Some(false),
        lifecycle_id: Some(before.lifecycle_id.clone()),
        expected_resource_version: None,
    };
    service
        .delete_app_controlled("durabledelete", request.clone())
        .await
        .expect("delete compute");
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    service
        .delete_app_controlled("durabledelete", request.clone())
        .await
        .expect("exact replay despite absent runtime");
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    let operation = service
        .get_control_operation_by_request("durabledelete", "delete-request")
        .await
        .expect("query")
        .expect("operation");
    assert_eq!(
        operation.kind,
        shared_types::UserAppOperationKind::DeleteCompute
    );
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
    let after = service
        .metadata
        .store
        .get_application("durabledelete")
        .await
        .expect("read")
        .expect("identity retained");
    assert_eq!(after.lifecycle_id, before.lifecycle_id);
    assert_eq!(after.state, shared_types::UserAppLifecycleState::Active);
    let mut another = request.clone();
    another.request_id = Some("deletealreadyabsent".into());
    service
        .delete_app_controlled("durabledelete", another)
        .await
        .expect("known lifecycle with absent compute is idempotent");
    let absent = service
        .get_control_operation_by_request("durabledelete", "deletealreadyabsent")
        .await
        .expect("query")
        .expect("record");
    assert_eq!(absent.state, shared_types::UserAppOperationState::Succeeded);
    let mut changed = request;
    changed.purge = Some(true);
    assert!(matches!(
        service
            .delete_app_controlled("durabledelete", changed)
            .await,
        Err(AppOperationError::Conflict(_))
    ));
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
pub(crate) async fn update_app_storage_resize_triggered() {
    let root = tempfile::tempdir().expect("tempdir");
    let (service, runtime) = created_app_service(root.path(), "appresize").await;
    let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

    service
        .update_app("appresize", update_request_with_storage(Some("200Gi")))
        .await
        .expect("update with storage");

    assert_eq!(
        runtime.resize_calls.get("appresize").map(|c| c.clone()),
        Some(vec!["200Gi".to_string()]),
        "resize target forwarded"
    );
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        create_calls_before + 1,
        "patch_deployment still applied after successful resize"
    );
}

/// 缩容拒绝（ShrinkRejected）→ update 整体 400 Validation，且 patch 不再执行
/// （resize 在 patch 之前——阻断顺序防"语义错误却滚动生效"）。
#[tokio::test]
pub(crate) async fn update_app_storage_shrink_rejected_blocks_update() {
    let root = tempfile::tempdir().expect("tempdir");
    let (service, runtime) = created_app_service(root.path(), "appshrink").await;
    *runtime.resize_outcome.lock().expect("outcome lock") =
        Some(StorageResizeOutcome::ShrinkRejected {
            current: "200Gi".into(),
            requested: "50Gi".into(),
        });
    let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

    let error = service
        .update_app("appshrink", update_request_with_storage(Some("50Gi")))
        .await
        .expect_err("shrink must be rejected");
    assert!(
        matches!(error, AppOperationError::Validation(_)),
        "got: {error}"
    );
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        create_calls_before,
        "patch_deployment must NOT run when resize rejected"
    );
}

/// resize 后端失败 → update 整体失败（storage 字段承诺生效，不静默降级）。
#[tokio::test]
pub(crate) async fn update_app_storage_resize_failure_blocks_update() {
    let root = tempfile::tempdir().expect("tempdir");
    let (service, runtime) = created_app_service(root.path(), "apprfail").await;
    runtime.resize_fails.store(true, Ordering::SeqCst);
    let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

    let error = service
        .update_app("apprfail", update_request_with_storage(Some("200Gi")))
        .await
        .expect_err("resize failure must block update");
    assert!(
        matches!(error, AppOperationError::Backend(_)),
        "got: {error}"
    );
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        create_calls_before,
        "patch_deployment must NOT run when resize failed"
    );
}

/// update 不带 resources.storage（None 或无 storage 字段）→ resize 不触发。
#[tokio::test]
pub(crate) async fn update_app_without_storage_skips_resize() {
    let root = tempfile::tempdir().expect("tempdir");
    let (service, runtime) = created_app_service(root.path(), "appnosize").await;

    service
        .update_app("appnosize", update_request_with_storage(None))
        .await
        .expect("update without storage");

    assert!(
        runtime.resize_calls.is_empty(),
        "resize must not be called without storage"
    );
}

/// query_apps 分页校验：page<1 / page_size∉[1,100] → 400（对齐 query_storage 与
/// publish tasks 口径；此前静默 clamp，超大 page 在 debug 构建乘法溢出 panic）。
#[tokio::test]
pub(crate) async fn query_apps_rejects_invalid_pagination_and_sort() {
    let service = test_service(
        tempfile::tempdir().expect("tempdir").path(),
        Arc::new(MockRuntime::default()),
    )
    .await;
    for (page, page_size) in [(0u32, 20u32), (1, 0), (1, 101)] {
        let request = QueryAppsRequest {
            page: Some(page),
            page_size: Some(page_size),
            ..QueryAppsRequest::default()
        };
        let error = service
            .query_apps(request)
            .await
            .expect_err("invalid pagination must 400");
        assert!(
            matches!(error, AppOperationError::Validation(_)),
            "page={page} page_size={page_size}: {error}"
        );
    }
    let request = QueryAppsRequest {
        sort_by: Some("bogus".into()),
        ..QueryAppsRequest::default()
    };
    let error = service
        .query_apps(request)
        .await
        .expect_err("invalid sort_by must 400");
    assert!(matches!(error, AppOperationError::Validation(_)));
}

/// 三档删除语义：delete(purge=true) 销毁存储但**保留**元数据行（误删找回）；
/// 仅独立 storage/destroy 接口删行。
#[tokio::test]
pub(crate) async fn production_storage_destroy_preserves_application_identity() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime).await;
    let persistence = service.metadata.store.clone();
    service
        .set_dev_cleanup(Arc::new(StubDevCleanup::default()))
        .expect("inject dev cleanup");
    let app_dir = root.path().join("apppurge");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");

    let mut create = create_request("apppurge");
    create.name = "keep-me".into();
    service.create_app(create).await.expect("create app");
    assert!(
        service
            .metadata
            .lookup("apppurge")
            .await
            .expect("metadata query")
            .is_some()
    );

    service
        .delete_app("apppurge", true, None)
        .await
        .expect("purge delete");
    assert!(
        service
            .metadata
            .lookup("apppurge")
            .await
            .expect("metadata query")
            .is_some(),
        "purge must retain metadata row (three-tier contract)"
    );
    assert!(
        persistence
            .list_applications(None, 256)
            .await
            .expect("persisted")
            .iter()
            .any(|r| r.app_id == "apppurge"),
        "PG row retained after purge"
    );

    service
        .destroy_app_storage(shared_types::UserappStage::Prod, "apppurge", "apppurge")
        .await
        .expect("explicit destroy");
    assert!(
        service
            .metadata
            .lookup("apppurge")
            .await
            .expect("metadata query")
            .is_some(),
        "storage destruction must retain application identity"
    );
}

/// query_apps 的 name/created_at 过滤:纯内存模式（无 metadata 持久化）维持忽略
/// （全量返回,旧行为）;注入持久化（PG 模式同构）后经内存 join 生效。
#[tokio::test]
pub(crate) async fn query_apps_name_filter_respects_metadata_mode() {
    use container_runtime_api::DeploymentStatus;

    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    for app_id in ["appalpha", "appbeta"] {
        runtime.deployments.insert(
            app_id.into(),
            DeploymentStatus {
                app_id: app_id.into(),
                ..Default::default()
            },
        );
    }
    let service = test_service(root.path(), runtime.clone()).await;
    let by_name = |name: &str| QueryAppsRequest {
        page: None,
        page_size: None,
        filters: Some(AppFilters {
            status: None,
            name: Some(name.into()),
            app_ids: None,
            created_at: None,
        }),
        sort_by: None,
        sort_order: None,
    };

    let alpha = service
        .metadata
        .store
        .ensure_identity("appalpha")
        .await
        .unwrap();
    // Distinct actual registration times, without an import-only backdoor.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    let beta = service
        .metadata
        .store
        .ensure_identity("appbeta")
        .await
        .unwrap();
    assert!(beta.created_at > alpha.created_at);
    for (row, name) in [(&alpha, "alpha"), (&beta, "beta")] {
        service
            .metadata
            .store
            .patch_metadata(&shared_types::UserAppMetadataPatch {
                app_id: row.app_id.clone(),
                lifecycle_id: row.lifecycle_id.clone(),
                expected_revision: row.metadata_revision,
                name: Some(Some(name.into())),
                tenant_id: None,
                space_id: None,
            })
            .await
            .unwrap();
    }

    let response = service.query_apps(by_name("alpha")).await.expect("query");
    assert_eq!(response.items.len(), 1, "name filter now effective");
    assert_eq!(response.items[0].app_id, "appalpha");

    // Use the first actual registration timestamp; beta lies outside the range.
    let response = service
        .query_apps(QueryAppsRequest {
            page: None,
            page_size: None,
            filters: Some(AppFilters {
                status: None,
                name: None,
                app_ids: None,
                created_at: Some(DateRange {
                    start: (alpha.created_at - chrono::Duration::seconds(1)).to_rfc3339(),
                    end: alpha.created_at.to_rfc3339(),
                }),
            }),
            sort_by: None,
            sort_order: None,
        })
        .await
        .expect("query by range");
    assert_eq!(response.items.len(), 1);
    assert_eq!(response.items[0].app_id, "appalpha");
}

// ===== purge_app（彻底删除：dev+prod 容器/PVC + 元数据行，幂等）=====

/// purge_app 测试基座：直构造 service + 注入 dev cleanup + 预置元数据行。
/// 返回 (service, runtime, dev_stub, persistence)。
async fn purge_test_service(
    root: &std::path::Path,
    runtime: Arc<MockRuntime>,
) -> (
    AppService,
    Arc<MockRuntime>,
    Arc<StubDevCleanup>,
    Arc<dyn shared_types::UserAppLifecycleStore>,
) {
    let service = test_service(root, runtime.clone()).await;
    let persistence = service.metadata.store.clone();
    let dev = Arc::new(StubDevCleanup::default());
    *service.dev_cleanup.write().expect("dev_cleanup lock") =
        Some(dev.clone() as Arc<dyn shared_types::UserappDevCleanup>);
    (service, runtime, dev, persistence)
}

/// 预置"app 存在"（deployments 直插——purge_app 不走 build_container_params，
/// 无需 create_app 全链路）+ 元数据行。
async fn seed_running_app(service: &AppService, runtime: &MockRuntime, app_id: &str) {
    runtime.deployments.insert(
        app_id.to_string(),
        DeploymentStatus {
            app_id: app_id.to_string(),
            replicas: 1,
            ready_replicas: 1,
            phase: "Running".into(),
            ..Default::default()
        },
    );
    service
        .metadata
        .record(app_id, Some("purgeme".into()), None, None)
        .await
        .expect("metadata registration");
}

/// happy path：计算面 + prod PVC + dev 环境 + 元数据行全删。
#[tokio::test]
pub(crate) async fn purge_app_deletes_everything_and_metadata_row() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let (service, runtime, dev, persistence) =
        purge_test_service(root.path(), runtime.clone()).await;
    seed_running_app(&service, &runtime, "appp1").await;

    service.purge_app("appp1").await.expect("purge app");

    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    assert!(runtime.deployments.get("appp1").is_none());
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
    assert!(
        service
            .metadata
            .lookup("appp1")
            .await
            .expect("metadata query")
            .is_none(),
        "purge_app must delete metadata row (permanent delete)"
    );
    assert!(
        persistence
            .list_applications(None, 256)
            .await
            .expect("persisted")
            .iter()
            .any(
                |r| r.app_id == "appp1" && r.state == shared_types::UserAppLifecycleState::Deleted
            ),
        "PG row deleted by purge_app"
    );
}

/// 幂等：app 不存在 = 任务成功，其余清理步骤照做（重入收敛）。
#[tokio::test]
pub(crate) async fn purge_app_on_absent_app_is_idempotent_and_still_cleans_rest() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let (service, runtime, dev, _persistence) =
        purge_test_service(root.path(), runtime.clone()).await;
    // 不预置 deployment（计算面缺席），仅预置元数据行使断言非空洞
    service
        .metadata
        .record("appp2", None, None, None)
        .await
        .expect("metadata registration");

    service.purge_app("appp2").await.expect("idempotent purge");

    assert_eq!(
        runtime.delete_calls.load(Ordering::SeqCst),
        0,
        "compute plane absent → skip delete_deployment"
    );
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
    assert!(
        service
            .metadata
            .lookup("appp2")
            .await
            .expect("metadata query")
            .is_none(),
        "metadata row still deleted on absent compute plane"
    );
}

/// dev 回收失败透传（确定性，区别于 delete_app purge 分支的 best-effort），
/// 元数据行保留（幂等重试收敛）。
#[tokio::test]
pub(crate) async fn purge_app_dev_cleanup_failure_propagates_and_keeps_metadata() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let (service, runtime, dev, _persistence) =
        purge_test_service(root.path(), runtime.clone()).await;
    seed_running_app(&service, &runtime, "appp3").await;
    dev.fails.store(true, Ordering::SeqCst);

    let error = service.purge_app("appp3").await.expect_err("must fail");

    assert!(matches!(error, AppOperationError::Backend(_)));
    assert!(
        error
            .to_string()
            .contains("destroy captured userapp dev resources"),
        "dev cleanup failure message, got: {error}"
    );
    // 前置步骤已执行（重试时幂等跳过/重做）
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
    assert!(
        service
            .metadata
            .lookup("appp3")
            .await
            .expect("metadata query")
            .is_some(),
        "metadata row kept for retry convergence"
    );
}

#[tokio::test]
async fn failed_purge_retains_both_runtime_and_registry_deletion_receipts() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime, dev, _) =
        purge_test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let app_id = "purgereceipts";
    seed_running_app(&service, &runtime, app_id).await;
    dev.fails.store(true, Ordering::SeqCst);
    assert!(service.purge_app(app_id).await.is_err());
    let identity = service
        .metadata
        .store
        .get_application(app_id)
        .await
        .expect("identity query")
        .expect("identity");
    let operation = service
        .metadata
        .store
        .get_operation(
            app_id,
            identity
                .active_operations
                .slot(shared_types::UserAppOperationScope::Application)
                .map(String::as_str)
                .expect("unfinished purge"),
        )
        .await
        .expect("operation query")
        .expect("operation");
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::RecoveryRequired
    );
    let checkpoint: shared_types::UserAppDeletionCheckpoint =
        serde_json::from_value(operation.checkpoint.clone()).expect("typed checkpoint");
    checkpoint
        .validate_operation(&operation)
        .expect("checkpoint belongs to original operation");
    assert_eq!(
        checkpoint.stage,
        shared_types::UserAppDeletionStage::ProductionStorageRemoved,
        "failed dev cleanup retains the last confirmed boundary"
    );
    assert!(
        service.activity.is_wake_blocked(app_id),
        "compute removal cannot unblock wake before purge commits"
    );
    service.activity.forget_app(app_id);
    service
        .rebuild_stopped_apps()
        .await
        .expect("restore fence for absent compute");
    assert!(
        service.activity.is_wake_blocked(app_id),
        "pending purge remains fenced even without a runtime list entry"
    );
    let mut replacement = operation.clone();
    replacement.lifecycle_id = "replacement-lifecycle".into();
    assert!(checkpoint.validate_operation(&replacement).is_err());
    let mut incomplete = checkpoint.clone();
    incomplete.development = None;
    assert!(incomplete.validate().is_err());
    let mut future_version = checkpoint.clone();
    future_version.schema_version = 2;
    assert!(future_version.validate().is_err());
    let mut missing_version = checkpoint.clone();
    missing_version
        .production
        .resources
        .push(shared_types::AppResourceIdentity {
            kind: shared_types::AppResourceKind::Deployment,
            name: "deployment".into(),
            uid: "uid".into(),
            resource_version: None,
        });
    assert!(missing_version.validate().is_err());
    let mut old_format = operation.checkpoint.clone();
    old_format
        .as_object_mut()
        .expect("object")
        .remove("context");
    assert!(
        serde_json::from_value::<shared_types::UserAppDeletionCheckpoint>(old_format).is_err(),
        "missing identity cannot be defaulted for recovery"
    );
    let production: shared_types::AppDeletionSnapshot =
        serde_json::from_value(operation.checkpoint["production"].clone())
            .expect("production receipt");
    assert_eq!(production.app_id, app_id);
    let development: shared_types::UserappDevDeletionReceipt =
        serde_json::from_value(operation.checkpoint["development"].clone())
            .expect("development receipt");
    assert_eq!(
        development,
        crate::test_support::dev_deletion_receipt(app_id)
    );
    assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
}

/// 状态查询失败不得当成"不存在"（对齐 fetch_runtime_status_or_err 两态分类）：
/// 透传 Backend，且未执行任何删除步骤。
#[tokio::test]
pub(crate) async fn purge_app_query_failure_propagates_not_treated_as_absent() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let (service, runtime, dev, _persistence) =
        purge_test_service(root.path(), runtime.clone()).await;
    seed_running_app(&service, &runtime, "appp4").await;
    runtime.status_fails.store(1, Ordering::SeqCst);

    let error = service.purge_app("appp4").await.expect_err("must fail");

    assert!(matches!(error, AppOperationError::Backend(_)));
    assert!(
        error.to_string().contains("capture purge resources"),
        "query failure message, got: {error}"
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    assert_eq!(dev.calls.load(Ordering::SeqCst), 0);
    assert!(
        service
            .metadata
            .lookup("appp4")
            .await
            .expect("metadata query")
            .is_some()
    );
}

/// dev_cleanup 未注入 → 硬错（确定性语义；对齐独立 dev destroy），元数据行保留。
#[tokio::test]
pub(crate) async fn purge_app_without_dev_cleanup_injected_is_hard_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    // 不注入 dev_cleanup（test_service 默认 None）
    let service = test_service(root.path(), runtime.clone()).await;
    seed_running_app(&service, &runtime, "appp5").await;

    let error = service.purge_app("appp5").await.expect_err("must fail");

    assert!(matches!(error, AppOperationError::Backend(_)));
    assert!(
        error.to_string().contains("dev cleanup not injected"),
        "not-injected message, got: {error}"
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    assert!(
        service
            .metadata
            .lookup("appp5")
            .await
            .expect("metadata query")
            .is_some()
    );
}

#[tokio::test]
async fn rejected_delete_version_releases_kubernetes_operation_before_return() {
    let root = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    runtime.deployments.insert(
        "versionlease".into(),
        DeploymentStatus {
            resource_version: Some("2".into()),
            ..Default::default()
        },
    );
    let mut service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .store
        .ensure_identity("versionlease")
        .await
        .expect("owner identity");
    service.config.access_mode = AppAccessMode::Kubernetes;
    assert!(matches!(
        service.delete_app("versionlease", false, Some("1")).await,
        Err(AppOperationError::Conflict(_))
    ));
    assert!(!runtime.lease_held.load(Ordering::SeqCst));
    service
        .acquire_process_release_lock("versionlease")
        .await
        .expect("next operation admitted")
        .finish()
        .await
        .expect("release");
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cancelled_docker_mutation_blocks_later_deletion_before_side_effects() {
    let root = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .store
        .ensure_identity("cancelledwriter")
        .await
        .expect("owner identity");
    let operation = service
        .acquire_process_release_lock("cancelledwriter")
        .await
        .expect("lease");
    operation.mark_mutating().expect("durable mutation marker");
    drop(operation);
    assert!(matches!(
        service.delete_app("cancelledwriter", false, None).await,
        Err(AppOperationError::Conflict(_))
    ));
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_lock_prefix_keeps_builder_shaped_ids_separate() {
    let root = tempfile::tempdir().expect("directory");
    let service = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let guard = service
        .acquire_process_release_lock("builderfoo")
        .await
        .expect("prod lease");
    let directory =
        std::path::Path::new(&service.config.operation_lock_root).join(".app-operation-locks");
    assert!(directory.join("prod-builderfoo.lock").exists());
    assert!(!directory.join("builderfoo.lock").exists());
    guard.finish().await.expect("release");
}

#[tokio::test]
async fn create_reserved_env_rejection_does_not_provision_or_retain_ownership() {
    for kubernetes in [false, true] {
        let root = tempfile::tempdir().expect("directory");
        let runtime = Arc::new(MockRuntime::default());
        let mut service = test_service(root.path(), runtime.clone()).await;
        if kubernetes {
            service.config.access_mode = AppAccessMode::Kubernetes;
        }
        let mut request = create_request("invalidenv");
        request.env = Some(std::collections::HashMap::from([(
            "RCODER_PINGAP_VERSION".into(),
            "forged".into(),
        )]));
        assert!(matches!(
            service.create_app(request).await,
            Err(AppOperationError::Validation(_))
        ));
        assert_eq!(runtime.ensure_workspace_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        service
            .try_acquire_process_release_lock("invalidenv")
            .await
            .expect("rejection releases ownership")
            .finish()
            .await
            .expect("release next operation");
    }
}

#[tokio::test]
async fn update_reserved_secrets_rejects_before_mutation_and_releases_ownership() {
    for kubernetes in [false, true] {
        for key in [
            shared_types::APP_DEPLOY_OPERATION_ID,
            shared_types::APP_DEPLOY_GENERATION_ID,
            "APP_CLI_DEPLOY_TOKEN",
        ] {
            let root = tempfile::tempdir().expect("directory");
            let runtime = Arc::new(MockRuntime::default());
            let mut service = test_service(root.path(), runtime.clone()).await;
            if kubernetes {
                service.config.access_mode = AppAccessMode::Kubernetes;
            }
            runtime.deployments.insert(
                "reservedsecret".into(),
                DeploymentStatus {
                    app_id: "reservedsecret".into(),
                    phase: "Running".into(),
                    resource_version: Some("before".into()),
                    ..Default::default()
                },
            );
            let mut request = update_request_with_storage(Some("200Gi"));
            request.secrets = Some(std::collections::HashMap::from([(
                key.into(),
                "forged".into(),
            )]));
            let error = service
                .update_app("reservedsecret", request)
                .await
                .expect_err("platform identity cannot be injected through secrets");
            assert!(matches!(error, AppOperationError::Validation(_)), "{error}");
            assert!(error.to_string().contains(key));
            assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.ensure_workspace_calls.load(Ordering::SeqCst), 0);
            assert!(runtime.resize_calls.is_empty());
            assert!(runtime.create_params_history.is_empty());
            assert_eq!(
                runtime
                    .deployments
                    .get("reservedsecret")
                    .unwrap()
                    .resource_version
                    .as_deref(),
                Some("before"),
            );
            service
                .try_acquire_process_release_lock("reservedsecret")
                .await
                .expect("validation failure releases ownership")
                .finish()
                .await
                .expect("release next operation");
        }
    }
}

#[tokio::test]
async fn failed_update_preparation_releases_lease_for_next_update() {
    let root = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    runtime.deployments.insert(
        "prepareretry".into(),
        DeploymentStatus {
            app_id: "prepareretry".into(),
            phase: "Running".into(),
            pod_ip: Some("10.0.0.1".into()),
            ..Default::default()
        },
    );
    runtime
        .patch_preparation_fails
        .store(true, Ordering::SeqCst);
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .store
        .ensure_identity("prepareretry")
        .await
        .expect("authoritative application identity");
    let request = || UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        image: Some("unavailable:image".into()),
        name: None,
        env: None,
        secrets: None,
        resources: None,
        tenant_id: None,
        space_id: None,
        recycle_enabled: None,
        idle_timeout_seconds: None,
        expected_resource_version: None,
    };
    for _ in 0..2 {
        let error = service
            .update_app("prepareretry", request())
            .await
            .expect_err("preparation fails");
        assert!(matches!(error, AppOperationError::Backend(_)));
        assert!(error.to_string().contains("image preparation failed"));
    }
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[cfg(test)]
mod purge_http_cancellation {
    use super::*;
    use std::time::Duration;
    use tokio::sync::Notify;

    struct ControlledCleanup {
        app_id: String,
        entered: Arc<Notify>,
        release: Arc<Notify>,
        completed: Arc<Notify>,
        outcome: u8,
    }
    #[async_trait::async_trait]
    impl shared_types::UserappDevCleanup for ControlledCleanup {
        async fn capture_with_lease(
            &self,
            app_id: &str,
            lease: Box<dyn shared_types::AppOperationLease>,
        ) -> Result<Box<dyn shared_types::UserappDevDeletion>, String> {
            let captured = self.capture(app_id).await;
            lease.release().await?;
            captured
        }

        async fn capture(
            &self,
            app_id: &str,
        ) -> Result<Box<dyn shared_types::UserappDevDeletion>, String> {
            Ok(Box::new(Self {
                app_id: app_id.into(),
                entered: self.entered.clone(),
                release: self.release.clone(),
                completed: self.completed.clone(),
                outcome: self.outcome,
            }))
        }
    }
    #[async_trait::async_trait]
    impl shared_types::UserappDevDeletion for ControlledCleanup {
        fn receipt(&self) -> shared_types::UserappDevDeletionReceipt {
            crate::test_support::dev_deletion_receipt(&self.app_id)
        }
        async fn cleanup(self: Box<Self>) -> Result<(), String> {
            tokio::spawn(async move {
                self.entered.notify_one();
                self.release.notified().await;
                self.completed.notify_one();
                match self.outcome {
                    0 => Ok(()),
                    1 => Err("runtime deletion outcome is unknown".into()),
                    _ => panic!("controlled deletion worker panic"),
                }
            })
            .await
            .map_err(|error| format!("cleanup worker failed: {error}"))?
        }
    }

    async fn cancelled_purge(outcome: u8) {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let (service, runtime, _, persistence) = purge_test_service(root.path(), runtime).await;
        let app_id = "cancelledhttppurge";
        seed_running_app(&service, &runtime, app_id).await;
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let completed = Arc::new(Notify::new());
        *service.dev_cleanup.write().expect("cleanup lock") = Some(Arc::new(ControlledCleanup {
            app_id: app_id.into(),
            entered: entered.clone(),
            release: release.clone(),
            completed: completed.clone(),
            outcome,
        }));
        let service = Arc::new(service);
        let state = Arc::new(crate::handlers::AppManagerState {
            app_service: service.clone(),
            http_client: reqwest::Client::builder()
                .no_proxy()
                .build()
                .expect("HTTP client"),
        });
        let caller = tokio::spawn(async move {
            crate::handlers::purge_app(
                axum::extract::State(state),
                axum::extract::Path(app_id.into()),
                Ok(axum::Json(PurgeAppRequest {
                    lifecycle_id: None,
                    request_id: Some("cancelled-purge-request".into()),
                })),
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(3), entered.notified())
            .await
            .expect("entered deletion");
        caller.abort();
        assert!(caller.await.expect_err("caller cancelled").is_cancelled());
        release.notify_one();
        tokio::time::timeout(Duration::from_secs(3), completed.notified())
            .await
            .expect("owned deletion completed");
        // The process lock is a deterministic completion barrier for the outer
        // purge: its guard must survive caller cancellation until all tail work.
        let next = tokio::time::timeout(
            Duration::from_secs(3),
            service.acquire_process_release_lock(app_id),
        )
        .await
        .expect("operation ended");
        let marker = root
            .path()
            .join(".app-operation-locks/prod-cancelledhttppurge.lock");

        if outcome == 0 {
            let next = next.expect("completed purge must release durable mutation marker");
            next.finish()
                .await
                .expect("release new read-only operation");
            assert!(
                service
                    .metadata
                    .lookup(app_id)
                    .await
                    .expect("metadata query")
                    .is_none(),
                "metadata tail must complete"
            );
            assert!(
                persistence
                    .list_applications(None, 256)
                    .await
                    .expect("metadata persistence")
                    .iter()
                    .any(|row| row.app_id == app_id
                        && row.state == shared_types::UserAppLifecycleState::Deleted)
            );
            assert!(std::fs::read(marker).expect("marker").is_empty());
        } else {
            assert!(next.is_err(), "unknown deletion must not release marker");
            assert!(
                service
                    .metadata
                    .lookup(app_id)
                    .await
                    .expect("metadata query")
                    .is_some()
            );
            assert!(!std::fs::read(marker).expect("marker").is_empty());
        }
    }
    #[tokio::test]
    async fn caller_cancelled_success_finishes_metadata_and_marker() {
        cancelled_purge(0).await;
    }
    #[tokio::test]
    async fn caller_cancelled_unknown_error_preserves_marker() {
        cancelled_purge(1).await;
    }
    #[tokio::test]
    async fn caller_cancelled_worker_panic_preserves_marker() {
        cancelled_purge(2).await;
    }
}

#[tokio::test]
async fn full_delete_deduplicates_and_rejects_old_lifecycle_after_recreation() {
    let root = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    let (service, runtime, dev, store) = purge_test_service(root.path(), runtime).await;
    let app_id = "purgerequestfence";
    seed_running_app(&service, &runtime, app_id).await;
    let original = store
        .get_application(app_id)
        .await
        .expect("read")
        .expect("identity");
    let request = shared_types::UserAppControlRequest {
        lifecycle_id: Some(original.lifecycle_id.clone()),
        request_id: Some("full-delete-request".into()),
    };
    service
        .purge_app_controlled(app_id, request.clone())
        .await
        .expect("delete");
    service
        .purge_app_controlled(app_id, request.clone())
        .await
        .expect("replay");
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
    let operation = store
        .get_operation_by_request(app_id, "full-delete-request")
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
    let replacement = store
        .recreate(app_id, &original.lifecycle_id, "rebuild-request")
        .await
        .expect("recreate");
    assert_ne!(original.lifecycle_id, replacement.lifecycle_id);
    assert!(service.purge_app_controlled(app_id, request).await.is_err());
    assert!(
        service.purge_app(app_id).await.is_err(),
        "missing lifecycle cannot authorize replacement deletion"
    );
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store
            .get_application(app_id)
            .await
            .expect("read")
            .expect("replacement")
            .state,
        shared_types::UserAppLifecycleState::Active
    );
}

#[tokio::test]
async fn creation_records_lifecycle_execution_and_replays_without_new_resources() {
    let root = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    let mut request = create_request("durablecreate");
    request.request_id = Some("create-request".into());
    service.create_app(request.clone()).await.expect("create");
    let context = {
        let history = runtime
            .create_params_history
            .get("durablecreate")
            .expect("create parameters");
        assert_eq!(history.len(), 1);
        history[0]
            .execution_context
            .clone()
            .expect("execution context")
    };
    context
        .validate_identity("durablecreate")
        .expect("valid credential");
    let persisted = service
        .metadata
        .store
        .get_operation_by_request("durablecreate", "create-request")
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(persisted.operation_id, context.operation_id);
    assert_eq!(persisted.lifecycle_id, context.lifecycle_id);
    assert_eq!(persisted.kind, shared_types::UserAppOperationKind::Create);
    assert_eq!(
        persisted.state,
        shared_types::UserAppOperationState::Succeeded
    );
    assert!(persisted.checkpoint.get("resource").is_some());
    service
        .create_app(request.clone())
        .await
        .expect("exact replay");
    assert_eq!(
        runtime
            .create_params_history
            .get("durablecreate")
            .expect("history")
            .len(),
        1
    );
    request.name = "changed-intent".into();
    assert!(service.create_app(request).await.is_err());
    assert_eq!(
        runtime
            .create_params_history
            .get("durablecreate")
            .expect("history")
            .len(),
        1
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn storage_expansion_receipt_is_bound_to_the_update_operation() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "resizereceipt").await;
    let request: UpdateAppRequest = serde_json::from_value(serde_json::json!({
        "user_id":"u-test", "request_id":"resize-operation", "resources":{"storage":"200Gi"},
        "image":"registry.example/app-runtime:v2"
    }))
    .expect("request");
    service
        .update_app("resizereceipt", request)
        .await
        .expect("update");
    let operation = service
        .metadata
        .store
        .get_operation_by_request("resizereceipt", "resize-operation")
        .await
        .expect("read")
        .expect("operation");
    let target: shared_types::UserAppStorageResizeTarget =
        serde_json::from_value(operation.checkpoint["storage_target"].clone())
            .expect("storage checkpoint");
    assert_eq!(target.context.operation_id, operation.operation_id);
    assert_eq!(target.context.lifecycle_id, operation.lifecycle_id);
    assert_eq!(
        target.resource.kind,
        shared_types::AppResourceKind::PersistentVolumeClaim
    );
    assert!(!target.resource.uid.is_empty());
    assert_eq!(target.current_size, "100Gi");
    assert_eq!(operation.checkpoint["requested_storage_size"], "200Gi");
    assert_eq!(
        runtime
            .resize_calls
            .get("resizereceipt")
            .expect("resize history")
            .as_slice(),
        &["200Gi".to_owned()]
    );
}

#[tokio::test]
async fn controlled_start_persists_target_and_replays_without_another_runtime_write() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durablestart").await;
    service.stop_app("durablestart").await.expect("stop");
    let request = shared_types::UserAppControlRequest {
        lifecycle_id: None,
        request_id: Some("start-request".into()),
    };
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    service
        .start_app_controlled("durablestart", request.clone())
        .await
        .expect("start");
    assert!(!service.activity.is_wake_blocked("durablestart"));
    let operation = service
        .metadata
        .store
        .get_operation_by_request("durablestart", "start-request")
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(operation.kind, shared_types::UserAppOperationKind::Start);
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
    let target: shared_types::UserAppMutationTarget =
        serde_json::from_value(operation.checkpoint["target"].clone())
            .expect("persisted start identity");
    assert_eq!(target.context.operation_id, operation.operation_id);
    assert_eq!(target.context.lifecycle_id, operation.lifecycle_id);
    service
        .start_app_controlled("durablestart", request.clone())
        .await
        .expect("replay");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    let mut stale = request;
    stale.lifecycle_id = Some("obsolete-life".into());
    assert!(
        service
            .start_app_controlled("durablestart", stale)
            .await
            .is_err()
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn controlled_restart_deduplicates_and_is_distinct_from_start() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durablerestart").await;
    let request = shared_types::UserAppControlRequest {
        lifecycle_id: None,
        request_id: Some("restart-request".into()),
    };
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    service
        .restart_app_controlled("durablerestart", request.clone())
        .await
        .expect("restart");
    let operation = service
        .metadata
        .store
        .get_operation_by_request("durablerestart", "restart-request")
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(operation.kind, shared_types::UserAppOperationKind::Restart);
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
    let target: shared_types::UserAppMutationTarget =
        serde_json::from_value(operation.checkpoint["target"].clone()).expect("restart receipt");
    assert_eq!(target.context.operation_id, operation.operation_id);
    service
        .restart_app_controlled("durablerestart", request.clone())
        .await
        .expect("replay");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert!(
        service
            .start_app_controlled("durablerestart", request.clone())
            .await
            .is_err(),
        "same request key cannot change operation kind"
    );
    let mut stale = request;
    stale.lifecycle_id = Some("obsolete-life".into());
    assert!(
        service
            .restart_app_controlled("durablerestart", stale)
            .await
            .is_err()
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
async fn explicit_retry_checks_owner_lifecycle_and_revision_before_execution() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "retrycontrol").await;
    let pending = admit_recovery_control(
        &service,
        "retrycontrol",
        shared_types::UserAppControlCommand::Restart,
    )
    .await;
    let request = shared_types::UserAppRetryRequest {
        lifecycle_id: pending.lifecycle_id.clone(),
        expected_revision: pending.revision,
    };
    for invalid in [
        shared_types::UserAppRetryRequest {
            lifecycle_id: "old-lifecycle".into(),
            ..request.clone()
        },
        shared_types::UserAppRetryRequest {
            expected_revision: pending.revision + 1,
            ..request.clone()
        },
    ] {
        assert!(
            service
                .retry_control_operation("retrycontrol", &pending.operation_id, invalid)
                .await
                .is_err()
        );
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .metadata
                .store
                .get_operation("retrycontrol", &pending.operation_id)
                .await
                .expect("read"),
            Some(pending.clone())
        );
    }
    for _ in 0..2 {
        let result = service
            .retry_control_operation("retrycontrol", &pending.operation_id, request.clone())
            .await
            .expect("retry or observe success");
        assert_eq!(result.operation_id, pending.operation_id);
        assert_eq!(result.state, shared_types::UserAppOperationState::Succeeded);
    }
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn explicit_retry_cannot_take_over_a_claimed_operation() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "retryclaimed").await;
    let pending = admit_recovery_control(
        &service,
        "retryclaimed",
        shared_types::UserAppControlCommand::Restart,
    )
    .await;
    let _claimed = OwnedOperation::claim_pending(service.metadata.store.clone(), pending.clone())
        .await
        .expect("claim")
        .expect("executor");
    let before = service
        .metadata
        .store
        .get_operation("retryclaimed", &pending.operation_id)
        .await
        .expect("read")
        .expect("operation");
    let result = service
        .retry_control_operation(
            "retryclaimed",
            &pending.operation_id,
            shared_types::UserAppRetryRequest {
                lifecycle_id: pending.lifecycle_id,
                expected_revision: before.revision,
            },
        )
        .await;
    assert!(matches!(result, Err(AppOperationError::InvalidState(_))));
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        service
            .metadata
            .store
            .get_operation("retryclaimed", &pending.operation_id)
            .await
            .expect("read"),
        Some(before)
    );
}

#[tokio::test]
async fn pending_delete_recovery_preserves_scope_and_original_operation() {
    for purge in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "recoverdelete").await;
        let dev = Arc::new(StubDevCleanup::default());
        service
            .set_dev_cleanup(dev.clone())
            .expect("attach cleanup");
        let pending = admit_recovery_control(
            &service,
            "recoverdelete",
            shared_types::UserAppControlCommand::DeleteResources {
                purge,
                expected_resource_version: None,
            },
        )
        .await;
        assert!(
            service
                .resume_pending_control(&pending)
                .await
                .expect("resume deletion")
        );
        assert!(
            !service
                .resume_pending_control(&pending)
                .await
                .expect("ignore old pending snapshot")
        );
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.destroy_pvc_calls.load(Ordering::SeqCst),
            usize::from(purge)
        );
        assert_eq!(dev.calls.load(Ordering::SeqCst), usize::from(purge));
        assert!(runtime.deployments.get("recoverdelete").is_none());
        let completed = service
            .metadata
            .store
            .get_operation("recoverdelete", &pending.operation_id)
            .await
            .expect("read")
            .expect("operation");
        assert_eq!(
            completed.state,
            shared_types::UserAppOperationState::Succeeded
        );
        let checkpoint: shared_types::UserAppDeletionCheckpoint =
            serde_json::from_value(completed.checkpoint.clone()).expect("typed checkpoint");
        checkpoint
            .validate_operation(&completed)
            .expect("original operation identity");
        assert_eq!(
            checkpoint.stage,
            if purge {
                shared_types::UserAppDeletionStage::DevelopmentRemoved
            } else {
                shared_types::UserAppDeletionStage::ComputeRemoved
            }
        );
        assert_eq!(checkpoint.development.is_some(), purge);
        assert_eq!(
            service
                .get_lifecycle("recoverdelete")
                .await
                .expect("retained lifecycle")
                .state,
            shared_types::UserAppLifecycleState::Active
        );
    }
}

#[tokio::test]
async fn pending_delete_recovery_checks_original_expected_resource_version() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recoverdeleteversion").await;
    runtime
        .deployments
        .get_mut("recoverdeleteversion")
        .expect("runtime")
        .resource_version = Some("new-version".into());
    let pending = admit_recovery_control(
        &service,
        "recoverdeleteversion",
        shared_types::UserAppControlCommand::DeleteResources {
            purge: false,
            expected_resource_version: Some("old-version".into()),
        },
    )
    .await;
    assert!(matches!(
        service.resume_pending_control(&pending).await,
        Err(AppOperationError::Conflict(_))
    ));
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    assert!(runtime.deployments.get("recoverdeleteversion").is_some());
    let failed = service
        .metadata
        .store
        .get_operation("recoverdeleteversion", &pending.operation_id)
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(failed.state, shared_types::UserAppOperationState::Failed);
    assert!(failed.checkpoint.is_null());
}

#[tokio::test]
async fn pending_full_delete_recovery_owns_deleting_lifecycle_until_completion() {
    for cleanup_fails in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "recoverfulldelete").await;
        let dev = Arc::new(StubDevCleanup::default());
        dev.fails.store(cleanup_fails, Ordering::SeqCst);
        service
            .set_dev_cleanup(dev.clone())
            .expect("attach cleanup");
        let pending = admit_recovery_control(
            &service,
            "recoverfulldelete",
            shared_types::UserAppControlCommand::DeleteApplication,
        )
        .await;
        let identity = service
            .get_lifecycle("recoverfulldelete")
            .await
            .expect("admitted identity");
        assert_eq!(
            identity.state,
            shared_types::UserAppLifecycleState::Deleting
        );
        assert_eq!(
            identity
                .active_operations
                .slot(pending.scope)
                .map(String::as_str),
            Some(pending.operation_id.as_str())
        );

        let result = service.resume_pending_control(&pending).await;
        assert_eq!(result.is_err(), cleanup_fails);
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
        assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
        let operation = service
            .metadata
            .store
            .get_operation("recoverfulldelete", &pending.operation_id)
            .await
            .expect("read")
            .expect("operation");
        let identity = service
            .get_lifecycle("recoverfulldelete")
            .await
            .expect("identity");
        assert_eq!(identity.lifecycle_id, pending.lifecycle_id);
        let checkpoint: shared_types::UserAppDeletionCheckpoint =
            serde_json::from_value(operation.checkpoint.clone()).expect("deletion checkpoint");
        checkpoint
            .validate_operation(&operation)
            .expect("captured identity");
        if cleanup_fails {
            assert_eq!(
                operation.state,
                shared_types::UserAppOperationState::RecoveryRequired
            );
            assert_eq!(
                identity.state,
                shared_types::UserAppLifecycleState::Deleting
            );
            assert_eq!(
                checkpoint.stage,
                shared_types::UserAppDeletionStage::ProductionStorageRemoved
            );
            assert!(service.activity.is_wake_blocked("recoverfulldelete"));
        } else {
            assert_eq!(
                operation.state,
                shared_types::UserAppOperationState::Succeeded
            );
            assert_eq!(identity.state, shared_types::UserAppLifecycleState::Deleted);
            assert_eq!(
                checkpoint.stage,
                shared_types::UserAppDeletionStage::DevelopmentRemoved
            );
            assert!(identity.active_operations.is_empty());
        }
        let repeated = service.resume_pending_control(&pending).await;
        if cleanup_fails {
            assert!(
                repeated.is_err(),
                "partial deletion must retain its resource lease"
            );
        } else {
            assert!(!repeated.expect("old completed scan ignored"));
        }
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
        assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn controlled_storage_destruction_checks_owner_and_replays_without_duplicate_cleanup() {
    for production in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "controlledstorage").await;
        let dev = Arc::new(StubDevCleanup::default());
        service
            .set_dev_cleanup(dev.clone())
            .expect("cleanup adapter");
        if production {
            runtime.deployments.remove("controlledstorage");
        }
        let stage = if production {
            shared_types::UserappStage::Prod
        } else {
            shared_types::UserappStage::Dev
        };
        let request = DestroyStorageRequest {
            confirm: "controlledstorage".into(),
            lifecycle_id: None,
            request_id: Some("storage-request".into()),
        };
        let operation_id = service
            .destroy_app_storage_controlled(stage, "controlledstorage", request.clone())
            .await
            .expect("destroy");
        assert_eq!(
            service
                .destroy_app_storage_controlled(stage, "controlledstorage", request)
                .await
                .expect("replay"),
            operation_id
        );
        assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            runtime.destroy_pvc_calls.load(Ordering::SeqCst),
            usize::from(production)
        );
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
        let operation = service
            .metadata
            .store
            .get_operation("controlledstorage", &operation_id)
            .await
            .expect("read")
            .expect("operation");
        assert_eq!(
            operation.state,
            shared_types::UserAppOperationState::Succeeded
        );
        let evidence: shared_types::UserAppStorageDestruction =
            serde_json::from_value(operation.checkpoint).expect("evidence");
        assert_eq!(evidence.production.is_some(), production);
        assert_eq!(evidence.context.operation_id, operation_id);
        assert_eq!(
            service
                .get_lifecycle("controlledstorage")
                .await
                .expect("identity")
                .state,
            shared_types::UserAppLifecycleState::Active
        );
    }
}

#[tokio::test]
async fn controlled_production_clear_preserves_scope_and_request_identity() {
    let directory = tempfile::tempdir().expect("directory");
    let (mut service, runtime) = created_app_service(directory.path(), "clearproduction").await;
    service.config.access_mode = AppAccessMode::Kubernetes;
    runtime.deployments.remove("clearproduction");
    let request = ClearStorageRequest {
        lifecycle_id: None,
        request_id: Some("clear-request".into()),
    };
    let id = service
        .clear_app_storage_controlled(
            shared_types::UserappStage::Prod,
            "clearproduction",
            request.clone(),
        )
        .await
        .expect("clear");
    assert_eq!(
        service
            .clear_app_storage_controlled(
                shared_types::UserappStage::Prod,
                "clearproduction",
                request
            )
            .await
            .expect("replay"),
        id
    );
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    let operation = service
        .metadata
        .store
        .get_operation("clearproduction", &id)
        .await
        .expect("read")
        .expect("operation");
    assert_eq!(
        operation.kind,
        shared_types::UserAppOperationKind::ClearProdStorage
    );
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::Succeeded
    );
    let evidence: shared_types::UserAppStorageClear =
        serde_json::from_value(operation.checkpoint).expect("evidence");
    assert!(matches!(
        evidence.target,
        shared_types::UserAppStorageClearTarget::Production { .. }
    ));
    assert_eq!(
        service
            .get_lifecycle("clearproduction")
            .await
            .expect("identity")
            .state,
        shared_types::UserAppLifecycleState::Active
    );
}

#[tokio::test]
async fn controlled_production_clear_refuses_existing_compute_before_storage_effects() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "clearrunning").await;
    let result = service
        .clear_app_storage_controlled(
            shared_types::UserappStage::Prod,
            "clearrunning",
            ClearStorageRequest {
                lifecycle_id: None,
                request_id: Some("clear-running-request".into()),
            },
        )
        .await;
    assert!(matches!(result, Err(AppOperationError::InvalidState(_))));
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    let operation = service
        .metadata
        .store
        .get_operation_by_request("clearrunning", "clear-running-request")
        .await
        .expect("read")
        .expect("recorded rejection");
    assert_eq!(operation.state, shared_types::UserAppOperationState::Failed);
    assert!(operation.checkpoint.is_null());
}

#[tokio::test]
async fn pending_storage_clear_reuses_operation_without_recreating_compute() {
    let directory = tempfile::tempdir().expect("directory");
    let (mut service, runtime) = created_app_service(directory.path(), "recoverclear").await;
    service.config.access_mode = AppAccessMode::Kubernetes;
    runtime.deployments.remove("recoverclear");
    let pending = admit_recovery_control(
        &service,
        "recoverclear",
        shared_types::UserAppControlCommand::ClearStorage { production: true },
    )
    .await;
    assert!(
        service
            .resume_pending_control(&pending)
            .await
            .expect("resume")
    );
    assert!(
        !service
            .resume_pending_control(&pending)
            .await
            .expect("old snapshot")
    );
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    let completed = service
        .metadata
        .store
        .get_operation("recoverclear", &pending.operation_id)
        .await
        .expect("read")
        .expect("record");
    assert_eq!(
        completed.state,
        shared_types::UserAppOperationState::Succeeded
    );
    assert_eq!(completed.operation_id, pending.operation_id);
    assert_eq!(completed.command, pending.command);
    assert_eq!(
        service
            .get_lifecycle("recoverclear")
            .await
            .expect("identity")
            .state,
        shared_types::UserAppLifecycleState::Active
    );
}

#[tokio::test]
async fn pending_configuration_recovery_uses_private_resolved_input_once() {
    for update in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "recoverconfig").await;
        let identity = service
            .get_lifecycle("recoverconfig")
            .await
            .expect("identity");
        let mut params = runtime
            .create_params_history
            .get("recoverconfig")
            .expect("history")[0]
            .clone();
        params.execution_context = None;
        params.mutation_target = None;
        params.env = Some(std::collections::HashMap::from([(
            "RECOVERY_SENTINEL".into(),
            "original-private-value".into(),
        )]));
        let previous = if update {
            Some(
                service
                    .fetch_runtime_status_or_err("recoverconfig")
                    .await
                    .expect("prior state"),
            )
        } else {
            runtime.deployments.remove("recoverconfig");
            None
        };
        let input = shared_types::UserAppExecutionInput::new(
            serde_json::to_string(&serde_json::json!({
                "version": 1, "params": params, "previous": previous,
            }))
            .expect("encode"),
        );
        let command = if update {
            shared_types::UserAppControlCommand::Update {
                input_digest: input.digest(),
            }
        } else {
            shared_types::UserAppControlCommand::Create {
                input_digest: input.digest(),
            }
        };
        let admission = shared_types::UserAppAdmission {
            runtime_policy_on_success: None,
            metadata: None,
            kind: command.kind(),
            command: Some(command),
            app_id: identity.app_id.clone(),
            lifecycle_id: Some(identity.lifecycle_id),
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: None,
            request_fingerprint: "a".repeat(64),
        };
        let pending = match service
            .metadata
            .store
            .admit_with_input(&admission, Some(&input))
            .await
            .expect("admit")
        {
            shared_types::UserAppAdmissionOutcome::Accepted(record) => record,
            _ => panic!("new operation"),
        };
        let before = runtime.create_calls.load(Ordering::SeqCst);
        assert!(
            service
                .resume_pending_control(&pending)
                .await
                .expect("recover")
        );
        assert!(
            !service
                .resume_pending_control(&pending)
                .await
                .expect("old snapshot")
        );
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), before + 1);
        let env = runtime
            .create_params_history
            .get("recoverconfig")
            .expect("history")
            .last()
            .expect("latest")
            .env
            .clone()
            .expect("env");
        assert_eq!(
            env.get("RECOVERY_SENTINEL").map(String::as_str),
            Some("original-private-value")
        );
        let completed = service
            .metadata
            .store
            .get_operation("recoverconfig", &pending.operation_id)
            .await
            .expect("read")
            .expect("operation");
        assert_eq!(
            completed.state,
            shared_types::UserAppOperationState::Succeeded
        );
        assert!(
            !serde_json::to_string(&completed)
                .expect("public record")
                .contains("original-private-value")
        );
    }
}

/// D′：已成功请求的幂等回放不触发任何新的 runtime 调用（replay 保留在
/// deploy_admitted 内部、ensure_identity 之后；新 app 的 lifecycle 在
/// ensure_identity 时创建，不存在时 replay 无法查重——此时 replay 跳过，
/// 走锁内首次受理，新 app 正常创建）。
#[tokio::test]
async fn completed_deploy_replay_is_idempotent() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "replayidempotent").await;
    let request = StartAppRequest {
        request_id: Some("replaylocked".into()),
        ..Default::default()
    };
    let first = service
        .start_app_enhanced("replayidempotent", request.clone())
        .await
        .expect("first control");
    let create_before = runtime.create_calls.load(Ordering::SeqCst);
    let policy_before = runtime.policy_calls.load(Ordering::SeqCst);

    let replayed = service
        .start_app_enhanced("replayidempotent", request)
        .await
        .expect("replay must return stored result");
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&replayed).unwrap()
    );
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        create_before,
        "replay must not re-create"
    );
    assert_eq!(
        runtime.policy_calls.load(Ordering::SeqCst),
        policy_before,
        "replay must not reapply overrides"
    );
}

#[tokio::test]
async fn enhanced_start_and_restart_deduplicate_complete_policy_intent() {
    for restart in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "compositestart").await;
        let request = StartAppRequest {
            request_id: Some("complete-control".into()),
            idle_timeout_seconds: Some(812),
            ..Default::default()
        };
        let first = if restart {
            service
                .restart_app_enhanced("compositestart", request.clone())
                .await
        } else {
            service
                .start_app_enhanced("compositestart", request.clone())
                .await
        }
        .expect("control");
        let second = if restart {
            service
                .restart_app_enhanced("compositestart", request.clone())
                .await
        } else {
            service
                .start_app_enhanced("compositestart", request.clone())
                .await
        }
        .expect("replay");
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&second).unwrap()
        );
        assert_eq!(
            runtime.policy_calls.load(Ordering::SeqCst),
            1,
            "replay must not reapply overrides"
        );
        let record = service
            .metadata
            .store
            .get_operation_by_request("compositestart", "complete-control")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first.operation_id.as_deref(),
            Some(record.operation_id.as_str())
        );
        assert_eq!(record.state, shared_types::UserAppOperationState::Succeeded);
        assert!(
            matches!(record.command, Some(shared_types::UserAppControlCommand::Deploy { restart: value, .. }) if value == restart)
        );
        assert_eq!(record.checkpoint["idle_timeout_seconds"], 812);
        assert_eq!(
            service
                .get_lifecycle("compositestart")
                .await
                .unwrap()
                .runtime_policy
                .idle_timeout_seconds,
            Some(812)
        );
        let changed = StartAppRequest {
            idle_timeout_seconds: Some(913),
            ..request
        };
        let error = if restart {
            service
                .restart_app_enhanced("compositestart", changed)
                .await
        } else {
            service.start_app_enhanced("compositestart", changed).await
        }
        .expect_err("changed intent");
        assert!(matches!(error, AppOperationError::Conflict(_)));
        assert_eq!(runtime.policy_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn pending_composite_deployment_recovers_original_control_and_completion() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recovercomposite").await;
    let request = StartAppRequest {
        request_id: Some("recovercomplete".into()),
        idle_timeout_seconds: Some(714),
        ..Default::default()
    };
    let previous = service
        .fetch_runtime_status_or_err("recovercomposite")
        .await
        .unwrap();
    let input = shared_types::UserAppExecutionInput::new(serde_json::to_string(&serde_json::json!({
        "version": 1, "request": request, "params": null, "previous": previous, "restart": false,
    })).unwrap());
    let identity = service.get_lifecycle("recovercomposite").await.unwrap();
    use sha2::Digest as _;
    let admission = shared_types::UserAppAdmission {
        metadata: None,
        runtime_policy_on_success: Some(shared_types::UserAppRuntimePolicy {
            recycle_enabled: Some(true),
            idle_timeout_seconds: Some(714),
            wake_on_traffic: Some(true),
        }),
        command: Some(shared_types::UserAppControlCommand::Deploy {
            restart: false,
            input_digest: input.digest(),
        }),
        app_id: identity.app_id,
        lifecycle_id: Some(identity.lifecycle_id),
        operation_id: uuid::Uuid::new_v4().to_string(),
        request_id: request.request_id.clone(),
        request_fingerprint: hex::encode(sha2::Sha256::digest(
            shared_types::encode_userapp_intent(&request).unwrap(),
        )),
        kind: shared_types::UserAppOperationKind::StartDeployment,
    };
    let pending = match service
        .metadata
        .store
        .admit_with_input(&admission, Some(&input))
        .await
        .unwrap()
    {
        shared_types::UserAppAdmissionOutcome::Accepted(record) => record,
        _ => panic!("new request"),
    };
    assert!(service.resume_pending_control(&pending).await.unwrap());
    let replay = service
        .start_app_enhanced("recovercomposite", request)
        .await
        .unwrap();
    assert_eq!(
        replay.operation_id.as_deref(),
        Some(pending.operation_id.as_str())
    );
    assert_eq!(replay.runtime.idle_timeout_seconds, Some(714));
    assert_eq!(runtime.policy_calls.load(Ordering::SeqCst), 1);
    assert!(!service.resume_pending_control(&pending).await.unwrap());
}
