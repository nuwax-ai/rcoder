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
        name: "r2-app".into(),
        user_id: "u-test".to_string(),
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
        .ensure_identity("port-drift", "u-test")
        .await
        .expect("owner identity");
    service.pingora = Some(Arc::new(PingoraProxyService::new(
        rcoder_proxy::ProxyConfig::default(),
    )));
    runtime.deployments.insert(
        "port-drift".into(),
        DeploymentStatus {
            app_id: "port-drift".into(),
            phase: "Running".into(),
            pod_ip: Some("10.0.0.1".into()),
            ..Default::default()
        },
    );
    runtime.specs.insert(
        "port-drift".into(),
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
        .register_pingora_backends("port-drift", &[9080], "10.0.0.1")
        .await;
    runtime.create_fails.store(true, Ordering::SeqCst);
    let request = UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        user_id: "u-test".into(),
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
    assert!(service.update_app("port-drift", request).await.is_err());
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        1,
        "patch was attempted"
    );
    assert_eq!(service.registered_http_ports("port-drift"), vec![9080]);
}

#[tokio::test]
async fn waiting_delete_rechecks_version_after_acquiring_operation_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .store
        .ensure_identity("delete-race", "u-test")
        .await
        .expect("owner identity");
    runtime.deployments.insert(
        "delete-race".into(),
        DeploymentStatus {
            app_id: "delete-race".into(),
            phase: "Running".into(),
            resource_version: Some("1".into()),
            ..Default::default()
        },
    );
    let writer = service
        .acquire_process_release_lock("delete-race")
        .await
        .expect("writer lock");
    let deletion = service.delete_app("delete-race", true, Some("1"));
    tokio::pin!(deletion);
    assert!(futures_util::poll!(deletion.as_mut()).is_pending());
    runtime
        .deployments
        .get_mut("delete-race")
        .expect("deployment")
        .resource_version = Some("2".into());
    drop(writer);
    assert!(matches!(
        deletion.await,
        Err(AppOperationError::Conflict(_))
    ));
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn independent_services_share_application_file_lock() {
    let root = tempfile::tempdir().expect("tempdir");
    let first = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let second = test_service(root.path(), Arc::new(MockRuntime::default())).await;
    let held = first
        .acquire_process_release_lock("cross-process")
        .await
        .expect("first lock");
    let contender = second.acquire_process_release_lock("cross-process");
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

/// R01：runtime 未返回创建凭据时，service 不得按名字补偿删除竞争赢家。
#[tokio::test]
pub(crate) async fn create_app_runtime_failure_does_not_delete_unowned_resources() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    runtime.create_fails.store(true, Ordering::SeqCst);
    let service = test_service(root.path(), runtime.clone()).await;
    // build_container_params 需 code/release.lock.toml，预铺现场
    let app_dir = root.path().join("app-r2");
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
        .create_app(create_request("app-r2"))
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

/// R2 对照：清理自身失败也不改变原始错误（只 warn）。
#[tokio::test]
pub(crate) async fn create_app_cleanup_failure_keeps_original_error() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    runtime.create_fails.store(true, Ordering::SeqCst);
    runtime.delete_fails.store(true, Ordering::SeqCst);
    let service = test_service(root.path(), runtime.clone()).await;
    let app_dir = root.path().join("app-r2b");
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
        .create_app(create_request("app-r2b"))
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
    let app_dir = root.path().join("app-meta");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");

    let mut create = create_request("app-meta");
    create.name = "alpha".into();
    service.create_app(create).await.expect("create app");
    assert_eq!(
        service
            .metadata
            .lookup("app-meta")
            .await
            .expect("metadata query")
            .and_then(|m| m.name),
        Some("alpha".into()),
        "create records name"
    );

    let update_no_name = UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        user_id: "u-test".into(),
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
        .update_app("app-meta", update_no_name.clone())
        .await
        .expect("update without name");
    assert_eq!(
        service
            .metadata
            .lookup("app-meta")
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
        .update_app("app-meta", update_with_name)
        .await
        .expect("update with name");
    assert_eq!(
        service
            .metadata
            .lookup("app-meta")
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
        .acquire_process_release_lock("app-busy")
        .await
        .expect("operation lock");

    let request = UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        user_id: "u-test".into(),
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
        .update_app("app-busy", request)
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
        user_id: "u-test".into(),
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
        user_id: identity.user_id,
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
    let (service, runtime) = created_app_service(directory.path(), "recover-control").await;
    let pending = admit_recovery_control(
        &service,
        "recover-control",
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
        .get_operation("recover-control", &pending.operation_id)
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
    let (service, runtime) = created_app_service(directory.path(), "recover-running").await;
    let pending = admit_recovery_control(
        &service,
        "recover-running",
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
        .get_operation("recover-running", &pending.operation_id)
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
    let (service, runtime) = created_app_service(directory.path(), "stale-wake-block").await;
    let service = Arc::new(service);
    service.attach_activity_coordinator().expect("coordinator");
    service.activity.mark_wake_blocked("stale-wake-block");
    let result =
        shared_types::AppWakeControl::ensure_running(service.activity.as_ref(), "stale-wake-block")
            .await;
    assert_eq!(result, shared_types::WakeOutcome::AlreadyRunning);
    assert!(!service.activity.is_wake_blocked("stale-wake-block"));
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        0,
        "old local state does not restart a running application"
    );
    service
        .stop_app_controlled(
            "stale-wake-block",
            shared_types::UserAppControlRequest {
                user_id: "u-test".into(),
                lifecycle_id: None,
                request_id: Some("committed-manual-stop".into()),
            },
        )
        .await
        .expect("intentional stop");
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    let result =
        shared_types::AppWakeControl::ensure_running(service.activity.as_ref(), "stale-wake-block")
            .await;
    assert!(matches!(result, shared_types::WakeOutcome::Failed(_)));
    assert!(service.activity.is_wake_blocked("stale-wake-block"));
    assert_eq!(
        runtime.scale_calls.load(Ordering::SeqCst),
        before,
        "committed manual stop is never overridden by traffic"
    );
}

#[tokio::test]
async fn pending_traffic_recovery_cannot_override_a_manual_stop() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recover-manual").await;
    service
        .stop_app_controlled(
            "recover-manual",
            shared_types::UserAppControlRequest {
                user_id: "u-test".into(),
                lifecycle_id: None,
                request_id: Some("manual-stop-before-recovery".into()),
            },
        )
        .await
        .expect("persist intentional stop");
    let prior_scale_calls = runtime.scale_calls.load(Ordering::SeqCst);
    let pending = admit_recovery_control(
        &service,
        "recover-manual",
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
        .get_operation("recover-manual", &pending.operation_id)
        .await
        .expect("operation query")
        .expect("operation");
    assert_eq!(failed.state, shared_types::UserAppOperationState::Failed);
    assert!(service.activity.is_wake_blocked("recover-manual"));
    assert!(failed.checkpoint.is_null());
}

#[tokio::test]
async fn stop_rejection_releases_ownership_but_uncertain_result_retains_it() {
    for status in [403, 408, 500] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "stop-outcome").await;
        runtime.stop_failure_status.store(status, Ordering::SeqCst);
        let request = shared_types::UserAppControlRequest {
            user_id: "u-test".into(),
            lifecycle_id: None,
            request_id: Some("stop-outcome-request".into()),
        };
        let error = service
            .stop_app_controlled("stop-outcome", request)
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
            .get_operation_by_request("stop-outcome", "stop-outcome-request")
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
        assert_eq!(service.activity.is_wake_blocked("stop-outcome"), !rejected);
        let next = service
            .try_acquire_process_release_lock("stop-outcome")
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
    let (service, runtime) = created_app_service(directory.path(), "durable-policy").await;
    let request = RecyclePolicyRequest {
        user_id: "u-test".into(),
        lifecycle_id: None,
        request_id: Some("policy-request".into()),
        recycle_enabled: Some(false),
        idle_timeout_seconds: Some(0),
        wake_on_traffic: Some(false),
    };
    service
        .set_recycle_policy("durable-policy", request.clone())
        .await
        .expect("policy");
    service
        .set_recycle_policy("durable-policy", request.clone())
        .await
        .expect("repeat");
    assert_eq!(runtime.policy_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 1);
    let identity = service
        .metadata
        .store
        .get_application("durable-policy")
        .await
        .expect("identity query")
        .expect("identity");
    assert_eq!(identity.runtime_policy.recycle_enabled, Some(false));
    let status = service
        .get_app("durable-policy")
        .await
        .expect("policy readback");
    assert_eq!(status.recycle_enabled, Some(false));
    assert_eq!(status.idle_timeout_seconds, Some(0));
    assert_eq!(status.wake_on_traffic, Some(false));
    let mut changed = request;
    changed.wake_on_traffic = Some(true);
    assert!(
        service
            .set_recycle_policy("durable-policy", changed)
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
    let mut request = create_request("initial-runtime-policy");
    request.recycle_enabled = Some(false);
    request.idle_timeout_seconds = Some(1800);
    service.create_app(request).await.expect("create");
    let identity = service
        .metadata
        .store
        .get_application("initial-runtime-policy")
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
    let (service, runtime) = created_app_service(directory.path(), "update-policy").await;
    service
        .set_recycle_policy(
            "update-policy",
            RecyclePolicyRequest {
                user_id: "u-test".into(),
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
        .update_app("update-policy", update.clone())
        .await
        .expect("configuration update");
    assert_eq!(result.recycle_enabled, Some(true));
    assert_eq!(result.idle_timeout_seconds, Some(900));
    assert_eq!(result.wake_on_traffic, Some(false));
    let calls = runtime.create_calls.load(Ordering::SeqCst);
    service
        .update_app("update-policy", update)
        .await
        .expect("exact replay after successful update");
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), calls);
    let next = service
        .acquire_process_release_lock("update-policy")
        .await
        .expect("successful update releases resource marker");
    next.finish().await.expect("release test lease");
}

#[tokio::test]
async fn pending_policy_recovery_applies_the_original_policy() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recover-policy").await;
    let policy = shared_types::UserAppRuntimePolicy {
        recycle_enabled: Some(false),
        idle_timeout_seconds: Some(450),
        wake_on_traffic: None,
    };
    let previous_wake = service
        .metadata
        .store
        .get_application("recover-policy")
        .await
        .expect("read previous policy")
        .expect("identity")
        .runtime_policy
        .wake_on_traffic;
    let pending = admit_recovery_control(
        &service,
        "recover-policy",
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
            .get_application("recover-policy")
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
    let (service, runtime) = created_app_service(root.path(), "durable-update").await;
    let mut request = update_request_with_storage(None);
    request.name = Some("updated-name".into());
    request.request_id = Some("update-request".into());
    service
        .update_app("durable-update", request.clone())
        .await
        .expect("update");
    let calls = runtime.create_calls.load(Ordering::SeqCst);
    service
        .update_app("durable-update", request.clone())
        .await
        .expect("exact retry");
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        calls,
        "completed retry must not reapply runtime configuration"
    );
    request.name = Some("different-intent".into());
    assert!(matches!(
        service.update_app("durable-update", request).await,
        Err(AppOperationError::Conflict(_))
    ));
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), calls);
    let context = runtime
        .create_params_history
        .get("durable-update")
        .and_then(|history| {
            history
                .last()
                .and_then(|params| params.execution_context.clone())
        })
        .expect("runtime received operation context");
    let target = runtime
        .create_params_history
        .get("durable-update")
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
        .get_operation("durable-update", &context.operation_id)
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
        .get_control_operation_by_request("durable-update", "u-test", "update-request")
        .await
        .expect("query by caller token")
        .expect("operation");
    assert_eq!(queried.operation_id, operation.operation_id);
    assert!(
        service
            .get_control_operation_by_request("durable-update", "foreign", "update-request")
            .await
            .is_err()
    );
    let app = service
        .metadata
        .store
        .get_application("durable-update")
        .await
        .expect("read identity")
        .expect("identity");
    assert_eq!(app.name.as_deref(), Some("updated-name"));
    assert!(app.current_operation_id.is_none());
}

