use super::*;

/// Exercise the real persistent intent check used by every non-explicit
/// ensure attempt. The lease/checkpoint fixtures model completed runtime work;
/// this is not a test of Docker/Kubernetes resource mutation.
#[tokio::test]
async fn superseded_ensure_crosses_control_but_never_reverses_completed_stop() {
    use shared_types::*;
    let directory = tempfile::tempdir().expect("directory");
    let store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
        &directory.path().join("control.turso.db"),
    )
    .await
    .expect("store");
    for (app_id, action) in [
        ("stoppedapp", ComputeControlAction::Stop),
        ("restartedapp", ComputeControlAction::Restart),
    ] {
        let app = store.ensure_identity(app_id).await.expect("app");
        let control = store
            .admit_compute_control(&ComputeControlRequest {
                app_id: app_id.into(),
                lifecycle_id: app.lifecycle_id.clone(),
                scope: UserAppOperationScope::Dev,
                operation_id: format!("{app_id}control"),
                request_id: format!("{app_id}request"),
                request_fingerprint: "a".repeat(64),
                action,
                restart_image_roll: false,
            })
            .await
            .expect("admit control");
        let identity = ComputeExecutorIdentity {
            app_id: app_id.into(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: control.operation_id.clone(),
            generation: control.generation,
            executor_id: "worker".into(),
        };
        store
            .claim_compute_control(&identity, control.revision)
            .await
            .expect("claim");
        let receipt = UserAppOperationLeaseReceipt::Kubernetes {
            service_type: ServiceType::UserappBuilder,
            namespace: "test".into(),
            name: "builderlease".into(),
            uid: format!("{app_id}lease"),
            resource_version: "123".into(),
            token: "fixturetoken".into(),
        };
        let mut record = store
            .bind_compute_lease(&identity, &receipt)
            .await
            .expect("lease");
        let (observed, started) = tokio::sync::oneshot::channel();
        let observed = std::cell::RefCell::new(Some(observed));
        let attempts = std::cell::Cell::new(0);
        let work = retry_builder_ensure(
            tokio::time::Instant::now() + std::time::Duration::from_secs(5),
            || {
                let attempt = attempts.get() + 1;
                attempts.set(attempt);
                let store = &store;
                let observed = &observed;
                async move {
                    if attempt == 1 {
                        return Err(anyhow::Error::new(
                            crate::userapp_builder::BuilderEnsureSuperseded {
                                operation_id: "cancelledensure".into(),
                            },
                        )
                        .context("old ensure cancelled"));
                    }
                    let access = store
                        .check_compute_access(app_id, UserAppOperationScope::Dev, false)
                        .await;
                    if let Some(observed) = observed.borrow_mut().take() {
                        assert!(matches!(
                            &access,
                            Err(UserAppStoreError::OperationInProgress(_))
                        ));
                        observed.send(()).expect("notify control observation");
                    }
                    access.map_err(anyhow::Error::from)
                }
            },
        );
        let complete_control = async {
            started.await.expect("waiter must see active control");
            let stages: &[ComputeControlStage] = if action == ComputeControlAction::Stop {
                &[
                    ComputeControlStage::Stopping,
                    ComputeControlStage::Stopped,
                    ComputeControlStage::Completed,
                ]
            } else {
                &[
                    ComputeControlStage::Stopping,
                    ComputeControlStage::Stopped,
                    ComputeControlStage::Starting,
                    ComputeControlStage::Verifying,
                    ComputeControlStage::Completed,
                ]
            };
            for &stage in stages {
                record = store
                    .advance_compute_control(&ComputeControlProgress {
                        identity: identity.clone(),
                        expected_revision: record.revision,
                        state: if stage == ComputeControlStage::Completed {
                            ComputeControlState::Succeeded
                        } else {
                            ComputeControlState::Running
                        },
                        stage,
                        checkpoint: serde_json::json!({"fixture": "acknowledged runtime work"}),
                        error_code: None,
                        error_message: None,
                    })
                    .await
                    .expect("complete control stage");
            }
            store
                .forget_compute_lease(&identity, &receipt)
                .await
                .expect("release receipt");
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(6), async {
            tokio::join!(work, complete_control)
        })
        .await
        .expect("bounded coordination");
        assert!(attempts.get() >= 3, "wait through cancellation and control");
        if action == ComputeControlAction::Stop {
            let error = result.expect_err("reads cannot undo Stop");
            assert!(matches!(
                error.downcast_ref::<UserAppStoreError>(),
                Some(UserAppStoreError::InvalidOperation(_))
            ));
            assert!(!waitable_builder_conflict(&error));
        } else {
            result.expect("Restart allows the next ensure");
        }
        assert_eq!(
            store
                .compute_desired_stopped(app_id, &app.lifecycle_id, UserAppOperationScope::Dev)
                .await
                .expect("intent"),
            action == ComputeControlAction::Stop
        );
    }
    store.shutdown().await.expect("shutdown");
}

