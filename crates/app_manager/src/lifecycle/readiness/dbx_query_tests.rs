//! 查询编排反例：真实生命周期存储、控制变化及共享 deadline。
use super::*;
use shared_types::{ComputeControlState, UserAppContainerStatus, UserAppLifecycleStore};

struct DbxControlProbe {
    store: Arc<dyn UserAppLifecycleStore>,
    request: Option<shared_types::ComputeControlRequest>,
    calls: std::sync::atomic::AtomicUsize,
    stall: bool,
}

#[async_trait::async_trait]
impl shared_types::DbxReadinessProber for DbxControlProbe {
    async fn probe(
        &self,
        _: &str,
        _: UserappStage,
        _: Duration,
    ) -> Result<shared_types::DbxReadinessObservation, String> {
        use std::sync::atomic::Ordering;
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0
            && let Some(request) = &self.request
        {
            self.store
                .admit_compute_control(request)
                .await
                .map_err(|error| error.to_string())?;
        }
        if self.stall {
            return std::future::pending().await;
        }
        Ok(shared_types::DbxReadinessObservation::new(
            shared_types::DbxReadinessStatus::Ready,
            None,
        )
        .with_compute_fact(shared_types::DbxComputeFact::Located))
    }
}

fn dbx_control_request(
    lifecycle_id: String,
    action: ComputeControlAction,
) -> shared_types::ComputeControlRequest {
    shared_types::ComputeControlRequest {
        app_id: "dbxreview".into(),
        lifecycle_id,
        scope: UserAppOperationScope::Dev,
        operation_id: "dbxcontrol".into(),
        request_id: "dbxrequest".into(),
        request_fingerprint: "d".repeat(64),
        action,
        restart_image_roll: false,
    }
}

#[tokio::test]
async fn dbx_query_rereads_stop_admitted_during_probe_and_isolates_prod() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(crate::test_support::MockRuntime::default());
    let (service, store) =
        crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
    let app = store.ensure_identity("dbxreview").await.unwrap();
    service
        .set_dbx_prober(Arc::new(DbxControlProbe {
            store: store.clone(),
            request: Some(dbx_control_request(
                app.lifecycle_id,
                ComputeControlAction::Stop,
            )),
            calls: AtomicUsize::new(0),
            stall: false,
        }))
        .unwrap();

    let response = service
        .get_app_dbx_readiness(UserappStage::Dev, "dbxreview")
        .await
        .unwrap();
    assert_eq!(response.container.status, UserAppContainerStatus::Stopping);
    assert_eq!(
        response.container.operation.as_ref().unwrap().operation_id,
        "dbxcontrol"
    );
    assert!(
        !response.ready,
        "Stop admission must invalidate the old ready reply"
    );
    let prod = service
        .get_app_dbx_readiness(UserappStage::Prod, "dbxreview")
        .await
        .unwrap();
    assert!(prod.ready);
    assert_eq!(prod.container.status, UserAppContainerStatus::Running);
    assert!(prod.container.operation.is_none());
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        store
            .get_compute_control("dbxreview", "dbxcontrol")
            .await
            .unwrap()
            .unwrap()
            .state,
        ComputeControlState::Pending
    );
}

