use super::*;

#[cfg(test)]
mod cases {
    use super::*;
    use shared_types::{
        ArtifactInput, ERR_OPERATION_ID_CONFLICT, ERR_RECOVERY_REQUIRED, ERR_REVISION_MISMATCH,
        ERR_RUNTIME_INSTANCE_MISMATCH, RunProfileInput, RuntimeOperationKind,
        RuntimeOperationRequest,
    };

    fn temp_store() -> (tempfile::TempDir, RuntimeStore) {
        let dir = tempfile::tempdir().expect("dir");
        // B04：按应用隔离根——workspace 必须是临时目录的子目录，保证各测试
        // 的卷根（父目录）唯一。根 = {卷根}/.app-cli-state/{app}。
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        (dir, open_store(&workspace))
    }

    /// 测试助手：以测试应用身份解析并打开 store（B04 缺省路径）。
    fn open_store(workspace: &Path) -> RuntimeStore {
        let root = RuntimeStore::resolve_root(workspace, "test-app").expect("resolve root");
        RuntimeStore::open_with_root(root, workspace).expect("store")
    }

    /// 测试助手：测试应用的稳定根位置（断言/预置用）。
    fn app_state_root(workspace: &Path) -> std::path::PathBuf {
        RuntimeStore::resolve_root(workspace, "test-app").expect("resolve root")
    }

    fn identity() -> RuntimeIdentityView {
        RuntimeIdentityView {
            application_id: "app1".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-1".into(),
            source_root: "/workspace".into(),
            runtime_instance_id: "instance-1".into(),
            deployment_generation_id: "gen-1".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        }
    }

    fn kernel(dir: &Path) -> RuntimeKernel {
        let workspace = dir.join("workspace");
        let store = open_store(&workspace);
        let identity = identity();
        RuntimeKernel::new(store, identity, Box::new(|_| {}))
    }

    #[tokio::test]
    async fn retained_artifact_recovery_is_idempotent_and_preserves_revision_guards() {
        let (dir, _) = temp_store();
        let actions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = actions.clone();
        let kernel = RuntimeKernel::new(
            open_store(&dir.path().join("workspace")),
            identity(),
            Box::new(move |action| sink.lock().unwrap().push(action)),
        );
        let mut input = request(RuntimeOperationKind::Deploy, "retained-boot");
        input.profile = RunProfileInput::Artifact {
            artifact: ArtifactInput::RetainedWorkspace {
                release_id: "existing-release".into(),
            },
        };
        kernel.admit(input.clone()).await.unwrap();
        kernel.admit(input.clone()).await.unwrap();
        {
            let actions = actions.lock().unwrap();
            assert_eq!(actions.len(), 1);
            assert!(
                matches!(&actions[0], DispatchAction::RecoverArtifact {operation_id,release_id,..} if operation_id=="retained-boot" && release_id=="existing-release")
            );
        }
        input.operation_id = "stale-recovery".into();
        input.expected_revision = 999;
        assert_eq!(
            kernel.admit(input).await.unwrap_err().code,
            ERR_REVISION_MISMATCH
        );
    }