#[tokio::test(start_paused = true)]
async fn conflict_deadline_preserves_last_blocker_without_another_attempt() {
    for budget_ms in [750, 1_000] {
        let attempts = std::cell::Cell::new(0);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
        // Same nesting as resolve_dev_addr: the full address lookup has
        // an outer deadline as well as the ensure waiter.
        let error = tokio::time::timeout_at(
            deadline,
            retry_builder_ensure(deadline, || {
                attempts.set(attempts.get() + 1);
                let mut conflict = blocker(shared_types::UserAppOperationScope::Dev);
                if let shared_types::UserAppStoreError::OperationInProgress(ref mut record) =
                    conflict
                {
                    record.operation_id = format!("control-{}", attempts.get());
                }
                std::future::ready(Err::<(), _>(anyhow::Error::new(conflict).context("ensure")))
            }),
        )
        .await
        .expect("inner waiter must retain the known conflict at the outer deadline")
        .expect_err("control is still in progress");
        assert_eq!(attempts.get(), 2, "must not retry at or after the deadline");
        let response = *builder_control_response(&error);
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("body");
        let envelope: serde_json::Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(
            envelope["code"],
            shared_types::error_codes::ERR_OPERATION_IN_PROGRESS
        );
        assert!(envelope.get("operation_id").is_none());
        assert_eq!(envelope["data"]["holder_operation_id"], "control-2");
        assert_eq!(envelope["data"]["retryable"], false);
        assert_eq!(envelope["blocker"]["operation_id"], "control-2");
    }
}

#[tokio::test(start_paused = true)]
async fn expired_forward_wait_does_not_begin_ensure() {
    let attempted = std::cell::Cell::new(false);
    let error = retry_builder_ensure(tokio::time::Instant::now(), || {
        attempted.set(true);
        std::future::ready(Ok::<(), anyhow::Error>(()))
    })
    .await
    .expect_err("expired deadline");
    assert!(
        !attempted.get(),
        "an expired request must not admit a new ensure"
    );
    assert!(error.is::<shared_types::UserAppWaitTimeout>());
}

fn blocker(scope: shared_types::UserAppOperationScope) -> shared_types::UserAppStoreError {
    shared_types::UserAppStoreError::OperationInProgress(shared_types::UserAppOperationBlocker {
        scope,
        operation_id: "op-1".into(),
        kind: shared_types::UserAppOperationKind::RestartBuilder,
        state: shared_types::UserAppOperationState::Running,
        step: "draining_previous".into(),
    })
}

/// 准入冲突（Dev scope）→ 可等待（既有行为回归锁）。
#[test]
fn dev_scope_admission_conflict_is_waitable() {
    let error = anyhow::Error::new(blocker(shared_types::UserAppOperationScope::Dev));
    assert!(waitable_builder_conflict(&error));
}

/// Prod scope 冲突不属于 builder 控制窗口 → 快速失败。
#[test]
fn prod_scope_conflict_is_not_waitable() {
    let error = anyhow::Error::new(blocker(shared_types::UserAppOperationScope::Prod));
    assert!(!waitable_builder_conflict(&error));
}

/// ensure 被在途控制操作取消（观察循环挂 BuilderEnsureSuperseded）→ 可等待。
/// 事故锚点：nuwax-k8s-test app 184 file-list 650ms 502，同窗口
/// git/status 走等待 8.1s 成功——两条路径在此合一。
#[test]
fn superseded_ensure_is_waitable() {
    let error = anyhow::Error::new(crate::userapp_builder::BuilderEnsureSuperseded {
        operation_id: "f1637d74".into(),
    })
    .context(
        "Builder operation f1637d74 ended as Failed: ensure UserappBuilder failed: \
             Builder creation cancelled after acknowledged writes",
    );
    assert!(waitable_builder_conflict(&error));
}

/// 其它失败（如镜像拉取失败）→ 维持快速失败，等待不扩大到真错误。
#[test]
fn ordinary_failure_is_not_waitable() {
    let error = anyhow::anyhow!("ensure UserappBuilder failed: image pull backoff");
    assert!(!waitable_builder_conflict(&error));
}