#[tokio::test]
async fn explicit_stop_is_durable_idempotent_and_blocks_traffic_wake() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durable-stop").await;
    let request = shared_types::UserAppControlRequest {
        user_id: "u-test".into(),
        lifecycle_id: None,
        request_id: Some("stop-request".into()),
    };
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    service
        .stop_app_controlled("durable-stop", request.clone())
        .await
        .expect("stop");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert!(service.activity.is_wake_blocked("durable-stop"));
    service
        .stop_app_controlled("durable-stop", request.clone())
        .await
        .expect("exact retry");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    let operation = service
        .get_control_operation_by_request("durable-stop", "u-test", "stop-request")
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
        .get_operation_by_request("durable-stop", "stop-request")
        .await
        .expect("stored operation")
        .expect("receipt");
    let target: shared_types::UserAppMutationTarget =
        serde_json::from_value(persisted.checkpoint["target"].clone())
            .expect("persisted physical stop target");
    assert_eq!(target.context.operation_id, operation.operation_id);
    assert_eq!(target.context.lifecycle_id, persisted.lifecycle_id);
    assert_eq!(target.context.user_id, "u-test");
    assert!(!target.resource.uid.is_empty());
    assert_eq!(persisted.checkpoint["wake_on_traffic"], false);
    let mut foreign = request;
    foreign.user_id = "foreign".into();
    assert!(
        service
            .stop_app_controlled("durable-stop", foreign)
            .await
            .is_err()
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn compute_delete_failure_keeps_wake_blocked_until_reconciliation() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "delete-fence").await;
    runtime.delete_fails.store(true, Ordering::SeqCst);
    let result = service
        .delete_app_controlled(
            "delete-fence",
            DeleteAppRequest {
                user_id: "u-test".into(),
                lifecycle_id: None,
                request_id: Some("delete-fence-request".into()),
                purge: Some(false),
                expected_resource_version: None,
            },
        )
        .await;
    assert!(result.is_err());
    assert!(service.activity.is_wake_blocked("delete-fence"));
    assert!(runtime.deployments.contains_key("delete-fence"));
    let operation = service
        .get_control_operation_by_request("delete-fence", "u-test", "delete-fence-request")
        .await
        .expect("query")
        .expect("operation");
    assert_eq!(
        operation.state,
        shared_types::UserAppOperationState::RecoveryRequired
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    service.activity.forget_app("delete-fence");
    service
        .rebuild_stopped_apps()
        .await
        .expect("restore activity from durable deletion");
    assert!(
        service.activity.is_wake_blocked("delete-fence"),
        "a still-running old container cannot remove the deletion fence on restart"
    );
}