#[tokio::test]
async fn dbx_query_reprobes_after_restart_admitted_during_probe() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(crate::test_support::MockRuntime::default());
    let (service, store) =
        crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
    let app = store.ensure_identity("dbxreview").await.unwrap();
    let prober = Arc::new(DbxControlProbe {
        store: store.clone(),
        request: Some(dbx_control_request(
            app.lifecycle_id,
            ComputeControlAction::Restart,
        )),
        calls: AtomicUsize::new(0),
        stall: false,
    });
    service.set_dbx_prober(prober.clone()).unwrap();
    let response = service
        .get_app_dbx_readiness(UserappStage::Dev, "dbxreview")
        .await
        .unwrap();
    assert_eq!(
        prober.calls.load(Ordering::SeqCst),
        2,
        "a changed control generation must discard and repeat its observation"
    );
    assert_eq!(
        response.container.status,
        UserAppContainerStatus::Restarting
    );
    assert_eq!(
        response.container.operation.as_ref().unwrap().operation_id,
        "dbxcontrol"
    );
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dbx_ready_reply_respects_an_already_accepted_stop_or_restart() {
    use shared_types::{DbxReadinessReason, DbxReadinessStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    for (action, status, reason, container) in [
        (
            ComputeControlAction::Stop,
            DbxReadinessStatus::Unknown,
            DbxReadinessReason::ComputeStopping,
            UserAppContainerStatus::Stopping,
        ),
        (
            ComputeControlAction::Restart,
            DbxReadinessStatus::Starting,
            DbxReadinessReason::ComputeStarting,
            UserAppContainerStatus::Restarting,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
        let app = store.ensure_identity("dbxreview").await.unwrap();
        let accepted = store
            .admit_compute_control(&dbx_control_request(app.lifecycle_id, action))
            .await
            .unwrap();
        let prober = Arc::new(DbxControlProbe {
            store: store.clone(),
            request: None,
            calls: AtomicUsize::new(0),
            stall: false,
        });
        service.set_dbx_prober(prober.clone()).unwrap();
        let response = service
            .get_app_dbx_readiness(UserappStage::Dev, "dbxreview")
            .await
            .unwrap();
        assert!(
            !response.ready,
            "an in-flight {action:?} must invalidate ready"
        );
        assert_eq!(response.status, status);
        assert_eq!(response.reason_code, Some(reason));
        assert_eq!(response.container.status, container);
        assert_eq!(
            response.container.operation.as_ref().unwrap().operation_id,
            accepted.operation_id
        );
        assert_eq!(prober.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .get_compute_control("dbxreview", &accepted.operation_id)
                .await
                .unwrap(),
            Some(accepted)
        );
        assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn dbx_deadline_preserves_accepted_restart_and_its_receipt() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(crate::test_support::MockRuntime::default());
    let (service, store) =
        crate::test_support::test_service_with_store(root.path(), runtime.clone()).await;
    let app = store.ensure_identity("dbxreview").await.unwrap();
    let accepted = store
        .admit_compute_control(&dbx_control_request(
            app.lifecycle_id,
            ComputeControlAction::Restart,
        ))
        .await
        .unwrap();
    let prober = Arc::new(DbxControlProbe {
        store: store.clone(),
        request: None,
        calls: AtomicUsize::new(0),
        stall: true,
    });
    let response = dbx::query_dbx_with_budget(
        "dbxreview",
        UserappStage::Dev,
        Duration::from_millis(250),
        || service.read_readiness_control("dbxreview", UserappStage::Dev),
        Some(prober.clone()),
    )
    .await
    .unwrap();
    assert_eq!(
        prober.calls.load(Ordering::SeqCst),
        1,
        "the runtime phase must be reached"
    );
    assert!(!response.ready);
    assert_eq!(response.status, shared_types::DbxReadinessStatus::Unknown);
    assert_eq!(
        response.reason_code,
        Some(shared_types::DbxReadinessReason::ObserveIncomplete)
    );
    assert_eq!(
        response.container.status,
        UserAppContainerStatus::Restarting
    );
    assert_eq!(
        response.container.operation.as_ref().unwrap().operation_id,
        accepted.operation_id
    );
    assert_eq!(
        store
            .get_compute_control("dbxreview", &accepted.operation_id)
            .await
            .unwrap(),
        Some(accepted)
    );
    assert_eq!(runtime.restart_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.create_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.delete_calls.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.scale_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dbx_recheck_stall_or_error_never_returns_the_prior_ready_reply() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for stalled in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime).await;
        let app = store.ensure_identity("dbxreview").await.unwrap();
        let accepted = store
            .admit_compute_control(&dbx_control_request(
                app.lifecycle_id,
                ComputeControlAction::Restart,
            ))
            .await
            .unwrap();
        let prober = Arc::new(DbxControlProbe {
            store: store.clone(),
            request: None,
            calls: AtomicUsize::new(0),
            stall: false,
        });
        let reads = AtomicUsize::new(0);
        let response = dbx::query_dbx_with_budget(
            "dbxreview",
            UserappStage::Dev,
            Duration::from_millis(250),
            || {
                let read = reads.fetch_add(1, Ordering::SeqCst);
                let service = &service;
                async move {
                    let control = service
                        .read_readiness_control("dbxreview", UserappStage::Dev)
                        .await?;
                    if read > 0 {
                        if stalled {
                            return std::future::pending().await;
                        }
                        return Err(AppOperationError::Backend(
                            "controlled recheck storage failure".into(),
                        ));
                    }
                    Ok(control)
                }
            },
            Some(prober.clone()),
        )
        .await;
        assert_eq!(reads.load(Ordering::SeqCst), 2);
        assert_eq!(prober.calls.load(Ordering::SeqCst), 1);
        if stalled {
            let response = response.unwrap();
            assert!(!response.ready);
            assert_eq!(response.status, shared_types::DbxReadinessStatus::Unknown);
            assert_eq!(
                response.reason_code,
                Some(shared_types::DbxReadinessReason::ObserveIncomplete)
            );
            assert_eq!(
                response.container.status,
                UserAppContainerStatus::Restarting
            );
            assert_eq!(
                response.container.operation.unwrap().operation_id,
                accepted.operation_id
            );
        } else {
            assert!(
                matches!(response, Err(AppOperationError::Backend(message)) if message == "controlled recheck storage failure")
            );
        }
    }
}

#[tokio::test]
async fn dbx_control_identity_change_discards_ready_and_old_located_fact() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for lifecycle_changed in [true, false] {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime).await;
        store.ensure_identity("dbxreview").await.unwrap();
        let prober = Arc::new(DbxControlProbe {
            store,
            request: None,
            calls: AtomicUsize::new(0),
            stall: false,
        });
        let reads = AtomicUsize::new(0);
        let response = dbx::query_dbx_with_budget(
            "dbxreview",
            UserappStage::Dev,
            Duration::from_secs(1),
            || {
                let read = reads.fetch_add(1, Ordering::SeqCst);
                let service = &service;
                async move {
                    let mut control = service
                        .read_readiness_control("dbxreview", UserappStage::Dev)
                        .await?;
                    // Supply successive authoritative identities after a real
                    // read, without creating/deleting physical resources.
                    if lifecycle_changed && read > 0 {
                        control.lifecycle_id = "replacement-lifecycle".into();
                    } else if !lifecycle_changed {
                        control.compute.revision += read as i64;
                    }
                    Ok(control)
                }
            },
            Some(prober.clone()),
        )
        .await
        .unwrap();
        assert!(!response.ready);
        assert_eq!(response.status, shared_types::DbxReadinessStatus::Unknown);
        assert_eq!(
            response.reason_code,
            Some(shared_types::DbxReadinessReason::InstanceChanged)
        );
        assert_eq!(response.container.status, UserAppContainerStatus::Unknown);
        assert!(response.container.operation.is_none());
        let expected_probes = if lifecycle_changed { 1 } else { 2 };
        assert_eq!(prober.calls.load(Ordering::SeqCst), expected_probes);
        assert_eq!(reads.load(Ordering::SeqCst), expected_probes + 1);
    }
}

