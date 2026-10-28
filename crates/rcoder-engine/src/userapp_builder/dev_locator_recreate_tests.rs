//! Recreate through the actual AppService -> dev locator -> durable store chain.
use super::*;
use shared_types::{
    AppDeletionSnapshot, BuilderDeletionSnapshot, UserAppDeletionCheckpoint, UserAppDeletionStage,
    UserAppLifecycleRecord, UserAppLifecycleState, UserAppOperationRecord, UserAppRecreateRequest,
    UserappDevDeletionReceipt,
};

pub(super) async fn seed_deleted_application(
    runtime: &LocatorRuntime,
) -> (UserAppLifecycleRecord, UserAppOperationRecord) {
    let mut operation = admit(
        runtime.store.as_ref(),
        APP_ID,
        UserAppOperationKind::DeleteApplication,
    )
    .await;
    let executor_id = "recreate-deletion-fixture";
    operation = runtime
        .store
        .advance(&UserAppOperationProgress {
            app_id: operation.app_id.clone(),
            lifecycle_id: operation.lifecycle_id.clone(),
            operation_id: operation.operation_id.clone(),
            expected_revision: operation.revision,
            executor_id: executor_id.into(),
            state: UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        })
        .await
        .expect("claim the actual deletion operation");
    let mut evidence = UserAppDeletionCheckpoint {
        schema_version: 1,
        stage: UserAppDeletionStage::Captured,
        context: UserAppExecutionContext {
            app_id: APP_ID.into(),
            lifecycle_id: operation.lifecycle_id.clone(),
            operation_id: operation.operation_id.clone(),
            executor_id: executor_id.into(),
            request_fingerprint: operation.request_fingerprint.clone(),
        },
        kind: UserAppOperationKind::DeleteApplication,
        production: AppDeletionSnapshot {
            app_id: APP_ID.into(),
            operation_id: "empty-production".into(),
            resources: vec![],
            directories: None,
        },
        development: Some(UserappDevDeletionReceipt {
            runtime: BuilderDeletionSnapshot {
                app_id: APP_ID.into(),
                operation_id: "empty-development".into(),
                resources: vec![],
                docker_bind_cleanup: false,
                resource_binding: None,
            },
            registry: None,
            lease: None,
            collaborators: vec![],
            directories: None,
        }),
    };
    for (stage, step) in [
        (UserAppDeletionStage::Captured, "resources_captured"),
        (UserAppDeletionStage::ComputeRemoved, "compute_removed"),
        (
            UserAppDeletionStage::ProductionStorageRemoved,
            "production_storage_removed",
        ),
        (
            UserAppDeletionStage::DevelopmentRemoved,
            "development_removed",
        ),
    ] {
        evidence.stage = stage;
        operation = runtime
            .store
            .advance(&UserAppOperationProgress {
                app_id: APP_ID.into(),
                lifecycle_id: operation.lifecycle_id.clone(),
                operation_id: operation.operation_id.clone(),
                expected_revision: operation.revision,
                executor_id: executor_id.into(),
                state: UserAppOperationState::Running,
                step: step.into(),
                checkpoint: serde_json::to_value(&evidence).expect("deletion evidence"),
                error_code: None,
                error_message: None,
            })
            .await
            .expect("persist each confirmed empty resource boundary");
    }
    operation = runtime
        .store
        .advance(&UserAppOperationProgress {
            app_id: APP_ID.into(),
            lifecycle_id: operation.lifecycle_id.clone(),
            operation_id: operation.operation_id.clone(),
            expected_revision: operation.revision,
            executor_id: executor_id.into(),
            state: UserAppOperationState::Succeeded,
            step: "confirmed".into(),
            checkpoint: operation.checkpoint.clone(),
            error_code: None,
            error_message: None,
        })
        .await
        .expect("commit the completed deletion");
    let tombstone = runtime
        .store
        .get_application(APP_ID)
        .await
        .expect("read deleted application")
        .expect("the durable tombstone must remain");
    assert_eq!(tombstone.state, UserAppLifecycleState::Deleted);
    assert_eq!(tombstone.lifecycle_id, operation.lifecycle_id);
    assert!(tombstone.active_operations.is_empty());
    (tombstone, operation)
}