#[tokio::test]
async fn compute_delete_replays_after_runtime_disappears_and_preserves_lifecycle() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durable-delete").await;
    let before = service
        .metadata
        .store
        .get_application("durable-delete")
        .await
        .expect("read")
        .expect("identity");
    let request = DeleteAppRequest {
        user_id: "u-test".into(),
        request_id: Some("delete-request".into()),
        purge: Some(false),
        lifecycle_id: Some(before.lifecycle_id.clone()),
        expected_resource_version: None,
    };
    service
        .delete_app_controlled("durable-delete", request.clone())
        .await
        .expect("delete compute");
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    service
        .delete_app_controlled("durable-delete", request.clone())
        .await
        .expect("exact replay despite absent runtime");
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    let operation = service
        .get_control_operation_by_request("durable-delete", "u-test", "delete-request")
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
        .get_application("durable-delete")
        .await
        .expect("read")
        .expect("identity retained");
    assert_eq!(after.lifecycle_id, before.lifecycle_id);
    assert_eq!(after.state, shared_types::UserAppLifecycleState::Active);
    let mut another = request.clone();
    another.request_id = Some("delete-already-absent".into());
    service
        .delete_app_controlled("durable-delete", another)
        .await
        .expect("known lifecycle with absent compute is idempotent");
    let absent = service
        .get_control_operation_by_request("durable-delete", "u-test", "delete-already-absent")
        .await
        .expect("query")
        .expect("record");
    assert_eq!(absent.state, shared_types::UserAppOperationState::Succeeded);
    let mut changed = request;
    changed.purge = Some(true);
    assert!(matches!(
        service
            .delete_app_controlled("durable-delete", changed)
            .await,
        Err(AppOperationError::Conflict(_))
    ));
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
pub(crate) async fn update_app_storage_resize_triggered() {
    let root = tempfile::tempdir().expect("tempdir");
    let (service, runtime) = created_app_service(root.path(), "app-resize").await;
    let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

    service
        .update_app("app-resize", update_request_with_storage(Some("200Gi")))
        .await
        .expect("update with storage");

    assert_eq!(
        runtime.resize_calls.get("app-resize").map(|c| c.clone()),
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
    let (service, runtime) = created_app_service(root.path(), "app-shrink").await;
    *runtime.resize_outcome.lock().expect("outcome lock") =
        Some(StorageResizeOutcome::ShrinkRejected {
            current: "200Gi".into(),
            requested: "50Gi".into(),
        });
    let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

    let error = service
        .update_app("app-shrink", update_request_with_storage(Some("50Gi")))
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
    let (service, runtime) = created_app_service(root.path(), "app-rfail").await;
    runtime.resize_fails.store(true, Ordering::SeqCst);
    let create_calls_before = runtime.create_calls.load(Ordering::SeqCst);

    let error = service
        .update_app("app-rfail", update_request_with_storage(Some("200Gi")))
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
    let (service, runtime) = created_app_service(root.path(), "app-nosize").await;

    service
        .update_app("app-nosize", update_request_with_storage(None))
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
    let app_dir = root.path().join("app-purge");
    tokio::fs::create_dir_all(app_dir.join("code"))
        .await
        .expect("create code dir");
    tokio::fs::write(
        app_dir.join("code").join("release.lock.toml"),
        release_lock(),
    )
    .await
    .expect("write release lock");

    let mut create = create_request("app-purge");
    create.user_id = "u-purge".into();
    create.name = "keep-me".into();
    service.create_app(create).await.expect("create app");
    assert!(
        service
            .metadata
            .lookup("app-purge")
            .await
            .expect("metadata query")
            .is_some()
    );

    service
        .delete_app("app-purge", true, None)
        .await
        .expect("purge delete");
    assert!(
        service
            .metadata
            .lookup("app-purge")
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
            .any(|r| r.app_id == "app-purge"),
        "PG row retained after purge"
    );

    service
        .destroy_app_storage(
            shared_types::UserappStage::Prod,
            "app-purge",
            "u-purge",
            "app-purge",
        )
        .await
        .expect("explicit destroy");
    assert!(
        service
            .metadata
            .lookup("app-purge")
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
    use shared_types::AppMetadataRecord;

    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    for app_id in ["app-alpha", "app-beta"] {
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
        user_id: "u1".into(),
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

    let legacy = vec![
        AppMetadataRecord {
            generation: uuid::Uuid::new_v4().to_string(),
            app_id: "app-alpha".into(),
            name: Some("alpha".into()),
            user_id: Some("u1".into()),
            tenant_id: None,
            space_id: None,
            created_at: chrono::Utc::now() - chrono::Duration::hours(2),
        },
        AppMetadataRecord {
            generation: uuid::Uuid::new_v4().to_string(),
            app_id: "app-beta".into(),
            name: Some("beta".into()),
            user_id: Some("u1".into()),
            tenant_id: None,
            space_id: None,
            created_at: chrono::Utc::now(),
        },
    ];
    for row in legacy {
        service
            .metadata
            .store
            .import_application(&row)
            .await
            .expect("import metadata");
    }

    let response = service.query_apps(by_name("alpha")).await.expect("query");
    assert_eq!(response.items.len(), 1, "name filter now effective");
    assert_eq!(response.items[0].app_id, "app-alpha");

    // created_at range:只含 2 小时前创建的 alpha
    let now = chrono::Utc::now();
    let response = service
        .query_apps(QueryAppsRequest {
            user_id: "u1".into(),
            page: None,
            page_size: None,
            filters: Some(AppFilters {
                status: None,
                name: None,
                app_ids: None,
                created_at: Some(DateRange {
                    start: (now - chrono::Duration::hours(3)).to_rfc3339(),
                    end: (now - chrono::Duration::hours(1)).to_rfc3339(),
                }),
            }),
            sort_by: None,
            sort_order: None,
        })
        .await
        .expect("query by range");
    assert_eq!(response.items.len(), 1);
    assert_eq!(response.items[0].app_id, "app-alpha");
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
        .record(
            app_id,
            Some("purge-me".into()),
            Some("u-purge".into()),
            None,
            None,
        )
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
    seed_running_app(&service, &runtime, "app-p1").await;

    service.purge_app("app-p1").await.expect("purge app");

    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 1);
    assert!(runtime.deployments.get("app-p1").is_none());
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 1);
    assert_eq!(dev.calls.load(Ordering::SeqCst), 1);
    assert!(
        service
            .metadata
            .lookup("app-p1")
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
                |r| r.app_id == "app-p1" && r.state == shared_types::UserAppLifecycleState::Deleted
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
        .record("app-p2", None, Some("u-purge".into()), None, None)
        .await
        .expect("metadata registration");

    service.purge_app("app-p2").await.expect("idempotent purge");

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
            .lookup("app-p2")
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
    seed_running_app(&service, &runtime, "app-p3").await;
    dev.fails.store(true, Ordering::SeqCst);

    let error = service.purge_app("app-p3").await.expect_err("must fail");

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
            .lookup("app-p3")
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
    let app_id = "purge-receipts";
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
                .current_operation_id
                .as_deref()
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
    seed_running_app(&service, &runtime, "app-p4").await;
    runtime.status_fails.store(1, Ordering::SeqCst);

    let error = service.purge_app("app-p4").await.expect_err("must fail");

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
            .lookup("app-p4")
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
    seed_running_app(&service, &runtime, "app-p5").await;

    let error = service.purge_app("app-p5").await.expect_err("must fail");

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
            .lookup("app-p5")
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
        "version-lease".into(),
        DeploymentStatus {
            resource_version: Some("2".into()),
            ..Default::default()
        },
    );
    let mut service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .store
        .ensure_identity("version-lease", "u-test")
        .await
        .expect("owner identity");
    service.config.access_mode = AppAccessMode::Kubernetes;
    assert!(matches!(
        service.delete_app("version-lease", false, Some("1")).await,
        Err(AppOperationError::Conflict(_))
    ));
    assert!(!runtime.lease_held.load(Ordering::SeqCst));
    service
        .acquire_process_release_lock("version-lease")
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
        .ensure_identity("cancelled-writer", "u-test")
        .await
        .expect("owner identity");
    let operation = service
        .acquire_process_release_lock("cancelled-writer")
        .await
        .expect("lease");
    operation.mark_mutating().expect("durable mutation marker");
    drop(operation);
    assert!(matches!(
        service.delete_app("cancelled-writer", false, None).await,
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
        .acquire_process_release_lock("builder-foo")
        .await
        .expect("prod lease");
    let directory =
        std::path::Path::new(&service.config.operation_lock_root).join(".app-operation-locks");
    assert!(directory.join("prod-builder-foo.lock").exists());
    assert!(!directory.join("builder-foo.lock").exists());
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
        let mut request = create_request("invalid-env");
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
            .try_acquire_process_release_lock("invalid-env")
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
                "reserved-secret".into(),
                DeploymentStatus {
                    app_id: "reserved-secret".into(),
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
                .update_app("reserved-secret", request)
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
                    .get("reserved-secret")
                    .unwrap()
                    .resource_version
                    .as_deref(),
                Some("before"),
            );
            service
                .try_acquire_process_release_lock("reserved-secret")
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
        "prepare-retry".into(),
        DeploymentStatus {
            app_id: "prepare-retry".into(),
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
        .ensure_identity("prepare-retry", "u-test")
        .await
        .expect("authoritative application identity");
    let request = || UpdateAppRequest {
        request_id: None,
        lifecycle_id: None,
        user_id: "u-test".into(),
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
            .update_app("prepare-retry", request())
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
        let app_id = "cancelled-http-purge";
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
                    user_id: "u-purge".into(),
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
            .join(".app-operation-locks/prod-cancelled-http-purge.lock");

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

/// A conflicting durable owner must be rejected before provision/create effects.
#[tokio::test]
async fn durable_owner_conflict_precedes_runtime_creation() {
    let root = tempfile::tempdir().expect("tempdir");
    let runtime = Arc::new(MockRuntime::default());
    let service = test_service(root.path(), runtime.clone()).await;
    service
        .metadata
        .record(
            "owner-conflict",
            None,
            Some("original-owner".into()),
            None,
            None,
        )
        .await
        .expect("register owner");
    let code = root.path().join("owner-conflict/code");
    tokio::fs::create_dir_all(&code)
        .await
        .expect("code directory");
    tokio::fs::write(code.join("release.lock.toml"), release_lock())
        .await
        .expect("manifest");
    let error = service
        .create_app(create_request("owner-conflict"))
        .await
        .expect_err("conflicting owner");
    assert!(matches!(error, AppOperationError::Conflict(_)), "{error}");
    assert_eq!(
        runtime.create_calls.load(Ordering::SeqCst),
        0,
        "owner rejection must precede runtime creation"
    );
    assert!(!runtime.deployments.contains_key("owner-conflict"));
}

#[tokio::test]
async fn full_delete_deduplicates_and_rejects_old_lifecycle_after_recreation() {
    let root = tempfile::tempdir().expect("directory");
    let runtime = Arc::new(MockRuntime::default());
    let (service, runtime, dev, store) = purge_test_service(root.path(), runtime).await;
    let app_id = "purge-request-fence";
    seed_running_app(&service, &runtime, app_id).await;
    let original = store
        .get_application(app_id)
        .await
        .expect("read")
        .expect("identity");
    let request = shared_types::UserAppControlRequest {
        user_id: "u-purge".into(),
        lifecycle_id: Some(original.lifecycle_id.clone()),
        request_id: Some("full-delete-request".into()),
    };
    let mut foreign = request.clone();
    foreign.user_id = "foreign-owner".into();
    assert!(service.purge_app_controlled(app_id, foreign).await.is_err());
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
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
        .recreate(app_id, "u-purge", &original.lifecycle_id, "rebuild-request")
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
    let mut request = create_request("durable-create");
    request.request_id = Some("create-request".into());
    service.create_app(request.clone()).await.expect("create");
    let context = {
        let history = runtime
            .create_params_history
            .get("durable-create")
            .expect("create parameters");
        assert_eq!(history.len(), 1);
        history[0]
            .execution_context
            .clone()
            .expect("execution context")
    };
    context
        .validate_identity("durable-create", Some("u-test"))
        .expect("valid credential");
    let persisted = service
        .metadata
        .store
        .get_operation_by_request("durable-create", "create-request")
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
            .get("durable-create")
            .expect("history")
            .len(),
        1
    );
    request.name = "changed-intent".into();
    assert!(service.create_app(request).await.is_err());
    assert_eq!(
        runtime
            .create_params_history
            .get("durable-create")
            .expect("history")
            .len(),
        1
    );
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn storage_expansion_receipt_is_bound_to_the_update_operation() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "resize-receipt").await;
    let request: UpdateAppRequest = serde_json::from_value(serde_json::json!({
        "user_id":"u-test", "request_id":"resize-operation", "resources":{"storage":"200Gi"}
    }))
    .expect("request");
    service
        .update_app("resize-receipt", request)
        .await
        .expect("update");
    let operation = service
        .metadata
        .store
        .get_operation_by_request("resize-receipt", "resize-operation")
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
            .get("resize-receipt")
            .expect("resize history")
            .as_slice(),
        &["200Gi".to_owned()]
    );
}

#[tokio::test]
async fn controlled_start_persists_target_and_replays_without_another_runtime_write() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durable-start").await;
    service.stop_app("durable-start").await.expect("stop");
    let request = shared_types::UserAppControlRequest {
        user_id: "u-test".into(),
        lifecycle_id: None,
        request_id: Some("start-request".into()),
    };
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    service
        .start_app_controlled("durable-start", request.clone())
        .await
        .expect("start");
    assert!(!service.activity.is_wake_blocked("durable-start"));
    let operation = service
        .metadata
        .store
        .get_operation_by_request("durable-start", "start-request")
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
        .start_app_controlled("durable-start", request.clone())
        .await
        .expect("replay");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    let mut stale = request;
    stale.lifecycle_id = Some("obsolete-life".into());
    assert!(
        service
            .start_app_controlled("durable-start", stale)
            .await
            .is_err()
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn controlled_restart_deduplicates_and_is_distinct_from_start() {
    let root = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(root.path(), "durable-restart").await;
    let request = shared_types::UserAppControlRequest {
        user_id: "u-test".into(),
        lifecycle_id: None,
        request_id: Some("restart-request".into()),
    };
    let before = runtime.scale_calls.load(Ordering::SeqCst);
    service
        .restart_app_controlled("durable-restart", request.clone())
        .await
        .expect("restart");
    let operation = service
        .metadata
        .store
        .get_operation_by_request("durable-restart", "restart-request")
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
        .restart_app_controlled("durable-restart", request.clone())
        .await
        .expect("replay");
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
    assert!(
        service
            .start_app_controlled("durable-restart", request.clone())
            .await
            .is_err(),
        "same request key cannot change operation kind"
    );
    let mut stale = request;
    stale.lifecycle_id = Some("obsolete-life".into());
    assert!(
        service
            .restart_app_controlled("durable-restart", stale)
            .await
            .is_err()
    );
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), before + 1);
}

#[tokio::test]
async fn explicit_retry_checks_owner_lifecycle_and_revision_before_execution() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "retry-control").await;
    let pending = admit_recovery_control(
        &service,
        "retry-control",
        shared_types::UserAppControlCommand::Restart,
    )
    .await;
    let request = shared_types::UserAppRetryRequest {
        user_id: "u-test".into(),
        lifecycle_id: pending.lifecycle_id.clone(),
        expected_revision: pending.revision,
    };
    for invalid in [
        shared_types::UserAppRetryRequest {
            user_id: "other-owner".into(),
            ..request.clone()
        },
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
                .retry_control_operation("retry-control", &pending.operation_id, invalid)
                .await
                .is_err()
        );
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            service
                .metadata
                .store
                .get_operation("retry-control", &pending.operation_id)
                .await
                .expect("read"),
            Some(pending.clone())
        );
    }
    for _ in 0..2 {
        let result = service
            .retry_control_operation("retry-control", &pending.operation_id, request.clone())
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
    let (service, runtime) = created_app_service(directory.path(), "retry-claimed").await;
    let pending = admit_recovery_control(
        &service,
        "retry-claimed",
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
        .get_operation("retry-claimed", &pending.operation_id)
        .await
        .expect("read")
        .expect("operation");
    let result = service
        .retry_control_operation(
            "retry-claimed",
            &pending.operation_id,
            shared_types::UserAppRetryRequest {
                user_id: "u-test".into(),
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
            .get_operation("retry-claimed", &pending.operation_id)
            .await
            .expect("read"),
        Some(before)
    );
}

#[tokio::test]
async fn pending_delete_recovery_preserves_scope_and_original_operation() {
    for purge in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "recover-delete").await;
        let dev = Arc::new(StubDevCleanup::default());
        service
            .set_dev_cleanup(dev.clone())
            .expect("attach cleanup");
        let pending = admit_recovery_control(
            &service,
            "recover-delete",
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
        assert!(runtime.deployments.get("recover-delete").is_none());
        let completed = service
            .metadata
            .store
            .get_operation("recover-delete", &pending.operation_id)
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
                .get_lifecycle("recover-delete", "u-test")
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
    let (service, runtime) = created_app_service(directory.path(), "recover-delete-version").await;
    runtime
        .deployments
        .get_mut("recover-delete-version")
        .expect("runtime")
        .resource_version = Some("new-version".into());
    let pending = admit_recovery_control(
        &service,
        "recover-delete-version",
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
    assert!(runtime.deployments.get("recover-delete-version").is_some());
    let failed = service
        .metadata
        .store
        .get_operation("recover-delete-version", &pending.operation_id)
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
        let (service, runtime) = created_app_service(directory.path(), "recover-full-delete").await;
        let dev = Arc::new(StubDevCleanup::default());
        dev.fails.store(cleanup_fails, Ordering::SeqCst);
        service
            .set_dev_cleanup(dev.clone())
            .expect("attach cleanup");
        let pending = admit_recovery_control(
            &service,
            "recover-full-delete",
            shared_types::UserAppControlCommand::DeleteApplication,
        )
        .await;
        let identity = service
            .get_lifecycle("recover-full-delete", "u-test")
            .await
            .expect("admitted identity");
        assert_eq!(
            identity.state,
            shared_types::UserAppLifecycleState::Deleting
        );
        assert_eq!(
            identity.current_operation_id.as_deref(),
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
            .get_operation("recover-full-delete", &pending.operation_id)
            .await
            .expect("read")
            .expect("operation");
        let identity = service
            .get_lifecycle("recover-full-delete", "u-test")
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
            assert!(service.activity.is_wake_blocked("recover-full-delete"));
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
            assert!(identity.current_operation_id.is_none());
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
        let (service, runtime) = created_app_service(directory.path(), "controlled-storage").await;
        let dev = Arc::new(StubDevCleanup::default());
        service
            .set_dev_cleanup(dev.clone())
            .expect("cleanup adapter");
        if production {
            runtime.deployments.remove("controlled-storage");
        }
        let stage = if production {
            shared_types::UserappStage::Prod
        } else {
            shared_types::UserappStage::Dev
        };
        let request = DestroyStorageRequest {
            user_id: "u-test".into(),
            confirm: "controlled-storage".into(),
            lifecycle_id: None,
            request_id: Some("storage-request".into()),
        };
        assert!(
            service
                .destroy_app_storage_controlled(
                    stage,
                    "controlled-storage",
                    DestroyStorageRequest {
                        user_id: "foreign-owner".into(),
                        ..request.clone()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(dev.calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
        let operation_id = service
            .destroy_app_storage_controlled(stage, "controlled-storage", request.clone())
            .await
            .expect("destroy");
        assert_eq!(
            service
                .destroy_app_storage_controlled(stage, "controlled-storage", request)
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
            .get_operation("controlled-storage", &operation_id)
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
                .get_lifecycle("controlled-storage", "u-test")
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
    let (mut service, runtime) = created_app_service(directory.path(), "clear-production").await;
    service.config.access_mode = AppAccessMode::Kubernetes;
    runtime.deployments.remove("clear-production");
    let request = ClearStorageRequest {
        user_id: "u-test".into(),
        lifecycle_id: None,
        request_id: Some("clear-request".into()),
    };
    assert!(
        service
            .clear_app_storage_controlled(
                shared_types::UserappStage::Prod,
                "clear-production",
                ClearStorageRequest {
                    user_id: "wrong-owner".into(),
                    ..request.clone()
                }
            )
            .await
            .is_err()
    );
    assert_eq!(runtime.destroy_pvc_calls.load(Ordering::SeqCst), 0);
    let id = service
        .clear_app_storage_controlled(
            shared_types::UserappStage::Prod,
            "clear-production",
            request.clone(),
        )
        .await
        .expect("clear");
    assert_eq!(
        service
            .clear_app_storage_controlled(
                shared_types::UserappStage::Prod,
                "clear-production",
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
        .get_operation("clear-production", &id)
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
            .get_lifecycle("clear-production", "u-test")
            .await
            .expect("identity")
            .state,
        shared_types::UserAppLifecycleState::Active
    );
}

#[tokio::test]
async fn controlled_production_clear_refuses_existing_compute_before_storage_effects() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "clear-running").await;
    let result = service
        .clear_app_storage_controlled(
            shared_types::UserappStage::Prod,
            "clear-running",
            ClearStorageRequest {
                user_id: "u-test".into(),
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
        .get_operation_by_request("clear-running", "clear-running-request")
        .await
        .expect("read")
        .expect("recorded rejection");
    assert_eq!(operation.state, shared_types::UserAppOperationState::Failed);
    assert!(operation.checkpoint.is_null());
}

#[tokio::test]
async fn pending_storage_clear_reuses_operation_without_recreating_compute() {
    let directory = tempfile::tempdir().expect("directory");
    let (mut service, runtime) = created_app_service(directory.path(), "recover-clear").await;
    service.config.access_mode = AppAccessMode::Kubernetes;
    runtime.deployments.remove("recover-clear");
    let pending = admit_recovery_control(
        &service,
        "recover-clear",
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
        .get_operation("recover-clear", &pending.operation_id)
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
            .get_lifecycle("recover-clear", "u-test")
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
        let (service, runtime) = created_app_service(directory.path(), "recover-config").await;
        let identity = service
            .get_lifecycle("recover-config", "u-test")
            .await
            .expect("identity");
        let mut params = runtime
            .create_params_history
            .get("recover-config")
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
                    .fetch_runtime_status_or_err("recover-config")
                    .await
                    .expect("prior state"),
            )
        } else {
            runtime.deployments.remove("recover-config");
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
            user_id: identity.user_id,
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
            .get("recover-config")
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
            .get_operation("recover-config", &pending.operation_id)
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

#[tokio::test]
async fn enhanced_start_and_restart_deduplicate_complete_policy_intent() {
    for restart in [false, true] {
        let directory = tempfile::tempdir().expect("directory");
        let (service, runtime) = created_app_service(directory.path(), "composite-start").await;
        let request = StartAppRequest {
            user_id: "u-test".into(),
            request_id: Some("complete-control".into()),
            idle_timeout_seconds: Some(812),
            ..Default::default()
        };
        let first = if restart {
            service
                .restart_app_enhanced("composite-start", request.clone())
                .await
        } else {
            service
                .start_app_enhanced("composite-start", request.clone())
                .await
        }
        .expect("control");
        let second = if restart {
            service
                .restart_app_enhanced("composite-start", request.clone())
                .await
        } else {
            service
                .start_app_enhanced("composite-start", request.clone())
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
            .get_operation_by_request("composite-start", "complete-control")
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
                .get_lifecycle("composite-start", "u-test")
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
                .restart_app_enhanced("composite-start", changed)
                .await
        } else {
            service.start_app_enhanced("composite-start", changed).await
        }
        .expect_err("changed intent");
        assert!(matches!(error, AppOperationError::Conflict(_)));
        assert_eq!(runtime.policy_calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn pending_composite_deployment_recovers_original_control_and_completion() {
    let directory = tempfile::tempdir().expect("directory");
    let (service, runtime) = created_app_service(directory.path(), "recover-composite").await;
    let request = StartAppRequest {
        user_id: "u-test".into(),
        request_id: Some("recover-complete".into()),
        idle_timeout_seconds: Some(714),
        ..Default::default()
    };
    let previous = service
        .fetch_runtime_status_or_err("recover-composite")
        .await
        .unwrap();
    let input = shared_types::UserAppExecutionInput::new(serde_json::to_string(&serde_json::json!({
        "version": 1, "request": request, "params": null, "previous": previous, "restart": false,
    })).unwrap());
    let identity = service
        .get_lifecycle("recover-composite", "u-test")
        .await
        .unwrap();
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
        user_id: identity.user_id,
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
        .start_app_enhanced("recover-composite", request)
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
