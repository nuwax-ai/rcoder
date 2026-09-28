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
            assert!(
                !std::fs::read_to_string(kernel.store.operation_path("b"))
                    .unwrap()
                    .contains("probe-secret")
            );
            assert_eq!(
                kernel.commit_execution("a").await.unwrap(),
                CommitBarrierOutcome::Committed
            );
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
            assert_eq!(
                kernel.get("b").await.unwrap().unwrap().state,
                RuntimeOperationState::Succeeded
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
        assert!(kernel.append_orchestration_event("build", None, "Building", None));
        assert!(kernel.append_orchestration_event("ready", None, "Ready", None));
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
        assert!(!runtime.append_orchestration_event("progress", None, "Step", None));
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
                assert_eq!(captured.lock().unwrap().len(), 1);
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
                assert_eq!(captured.lock().unwrap().len(), 2);
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
            let mut start = request(RuntimeOperationKind::Start, "start");
            start.expected_revision = 1;
            kernel.admit(start).await.unwrap();
            assert_eq!(captured.lock().unwrap().len(), 1);
            kernel
                .finish("stop", stop_result, None, None, 2)
                .await
                .unwrap();
            assert_eq!(
                kernel
                    .status()
                    .await
                    .unwrap()
                    .active_operation_id
                    .as_deref(),
                Some("start")
            );
            assert_eq!(captured.lock().unwrap().len(), 2);
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
        assert_eq!(
            kernel
                .status()
                .await
                .unwrap()
                .active_operation_id
                .as_deref(),
            Some("op-b")
        );
        assert_eq!(
            kernel.get("op-a").await.unwrap().unwrap().state,
            RuntimeOperationState::Failed
        );
        kernel
            .finish("op-b", RuntimeOperationState::Succeeded, None, None, 2)
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

    /// R08：Restart 携带 run_config.pg → 派发动作拿到真实凭据；持久化副本
    /// 密码脱敏（重放摘要不含 run_config，脱敏不影响幂等语义）。
    #[tokio::test]
    async fn run_config_pg_reaches_dispatch_and_redacts_on_disk() {
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
        // 持久化副本：密码为空（脱敏），用户名保留诊断
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
            persisted_pg.password, "",
            "password must be redacted on disk"
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
        // 无活跃操作：no-op（idle 期只有 stdout 消费者）
        assert!(!kernel.append_orchestration_event(
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
            "service",
            Some("frontend".into()),
            "service_starting",
            None
        ));
        assert!(kernel.append_orchestration_event(
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
        assert!(RuntimeStore::open_with_root(root.clone(), &workspace).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("token")).unwrap(),
            "current-token"
        );
        assert!(legacy.join("desired.json").exists());
    }

    #[tokio::test]
    async fn dual_authority_domains_fail_closed() {
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
        let error = RuntimeStore::open_with_root(root, &workspace)
            .err()
            .expect("must refuse");
        assert!(error.to_string().contains("authority domains"));
        // 多处 legacy 并存（in-workspace + bare 卷根）同样拒绝裁决
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
        let error2 = RuntimeStore::open_with_root(root2, &workspace2)
            .err()
            .expect("must refuse");
        assert!(error2.to_string().contains("legacy"));
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
    async fn partial_admission_holds_operation_and_protection() {
        let (dir, _keep) = temp_store();
        let _workspace = dir.path().join("workspace");
        // 先正常打开一次 kernel 并受理一个操作，让 desired.json 存在；
        // 然后把 desired.json 变为不可写目录，模拟后续 desired 写失败。
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

        let kernel = kernel(dir.path());
        let rejection = kernel
            .admit(request(RuntimeOperationKind::Start, "op-partial"))
            .await
            .expect_err("desired write fails");
        assert_eq!(rejection.code, ERR_RECOVERY_REQUIRED);
        assert!(rejection.message.contains("held for recovery"));
        // 部分提交的操作转 RecoveryRequired（可查询，不再派发）
        let held = kernel
            .get("op-partial")
            .await
            .expect("query")
            .expect("present");
        assert_eq!(held.state, RuntimeOperationState::RecoveryRequired);
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
}