async fn attach_recreate_service(
    mut state: Arc<AppState>,
    runtime: &Arc<LocatorRuntime>,
) -> Arc<AppState> {
    let config = app_manager::config::AppManagerConfig {
        // A controlled Kubernetes lease keeps this native protocol fixture out
        // of Docker's production lock root. Physical targets still cover both
        // Docker containers and Kubernetes StatefulSet/Pod identity shapes.
        access_mode: app_manager::config::AppAccessMode::Kubernetes,
        http_expose: container_runtime_api::HttpExpose::Pingora,
        ..Default::default()
    };
    let service = Arc::new(
        app_manager::service::AppService::new(
            config,
            runtime.clone(),
            state.activity.clone(),
            None,
            runtime.store.clone(),
        )
        .await
        .expect("construct the actual recreation service"),
    );
    let unique_state =
        Arc::get_mut(&mut state).expect("fixture has not published another AppState Arc or Weak");
    unique_state.app_service = service.clone();
    unique_state.userapp_op_flight = service.operation_flight();
    service
        .set_dev_locator(Arc::new(make_locator(&state)))
        .expect("attach the actual weak AppState dev locator");
    state
}

#[tokio::test]
async fn deleted_application_recreates_after_verified_builder_absence() {
    for docker in [false, true] {
        let (state, runtime, _directory) = fixture_on_runtime(vec![None], false, docker).await;
        let (tombstone, original_deletion) = seed_deleted_application(&runtime).await;
        let state = attach_recreate_service(state, &runtime).await;
        let request = UserAppRecreateRequest {
            expected_lifecycle_id: tombstone.lifecycle_id.clone(),
            request_id: "recreate-missing-builder".into(),
        };
        let recreated = state
            .app_service
            .recreate_identity(APP_ID, request.clone())
            .await
            .expect("Deleted plus confirmed physical absence must permit recreation");
        assert_eq!(recreated.app_id, APP_ID);
        assert_eq!(recreated.state, UserAppLifecycleState::Active);
        assert_ne!(recreated.lifecycle_id, tombstone.lifecycle_id);
        assert_eq!(recreated.lifecycle_epoch, tombstone.lifecycle_epoch + 1);
        let captures = runtime.captures.load(Ordering::SeqCst);
        let readiness_reads = runtime.readiness_reads.load(Ordering::SeqCst);
        assert!(captures > 0, "absence must use captured physical identity");
        assert!(readiness_reads > 0, "absence must observe actual readiness");
        assert_eq!(
            state
                .app_service
                .recreate_identity(APP_ID, request)
                .await
                .expect("replay the same original recreation request"),
            recreated,
            "the original receipt must return the same new lifecycle"
        );
        assert_eq!(runtime.captures.load(Ordering::SeqCst), captures);
        assert_eq!(
            runtime.readiness_reads.load(Ordering::SeqCst),
            readiness_reads
        );
        assert!(
            state
                .app_service
                .recreate_identity(
                    APP_ID,
                    UserAppRecreateRequest {
                        expected_lifecycle_id: tombstone.lifecycle_id.clone(),
                        request_id: "different-recreation-request".into(),
                    },
                )
                .await
                .is_err(),
            "an old lifecycle with a new request must not create another generation"
        );
        assert!(matches!(
            state
                .userapp_store
                .admit(&UserAppAdmission {
                    app_id: APP_ID.into(),
                    lifecycle_id: Some(tombstone.lifecycle_id.clone()),
                    operation_id: "late-old-life-ensure".into(),
                    request_id: Some("late-old-life-ensure-request".into()),
                    request_fingerprint: "a".repeat(64),
                    kind: UserAppOperationKind::EnsureBuilder,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await,
            Err(shared_types::UserAppStoreError::LifecycleConflict)
        ));
        assert_eq!(
            state.userapp_store.get_application(APP_ID).await.unwrap(),
            Some(recreated)
        );
        assert_eq!(
            state
                .userapp_store
                .get_operation(APP_ID, &original_deletion.operation_id)
                .await
                .unwrap(),
            Some(original_deletion),
            "recreation must preserve the old lifecycle's completed deletion evidence"
        );
        assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn recreate_deleted_does_not_replace_retained_or_unconfirmed_builder() {
    for docker in [false, true] {
        for scenario in [
            "stopped",
            "running",
            "foreign-lifecycle",
            "unknown",
            "starting",
            "failed",
            "orphan-pod",
        ] {
            let (state, runtime, _directory) =
                fixture_on_runtime(vec![None], scenario == "foreign-lifecycle", docker).await;
            let (tombstone, original_deletion) = seed_deleted_application(&runtime).await;
            // Inject a late physical observation after confirmed deletion. The
            // tombstone alone cannot authorize replacing this retained compute.
            runtime.workload_exists.store(
                !matches!(scenario, "unknown" | "orphan-pod"),
                Ordering::SeqCst,
            );
            *runtime.readiness.lock().unwrap() = match scenario {
                "unknown" => shared_types::UserAppNoComputeState::Unknown,
                "starting" => shared_types::UserAppNoComputeState::Starting,
                "failed" => shared_types::UserAppNoComputeState::Failed,
                _ => shared_types::UserAppNoComputeState::Stopped,
            };
            if scenario == "orphan-pod" {
                runtime.pod_exists.store(true, Ordering::SeqCst);
                *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Missing;
                assert!(
                    runtime.target(&runtime.native_context).validate().is_err(),
                    "a Pod without its captured workload is not valid absence evidence"
                );
            }
            if scenario == "running" {
                let mut running = running_builder();
                if docker {
                    running.workload_uid = None;
                }
                *runtime.observations.lock().unwrap() = vec![Some(running)];
                runtime.pod_exists.store(!docker, Ordering::SeqCst);
                *runtime.running_observation.lock().unwrap() =
                    Some(container_runtime_api::UserAppReadinessTarget {
                        app_id: APP_ID.into(),
                        stage: shared_types::UserappStage::Dev,
                        instance: if docker {
                            container_runtime_api::UserAppReadinessInstance::Docker {
                                container_id: runtime.pod_uid.clone(),
                                started_at: None,
                            }
                        } else {
                            container_runtime_api::UserAppReadinessInstance::Kubernetes {
                                namespace: "test-ns".into(),
                                pod_name: "builder-locator-app-0".into(),
                                pod_uid: runtime.pod_uid.clone(),
                                container_name: "agent".into(),
                                container_id: None,
                                owner_uid: runtime.workload_uid.clone(),
                            }
                        },
                        address: None,
                        published_address: None,
                        dbx_published_address: None,
                    });
            }
            let state = attach_recreate_service(state, &runtime).await;
            let result = state
                .app_service
                .recreate_identity(
                    APP_ID,
                    UserAppRecreateRequest {
                        expected_lifecycle_id: tombstone.lifecycle_id.clone(),
                        request_id: format!("recreate-reject-{scenario}"),
                    },
                )
                .await;
            assert!(result.is_err(), "docker={docker}, scenario={scenario}");
            assert_eq!(
                state.userapp_store.get_application(APP_ID).await.unwrap(),
                Some(tombstone),
                "an unconfirmed absence must not replace the durable lifecycle"
            );
            assert_eq!(
                state
                    .userapp_store
                    .get_operation(APP_ID, &original_deletion.operation_id)
                    .await
                    .unwrap(),
                Some(original_deletion)
            );
            assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn deleted_application_cannot_publish_normal_builder_or_log_addresses() {
    for docker in [false, true] {
        let (state, runtime, _directory) = fixture_on_runtime(vec![None], false, docker).await;
        let (tombstone, original_deletion) = seed_deleted_application(&runtime).await;
        runtime.workload_exists.store(true, Ordering::SeqCst);
        runtime.pod_exists.store(!docker, Ordering::SeqCst);
        let mut running = running_builder();
        if docker {
            running.workload_uid = None;
        }
        *runtime.observations.lock().unwrap() = vec![Some(running)];
        let locator = make_locator(&state);
        assert!(
            shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID)
                .await
                .is_err(),
            "the recreate-only absence observation must not relax active address policy"
        );
        assert!(
            shared_types::UserappDevLocator::dev_logs_file_server_addr(&locator, APP_ID)
                .await
                .is_err()
        );
        assert_eq!(
            state.userapp_store.get_application(APP_ID).await.unwrap(),
            Some(tombstone)
        );
        assert_eq!(
            state
                .userapp_store
                .get_operation(APP_ID, &original_deletion.operation_id)
                .await
                .unwrap(),
            Some(original_deletion)
        );
        assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn deleted_application_recreation_propagates_capture_and_readiness_errors() {
    for docker in [false, true] {
        for failure in ["capture", "readiness"] {
            let (state, runtime, _directory) = fixture_on_runtime(vec![None], false, docker).await;
            let (tombstone, original_deletion) = seed_deleted_application(&runtime).await;
            if failure == "capture" {
                runtime.capture_fails.store(true, Ordering::SeqCst);
            } else {
                runtime.readiness_fails.store(true, Ordering::SeqCst);
            }
            let state = attach_recreate_service(state, &runtime).await;
            let request = UserAppRecreateRequest {
                expected_lifecycle_id: tombstone.lifecycle_id.clone(),
                request_id: format!("recreate-{failure}-error"),
            };
            assert!(
                state
                    .app_service
                    .recreate_identity(APP_ID, request)
                    .await
                    .is_err(),
                "docker={docker}: {failure} failure must not become physical absence"
            );
            if failure == "capture" {
                assert!(runtime.captures.load(Ordering::SeqCst) > 0);
            } else {
                assert!(runtime.readiness_reads.load(Ordering::SeqCst) > 0);
            }
            assert_eq!(
                state.userapp_store.get_application(APP_ID).await.unwrap(),
                Some(tombstone)
            );
            assert_eq!(
                state
                    .userapp_store
                    .get_operation(APP_ID, &original_deletion.operation_id)
                    .await
                    .unwrap(),
                Some(original_deletion)
            );
            assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
            assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn recreation_observing_a_new_lifecycle_cannot_create_a_third_generation() {
    for docker in [false, true] {
        let (state, runtime, _directory) = fixture_on_runtime(vec![None], false, docker).await;
        let (tombstone, original_deletion) = seed_deleted_application(&runtime).await;
        runtime.recreate_on_readiness.store(true, Ordering::SeqCst);
        let state = attach_recreate_service(state, &runtime).await;
        let request = UserAppRecreateRequest {
            expected_lifecycle_id: tombstone.lifecycle_id.clone(),
            request_id: "recreate-stale-observer".into(),
        };
        assert!(
            state
                .app_service
                .recreate_identity(APP_ID, request.clone())
                .await
                .is_err(),
            "the read-only observation must detect a different lifecycle committed during await"
        );
        assert!(runtime.readiness_reads.load(Ordering::SeqCst) > 0);
        assert!(!runtime.recreate_on_readiness.load(Ordering::SeqCst));
        let concurrent = state
            .userapp_store
            .get_application(APP_ID)
            .await
            .unwrap()
            .expect("the other recreation committed a real durable identity");
        assert_eq!(concurrent.state, UserAppLifecycleState::Active);
        assert_ne!(concurrent.lifecycle_id, tombstone.lifecycle_id);
        assert_eq!(concurrent.lifecycle_epoch, tombstone.lifecycle_epoch + 1);
        assert_eq!(
            state
                .userapp_store
                .recreate(APP_ID, &tombstone.lifecycle_id, "concurrent-recreate")
                .await
                .expect("read back the actual concurrent recreation receipt"),
            concurrent
        );
        for _ in 0..2 {
            assert!(
                state
                    .app_service
                    .recreate_identity(APP_ID, request.clone())
                    .await
                    .is_err(),
                "the rejected observer must not acquire a successful recreation receipt"
            );
        }
        assert_eq!(
            state.userapp_store.get_application(APP_ID).await.unwrap(),
            Some(concurrent),
            "repeated stale requests must neither create a third life nor rewrite the winner"
        );
        assert_eq!(
            state
                .userapp_store
                .get_operation(APP_ID, &original_deletion.operation_id)
                .await
                .unwrap(),
            Some(original_deletion)
        );
        assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
    }
}
