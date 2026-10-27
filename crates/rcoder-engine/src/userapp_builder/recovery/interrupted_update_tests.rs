//! The NW-295 predecessor through the real engine drain and lifecycle store.
use super::*;
use crate::userapp_builder::compute_control;
use crate::userapp_builder::creation::fence_settler_tests::{FenceRuntime, runtime_state};
use shared_types::*;
use std::sync::{Arc, atomic::Ordering};

async fn run_case(interrupted: bool, physically_ready: bool, expired: bool) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("drain.db"),
        )
        .await
        .unwrap(),
    );
    let app = store.ensure_identity("updated-deployment").await.unwrap();
    let input = UserAppExecutionInput::new(serde_json::json!({"version":1,"restart":true,
        "params":{},"previous":{},"request":{"pg":{"username":"test_role","password":"fixture-only"}}}).to_string());
    let UserAppAdmissionOutcome::Accepted(pending) = store
        .admit_with_input(
            &UserAppAdmission {
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: "original-deployment".into(),
                request_id: Some("original-request".into()),
                request_fingerprint: "b".repeat(64),
                kind: UserAppOperationKind::RestartDeployment,
                command: Some(UserAppControlCommand::Deploy {
                    restart: true,
                    serving_recovery: false,
                    input_digest: input.digest(),
                }),
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await
        .unwrap()
    else {
        panic!("new deployment")
    };
    let context = UserAppExecutionContext {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        operation_id: pending.operation_id.clone(),
        executor_id: "original-worker".into(),
        request_fingerprint: pending.request_fingerprint.clone(),
    };
    let checkpoint = serde_json::json!({
        "target":UserAppMutationTarget {context:context.clone(),resource:AppResourceIdentity {
            kind:AppResourceKind::Deployment,name:"rcoder-app-updated-deployment".into(),uid:"original-uid".into(),resource_version:Some("7".into()),
        }},
        "resource":{"container_id":"rcoder-app-updated-deployment","project_id":app.app_id},
        "storage_target":null,"requested_storage_size":null,
    });
    let old = store
        .advance(&UserAppOperationProgress {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: pending.operation_id,
            expected_revision: pending.revision,
            executor_id: context.executor_id.clone(),
            state: UserAppOperationState::Running,
            step: "runtime_updated".into(),
            checkpoint,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    store
        .bind_operation_deadline(
            &old.app_id,
            &old.operation_id,
            &old.lifecycle_id,
            chrono::Utc::now().timestamp_millis() + if expired { -1 } else { 60_000 },
        )
        .await
        .unwrap();
    let successor = if interrupted {
        let next = store
            .admit_compute_control(&ComputeControlRequest {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                scope: UserAppOperationScope::Prod,
                operation_id: "next-restart".into(),
                request_id: "next-request".into(),
                request_fingerprint: "a".repeat(64),
                action: ComputeControlAction::Restart,
                restart_image_roll: false,
                builder_startup_mode: Default::default(),
            })
            .await
            .unwrap();
        let identity = ComputeExecutorIdentity {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: next.scope,
            operation_id: next.operation_id.clone(),
            generation: next.generation,
            executor_id: "next-worker".into(),
        };
        let next = store
            .claim_compute_control(&identity, next.revision)
            .await
            .unwrap();
        Some((next, identity))
    } else {
        None
    };
    let runtime = Arc::new(FenceRuntime::default());
    let state = runtime_state(runtime.clone(), store.clone(), None).await;
    if let Some((next, identity)) = &successor {
        assert!(
            !compute_control::drain_previous_once(&state, next, identity)
                .await
                .unwrap()
        );
    }
    let readiness = app_manager::service::AppService::new(
        app_manager::config::AppManagerConfig::default(),
        runtime.clone(),
        state.activity.clone(),
        None,
        store.clone(),
    )
    .await
    .unwrap();
    readiness
        .set_readiness_reader(Arc::new(ReadyReader(physically_ready)))
        .unwrap();
    assert!(
        !readiness
            .get_app_readiness(UserappStage::Prod, &app.app_id)
            .await
            .unwrap()
            .ready
    );
    {
        let mut saturated = RecoveryTasks::default();
        for index in 0..MAX_RECOVERIES - 1 {
            assert!(saturated.push(
                format!("busy-{index}"),
                std::future::pending::<anyhow::Result<()>>()
            ));
        }
        discover(&state, &mut saturated, &mut None).await.unwrap();
        assert!(
            !saturated
                .active
                .contains(&format!("deployment-update-expire:{}", old.operation_id)),
            "the expiry observer must leave the last slot to normal recovery work"
        );
    }
    let mut tasks = RecoveryTasks::default();
    discover(&state, &mut tasks, &mut None).await.unwrap();
    while let Some((_, result)) = tasks.next().await {
        result.unwrap();
    }
    let closed = store
        .get_operation(&app.app_id, &old.operation_id)
        .await
        .unwrap()
        .unwrap();
    if !expired {
        assert_eq!(closed.state, UserAppOperationState::Running);
        assert!(
            !readiness
                .get_app_readiness(UserappStage::Prod, &app.app_id)
                .await
                .unwrap()
                .ready
        );
        assert!(
            store
                .advance(&UserAppOperationProgress {
                    app_id: app.app_id,
                    lifecycle_id: app.lifecycle_id,
                    operation_id: old.operation_id,
                    expected_revision: old.revision,
                    executor_id: context.executor_id,
                    state: UserAppOperationState::Running,
                    step: "explicit_pg_applying".into(),
                    checkpoint: old.checkpoint,
                    error_code: None,
                    error_message: None,
                })
                .await
                .is_ok(),
            "a legitimate in-budget worker retains its next-write checkpoint"
        );
        return;
    }
    assert_eq!(closed.state, UserAppOperationState::Failed);
    assert_eq!(closed.checkpoint["target"], old.checkpoint["target"]);
    assert!(
        store
            .advance(&UserAppOperationProgress {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                operation_id: old.operation_id,
                expected_revision: old.revision,
                executor_id: context.executor_id,
                state: UserAppOperationState::Running,
                step: "explicit_pg_applying".into(),
                checkpoint: old.checkpoint,
                error_code: None,
                error_message: None,
            })
            .await
            .is_err()
    );
    if let Some((next, identity)) = &successor {
        assert!(
            compute_control::drain_previous_once(&state, next, identity)
                .await
                .unwrap(),
            "repeat observation is idempotent"
        );
        assert!(
            !readiness
                .get_app_readiness(UserappStage::Prod, &app.app_id)
                .await
                .unwrap()
                .ready,
            "the successor is still legitimately running"
        );
    } else {
        let observed = readiness
            .get_app_readiness(UserappStage::Prod, &app.app_id)
            .await
            .unwrap();
        assert_eq!(
            observed.ready, physically_ready,
            "expiry must reveal the real runtime state"
        );
        assert_eq!(
            observed.status,
            if physically_ready {
                UserAppReadinessStatus::Ready
            } else {
                UserAppReadinessStatus::Starting
            }
        );
    }
    assert_eq!(runtime.hot_writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.compute_prepares.load(Ordering::SeqCst),
        0,
        "drain itself must not issue runtime writes"
    );
}

struct ReadyReader(bool);
#[async_trait::async_trait]
impl UserAppReadinessReader for ReadyReader {
    async fn observe(
        &self,
        _: &str,
        _: UserappStage,
        _: Duration,
    ) -> Result<UserAppReadinessObservation, String> {
        Ok(UserAppReadinessObservation::Snapshot {
            physical: UserAppReadinessPhysical {
                instance_id: Some("original-workload-uid".into()),
                address: None,
                channel: UserAppReadinessChannel::Direct,
            },
            snapshot: Box::new(UserAppBusinessReadiness {
                ready: self.0,
                status: if self.0 {
                    UserAppReadinessStatus::Ready
                } else {
                    UserAppReadinessStatus::Starting
                },
                reason_code: None,
                checked_at: chrono::Utc::now().to_rfc3339(),
                runtime_instance_id: Some("live-owner".into()),
                serving_release_id: Some("live-release".into()),
                target_release_id: None,
                operation_id: None,
                observation_revision: 1,
                proxy: UserAppProxyReadiness {
                    ready: self.0,
                    status: if self.0 {
                        UserAppReadinessStatus::Ready
                    } else {
                        UserAppReadinessStatus::Starting
                    },
                    reason_code: None,
                    error_origin_contract: None,
                    confirmation: None,
                },
                services: vec![],
            }),
        })
    }
}
#[tokio::test]
async fn expired_runtime_updated_deployment_unblocks_existing_drain() {
    run_case(true, true, true).await;
}
#[tokio::test]
async fn expired_runtime_updated_deployment_restores_live_readiness() {
    run_case(false, true, true).await;
}

#[tokio::test]
async fn expired_runtime_updated_deployment_does_not_fabricate_ready() {
    run_case(false, false, true).await;
}

#[tokio::test]
async fn in_budget_runtime_updated_deployment_keeps_its_worker_authority() {
    run_case(false, true, false).await;
}