#[tokio::test]
async fn dbx_noncooperative_late_read_or_probe_cannot_return_ready() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct LateProbe {
        calls: AtomicUsize,
        delay: bool,
    }
    #[async_trait::async_trait]
    impl shared_types::DbxReadinessProber for LateProbe {
        async fn probe(
            &self,
            _: &str,
            _: UserappStage,
            _: Duration,
        ) -> Result<shared_types::DbxReadinessObservation, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.delay {
                // Deliberately avoid yielding so timeout_at's inner-first
                // poll cannot alone reject a late successful result.
                std::thread::sleep(Duration::from_millis(300));
            }
            Ok(shared_types::DbxReadinessObservation::new(
                shared_types::DbxReadinessStatus::Ready,
                None,
            )
            .with_compute_fact(shared_types::DbxComputeFact::Located))
        }
    }
    // First read, probe and final recheck each exercise the same deadline.
    for late_stage in 0..3 {
        let root = tempfile::tempdir().unwrap();
        let runtime = Arc::new(crate::test_support::MockRuntime::default());
        let (service, store) =
            crate::test_support::test_service_with_store(root.path(), runtime).await;
        let app = store.ensure_identity("dbxreview").await.unwrap();
        let accepted = store
            .admit_compute_control(&dbx_control_request(
                app.lifecycle_id,
                ComputeControlAction::Restart,
            ))
            .await
            .unwrap();
        let prober = Arc::new(LateProbe {
            calls: AtomicUsize::new(0),
            delay: late_stage == 1,
        });
        let reads = AtomicUsize::new(0);
        let response = dbx::query_dbx_with_budget(
            "dbxreview",
            UserappStage::Dev,
            Duration::from_millis(250),
            || {
                let read = reads.fetch_add(1, Ordering::SeqCst);
                let service = &service;
                async move {
                    let control = service
                        .read_readiness_control("dbxreview", UserappStage::Dev)
                        .await?;
                    if (late_stage == 0 && read == 0) || (late_stage == 2 && read == 1) {
                        std::thread::sleep(Duration::from_millis(300));
                    }
                    Ok(control)
                }
            },
            Some(prober.clone()),
        )
        .await
        .unwrap();
        assert!(!response.ready);
        assert_eq!(response.status, shared_types::DbxReadinessStatus::Unknown);
        assert_eq!(
            response.reason_code,
            Some(shared_types::DbxReadinessReason::ObserveIncomplete)
        );
        assert_eq!(
            response.container.status,
            UserAppContainerStatus::Restarting
        );
        assert_eq!(
            response.container.operation.unwrap().operation_id,
            accepted.operation_id
        );
        assert_eq!(
            prober.calls.load(Ordering::SeqCst),
            usize::from(late_stage != 0)
        );
        assert_eq!(
            reads.load(Ordering::SeqCst),
            if late_stage == 2 { 2 } else { 1 }
        );
    }
}