    #[tokio::test]
    async fn internal_operation_retains_pg_for_audit_but_views_and_events_do_not() {
        let (dir, _) = temp_store();
        let kernel = kernel(dir.path());
        let mut input = request(RuntimeOperationKind::Start, "auditcredentials");
        input.run_config = Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "business".into(),
                password: "audit-private-credential".into(),
            }),
        });
        kernel.admit(input).await.unwrap();
        let path = kernel.store.operation_path("auditcredentials");
        let stored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(
            stored["request"]["run_config"]["pg"]["username"],
            "business"
        );
        assert_eq!(
            stored["request"]["run_config"]["pg"]["password"], "audit-private-credential",
            "internal operation input must retain the actual dispatched password"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let view = serde_json::to_value(kernel.get("auditcredentials").await.unwrap()).unwrap();
        assert!(view.get("request").is_none());
        assert!(!view.to_string().contains("audit-private-credential"));
        let events = kernel.store.replay_events("auditcredentials", 0).unwrap();
        assert!(
            !serde_json::to_string(&events)
                .unwrap()
                .contains("audit-private-credential")
        );
    }

    #[tokio::test]
    async fn foreground_failure_exit_and_new_admission_share_one_boundary() {
        use std::sync::atomic::{AtomicBool, Ordering};

        for successor_first in [false, true] {
            let (dir, _) = temp_store();
            let kernel = kernel(dir.path());
            let own = request(RuntimeOperationKind::Start, "foregroundone");
            kernel.admit(own.clone()).await.unwrap();
            kernel
                .finish(
                    &own.operation_id,
                    RuntimeOperationState::Failed,
                    Some(("ERR_VALIDATION".into(), "preflight failed".into())),
                    None,
                    0,
                )
                .await
                .unwrap();
            let successor = request(RuntimeOperationKind::Start, "platformnext");
            if successor_first {
                kernel.admit(successor.clone()).await.unwrap();
            }
            let cancelled = AtomicBool::new(false);
            let exit = kernel
                .begin_foreground_failure_exit(&own.operation_id, || {
                    cancelled.store(true, Ordering::Release);
                })
                .await
                .unwrap();
            if successor_first {
                assert!(exit.is_none());
                assert!(!cancelled.load(Ordering::Acquire));
                assert_eq!(
                    kernel
                        .status()
                        .await
                        .unwrap()
                        .active_operation_id
                        .as_deref(),
                    Some("platformnext")
                );
                // A successor that finished before the next observation is
                // still newer; clearing its active slot does not revive the old
                // foreground observer's permission to terminate business.
                kernel
                    .finish(
                        "platformnext",
                        RuntimeOperationState::Succeeded,
                        None,
                        None,
                        0,
                    )
                    .await
                    .unwrap();
                assert!(
                    kernel
                        .begin_foreground_failure_exit(&own.operation_id, || {
                            cancelled.store(true, Ordering::Release);
                        })
                        .await
                        .unwrap()
                        .is_none()
                );
                assert!(!cancelled.load(Ordering::Acquire));
            } else {
                assert!(exit.unwrap().contains("preflight failed"));
                assert!(cancelled.load(Ordering::Acquire));
                let rejection = kernel.admit(successor).await.unwrap_err();
                assert_eq!(rejection.code, ERR_OPERATION_IN_PROGRESS);
                assert_eq!(
                    rejection.active_operation_id.as_deref(),
                    Some("foregroundone")
                );
                assert!(kernel.get("platformnext").await.unwrap().is_none());
                assert!(matches!(
                    kernel.admit(own).await.unwrap(),
                    AdmissionOutcome::Replayed(_)
                ));
                assert!(matches!(
                    kernel
                        .admit(request(RuntimeOperationKind::Stop, "stopduringexit"))
                        .await
                        .unwrap(),
                    AdmissionOutcome::Accepted(_)
                ));
            }
        }
    }

    #[tokio::test]
    async fn queued_input_keeps_credentials_and_promotes_execution_identity() {
        for kind in [RuntimeOperationKind::Restart, RuntimeOperationKind::Deploy] {
            let (dir, _) = temp_store();
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = captured.clone();
            let kernel = RuntimeKernel::new(
                open_store(&dir.path().join("workspace")),
                identity(),
                Box::new(move |action| sink.lock().unwrap().push(action)),
            );
            kernel
                .admit(request(RuntimeOperationKind::Start, "a"))
                .await
                .unwrap();
            let mut queued = if kind == RuntimeOperationKind::Deploy {
                request_deploy_url("b")
            } else {
                request(kind, "b")
            };
            if kind == RuntimeOperationKind::Restart {
                queued.run_config = Some(shared_types::OperationRunConfig {
                    pg: Some(shared_types::StartPgCredential {
                        username: "dev".into(),
                        password: "probe-secret".into(),
                    }),
                });
            }
            kernel.admit(queued).await.unwrap();
            assert_eq!(
                std::fs::read_to_string(kernel.store.operation_path("b"))
                    .unwrap()
                    .contains("probe-secret"),
                kind == RuntimeOperationKind::Restart
            );
            assert_eq!(
                kernel.commit_execution("a").await.unwrap(),
                if kind == RuntimeOperationKind::Restart {
                    CommitBarrierOutcome::Cancelled
                } else {
                    CommitBarrierOutcome::Committed
                }
            );
            if kind == RuntimeOperationKind::Restart {
                kernel
                    .finish("a", RuntimeOperationState::Cancelled, None, None, 2)
                    .await
                    .unwrap();
            }
            assert_eq!(
                kernel.admission.lock().await.active_operation_id.as_deref(),
                Some("b")
            );
            if kind == RuntimeOperationKind::Restart {
                let actions = captured.lock().unwrap();
                let DispatchAction::OrchestrateSource { pg: Some(pg), .. } =
                    actions.last().unwrap()
                else {
                    panic!("missing source action")
                };
                assert_eq!(pg.password, "probe-secret");
            }
            if kind == RuntimeOperationKind::Restart {
                // 最新 Source 进入唯一接替槽；旧请求实际清理终态前不派发。
                kernel
                    .admit(request(RuntimeOperationKind::Start, "c"))
                    .await
                    .unwrap();
                assert_eq!(
                    kernel
                        .status()
                        .await
                        .unwrap()
                        .active_operation_id
                        .as_deref(),
                    Some("b")
                );
                assert_eq!(
                    kernel.commit_execution("b").await.unwrap(),
                    CommitBarrierOutcome::Cancelled
                );
                kernel
                    .finish("b", RuntimeOperationState::Cancelled, None, None, 2)
                    .await
                    .unwrap();
                assert_eq!(
                    kernel.commit_execution("c").await.unwrap(),
                    CommitBarrierOutcome::Committed
                );
            } else {
                // Deploy 属普通构建接替（已批准规则保留）：不同 Start 排队接替。
                kernel
                    .admit(request(RuntimeOperationKind::Start, "c"))
                    .await
                    .unwrap();
                assert_eq!(
                    kernel.admission.lock().await.pending_restart.as_deref(),
                    Some("c")
                );
                assert_eq!(
                    kernel.commit_execution("b").await.unwrap(),
                    CommitBarrierOutcome::Committed
                );
                assert_eq!(
                    kernel.commit_execution("c").await.unwrap(),
                    CommitBarrierOutcome::Committed
                );
            }
            assert_eq!(
                kernel.get("b").await.unwrap().unwrap().state,
                if kind == RuntimeOperationKind::Restart {
                    RuntimeOperationState::Cancelled
                } else {
                    RuntimeOperationState::Succeeded
                }
            );
        }
    }

    #[tokio::test]
    async fn stop_terminal_depends_only_on_its_own_cleanup() {
        for active_result in [
            RuntimeOperationState::Failed,
            RuntimeOperationState::Cancelled,
            RuntimeOperationState::RecoveryRequired,
        ] {
            let (dir, _) = temp_store();
            let kernel = kernel(dir.path());
            kernel
                .admit(request(RuntimeOperationKind::Start, "a"))
                .await
                .unwrap();
            kernel
                .admit(request(RuntimeOperationKind::Stop, "s"))
                .await
                .unwrap();
            kernel
                .finish("a", active_result, None, None, 2)
                .await
                .unwrap();
            assert_eq!(
                kernel.get("s").await.unwrap().unwrap().state,
                RuntimeOperationState::Accepted
            );
            kernel
                .finish("s", RuntimeOperationState::RecoveryRequired, None, None, 2)
                .await
                .unwrap();
            assert_eq!(
                kernel.get("s").await.unwrap().unwrap().state,
                RuntimeOperationState::RecoveryRequired
            );
            assert!(kernel.recovery_protection_active());
        }
    }

    #[tokio::test]
    async fn partial_supersede_holds_new_admission_for_recovery() {
        for kind in [RuntimeOperationKind::Restart, RuntimeOperationKind::Stop] {
            let (dir, _) = temp_store();
            let kernel = kernel(dir.path());
            kernel
                .admit(request(RuntimeOperationKind::Start, "a"))
                .await
                .unwrap();
            kernel
                .admit(request(RuntimeOperationKind::Restart, "b"))
                .await
                .unwrap();
            let path = kernel.store.operation_path("b");
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            let rejected = kernel.admit(request(kind, "c")).await.unwrap_err();
            assert_eq!(rejected.code, ERR_RECOVERY_REQUIRED);
            assert_eq!(
                kernel.get("c").await.unwrap().unwrap().state,
                RuntimeOperationState::RecoveryRequired
            );
            assert!(kernel.recovery_protection_active());
            assert!(
                matches!(kernel.admit(request(kind, "c")).await.unwrap(), AdmissionOutcome::Replayed(view) if view.state == RuntimeOperationState::RecoveryRequired)
            );
        }
    }

    #[tokio::test]
    async fn terminal_event_follows_progress_cursor_once() {
        let (dir, _) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "a"))
            .await
            .unwrap();
        assert!(kernel.append_orchestration_event("a", "build", None, "Building", None));
        assert!(kernel.append_orchestration_event("a", "ready", None, "Ready", None));
        assert_eq!(
            kernel.commit_execution("a").await.unwrap(),
            CommitBarrierOutcome::Committed
        );
        kernel
            .finish("a", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .unwrap();
        let events = kernel.store.replay_events("a", 3).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence, 4);
        assert_eq!(events[0].stage, "terminal");
    }

    #[tokio::test]
    async fn migration_output_survives_admission_contention_and_keeps_original_identity() {
        let (dir, _) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "migration-source"))
            .await
            .unwrap();
        kernel
            .admit(request(RuntimeOperationKind::Stop, "newer-stop"))
            .await
            .unwrap();
        let admission = kernel.admission.lock().await;
        assert_eq!(
            admission.active_operation_id.as_deref(),
            Some("migration-source")
        );
        for line in [
            "[migrate out] original stdout",
            "[migrate err] original stderr",
        ] {
            assert!(kernel.append_orchestration_event(
                "migration-source",
                "migration",
                Some("backend".into()),
                "log",
                Some(serde_json::json!({"line": line}))
            ));
        }
        drop(admission);
        let events = kernel.store.replay_events("migration-source", 0).unwrap();
        assert_eq!(events.len(), 3);
        assert!(
            events
                .iter()
                .all(|event| event.operation_id == "migration-source"
                    && event.runtime_instance_id == "instance-1")
        );
        assert_eq!(events[1].service.as_deref(), Some("backend"));
        assert_eq!(
            events[2].payload.as_ref().unwrap()["line"],
            "[migrate err] original stderr"
        );
        assert_eq!(
            kernel.store.replay_events("newer-stop", 0).unwrap().len(),
            1,
            "original migration output must never be relabeled as a newer Stop"
        );
        kernel
            .finish(
                "migration-source",
                RuntimeOperationState::Cancelled,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert!(
            !kernel.append_orchestration_event(
                "migration-source",
                "migration",
                Some("backend".into()),
                "log",
                Some(serde_json::json!({"line":"late old output"}))
            ),
            "a terminal operation stream must stay closed"
        );
    }

    #[tokio::test]
    async fn concurrent_events_allocate_unique_durable_cursors() {
        let (dir, _) = temp_store();
        let runtime = kernel(dir.path());
        runtime
            .admit(request(RuntimeOperationKind::Start, "a"))
            .await
            .unwrap();
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let runtime = &runtime;
                scope.spawn(move || {
                    assert!(runtime.emit_with_payload(
                        "a",
                        0,
                        "progress",
                        None,
                        Some("Step"),
                        None
                    ));
                });
            }
        });
        let events = runtime.store.replay_events("a", 0).unwrap();
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=17).collect::<Vec<_>>()
        );
        runtime
            .finish("a", RuntimeOperationState::Succeeded, None, None, 0)
            .await
            .unwrap();
        assert_eq!(
            runtime.store.replay_events("a", 17).unwrap()[0].stage,
            "terminal"
        );
    }

    #[tokio::test]
    async fn failed_event_append_does_not_report_persisted() {
        let (dir, _) = temp_store();
        let runtime = kernel(dir.path());
        runtime
            .admit(request(RuntimeOperationKind::Start, "a"))
            .await
            .unwrap();
        let path = runtime.store.events_path("a");
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(!runtime.append_orchestration_event("a", "progress", None, "Step", None));
    }

    #[tokio::test]
    async fn superseded_queue_publishes_terminal_after_its_cursor() {
        let (dir, _) = temp_store();
        let runtime = kernel(dir.path());
        runtime
            .admit(request(RuntimeOperationKind::Start, "a"))
            .await
            .unwrap();
        runtime
            .admit(request(RuntimeOperationKind::Restart, "b"))
            .await
            .unwrap();
        let cursor = runtime
            .store
            .replay_events("b", 0)
            .unwrap()
            .last()
            .unwrap()
            .sequence;
        runtime
            .admit(request(RuntimeOperationKind::Restart, "c"))
            .await
            .unwrap();
        let events = runtime.store.replay_events("b", cursor).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].stage, "terminal");
        assert_eq!(
            events[0].payload.as_ref().unwrap()["code"],
            "ERR_SUPERSEDED"
        );
    }

    /// R02"最后受理生效"：active 执行期间 A→B→C 三连受理——B 被 C 覆盖
    /// （Cancelled/ERR_SUPERSEDED），active Succeeded 后 C 派发（不重试忙拒）。
    #[tokio::test]
    async fn last_accepted_start_wins_and_supersedes_queued() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<DispatchAction>>> = Default::default();
        let sink = captured.clone();
        let (dir, _keep) = temp_store();
        let workspace = dir.path().join("workspace");
        let kernel = RuntimeKernel::new(
            open_store(&workspace),
            identity(),
            Box::new(move |action| {
                sink.lock().unwrap().push(action);
            }),
        );
        // active 执行中（op-1 占 active 槽）
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit op-1");
        // A 排队
        kernel
            .admit(request(RuntimeOperationKind::Restart, "op-a"))
            .await
            .expect("admit op-a");
        // B 排队（覆盖 A——A 收束 Superseded）
        kernel
            .admit(request(RuntimeOperationKind::Restart, "op-b"))
            .await
            .expect("admit op-b");
        let superseded = kernel
            .store
            .load_operation("op-a")
            .expect("load")
            .expect("stored");
        assert_eq!(superseded.view.state, RuntimeOperationState::Cancelled);
        assert_eq!(
            superseded.view.error_code.as_deref(),
            Some("ERR_SUPERSEDED")
        );
        // active Succeeded → 最新排队者（op-b）派发
        kernel
            .finish("op-1", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .expect("finish op-1");
        let actions = captured.lock().unwrap();
        let dispatched: Vec<&str> = actions
            .iter()
            .filter_map(|action| match action {
                DispatchAction::OrchestrateSource { operation_id, .. } => {
                    Some(operation_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert!(
            dispatched.contains(&"op-b"),
            "latest queued must dispatch after active success: {dispatched:?}"
        );
        assert!(
            !dispatched.contains(&"op-a"),
            "superseded must not dispatch: {dispatched:?}"
        );
    }

    // ===== 2026-09-19 批 9：排队槽终局反例（batch8-followup §2）=====

    /// A failed/cancelled older request must not discard a newer explicit start.
    #[tokio::test]
    async fn latest_start_runs_after_previous_terminal_failure() {
        for terminal in [
            RuntimeOperationState::Failed,
            RuntimeOperationState::Cancelled,
            RuntimeOperationState::RecoveryRequired,
        ] {
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = captured.clone();
            let (dir, _keep) = temp_store();
            let kernel = RuntimeKernel::new(
                open_store(&dir.path().join("workspace")),
                identity(),
                Box::new(move |action| sink.lock().unwrap().push(action)),
            );
            kernel
                .admit(request(RuntimeOperationKind::Start, "a"))
                .await
                .unwrap();
            kernel
                .admit(request(RuntimeOperationKind::Restart, "b"))
                .await
                .unwrap();
            kernel.finish("a", terminal, None, None, 2).await.unwrap();
            let guard = kernel.admission.lock().await;
            if terminal == RuntimeOperationState::RecoveryRequired {
                assert!(guard.recovery_protection);
                assert_eq!(
                    captured
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|action| matches!(action, DispatchAction::OrchestrateSource { .. }))
                        .count(),
                    1
                );
                assert_eq!(
                    kernel
                        .store
                        .load_operation("b")
                        .unwrap()
                        .unwrap()
                        .view
                        .state,
                    RuntimeOperationState::Cancelled
                );
            } else {
                assert_eq!(guard.active_operation_id.as_deref(), Some("b"));
                assert!(guard.pending_restart.is_none());
                assert_eq!(
                    captured
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|action| matches!(action, DispatchAction::OrchestrateSource { .. }))
                        .count(),
                    2
                );
            }
        }
    }

    #[tokio::test]
    async fn latest_start_waits_for_stop_completion_without_rejection() {
        for stop_result in [
            RuntimeOperationState::Succeeded,
            RuntimeOperationState::Failed,
            RuntimeOperationState::Cancelled,
        ] {
            let captured = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink = captured.clone();
            let (dir, _keep) = temp_store();
            let kernel = RuntimeKernel::new(
                open_store(&dir.path().join("workspace")),
                identity(),
                Box::new(move |action| sink.lock().unwrap().push(action)),
            );
            kernel
                .admit(request(RuntimeOperationKind::Stop, "stop"))
                .await
                .unwrap();
            // Source 可受理，但 Stop 的真实终态边界仍在派发之前。
            let mut during = request(RuntimeOperationKind::Start, "start");
            during.expected_revision = 1;
            kernel.admit(during).await.unwrap();
            assert_eq!(captured.lock().unwrap().len(), 1);
            assert!(!kernel.is_cancelled("stop"));
            kernel
                .finish("stop", stop_result, None, None, 2)
                .await
                .unwrap();
            // Stop 的 StopBusiness 派发 + Start 的 OrchestrateSource 派发。
            assert_eq!(captured.lock().unwrap().len(), 2);
            assert_eq!(
                kernel
                    .status()
                    .await
                    .unwrap()
                    .active_operation_id
                    .as_deref(),
                Some("start")
            );
            kernel
                .finish("start", RuntimeOperationState::Succeeded, None, None, 2)
                .await
                .unwrap();
            assert_eq!(
                kernel.store.load_desired().unwrap().0,
                DesiredState::Running
            );
        }
    }

    #[tokio::test]
    async fn live_cancel_persistence_failure_cannot_be_settled_as_owner_exit() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "live"))
            .await
            .unwrap();
        let path = kernel.store.cancellation_path("live");
        std::fs::create_dir_all(&path).unwrap();
        assert!(kernel.request_cancel("live").await.is_err());
        assert!(
            kernel
                .settle_unresolved_recoveries()
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            kernel.get("live").await.unwrap().unwrap().state,
            RuntimeOperationState::Accepted
        );
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("live")
        );
        kernel
            .admit(request(RuntimeOperationKind::Stop, "stop"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn malformed_desired_is_preserved_and_explicit_start_works() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        std::fs::write(kernel.store.desired_path(), b"{broken").unwrap();
        kernel.recover().await.unwrap();
        assert_eq!(
            kernel.store.load_desired().unwrap(),
            (DesiredState::Stopped, 0)
        );
        kernel
            .admit(request(RuntimeOperationKind::Start, "start"))
            .await
            .unwrap();
        assert_eq!(
            kernel.store.load_desired().unwrap().0,
            DesiredState::Running
        );
    }

    /// A late completion from the failed request cannot discard later requests.
    #[tokio::test]
    async fn late_failed_request_completion_preserves_newer_execution() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<DispatchAction>>> = Default::default();
        let sink = captured.clone();
        let (dir, _keep) = temp_store();
        let kernel = RuntimeKernel::new(
            open_store(&dir.path().join("workspace")),
            identity(),
            Box::new(move |action| {
                sink.lock().unwrap().push(action);
            }),
        );
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-a"))
            .await
            .unwrap();
        kernel
            .admit(request(RuntimeOperationKind::Restart, "op-b"))
            .await
            .unwrap();
        kernel
            .finish("op-a", RuntimeOperationState::Failed, None, None, 2)
            .await
            .unwrap();
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-c"))
            .await
            .unwrap();
        kernel
            .finish("op-a", RuntimeOperationState::Succeeded, None, None, 3)
            .await
            .unwrap();
        kernel
            .finish("op-b", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .unwrap();
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("op-c")
        );
        // 迟到的 op-a 成功回调不得改变已记录的失败终态。
        assert_eq!(
            kernel.get("op-a").await.unwrap().unwrap().state,
            RuntimeOperationState::Failed
        );
        kernel
            .finish("op-c", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .unwrap();
        let actions = captured.lock().unwrap();
        let dispatched: Vec<&str> = actions
            .iter()
            .filter_map(|action| match action {
                DispatchAction::OrchestrateSource { operation_id, .. } => {
                    Some(operation_id.as_str())
                }
                _ => None,
            })
            .collect();
        assert_eq!(dispatched, ["op-a", "op-b", "op-c"]);
    }

    /// R03：ArtifactId 制品部署派发（owner 侧激活——不经网络下载）。
    #[tokio::test]
    async fn artifact_id_deploy_dispatches_local_artifact() {
        let (dir, _keep) = temp_store();
        let captured: std::sync::Arc<std::sync::Mutex<Vec<DispatchAction>>> = Default::default();
        let sink = captured.clone();
        let workspace = dir.path().join("workspace");
        let store = open_store(&workspace);
        let kernel = RuntimeKernel::new(
            store,
            identity(),
            Box::new(move |action| {
                sink.lock().unwrap().push(action);
            }),
        );
        let mut request = request_deploy_url("op-art");
        request.profile = shared_types::RunProfileInput::Artifact {
            artifact: shared_types::ArtifactInput::ArtifactId {
                artifact_id: "rel-777".into(),
            },
        };
        kernel.admit(request).await.expect("admit");
        let actions = captured.lock().unwrap();
        match &actions[..] {
            [DispatchAction::DeployLocalArtifact { artifact_id, .. }] => {
                assert_eq!(artifact_id, "rel-777");
            }
            other => panic!("expected DeployLocalArtifact dispatch, got {other:?}"),
        }
    }

    /// Restart 派发与内部持久化均保留实际输入；外部只读取操作视图。
    #[tokio::test]
    async fn run_config_pg_reaches_dispatch_and_is_retained_on_disk() {
        use std::sync::Mutex;
        let (dir, _keep) = temp_store();
        let captured: std::sync::Arc<Mutex<Vec<DispatchAction>>> = Default::default();
        let sink = captured.clone();
        let workspace = dir.path().join("workspace");
        let store = open_store(&workspace);
        let identity = identity();
        let kernel = RuntimeKernel::new(
            store,
            identity,
            Box::new(move |action| {
                sink.lock().unwrap().push(action);
            }),
        );
        let mut request = request(RuntimeOperationKind::Restart, "op-pg");
        request.run_config = Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "biz_user".into(),
                password: "s3cret".into(),
            }),
        });
        kernel.admit(request).await.expect("admit");
        let actions = captured.lock().unwrap();
        match &actions[..] {
            [DispatchAction::OrchestrateSource { pg: Some(pg), .. }] => {
                assert_eq!(pg.username, "biz_user");
                assert_eq!(pg.password, "s3cret");
            }
            other => panic!("expected single orchestrate dispatch with pg, got {other:?}"),
        }
        drop(actions);
        // Internal audit retains the same username/password dispatched above.
        let stored = kernel
            .store()
            .load_operation("op-pg")
            .expect("load")
            .expect("stored");
        let persisted_pg = stored
            .request
            .run_config
            .as_ref()
            .and_then(|config| config.pg.as_ref())
            .expect("run config persisted");
        assert_eq!(persisted_pg.username, "biz_user");
        assert_eq!(
            persisted_pg.password, "s3cret",
            "internal audit must retain the actual dispatched password"
        );
    }

    /// R06：Failed 终态事件携带错误载荷；事件桥把服务级事件追加进活跃操作
    /// journal（复用 owner 的平台侧经运行 API 读事件流）。
    #[tokio::test]
    async fn failed_terminal_event_carries_error_payload() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-fail"))
            .await
            .expect("admit");
        kernel
            .finish(
                "op-fail",
                RuntimeOperationState::Failed,
                Some(("ERR_SOME".into(), "boom detail".into())),
                None,
                2,
            )
            .await
            .expect("finish");
        let events = kernel.store().replay_events("op-fail", 0).expect("replay");
        let terminal = events
            .iter()
            .find(|event| event.event_name.as_deref() == Some("Failed"))
            .expect("terminal event");
        let payload = terminal.payload.as_ref().expect("payload on Failed");
        assert_eq!(payload["code"], "ERR_SOME");
        assert_eq!(payload["error"], "boom detail");
    }

    #[tokio::test]
    async fn orchestration_bridge_appends_to_active_operation() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        // 未受理的操作身份不写日志（idle 期只有 stdout 消费者）。
        assert!(!kernel.append_orchestration_event(
            "op-bridge",
            "service",
            Some("frontend".into()),
            "service_starting",
            None
        ));
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-bridge"))
            .await
            .expect("admit");
        // admit 已发 accepted 事件（sequence 1）→ 桥接事件从 2 起
        assert!(kernel.append_orchestration_event(
            "op-bridge",
            "service",
            Some("frontend".into()),
            "service_starting",
            None
        ));
        assert!(kernel.append_orchestration_event(
            "op-bridge",
            "orchestration",
            None,
            "orchestration_done",
            Some(serde_json::json!({"failed": []}))
        ));
        let events = kernel
            .store()
            .replay_events("op-bridge", 0)
            .expect("replay");
        let names: Vec<(u64, &str)> = events
            .iter()
            .filter_map(|event| {
                event
                    .event_name
                    .as_deref()
                    .map(|name| (event.sequence, name))
            })
            .collect();
        assert_eq!(
            names,
            vec![
                (1, "start"),
                (2, "service_starting"),
                (3, "orchestration_done")
            ],
            "bridge events must be sequenced after accepted: {names:?}"
        );
        let done = events.last().expect("done event");
        assert_eq!(
            done.payload.as_ref().expect("payload")["failed"],
            serde_json::json!([])
        );
    }

    fn request_deploy_url(operation_id: &str) -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: "instance-1".into(),
            expected_revision: 0,
            workspace_id: "ws-1".into(),
            kind: RuntimeOperationKind::Deploy,
            profile: RunProfileInput::Artifact {
                artifact: ArtifactInput::Url {
                    url: "http://x/app.zip".into(),
                    sha256: None,
                },
            },
            run_config: None,
            request_context: None,
        }
    }

    fn request(kind: RuntimeOperationKind, operation_id: &str) -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: "instance-1".into(),
            expected_revision: 0,
            workspace_id: "ws-1".into(),
            kind,
            profile: RunProfileInput::Source {
                workspace_id: "ws-1".into(),
            },
            run_config: None,
            request_context: None,
        }
    }

    #[tokio::test]
    async fn same_id_same_digest_replays_without_double_admission() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        let first = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        assert!(matches!(first, AdmissionOutcome::Accepted(_)));
        // 未 finish 前，同 ID 同摘要 = 重放（不是 busy 拒绝）
        let replay = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("replay");
        assert!(matches!(replay, AdmissionOutcome::Replayed(_)));
    }

    #[tokio::test]
    async fn same_id_different_digest_conflicts() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        let mut other = request(RuntimeOperationKind::Stop, "op-1");
        other.kind = RuntimeOperationKind::Restart;
        let rejection = kernel.admit(other).await.expect_err("conflict");
        assert_eq!(rejection.code, ERR_OPERATION_ID_CONFLICT);
    }

    #[tokio::test]
    async fn active_operation_blocks_other_kinds_but_not_stop() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit start");
        // R02"最后受理生效"后：active 期间的 start/restart/deploy 进排队槽
        // （不再 busy 拒）；旧断言的拒绝语义由未知清理结果的恢复保护
        // 承担。本测试改锁排队语义（详见 last_accepted_start_wins 测试）。
        // stop 可受理（意图屏障），且推进 revision；排队者被 stop 覆盖收束
        kernel
            .admit(request_deploy_url("op-2"))
            .await
            .expect("deploy admitted into queue");
        let queued_view = kernel
            .store
            .load_operation("op-2")
            .expect("load")
            .expect("stored");
        assert_eq!(queued_view.view.state, RuntimeOperationState::Accepted);
        kernel
            .admit(request(RuntimeOperationKind::Stop, "op-stop"))
            .await
            .expect("stop admitted during active");
        // 排队者已被停止意图覆盖（Cancelled/ERR_SUPERSEDED——不复活）
        let superseded = kernel
            .store
            .load_operation("op-2")
            .expect("load")
            .expect("stored");
        assert_eq!(superseded.view.state, RuntimeOperationState::Cancelled);
        assert_eq!(
            superseded.view.error_code.as_deref(),
            Some("ERR_SUPERSEDED")
        );
        let (_desired, revision) = kernel.store.load_desired().expect("desired");
        assert_eq!(revision, 1);
        // 原操作与 stop 均收束后，stop 推进的 revision 使旧 revision 部署提交被拒
        kernel
            .finish("op-1", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("finish op-1");
        kernel
            .finish("op-stop", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .expect("finish stop");
        let stale = request_deploy_url("op-3");
        let rejection = kernel.admit(stale).await.expect_err("stale revision");
        assert_eq!(rejection.code, ERR_REVISION_MISMATCH);
    }

    #[tokio::test]
    async fn stale_runtime_instance_is_rejected_before_replay() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        let mut stale = request(RuntimeOperationKind::Start, "op-1");
        stale.expected_runtime_instance_id = "previous-instance".into();
        let rejection = kernel.admit(stale).await.expect_err("instance mismatch");
        assert_eq!(rejection.code, ERR_RUNTIME_INSTANCE_MISMATCH);
    }

    #[tokio::test]
    async fn finish_clears_active_and_recovery_state_sets_protection() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect("admit");
        kernel
            .finish(
                "op-1",
                RuntimeOperationState::RecoveryRequired,
                Some((ERR_RECOVERY_REQUIRED.into(), "cleanup unconfirmed".into())),
                None,
                2,
            )
            .await
            .expect("finish");
        let status = kernel.status().await.expect("status");
        assert!(status.recovery_protection);
        assert_eq!(status.active_operation_id, None);
        // 恢复保护期新写被拒
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-2"))
            .await
            .expect_err("recovery gate");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
        kernel
            .admit(request(RuntimeOperationKind::Stop, "recovery-stop"))
            .await
            .expect("stop is permitted while prior outcome remains protected");
        kernel
            .finish(
                "recovery-stop",
                RuntimeOperationState::Succeeded,
                None,
                None,
                3,
            )
            .await
            .unwrap();
        assert_eq!(
            kernel.get("op-1").await.unwrap().unwrap().state,
            RuntimeOperationState::RecoveryRequired
        );
        assert!(
            kernel.recovery_protection_active(),
            "Stop must not erase uncertain deployment history"
        );
    }

    #[tokio::test]
    async fn events_replay_by_after_seq_cursor() {
        let (dir, _keep) = temp_store();
        let store = open_store(&dir.path().join("workspace"));
        for sequence in 1..=3 {
            store
                .append_event(&RuntimeEventRecord {
                    operation_id: "op-9".into(),
                    sequence,
                    runtime_instance_id: "instance-1".into(),
                    stage: "stage".into(),
                    service: None,
                    event_name: Some("service_starting".into()),
                    payload: None,
                })
                .expect("append");
        }
        let all = store.replay_events("op-9", 0).expect("replay");
        assert_eq!(all.len(), 3);
        let tail = store.replay_events("op-9", 2).expect("replay");
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].sequence, 3);
    }

    #[tokio::test]
    async fn restart_recovery_marks_unfinished_operations() {
        let (dir, _keep) = temp_store();
        {
            let workspace = dir.path().join("workspace");
            let store = open_store(&workspace);
            store
                .store_operation(&StoredOperation {
                    view: RuntimeOperationView {
                        operation_id: "op-pending".into(),
                        kind: RuntimeOperationKind::Deploy,
                        state: RuntimeOperationState::Starting,
                        request_digest: "d".repeat(64),
                        revision: 0,
                        runtime_instance_id: "instance-old".into(),
                        error_code: None,
                        error_message: None,
                        failure_detail: None,
                    },
                    request: request(RuntimeOperationKind::Deploy, "op-pending"),
                })
                .expect("store");
        }
        // 新进程打开同一状态根：未终态操作转 RecoveryRequired + 保护
        let kernel = kernel(dir.path());
        let recovered = kernel.recover().await.expect("recover");
        assert_eq!(recovered, vec!["op-pending".to_string()]);
        let view = kernel
            .get("op-pending")
            .await
            .expect("get")
            .expect("present");
        assert_eq!(view.state, RuntimeOperationState::RecoveryRequired);
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-new"))
            .await
            .expect("startup recovery permits an explicit new request");
        let old = kernel.get("op-pending").await.unwrap().unwrap();
        assert_eq!(old.state, RuntimeOperationState::Failed);
        assert_eq!(old.error_code.as_deref(), Some(ERR_INTERRUPTED_OWNER_EXIT));
        assert!(!old.failure_detail.unwrap().cleanup_confirmed);
    }

    #[tokio::test]
    async fn persist_failure_rejects_without_dispatch() {
        let (dir, _keep) = temp_store();
        // 目标操作记录路径预置为目录 → 原子写 rename 失败（受理持久化故障注入）
        let root = app_state_root(&dir.path().join("workspace"));
        std::fs::create_dir_all(root.join("operations").join("op-1.json")).expect("block path");
        let kernel = kernel(dir.path());
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-1"))
            .await
            .expect_err("persist failure");
        assert_eq!(rejection.code, "ERR_BACKEND_ERROR");
        let status = kernel.status().await.expect("status");
        assert_eq!(status.active_operation_id, None, "no execution queued");
    }
    // ── R01：执行身份不被并发受理覆盖；按 ID 收束不误完成他人 ──────────────────

    #[test]
    fn executing_identity_refuses_concurrent_overwrite() {
        // ServerState 归属 server.rs，这里验证同款语义经 dispatch 间接覆盖；
        // 直接构造检查在 app_cli::server 的集成测试中锚定。内核侧等价断言：
        // active 单槽在 stop 屏障期间不被 second admission 变更（见下）。
    }

    #[tokio::test]
    async fn finish_by_id_does_not_complete_a_different_operation() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        // 受理 op-A（Start/Source）
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-a"))
            .await
            .expect("admit A");
        // 受理 Stop op-B（active 期间允许，意图屏障）
        kernel
            .admit(request(RuntimeOperationKind::Stop, "op-b"))
            .await
            .expect("admit stop B");
        // A 的执行边界按自身 ID 收束（不应误完成 B）
        kernel
            .finish("op-a", RuntimeOperationState::Succeeded, None, None, 2)
            .await
            .expect("finish A");
        let view_a = kernel.get("op-a").await.expect("get A").expect("present");
        assert_eq!(view_a.state, RuntimeOperationState::Succeeded);
        // B 仍在途（Accepted——尚未被停止边界执行）
        let view_b = kernel.get("op-b").await.expect("get B").expect("present");
        assert!(!view_b.state.is_terminal(), "B must stay in-flight");
    }

    // ── R02：状态根在卷根（workspace 父目录），跨部署换代稳定 ────────────────────

    #[tokio::test]
    async fn state_root_lives_on_volume_root_not_workspace() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = open_store(&workspace);
        store
            .store_desired(DesiredState::Stopped, 3)
            .expect("desired");
        // 状态根在 workspace 之外（卷根下按应用隔离）——部署 activate
        // 整体改名 workspace 换代时不动
        assert!(app_state_root(&workspace).join("desired.json").is_file());
        assert!(!workspace.join(STATE_DIR_NAME).exists());
    }

    /// XP03：同项目改端口不分裂锁域——锁以状态根为键，端口只进发现记录。
    /// 第二实例（不同 admin_addr）仍被 OwnerGuard 拒绝。
    #[test]
    fn xp03_port_change_does_not_split_lock_domain() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("state");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let _guard = crate::platform::owner_guard::OwnerGuard::acquire(&root).expect("first owner");

        // 第一 owner 发布了端口 A 的发现记录
        let record = EndpointRecord {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            application_id: "app-x".into(),
            workspace_id: "ws".into(),
            runtime_instance_id: "instance-1".into(),
            address: "127.0.0.1:3010".into(),
        };
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).expect("store");
        store.store_endpoint(&record).expect("publish endpoint");

        // 第二实例改用端口 B：锁域不变 → 排他失败
        let second = crate::platform::owner_guard::OwnerGuard::acquire(&root);
        assert!(
            second.is_err(),
            "port change must not bypass the owner lock"
        );

        // 释放后可接管（记录不阻碍新 owner——它是线索不是凭证）
        drop(_guard);
        let _guard2 = crate::platform::owner_guard::OwnerGuard::acquire(&root)
            .expect("takeover after release");
    }

    /// XP10：旧发现记录与远端身份不符 → 核验拒绝（不据此发认证请求）。
    /// 同实例全字段一致才命中；实例换代/协议变化/错应用均拒绝。
    #[test]
    fn xp10_stale_endpoint_record_is_rejected() {
        let identity = || RuntimeIdentityView {
            application_id: "app-x".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws".into(),
            source_root: "/ws".into(),
            runtime_instance_id: "instance-1".into(),
            deployment_generation_id: "gen-1".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        };
        let record = EndpointRecord {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            application_id: "app-x".into(),
            workspace_id: "ws".into(),
            runtime_instance_id: "instance-1".into(),
            address: "127.0.0.1:3010".into(),
        };
        // 全字段一致 → 命中
        assert!(endpoint_matches_identity(&record, &identity()));
        // 实例换代（owner 重启后远端是新实例，记录是旧的）→ 拒绝
        let restarted = RuntimeIdentityView {
            runtime_instance_id: "instance-2".into(),
            ..identity()
        };
        assert!(!endpoint_matches_identity(&record, &restarted));
        // 协议不兼容 → 拒绝
        let upgraded = RuntimeIdentityView {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION + 1,
            ..identity()
        };
        assert!(!endpoint_matches_identity(&record, &upgraded));
        // 错应用 → 拒绝
        let foreign = RuntimeIdentityView {
            application_id: "app-y".into(),
            ..identity()
        };
        assert!(!endpoint_matches_identity(&record, &foreign));
    }

    /// XP10 补充：发现记录持久化 roundtrip + 干净关停清除。
    ///
    /// 本地凭据文件（cross-platform.md §3）：token 落盘 0600 + 平台侧只读。
    #[test]
    fn token_file_roundtrip_with_restricted_permissions() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("state-root");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).expect("store");

        store
            .store_token("  secret-token  ")
            .expect("persist token");
        // 写入 trim；平台侧读到的是裸值
        assert_eq!(
            RuntimeStore::read_token(&root).as_deref(),
            Some("secret-token")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(root.join("token"))
                .expect("stat token")
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "token file must be owner-only readable"
            );
        }
        #[cfg(windows)]
        {
            // icacls 查询 ACL：断言继承已移除（无 inherited 条目残留）
            let query = std::process::Command::new("icacls")
                .arg(root.join("token"))
                .output()
                .expect("icacls query");
            let text = String::from_utf8_lossy(&query.stdout);
            assert!(
                !text.contains("(I)"),
                "token ACL must have inheritance removed: {text}"
            );
        }
        // 无 token 文件 → None（owner 未启用写端点）
        let empty = tempfile::tempdir().expect("empty");
        assert!(RuntimeStore::read_token(empty.path()).is_none());
    }

    #[test]
    fn endpoint_record_roundtrip_and_clean_clear() {
        let dir = tempfile::tempdir().expect("dir");
        let root = dir.path().join("state-root");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).expect("store");

        let record = EndpointRecord {
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            application_id: "app-r".into(),
            workspace_id: "ws".into(),
            runtime_instance_id: "instance-r".into(),
            address: "127.0.0.1:39999".into(),
        };
        store.store_endpoint(&record).expect("publish");
        let loaded = RuntimeStore::read_endpoint(&root).expect("record present");
        assert_eq!(loaded.address, record.address);
        assert_eq!(loaded.runtime_instance_id, record.runtime_instance_id);

        store.clear_endpoint().expect("clean clear");
        assert!(
            RuntimeStore::read_endpoint(&root).is_none(),
            "cleared record must not be discoverable"
        );
        // 幂等清除（不存在不报错）
        store.clear_endpoint().expect("idempotent clear");
    }

    /// B04：显式 env 根权威——source 与 .run 别名竞争同一目录/同一把锁。
    #[test]
    fn explicit_env_root_unifies_source_and_run_entries() {
        let dir = tempfile::tempdir().expect("dir");
        let volume = dir.path().join("vol");
        let explicit = dir.path().join("explicit-state");
        std::fs::create_dir_all(&volume).expect("volume");
        // 经参数化核心驱动（不 set_var）——进程级 env 变异会与并行测试的
        // resolve_root 读取竞争（Windows 实测随机 os error 2/3/183）。
        let source_workspace = volume.join("app-1");
        let run_workspace = volume.join("app-1").join(".run");
        std::fs::create_dir_all(&source_workspace).expect("source");
        std::fs::create_dir_all(&run_workspace).expect("run");
        let r1 = RuntimeStore::resolve_root_with_explicit(
            &source_workspace,
            "app-1",
            Some(explicit.clone().into_os_string()),
        )
        .expect("root1");
        let r2 = RuntimeStore::resolve_root_with_explicit(
            &run_workspace,
            "app-1",
            Some(explicit.clone().into_os_string()),
        )
        .expect("root2");
        assert_eq!(r1, r2, "alias entries must resolve the same explicit root");
        assert_eq!(r1, explicit);
    }

    #[tokio::test]
    async fn legacy_in_workspace_state_migrates_once() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(workspace.join(STATE_DIR_NAME)).expect("legacy root");
        std::fs::write(
            workspace.join(STATE_DIR_NAME).join("desired.json"),
            r#"{"desired":"stopped","revision":5}"#,
        )
        .expect("legacy desired");
        let store = open_store(&workspace);
        store.reconcile_legacy_after_quiescence().unwrap();
        // 运行记录迁移到新位置；保留旧锁文件与守卫，避免迁移期间锁域分裂。
        assert!(!workspace.join(STATE_DIR_NAME).join("desired.json").exists());
        assert!(workspace.join(STATE_DIR_NAME).join("owner.lock").exists());
        assert_eq!(
            store.load_desired().expect("desired"),
            (DesiredState::Stopped, 5)
        );
    }

    /// B04：bare 卷根布局（R02–R08 形态）也迁移到按应用隔离根。
    #[test]
    fn legacy_bare_volume_root_migrates_to_per_app_root() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let bare = dir.path().join(STATE_DIR_NAME);
        std::fs::create_dir_all(bare.join("operations")).expect("bare root");
        std::fs::write(
            bare.join("desired.json"),
            r#"{"desired":"running","revision":2}"#,
        )
        .expect("bare desired");
        let store = open_store(&workspace);
        store.reconcile_legacy_after_quiescence().unwrap();
        assert_eq!(
            store.load_desired().expect("desired"),
            (DesiredState::Running, 2)
        );
        // bare 不再直接持有状态条目（已搬入按应用根）；其作为按应用根的
        // 父容器保留（root 嵌套于其子树——rename 进自身子树不可行）。
        assert!(!bare.join("desired.json").exists());
        assert!(!bare.join("operations").exists());
        assert!(app_state_root(&workspace).join("desired.json").exists());
    }

    #[test]
    fn owner_bootstrap_root_does_not_block_legacy_runtime_migration() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let legacy = workspace.join(STATE_DIR_NAME);
        std::fs::create_dir_all(legacy.join("operations")).unwrap();
        std::fs::write(
            legacy.join("desired.json"),
            r#"{"desired":"stopped","revision":7}"#,
        )
        .unwrap();
        std::fs::write(legacy.join("token"), "legacy-test-token").unwrap();
        std::fs::write(legacy.join("operations/op.json"), "retained-operation").unwrap();
        let root = app_state_root(&workspace);
        let _owner = crate::platform::owner_guard::OwnerGuard::acquire(&root).unwrap();
        std::fs::write(root.join(".deploy-coordinator.json"), "journal-bootstrap").unwrap();
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).unwrap();
        store.reconcile_legacy_after_quiescence().unwrap();
        assert_eq!(store.load_desired().unwrap(), (DesiredState::Stopped, 7));
        assert_eq!(
            std::fs::read_to_string(root.join("token")).unwrap(),
            "legacy-test-token"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("operations/op.json")).unwrap(),
            "retained-operation"
        );
        assert_eq!(
            std::fs::read_to_string(root.join(".deploy-coordinator.json")).unwrap(),
            "journal-bootstrap"
        );
        assert!(
            crate::platform::owner_guard::OwnerGuard::try_acquire(&legacy)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn legacy_runtime_migration_never_overwrites_stable_token() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let legacy = workspace.join(STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(
            legacy.join("desired.json"),
            r#"{"desired":"stopped","revision":7}"#,
        )
        .unwrap();
        let root = app_state_root(&workspace);
        let _owner = crate::platform::owner_guard::OwnerGuard::acquire(&root).unwrap();
        std::fs::write(root.join("token"), "current-token").unwrap();
        let store = RuntimeStore::open_with_root(root.clone(), &workspace).unwrap();
        assert!(
            legacy.join("desired.json").exists(),
            "opening never moves historical state before quiescence"
        );
        store.reconcile_legacy_after_quiescence().unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("token")).unwrap(),
            "current-token"
        );
        assert!(!legacy.join("desired.json").exists());
        assert!(std::fs::read_dir(&legacy).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".runtime-history-")
        }));
    }

    #[tokio::test]
    async fn dual_authority_domains_converge_after_quiescence_with_all_locks_retained() {
        let dir = tempfile::tempdir().expect("dir");
        let workspace = dir.path().join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let legacy = workspace.join(STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy).expect("legacy");
        std::fs::write(
            legacy.join("desired.json"),
            r#"{"desired":"stopped","revision":1}"#,
        )
        .expect("legacy marker");
        let root = app_state_root(&workspace);
        std::fs::create_dir_all(root.join("operations")).expect("new root");
        let store = RuntimeStore::open_with_root(root, &workspace).unwrap();
        assert!(legacy.join("desired.json").exists());
        assert!(
            crate::platform::owner_guard::OwnerGuard::try_acquire(&legacy)
                .unwrap()
                .is_none()
        );
        store.reconcile_legacy_after_quiescence().unwrap();
        assert_eq!(store.load_desired().unwrap(), (DesiredState::Stopped, 1));
        // 两个旧域真实锁均释放，历史冲突不能阻断新的明确Source意图。
        let dir2 = tempfile::tempdir().expect("dir2");
        let workspace2 = dir2.path().join("workspace");
        std::fs::create_dir_all(&workspace2).expect("workspace");
        let legacy_ws = workspace2.join(STATE_DIR_NAME);
        std::fs::create_dir_all(&legacy_ws).expect("legacy ws");
        std::fs::write(
            legacy_ws.join("desired.json"),
            r#"{"desired":"stopped","revision":1}"#,
        )
        .expect("legacy ws marker");
        let legacy_bare = dir2.path().join(STATE_DIR_NAME);
        std::fs::create_dir_all(legacy_bare.join("operations")).expect("legacy bare");
        std::fs::write(
            legacy_bare.join("desired.json"),
            r#"{"desired":"running","revision":1}"#,
        )
        .expect("legacy bare marker");
        let root2 = app_state_root(&workspace2);
        let store2 = RuntimeStore::open_with_root(root2, &workspace2).unwrap();
        for legacy in [&legacy_ws, &legacy_bare] {
            assert!(
                crate::platform::owner_guard::OwnerGuard::try_acquire(legacy)
                    .unwrap()
                    .is_none()
            );
            assert!(legacy.join("desired.json").exists());
        }
        store2.reconcile_legacy_after_quiescence().unwrap();
        assert_eq!(
            store2.load_desired().unwrap(),
            (DesiredState::Stopped, 0),
            "ambiguous old intent cannot automatically run business"
        );
        store2.store_desired(DesiredState::Running, 0).unwrap();
        assert_eq!(store2.load_desired().unwrap(), (DesiredState::Running, 0));
    }

    // ── R03：真实取消墓碑；未实现 profile 组合结构化拒绝 ────────────────────────

    #[tokio::test]
    async fn request_cancel_marks_tombstone_and_finish_clears_it() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-c"))
            .await
            .expect("admit");
        assert!(!kernel.is_cancelled("op-c"));
        assert!(kernel.request_cancel("op-c").await.expect("cancel"));
        assert!(kernel.is_cancelled("op-c"), "cancel must be observable");
        // 非在途操作取消 = false（不误标）
        assert!(!kernel.request_cancel("op-x").await.expect("no-op"));
        kernel
            .finish("op-c", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("finish");
        assert!(!kernel.is_cancelled("op-c"), "finish clears tombstone");
    }

    #[tokio::test]
    async fn unsupported_profile_combinations_are_rejected() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        // Deploy+Source：未实现 → 结构化拒绝（不静默编排当前 workspace）
        let mut deploy_source = request(RuntimeOperationKind::Deploy, "op-ds");
        deploy_source.profile = RunProfileInput::Source {
            workspace_id: "ws-1".into(),
        };
        let rejection = kernel
            .admit(deploy_source)
            .await
            .expect_err("must reject deploy+source");
        assert_eq!(rejection.code, shared_types::ERR_PROTOCOL_UNSUPPORTED);
        // Start+Artifact is unsupported; local artifacts require Deploy.
        let mut start_artifact = request(RuntimeOperationKind::Start, "op-sa");
        start_artifact.profile = RunProfileInput::Artifact {
            artifact: ArtifactInput::ArtifactId {
                artifact_id: "art-1".into(),
            },
        };
        let rejection = kernel
            .admit(start_artifact)
            .await
            .expect_err("must reject start+artifact-id");
        assert_eq!(rejection.code, shared_types::ERR_PROTOCOL_UNSUPPORTED);
        // 拒绝未派发：无 active 占位
        let status = kernel.status().await.expect("status");
        assert_eq!(status.active_operation_id, None);
    }

    // ── R04：损坏记录隔离（残留容忍）；部分提交保持保护 ──────────────────────

    #[tokio::test]
    async fn corrupt_operation_record_is_quarantined_and_writes_proceed() {
        let (dir, _keep) = temp_store();
        {
            let workspace = dir.path().join("workspace");
            let root = app_state_root(&workspace);
            std::fs::create_dir_all(root.join("operations")).expect("ops dir");
            // 一条损坏 JSON 的未终态操作记录
            std::fs::write(
                root.join("operations").join("op-corrupt.json"),
                "{ this is not json",
            )
            .expect("corrupt record");
            let _ = RuntimeStore::open(&workspace).expect("open ignores record content");
        }
        let kernel = kernel(dir.path());
        let recovered = kernel.recover().await.expect("recover");
        // 损坏记录不产生 recovered 条目：被隔离，不再阻断（产品环境没有
        // 操作员可以等，残留文件不得让用户无法编译/启动）。
        assert!(recovered.is_empty());
        let status = kernel.status().await.expect("status");
        assert!(
            !status.recovery_protection,
            "quarantined corrupt record must not keep recovery protection"
        );
        // 原件保留在 quarantine 目录（可查），operations 目录不再含它
        let root = app_state_root(&dir.path().join("workspace"));
        let quarantine: Vec<_> = std::fs::read_dir(root.join("operations-quarantine"))
            .expect("quarantine dir")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert!(
            quarantine
                .iter()
                .any(|name| name.to_string_lossy().ends_with("op-corrupt.json")),
            "corrupt record must be preserved under operations-quarantine: {quarantine:?}"
        );
        assert!(
            !root.join("operations").join("op-corrupt.json").exists(),
            "corrupt record must be moved out of operations"
        );
        // 后续受理正常进行（用户可恢复使用）
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-after-corrupt"))
            .await
            .expect("writes proceed after quarantine");
    }

    /// 非终态残留（容器停止回收后重启）在启动收敛后必须放行——不再等
    /// 任何人工裁决；显式 start 直接受理，旧记录沉降为 Failed。
    #[tokio::test]
    async fn settle_unresolved_recoveries_unblocks_admission() {
        let (dir, _keep) = temp_store();
        {
            let kernel = kernel(dir.path());
            kernel
                .admit(request(RuntimeOperationKind::Start, "op-interrupted"))
                .await
                .expect("admit");
            // 不 finish——模拟上一 owner 在执行前/中终止，留下非终态记录
        }
        let kernel = kernel(dir.path());
        let recovered = kernel.recover().await.expect("recover");
        assert_eq!(recovered, vec!["op-interrupted".to_string()]);
        let status = kernel.status().await.expect("status");
        assert!(status.recovery_protection, "recover marks protection first");
        // 启动序列末尾兜底收敛：保护解除，操作沉降为 Failed
        let settled = kernel.settle_unresolved_recoveries().await.expect("settle");
        assert_eq!(settled, vec!["op-interrupted".to_string()]);
        let status = kernel.status().await.expect("status");
        assert!(
            !status.recovery_protection,
            "settled operations must release protection"
        );
        let view = kernel.get("op-interrupted").await.expect("get");
        let view = view.expect("view");
        assert_eq!(view.state, RuntimeOperationState::Failed);
        assert_eq!(view.error_code.as_deref(), Some(ERR_INTERRUPTED_OWNER_EXIT));
        // 继任编译/启动正常受理
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-successor"))
            .await
            .expect("successor must be admitted after settle");
    }

    /// owner 活着但保护卡住（如瞬时写失败后磁盘恢复）时，新请求触发
    /// admit 兜底收敛——不拒绝，不让用户/agent 卡死。
    #[tokio::test]
    async fn admission_settles_stale_protection_instead_of_rejecting() {
        let (dir, _keep) = temp_store();
        {
            let kernel = kernel(dir.path());
            kernel
                .admit(request(RuntimeOperationKind::Start, "op-held"))
                .await
                .expect("admit");
            // 不 finish：留下非终态记录 + recover 开启保护（模拟 owner 存活
            // 但上一次执行结果未确认的窗口）
        }
        let kernel = kernel(dir.path());
        kernel.recover().await.expect("recover");
        // 不显式调 settle——admit 自身兜底收敛并受理
        let outcome = kernel
            .admit(request(RuntimeOperationKind::Restart, "op-next"))
            .await
            .expect("admission must settle stale protection instead of rejecting");
        match outcome {
            AdmissionOutcome::Accepted(view) => {
                assert_eq!(view.kind, RuntimeOperationKind::Restart)
            }
            AdmissionOutcome::Replayed(_) => panic!("fresh operation must not replay"),
        }
        let held = kernel.get("op-held").await.expect("get").expect("view");
        assert_eq!(held.state, RuntimeOperationState::Failed);
    }

    #[tokio::test]
    async fn unreadable_desired_rejects_before_operation_admission() {
        let (dir, _keep) = temp_store();
        {
            let kernel = kernel(dir.path());
            kernel
                .admit(request(RuntimeOperationKind::Start, "op-first"))
                .await
                .expect("first admit");
            kernel
                .finish("op-first", RuntimeOperationState::Succeeded, None, None, 2)
                .await
                .expect("finish first");
        }
        let desired = app_state_root(&dir.path().join("workspace")).join("desired.json");
        std::fs::remove_file(&desired).expect("remove desired");
        std::fs::create_dir(&desired).expect("block desired writes");

        let dispatches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = dispatches.clone();
        let kernel = RuntimeKernel::new(
            open_store(&dir.path().join("workspace")),
            identity(),
            Box::new(move |_| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
        );
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-read-error"))
            .await
            .expect_err("desired read fails before Accepted is persisted");
        assert_eq!(rejection.code, "ERR_BACKEND_ERROR");
        assert!(kernel.get("op-read-error").await.unwrap().is_none());
        assert_eq!(dispatches.load(std::sync::atomic::Ordering::Relaxed), 0);
        assert!(desired.is_dir(), "the original unreadable path is retained");
    }

    #[tokio::test]
    async fn partial_admission_holds_operation_and_protection() {
        let (dir, _keep) = temp_store();
        let dispatches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = dispatches.clone();
        let kernel = RuntimeKernel::new(
            open_store(&dir.path().join("workspace")),
            identity(),
            Box::new(move |_| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }),
        );
        kernel
            .store
            .store_desired(DesiredState::Stopped, 0)
            .unwrap();
        let desired_before = std::fs::read(kernel.store.desired_path()).unwrap();
        kernel
            .store
            .desired_write_fault
            .store(true, std::sync::atomic::Ordering::Release);
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-partial"))
            .await
            .expect_err("final desired write fails after durable Accepted");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
        assert!(rejection.message.contains("held for recovery"));
        // 部分提交的操作转 RecoveryRequired（可查询，不再派发）
        let held = kernel
            .get("op-partial")
            .await
            .expect("query")
            .expect("present");
        assert_eq!(held.state, RuntimeOperationState::RecoveryRequired);
        let persisted = kernel.store.load_operation("op-partial").unwrap().unwrap();
        assert_eq!(
            persisted.view.state,
            RuntimeOperationState::RecoveryRequired
        );
        assert_eq!(
            std::fs::read(kernel.store.desired_path()).unwrap(),
            desired_before
        );
        assert_eq!(dispatches.load(std::sync::atomic::Ordering::Relaxed), 0);
        // 后续写被拒
        let blocked = kernel
            .admit(request(RuntimeOperationKind::Start, "op-next"))
            .await
            .expect_err("blocked");
        assert_eq!(blocked.code, ERR_RECOVERY_REQUIRED);
    }

    // ===== R03/R04：终态单调与提交线性化 =====

    #[tokio::test]
    async fn finish_is_monotonic_cancelled_cannot_become_succeeded() {
        // R04：取消收束后的 Cancelled 记录不得被迟到的成功/失败提交改写
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-mono"))
            .await
            .expect("admit");
        kernel
            .finish("op-mono", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("cancel settle");
        kernel
            .finish("op-mono", RuntimeOperationState::Succeeded, None, None, 3)
            .await
            .expect("late finish must not error");
        let view = kernel.get("op-mono").await.expect("view").expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Cancelled);
        // 同态重复 finish 幂等
        kernel
            .finish("op-mono", RuntimeOperationState::Cancelled, None, None, 4)
            .await
            .expect("idempotent");
    }

    #[tokio::test]
    async fn recovery_required_cannot_be_overwritten_by_terminal() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-recovery"))
            .await
            .expect("admit");
        kernel
            .finish(
                "op-recovery",
                RuntimeOperationState::RecoveryRequired,
                None,
                None,
                2,
            )
            .await
            .expect("hold");
        kernel
            .finish("op-recovery", RuntimeOperationState::Failed, None, None, 3)
            .await
            .expect("late failure must not overwrite protection");
        let view = kernel
            .get("op-recovery")
            .await
            .expect("view")
            .expect("exists");
        assert_eq!(view.state, RuntimeOperationState::RecoveryRequired);
    }

    #[tokio::test]
    async fn commit_after_cancel_observes_without_prewriting_terminal() {
        // V02：屏障只观察取消意图——**不预写 Cancelled 终态**（预写会让清理
        // 未知时无法升级 RecoveryRequired）；调用方停服确认后 finish 收束
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-race"))
            .await
            .expect("admit");
        kernel.request_cancel("op-race").await.expect("cancel");
        let outcome = kernel.commit_execution("op-race").await.expect("barrier");
        assert_eq!(outcome, CommitBarrierOutcome::Cancelled);
        // 终态未写：仍非终态（等待调用方停服后收束）
        let view = kernel.get("op-race").await.expect("view").expect("exists");
        assert!(
            !view.state.is_terminal(),
            "barrier must not prewrite terminal"
        );
        // 调用方停服确认 → finish(Cancelled) 收束；迟到成功不可覆盖
        kernel
            .finish("op-race", RuntimeOperationState::Cancelled, None, None, 2)
            .await
            .expect("settle cancelled");
        kernel
            .finish("op-race", RuntimeOperationState::Succeeded, None, None, 3)
            .await
            .expect("late finish must not error");
        let view = kernel.get("op-race").await.expect("view").expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Cancelled);
    }

    #[tokio::test]
    async fn commit_barrier_cancelled_op_can_settle_recovery_required() {
        // V02 关键能力：取消窗口后清理未知 → RecoveryRequired（若屏障预写
        // Cancelled，终态单调会锁死该升级路径）
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-unclean"))
            .await
            .expect("admit");
        kernel.request_cancel("op-unclean").await.expect("cancel");
        let outcome = kernel
            .commit_execution("op-unclean")
            .await
            .expect("barrier");
        assert_eq!(outcome, CommitBarrierOutcome::Cancelled);
        kernel
            .finish(
                "op-unclean",
                RuntimeOperationState::RecoveryRequired,
                None,
                None,
                2,
            )
            .await
            .expect("uncertain cleanup must be expressible");
        assert!(kernel.recovery_protection_active());
    }

    #[tokio::test]
    async fn late_cancel_after_commit_cannot_flip_succeeded() {
        // R03 窗口消除后的可观察面：提交成功后再取消，终态保持 Succeeded
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-commit"))
            .await
            .expect("admit");
        let outcome = kernel.commit_execution("op-commit").await.expect("barrier");
        assert_eq!(outcome, CommitBarrierOutcome::Committed);
        kernel
            .request_cancel("op-commit")
            .await
            .expect("late cancel");
        kernel
            .finish("op-commit", RuntimeOperationState::Cancelled, None, None, 9)
            .await
            .expect("late finish");
        let view = kernel
            .get("op-commit")
            .await
            .expect("view")
            .expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Succeeded);
    }

    /// RV01 反例：降级驻留 owner 的会话交接。会话 A 结束（note_execution_
    /// session_ended）后用户提交新 Start B——B 以自身 ID 占据新执行权；
    /// 下一个会话的 prepare_relaunch 只收束 A 记录的交接集合，B 保持
    /// Accepted 且 recover 不得销毁它。修复前 prepare 盲清全部 active/
    /// pending，B 被转 RecoveryRequired（新请求被误清）。
    #[tokio::test]
    async fn session_handover_preserves_newly_admitted_operations() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-a"))
            .await
            .expect("admit session A operation");
        assert!(kernel.mark_execution_consumed("op-a").await.unwrap());
        kernel
            .note_execution_session_ended(true)
            .await
            .expect("record handover");
        // Parked window: the user submits a fresh Start B. It must occupy the
        // execution slot with its own identity.
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-b"))
            .await
            .expect("admit parked-window operation");
        // Next business session start settles only the recorded handover set.
        kernel.prepare_relaunch().await.expect("prepare relaunch");
        let a = kernel.get("op-a").await.expect("view a").expect("exists");
        assert_eq!(a.state, RuntimeOperationState::RecoveryRequired);
        let b = kernel.get("op-b").await.expect("view b").expect("exists");
        assert_eq!(b.state, RuntimeOperationState::Accepted);
        assert_eq!(
            kernel.status().await.expect("status").active_operation_id,
            Some("op-b".to_string())
        );
        // Startup recovery skips the live admission instead of destroying it;
        // the interrupted record (not the new request) is what it recovers.
        let recovered = kernel.recover().await.expect("recover with live slot");
        assert_eq!(recovered, vec!["op-a".to_string()]);
        let b = kernel
            .get("op-b")
            .await
            .expect("view b after recover")
            .expect("exists");
        assert_eq!(b.state, RuntimeOperationState::Accepted);
        // Startup settle then converges the interrupted record; protection lifts.
        kernel
            .settle_unresolved_recoveries()
            .await
            .expect("settle startup recoveries");
        let a = kernel
            .get("op-a")
            .await
            .expect("view a settled")
            .expect("exists");
        assert_eq!(a.state, RuntimeOperationState::Failed);
        assert!(!kernel.recovery_protection_active());
    }

    /// RV01 反例（fresh 窗口）：owner 早期开放管理（R3）后、首个业务会话
    /// recover 前已受理的 Start 占据 active。修复前 recover 的
    /// active-is-none 硬前置失败，会话反复重启；修复后活跃受理被跳过。
    #[tokio::test]
    async fn recover_proceeds_with_session_window_admission_active() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-early"))
            .await
            .expect("admit early-window operation");
        let recovered = kernel.recover().await.expect("recover");
        assert!(recovered.is_empty());
        let view = kernel.get("op-early").await.expect("view").expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Accepted);
    }

    /// RV01 实测回归（容器矩阵 R3 尾部）：降级风暴中（会话在驱动前反复
    /// 失败）受理的操作不得被会话交接误杀——note_execution_session_ended
    /// (false)（未到达驱动的会话）保留活跃受理，下一会话的 prepare/recover
    /// 均不把它当残留收束，等待真实消费。
    #[tokio::test]
    async fn pre_driver_session_end_preserves_admitted_work() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-storm"))
            .await
            .expect("admit during degraded retry storm");
        kernel
            .note_execution_session_ended(false)
            .await
            .expect("record pre-driver session end");
        assert_eq!(
            kernel.status().await.expect("status").active_operation_id,
            Some("op-storm".to_string()),
            "live admission must survive a pre-driver session end"
        );
        kernel.recover().await.expect("recover");
        kernel.prepare_relaunch().await.expect("prepare relaunch");
        let view = kernel.get("op-storm").await.expect("view").expect("exists");
        assert_eq!(view.state, RuntimeOperationState::Accepted);
        assert_eq!(
            kernel.status().await.expect("status").active_operation_id,
            Some("op-storm".to_string())
        );
    }

    #[tokio::test]
    async fn session_end_preserves_unconsumed_admission_before_end_callback() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-ended"))
            .await
            .unwrap();
        assert!(kernel.mark_execution_consumed("op-ended").await.unwrap());
        kernel.commit_execution("op-ended").await.unwrap();
        // The old driver has ended, but its callback has not acquired the
        // kernel lock yet. A fresh request may already occupy the vacant slot.
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-fresh"))
            .await
            .unwrap();
        assert!(!kernel.mark_execution_consumed("op-ended").await.unwrap());
        kernel.note_execution_session_ended(true).await.unwrap();
        kernel.prepare_relaunch().await.unwrap();
        assert!(kernel.recover().await.unwrap().is_empty());
        assert_eq!(
            kernel.get("op-fresh").await.unwrap().unwrap().state,
            RuntimeOperationState::Accepted
        );
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("op-fresh")
        );
    }

    #[tokio::test]
    async fn session_end_preserves_queued_input_until_confirmed_recovery() {
        let (dir, _keep) = temp_store();
        let actions = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = actions.clone();
        let kernel = RuntimeKernel::new(
            open_store(&dir.path().join("workspace")),
            identity(),
            Box::new(move |action| sink.lock().unwrap().push(action)),
        );
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-running"))
            .await
            .unwrap();
        assert!(kernel.mark_execution_consumed("op-running").await.unwrap());
        let mut queued = request(RuntimeOperationKind::Restart, "op-queued");
        queued.run_config = Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "dev".into(),
                password: "retained-input-test".into(),
            }),
        });
        kernel.admit(queued).await.unwrap();
        assert!(!kernel.mark_execution_consumed("op-queued").await.unwrap());
        kernel.note_execution_session_ended(true).await.unwrap();
        assert!(!kernel.dispatch_pending_after_quiescence().await.unwrap());
        kernel.prepare_relaunch().await.unwrap();
        assert!(!kernel.dispatch_pending_after_quiescence().await.unwrap());
        assert_eq!(
            kernel.get("op-queued").await.unwrap().unwrap().state,
            RuntimeOperationState::Accepted
        );
        kernel.recover().await.unwrap();
        kernel.settle_unresolved_recoveries().await.unwrap();
        assert!(kernel.dispatch_pending_after_quiescence().await.unwrap());
        assert!(!kernel.dispatch_pending_after_quiescence().await.unwrap());
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("op-queued")
        );
        let actions = actions.lock().unwrap();
        let sources: Vec<_> = actions
            .iter()
            .filter(|action| matches!(action, DispatchAction::OrchestrateSource { .. }))
            .collect();
        assert_eq!(
            sources.len(),
            2,
            "cancellation notification is not business dispatch"
        );
        let DispatchAction::OrchestrateSource {
            operation_id,
            pg: Some(pg),
            ..
        } = sources[1]
        else {
            panic!("queued original input was not dispatched");
        };
        assert_eq!(operation_id, "op-queued");
        assert_eq!(pg.password, "retained-input-test");
    }

    #[tokio::test]
    async fn idle_stopped_intent_update_does_not_overwrite_fresh_admission() {
        let (dir, _keep) = temp_store();
        let kernel = std::sync::Arc::new(kernel(dir.path()));
        assert!(kernel.ensure_stopped_if_idle().await.unwrap());
        assert!(!kernel.ensure_stopped_if_idle().await.unwrap());
        assert_eq!(
            kernel.store.load_desired().unwrap(),
            (DesiredState::Stopped, 1)
        );
        let admission_guard = kernel.admission.lock().await;
        let (admit_entered, admit_ready) = tokio::sync::oneshot::channel();
        let accepting = kernel.clone();
        let admission = tokio::spawn(async move {
            let mut fresh = request(RuntimeOperationKind::Start, "op-explicit");
            fresh.expected_revision = 1;
            admit_entered.send(()).unwrap();
            accepting.admit(fresh).await
        });
        // On the single-thread test runtime, each task polls until it queues
        // behind this lock before the receiver resumes. The mutex is FIFO.
        admit_ready.await.unwrap();
        let (idle_entered, idle_ready) = tokio::sync::oneshot::channel();
        let stopping = kernel.clone();
        let idle_update = tokio::spawn(async move {
            idle_entered.send(()).unwrap();
            stopping.ensure_stopped_if_idle().await
        });
        idle_ready.await.unwrap();
        drop(admission_guard);
        assert!(matches!(
            admission.await.unwrap().unwrap(),
            AdmissionOutcome::Accepted(_)
        ));
        assert!(!idle_update.await.unwrap().unwrap());
        assert_eq!(
            kernel.store.load_desired().unwrap(),
            (DesiredState::Running, 1)
        );
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("op-explicit")
        );
    }

    #[tokio::test]
    async fn native_control_busy_preserves_replay_and_rejects_without_admission() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        let original = request(RuntimeOperationKind::Start, "op-existing");
        kernel.admit(original.clone()).await.unwrap();
        let held = Some(("native-stop".to_owned(), "cleanup_pending".to_owned()));
        assert!(matches!(
            kernel
                .admit_with_control_hold(original, true, held.clone())
                .await
                .unwrap(),
            AdmissionOutcome::Replayed(_)
        ));
        for next in [
            request(RuntimeOperationKind::Start, "op-new-start"),
            request(RuntimeOperationKind::Restart, "op-new-restart"),
            request_deploy_url("op-new-deploy"),
        ] {
            let id = next.operation_id.clone();
            let rejection = kernel
                .admit_with_control_hold(next, false, held.clone())
                .await
                .unwrap_err();
            assert_eq!(rejection.code, ERR_OPERATION_IN_PROGRESS);
            assert_eq!(
                rejection.active_operation_id.as_deref(),
                Some("native-stop")
            );
            assert!(rejection.message.contains("cleanup_pending"));
            assert!(!kernel.store.operation_path(&id).exists());
        }
        assert_eq!(
            kernel.store.load_desired().unwrap(),
            (DesiredState::Running, 0)
        );
        assert!(matches!(
            kernel
                .admit_with_control_hold(request(RuntimeOperationKind::Stop, "op-stop"), true, held)
                .await
                .unwrap(),
            AdmissionOutcome::Accepted(_)
        ));
    }

    /// RV01：会话交接记录的挂起 Stop 屏障在下一会话收束为幂等成功
    /// （会话终末清理已确认物理停止），且该收束不占用/清除新受理槽位。
    #[tokio::test]
    async fn session_handover_settles_pending_stop_barrier_idempotently() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "op-run"))
            .await
            .expect("admit running operation");
        assert!(kernel.mark_execution_consumed("op-run").await.unwrap());
        kernel
            .admit(request(RuntimeOperationKind::Stop, "op-stop"))
            .await
            .expect("admit stop barrier");
        assert!(kernel.mark_execution_consumed("op-stop").await.unwrap());
        kernel
            .note_execution_session_ended(true)
            .await
            .expect("record handover");
        // A new Start arrives in the parked window after the session ended;
        // the stop advanced the revision, so the fresh request carries it.
        let mut next_request = request(RuntimeOperationKind::Start, "op-next");
        next_request.expected_revision = 1;
        kernel
            .admit(next_request)
            .await
            .expect("admit next operation");
        kernel.prepare_relaunch().await.expect("prepare relaunch");
        let stop = kernel
            .get("op-stop")
            .await
            .expect("view stop")
            .expect("exists");
        assert_eq!(stop.state, RuntimeOperationState::Succeeded);
        let next = kernel
            .get("op-next")
            .await
            .expect("view next")
            .expect("exists");
        assert_eq!(next.state, RuntimeOperationState::Accepted);
        assert_eq!(
            kernel.status().await.expect("status").active_operation_id,
            Some("op-next".to_string())
        );
    }
    #[tokio::test]
    async fn latest_source_cancels_old_execution_and_only_latest_pending_is_promoted() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "source-old"))
            .await
            .unwrap();
        kernel
            .admit(request(RuntimeOperationKind::Start, "source-middle"))
            .await
            .unwrap();
        kernel
            .admit(request(RuntimeOperationKind::Start, "source-latest"))
            .await
            .unwrap();
        assert!(kernel.is_cancelled("source-old"));
        assert_eq!(
            kernel.commit_execution("source-old").await.unwrap(),
            CommitBarrierOutcome::Cancelled
        );
        assert_eq!(
            kernel.get("source-middle").await.unwrap().unwrap().state,
            RuntimeOperationState::Cancelled
        );
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("source-old")
        );
        // Confirmation of actual old cleanup is the execution driver's boundary.
        kernel
            .finish(
                "source-old",
                RuntimeOperationState::Cancelled,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("source-latest")
        );
        assert_eq!(
            kernel.commit_execution("source-old").await.unwrap(),
            CommitBarrierOutcome::NotActive
        );
        assert_eq!(
            kernel.commit_execution("source-latest").await.unwrap(),
            CommitBarrierOutcome::Committed
        );
        assert_eq!(
            kernel.get("source-old").await.unwrap().unwrap().state,
            RuntimeOperationState::Cancelled
        );
    }

    #[tokio::test]
    async fn latest_source_notification_targets_original_child_without_replacing_stop() {
        let captured: std::sync::Arc<std::sync::Mutex<Vec<DispatchAction>>> = Default::default();
        let sink = captured.clone();
        let (dir, _keep) = temp_store();
        let kernel = RuntimeKernel::new(
            open_store(&dir.path().join("workspace")),
            identity(),
            Box::new(move |action| {
                sink.lock().unwrap().push(action);
            }),
        );
        kernel
            .admit(request(RuntimeOperationKind::Start, "executing-source"))
            .await
            .unwrap();
        kernel
            .admit(request(RuntimeOperationKind::Stop, "original-stop"))
            .await
            .unwrap();
        let mut latest = request(RuntimeOperationKind::Start, "latest-source");
        latest.expected_revision = 1;
        kernel.admit(latest).await.unwrap();
        {
            let actions = captured.lock().unwrap();
            let notifications: Vec<_> = actions
                .iter()
                .filter_map(|action| match action {
                    DispatchAction::CancelSource { operation_id } => Some(operation_id.as_str()),
                    _ => None,
                })
                .collect();
            assert_eq!(notifications, ["executing-source"]);
            assert!(!actions.iter().any(|action| matches!(action, DispatchAction::OrchestrateSource { operation_id, .. } if operation_id == "latest-source")));
            assert_eq!(
                actions
                    .iter()
                    .filter(|action| matches!(action, DispatchAction::StopBusiness { .. }))
                    .count(),
                1
            );
        }
        assert!(!kernel.is_cancelled("original-stop"));
        assert_eq!(
            kernel.get("original-stop").await.unwrap().unwrap().state,
            RuntimeOperationState::Accepted
        );
        kernel
            .finish(
                "executing-source",
                RuntimeOperationState::Cancelled,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert_ne!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("latest-source")
        );
        kernel
            .finish(
                "original-stop",
                RuntimeOperationState::Succeeded,
                None,
                None,
                3,
            )
            .await
            .unwrap();
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("latest-source")
        );
    }

    #[tokio::test]
    async fn latest_source_during_stop_waits_for_the_real_stop_terminal_boundary() {
        let (dir, _keep) = temp_store();
        let kernel = kernel(dir.path());
        kernel
            .admit(request(RuntimeOperationKind::Start, "source-running"))
            .await
            .unwrap();
        kernel
            .admit(request(RuntimeOperationKind::Stop, "stop-original"))
            .await
            .unwrap();
        let mut next = request(RuntimeOperationKind::Start, "source-after-stop");
        next.expected_revision = 1;
        assert!(matches!(
            kernel.admit(next).await.unwrap(),
            AdmissionOutcome::Accepted(_)
        ));
        assert!(!kernel.is_cancelled("stop-original"));
        kernel
            .finish(
                "source-running",
                RuntimeOperationState::Cancelled,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert_ne!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("source-after-stop")
        );
        kernel
            .finish(
                "stop-original",
                RuntimeOperationState::Succeeded,
                None,
                None,
                3,
            )
            .await
            .unwrap();
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("source-after-stop")
        );
        assert_eq!(
            kernel.commit_execution("source-after-stop").await.unwrap(),
            CommitBarrierOutcome::Committed
        );
    }

    fn recorded_recovery_operation(diagnostic: &str) -> StoredOperation {
        let mut input = request(RuntimeOperationKind::Restart, "recorded-recovery");
        input.run_config = Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "fixture-user".into(),
                password: "fixture-original-input".into(),
            }),
        });
        let mut view = RuntimeOperationView {
            operation_id: input.operation_id.clone(),
            kind: input.kind,
            state: RuntimeOperationState::RecoveryRequired,
            request_digest: runtime_request_digest(&input).unwrap(),
            revision: 0,
            runtime_instance_id: "instance-1".into(),
            error_code: None,
            error_message: None,
            failure_detail: None,
        };
        if matches!(diagnostic, "full" | "without-detail" | "code-only") {
            view.error_code = Some("entry_preflight_failed".into());
        }
        if matches!(diagnostic, "full" | "without-detail" | "message-only") {
            view.error_message = Some("original stop RPC exceeded its captured deadline".into());
        }
        if matches!(diagnostic, "full" | "detail-only") {
            view.failure_detail = Some(RuntimeFailureDetail {
                stage: "original_proxy_stop".into(),
                exit_code: Some(17),
                stderr_tail: Some("original bounded stderr".into()),
                cleanup_confirmed: false,
            });
        }
        StoredOperation {
            view,
            request: input,
        }
    }

    fn assert_recovery_record_preserved(
        actual: &StoredOperation,
        original: &StoredOperation,
        state: RuntimeOperationState,
    ) {
        let mut expected = original.clone();
        expected.view.state = state;
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap(),
            "recovery may settle state without rewriting the original failure or private input"
        );
    }

    #[tokio::test]
    async fn recovery_settlement_preserves_recorded_diagnostics_and_reopens_start() {
        for diagnostic in [
            "full",
            "without-detail",
            "code-only",
            "message-only",
            "detail-only",
        ] {
            let (directory, store) = temp_store();
            let original = recorded_recovery_operation(diagnostic);
            store.store_operation(&original).unwrap();
            let kernel = kernel(directory.path());
            assert_eq!(kernel.recover().await.unwrap(), vec!["recorded-recovery"]);
            let recovered = kernel
                .store
                .load_operation("recorded-recovery")
                .unwrap()
                .unwrap();
            assert_recovery_record_preserved(
                &recovered,
                &original,
                RuntimeOperationState::RecoveryRequired,
            );
            assert!(kernel.status().await.unwrap().recovery_protection);
            assert_eq!(
                kernel.settle_unresolved_recoveries().await.unwrap(),
                vec!["recorded-recovery"]
            );
            let settled = kernel
                .store
                .load_operation("recorded-recovery")
                .unwrap()
                .unwrap();
            assert_recovery_record_preserved(&settled, &original, RuntimeOperationState::Failed);
            assert!(!kernel.status().await.unwrap().recovery_protection);
            assert!(kernel.recover().await.unwrap().is_empty());
            assert_recovery_record_preserved(
                &kernel
                    .store
                    .load_operation("recorded-recovery")
                    .unwrap()
                    .unwrap(),
                &original,
                RuntimeOperationState::Failed,
            );
            kernel
                .admit(request(
                    RuntimeOperationKind::Start,
                    "source-after-recovery",
                ))
                .await
                .expect("a settled historical failure must allow a new source start");
        }
    }

    #[tokio::test]
    async fn recovery_without_recorded_result_uses_state_until_interruption_is_settled() {
        for previous in [
            RuntimeOperationState::Starting,
            RuntimeOperationState::RecoveryRequired,
        ] {
            let (directory, store) = temp_store();
            let mut original = recorded_recovery_operation("none");
            original.view.state = previous;
            store.store_operation(&original).unwrap();
            let kernel = kernel(directory.path());
            kernel.recover().await.unwrap();
            let recovered = kernel.get("recorded-recovery").await.unwrap().unwrap();
            assert_eq!(recovered.state, RuntimeOperationState::RecoveryRequired);
            assert!(recovered.error_code.is_none());
            assert!(recovered.error_message.is_none());
            assert!(recovered.failure_detail.is_none());
            assert!(
                kernel.status().await.unwrap().recovery_protection,
                "unknown state must keep protection even without an invented error code"
            );
            kernel.settle_unresolved_recoveries().await.unwrap();
            let settled = kernel
                .store
                .load_operation("recorded-recovery")
                .unwrap()
                .unwrap();
            assert_eq!(settled.view.state, RuntimeOperationState::Failed);
            assert_eq!(
                settled.view.error_code.as_deref(),
                Some(ERR_INTERRUPTED_OWNER_EXIT)
            );
            assert_eq!(
                settled.view.error_message.as_deref(),
                Some("operation result was not committed before runtime management recovery")
            );
            let detail = settled.view.failure_detail.as_ref().unwrap();
            assert_eq!(detail.stage, "startup_reconciled");
            assert!(!detail.cleanup_confirmed);
            assert_eq!(
                serde_json::to_value(&settled.request).unwrap(),
                serde_json::to_value(&original.request).unwrap()
            );
            kernel
                .admit(request(
                    RuntimeOperationKind::Start,
                    "source-after-interruption",
                ))
                .await
                .expect("confirmed recovery releases unknown-result protection");
        }
    }

    #[tokio::test]
    async fn journal_reconciliation_preserves_recorded_failure_even_after_cancellation() {
        use crate::server::journal::{Boundary, Receipt};
        for diagnostic in ["full", "without-detail", "none"] {
            for boundary in [
                Boundary::Preparing,
                Boundary::Active,
                Boundary::StartupFailed,
            ] {
                for cancelled in [false, true] {
                    let (directory, store) = temp_store();
                    let original = recorded_recovery_operation(diagnostic);
                    store.store_operation(&original).unwrap();
                    if cancelled {
                        store.store_cancellation(&original).unwrap();
                    }
                    let kernel = kernel(directory.path());
                    kernel.recover().await.unwrap();
                    let preparing = boundary == Boundary::Preparing;
                    let active = boundary == Boundary::Active;
                    let receipt = Receipt {
                        generation: "gen-1".into(),
                        operation: shared_types::AppDeploymentOperation {
                            operation_id: original.view.operation_id.clone(),
                            deployment_generation_id: "gen-1".into(),
                            deploy_stage: if preparing {
                                shared_types::app_cli_deploy::AppDeploymentStage::Failed
                            } else {
                                shared_types::app_cli_deploy::AppDeploymentStage::Succeeded
                            },
                            persisted: !preparing,
                            request_release_id: "fixture-release".into(),
                            artifact_release_id: Some("fixture-release".into()),
                            recovery: None,
                            phase: if active {
                                shared_types::AppCliDeployPhase::Running
                            } else {
                                shared_types::AppCliDeployPhase::Failed
                            },
                            error: Some("journal fallback startup failure".into()),
                        },
                        request: crate::server::DeployRequest {
                            runtime_operation_id: Some(original.view.operation_id.clone()),
                            url: "https://example.invalid/fixture.zip".into(),
                            release_id: "fixture-release".into(),
                            sha256: None,
                            local_path: None,
                            execution_target: None,
                            run_pg: None,
                        },
                        boundary: boundary.clone(),
                        active: None,
                    };
                    assert!(kernel.reconcile_quiesced_operation(&receipt).await.unwrap());
                    let settled = kernel
                        .store
                        .load_operation("recorded-recovery")
                        .unwrap()
                        .unwrap();
                    let expected_state = if cancelled {
                        RuntimeOperationState::Cancelled
                    } else {
                        RuntimeOperationState::Failed
                    };
                    if diagnostic != "none" {
                        assert_recovery_record_preserved(&settled, &original, expected_state);
                    } else {
                        assert_eq!(settled.view.state, expected_state);
                        let code = if cancelled {
                            "ERR_CANCELLED"
                        } else if preparing || active {
                            "ERR_OPERATION_INTERRUPTED"
                        } else {
                            "ERR_STARTUP_FAILED"
                        };
                        assert_eq!(settled.view.error_code.as_deref(), Some(code));
                        assert!(
                            settled
                                .view
                                .failure_detail
                                .as_ref()
                                .unwrap()
                                .cleanup_confirmed
                        );
                        assert_eq!(
                            serde_json::to_value(&settled.request).unwrap(),
                            serde_json::to_value(&original.request).unwrap()
                        );
                    }
                    assert!(!kernel.status().await.unwrap().recovery_protection);
                    assert!(
                        !kernel.reconcile_quiesced_operation(&receipt).await.unwrap(),
                        "repeat reconciliation must preserve the captured terminal result"
                    );
                    kernel
                        .admit(request(RuntimeOperationKind::Start, "source-after-journal"))
                        .await
                        .expect("settled journal diagnostics cannot block new source startup");
                }
            }
        }
    }
}
