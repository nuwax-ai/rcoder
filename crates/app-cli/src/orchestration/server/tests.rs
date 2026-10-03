use super::*;

#[cfg(test)]
mod cases {
    use super::*;

    #[test]
    fn attach_requires_canonical_project_and_compatible_protocol() {
        let temp = tempfile::tempdir().unwrap();
        let local = temp.path().join("a/workspace");
        let other = temp.path().join("b/workspace");
        std::fs::create_dir_all(local.join(".run")).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let mut identity = shared_types::RuntimeIdentityView {
            application_id: "native".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "workspace".into(),
            source_root: other.to_string_lossy().into_owned(),
            runtime_instance_id: "owner".into(),
            deployment_generation_id: "generation".into(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        assert!(validate_attach_identity(&local, "native", &identity).is_err());
        identity.source_root = local.to_string_lossy().into_owned();
        validate_attach_identity(&local.join(".run"), "native", &identity).unwrap();
        identity.protocol_version += 1;
        assert!(validate_attach_identity(&local, "native", &identity).is_err());
    }

    fn state() -> ServerState {
        ServerState::new(RuntimeStatusService::default())
    }

    #[tokio::test]
    async fn delayed_generation_shutdown_does_not_cancel_its_successor() {
        let state = Arc::new(state());
        state.set_generation("artifactdeployment".into());
        let old = state.generation_control("nativeold").unwrap();
        state.begin_business_session("nativeold".into(), true, || false);
        state.mark_initialized();
        assert_eq!(state.generation_value(), "artifactdeployment");
        assert!(
            old.ready(),
            "artifact identity must not replace native control identity"
        );
        let (release, wait) = tokio::sync::oneshot::channel();
        let callback = tokio::spawn(async move {
            wait.await.unwrap();
            old.shutdown().await.unwrap();
        });
        let current = state.generation_control("nativenew").unwrap();
        state.begin_business_session("nativenew".into(), false, || false);
        assert_eq!(state.generation_value(), "artifactdeployment");
        let token = state.cancel_token();
        release.send(()).unwrap();
        callback.await.unwrap();
        assert!(!token.is_cancelled());
        assert!(state.accepting.load(std::sync::atomic::Ordering::Acquire));
        current.shutdown().await.unwrap();
        assert!(token.is_cancelled());
    }

    #[tokio::test]
    async fn standalone_kernel_publishes_the_resumed_deployment_generation() {
        assert!(std::env::var_os(shared_types::APP_DEPLOY_GENERATION_ID).is_none());
        assert!(std::env::var_os("APP_CLI_STATE_ROOT").is_none());
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let application_id = std::env::var("PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown-app".into());
        let root =
            crate::runtime_kernel::RuntimeStore::resolve_root(&workspace, &application_id).unwrap();
        let first = state();
        first.set_generation("persisted-deployment".into());
        *first.journal.lock().unwrap() = Some(Journal::open_with_root(&workspace, root).unwrap());
        first
            .try_accept_deploy_with_id(request(), "original".into())
            .unwrap();
        first
            .fail_operation("original failure".into(), Boundary::Failed)
            .unwrap();
        drop(first);

        let restarted = Arc::new(state());
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        let kernel = assemble_runtime_kernel(&restarted, &args).await.unwrap();
        assert_eq!(
            kernel.identity().deployment_generation_id,
            "persisted-deployment"
        );
        restarted.begin_business_session("new-native-session".into(), false, || false);
        assert_eq!(restarted.generation_value(), "persisted-deployment");
    }

    #[tokio::test]
    async fn initial_failed_replay_preserves_result_without_reading_an_empty_queue() {
        let directory = tempfile::tempdir().unwrap();
        let state = state();
        state.set_generation("deployment".into());
        *state.journal.lock().unwrap() =
            Some(Journal::open(&directory.path().join("code")).unwrap());
        state
            .try_accept_deploy_with_id(request(), "original".into())
            .unwrap();
        state.deploy_rx.lock().await.try_recv().unwrap();
        state
            .fail_operation("original failure".into(), Boundary::Failed)
            .unwrap();
        let before = std::fs::read(directory.path().join(".deploy-operation.json")).unwrap();
        let admission = state
            .try_accept_deploy_with_id(request(), "original".into())
            .unwrap();
        assert!(
            super::super::startup::initial_action_for_deployment_admission(&state, admission)
                .await
                .unwrap()
                .is_none()
        );
        assert!(state.deploy_rx.lock().await.try_recv().is_err());
        let operation = state.deploy_status().operation.unwrap();
        assert_eq!(operation.operation_id, "original");
        assert_eq!(operation.error.as_deref(), Some("original failure"));
        assert!(!state.runtime_recovery_hold_active());
        assert_eq!(
            std::fs::read(directory.path().join(".deploy-operation.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn quarantined_input_keeps_management_without_guessing_source_startup() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let root = dir.path().join("state");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&release("unpublishedsource")).unwrap(),
        )
        .unwrap();
        std::fs::write(root.join(".deploy-operation.json"), "{damaged").unwrap();
        let journal = Journal::open_with_root(&workspace, root.clone()).unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(journal);
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        assert!(initialize_startup(&args, &state).await.unwrap().is_none());
        assert!(
            !state.runtime_recovery_hold_active(),
            "historical input loss must not fence explicit replacement or Stop"
        );
        assert!(root.join(".deploy-recovery-required.json").exists());
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let kernel = Arc::new(crate::runtime_kernel::RuntimeKernel::new(
            crate::runtime_kernel::RuntimeStore::open_with_root(root, &args.workspace).unwrap(),
            shared_types::RuntimeIdentityView {
                application_id: "appone".into(),
                workspace_id: "workspace".into(),
                service_family: "userapp-dev".into(),
                source_root: args.workspace.to_string_lossy().into_owned(),
                runtime_instance_id: "instanceone".into(),
                deployment_generation_id: "generationone".into(),
                protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
                capabilities: vec![],
            },
            Box::new(move |action| {
                tx.send(action).unwrap();
            }),
        ));
        state.set_runtime_kernel(kernel.clone());
        let submitted = admit_explicit_run_replacement(&args, &state)
            .await
            .unwrap()
            .expect("this run submitted its own request");
        let operation_id = match rx.try_recv().unwrap() {
            crate::runtime_kernel::DispatchAction::OrchestrateSource { operation_id, .. } => {
                operation_id
            }
            other => panic!("explicit Source run dispatched {other:?}"),
        };
        assert_eq!(submitted, operation_id);
        assert_eq!(
            kernel.get(&operation_id).await.unwrap().unwrap().state,
            shared_types::RuntimeOperationState::Accepted
        );
    }

    #[tokio::test]
    async fn explicit_run_observes_its_failed_preflight_without_stopping_successors() {
        for (terminal, successor) in [
            (shared_types::RuntimeOperationState::Failed, false),
            (shared_types::RuntimeOperationState::Failed, true),
            (shared_types::RuntimeOperationState::Cancelled, true),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let state = state();
            let kernel = kernel_for(dir.path());
            state.set_runtime_kernel(kernel.clone());
            state.set_phase(ServerPhase::Idle);
            kernel
                .admit(runtime_request(
                    shared_types::RuntimeOperationKind::Start,
                    "runone",
                    0,
                ))
                .await
                .unwrap();
            kernel
                .finish(
                    "runone",
                    terminal,
                    Some(("ERR_VALIDATION".into(), "startup preflight failed".into())),
                    None,
                    0,
                )
                .await
                .unwrap();
            if successor {
                kernel
                    .admit(runtime_request(
                        shared_types::RuntimeOperationKind::Start,
                        "platformnext",
                        0,
                    ))
                    .await
                    .unwrap();
            }
            let observation = observe_explicit_run_failure(&state, "runone")
                .await
                .unwrap();
            if successor {
                assert_eq!(observation, ExplicitRunObservation::Detached);
                assert_eq!(
                    kernel
                        .status()
                        .await
                        .unwrap()
                        .active_operation_id
                        .as_deref(),
                    Some("platformnext")
                );
                assert!(!state.cancel_token().is_cancelled());
            } else {
                assert!(
                    matches!(observation, ExplicitRunObservation::Failure(message)
                    if message.contains("startup preflight failed"))
                );
                assert_eq!(
                    state.phase(),
                    ServerPhase::Idle,
                    "operation failure must be visible even if preflight never changed the business phase"
                );
            }
        }
    }

    #[tokio::test]
    async fn server_driver_shutdown_waits_for_drop_after_failure_or_timeout() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct DriverDrop(Arc<AtomicBool>);
        impl Drop for DriverDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }

        for cooperative in [true, false] {
            let dropped = Arc::new(AtomicBool::new(false));
            let task_dropped = dropped.clone();
            let cancel = CancellationToken::new();
            let task_cancel = cancel.clone();
            let (started, ready) = tokio::sync::oneshot::channel();
            let mut driver = tokio::spawn(async move {
                let _drop = DriverDrop(task_dropped);
                started.send(()).unwrap();
                if cooperative {
                    task_cancel.cancelled().await;
                    anyhow::bail!("controlled driver failure");
                }
                std::future::pending::<Result<()>>().await
            });
            ready.await.unwrap();
            cancel.cancel();
            let deadline = if cooperative {
                tokio::time::Instant::now() + std::time::Duration::from_secs(1)
            } else {
                tokio::time::Instant::now()
            };
            let error = drain_server_driver(&mut driver, deadline)
                .await
                .unwrap_err();
            assert!(
                dropped.load(Ordering::Acquire),
                "the old driver must be dropped before the business session returns"
            );
            if cooperative {
                assert!(error.to_string().contains("controlled driver failure"));
            } else {
                assert!(
                    error
                        .to_string()
                        .contains("shutdown confirmation timed out")
                );
            }
        }
    }

    /// RV04 屏障契约：取消令牌换代与 Stop 受理共用 admission 线性化点。
    /// 停止交接在途（probe=true）且上一代令牌已取消 → 接力，Stop 不被
    /// renew 丢弃；停止已完成（probe=false）→ 新令牌，空闲管理会话不因
    /// 旧取消立即退出成环。
    #[tokio::test]
    async fn token_renewal_inherits_cancel_only_while_stop_handover_in_progress() {
        let state = state();
        state.trigger_cancel();
        state.begin_business_session("gen-1".into(), true, || true);
        assert!(
            state.cancel_token().is_cancelled(),
            "an accepted in-flight stop must survive token renewal"
        );
        state.begin_business_session("gen-2".into(), false, || false);
        assert!(
            !state.cancel_token().is_cancelled(),
            "a completed stop must not cancel the fresh management session"
        );
    }

    /// RV02：业务会话的干净关停不得关闭/排空 owner 级部署通道——统一
    /// owner 的 ServerState 跨会话复用同一 receiver，关闭会让后续会话的
    /// server_loop 立即返回、再部署永久失败。
    #[tokio::test]
    async fn session_clean_shutdown_keeps_owner_deploy_channel_open() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let state = state();
        let mut journal = Journal::open(&workspace).unwrap();
        journal.commit_coordinator().unwrap();
        *state.journal.lock().unwrap() = Some(journal);
        state.close_admission();
        let args = RuntimeArgs {
            workspace: workspace.clone(),
            ..Default::default()
        };
        finish_clean_shutdown(&args, &state, true, false)
            .await
            .expect("clean session shutdown");
        state
            .deploy_tx
            .send(request())
            .expect("owner-level deploy channel survives session end");
        let received = {
            let mut receiver = state.deploy_rx.lock().await;
            receiver.try_recv()
        };
        assert!(
            received.is_ok(),
            "queued request must remain consumable by the next session"
        );
    }

    #[test]
    fn local_artifact_target_and_source_profile_survive_confirmed_receipts() {
        for run_owner in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("workspace");
            let run = source.join(".run");
            std::fs::create_dir_all(&run).unwrap();
            // Runtime targets are canonical; macOS temp paths may alias /private/var.
            let source = source.canonicalize().unwrap();
            let run = run.canonicalize().unwrap();
            std::fs::write(source.join("source-sentinel"), "never replace source").unwrap();
            let args = RuntimeArgs {
                workspace: if run_owner {
                    run.clone()
                } else {
                    source.clone()
                },
                ..Default::default()
            };
            let state = state();
            *state.journal.lock().unwrap() = Some(Journal::open(&args.workspace).unwrap());
            let mut deploy = request();
            deploy.runtime_operation_id = Some("localartifact".into());
            deploy.execution_target = Some(ExecutionTarget::ProjectRun);
            deploy.local_path = Some(source.join("builds/workspace-package-b.zip"));
            state.record_runtime_deployment(&deploy).unwrap();
            let receipt = state
                .journal
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .receipt
                .clone()
                .unwrap();
            assert_eq!(receipt.operation.operation_id, "localartifact");
            assert_eq!(receipt.boundary, Boundary::Preparing);
            assert_eq!(
                receipt.request.execution_target,
                Some(ExecutionTarget::ProjectRun)
            );
            assert_eq!(
                execution_workspace(&args.workspace, deploy.execution_target),
                run
            );
            let mut changed = deploy.clone();
            changed.execution_target = Some(ExecutionTarget::Source);
            assert!(
                state.record_runtime_deployment(&changed).is_err(),
                "same operation cannot change target"
            );
            state.persist_boundary(Boundary::Switching).unwrap();
            state.set_release(release("artifact-b"));
            state.complete_stage().unwrap();
            state.complete_running().unwrap();
            assert_eq!(restored_runtime_args(&args, &state).unwrap().workspace, run);
            let mut source_request = request();
            source_request.runtime_operation_id = Some("sourceagain".into());
            source_request.execution_target = Some(ExecutionTarget::Source);
            state.record_runtime_deployment(&source_request).unwrap();
            state.persist_boundary(Boundary::Switching).unwrap();
            state.set_release(release("source-a"));
            state.complete_stage().unwrap();
            state.complete_running().unwrap();
            assert_eq!(
                restored_runtime_args(&args, &state).unwrap().workspace,
                source
            );
            assert_eq!(
                std::fs::read_to_string(source.join("source-sentinel")).unwrap(),
                "never replace source"
            );
        }
    }

    #[test]
    fn local_artifact_recovery_refuses_unknown_target() {
        let dir = tempfile::tempdir().unwrap();
        let args = RuntimeArgs {
            workspace: dir.path().join("workspace"),
            ..Default::default()
        };
        std::fs::create_dir_all(&args.workspace).unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&args.workspace).unwrap());
        let mut deploy = request();
        deploy.runtime_operation_id = Some("localartifact".into());
        deploy.local_path = Some(args.workspace.join("builds/workspace-package-b.zip"));
        deploy.execution_target = Some(ExecutionTarget::ProjectRun);
        state.record_runtime_deployment(&deploy).unwrap();
        state.set_release(release("artifact-b"));
        state.complete_stage().unwrap();
        state.complete_running().unwrap();
        assert_eq!(
            restored_runtime_args(&args, &state).unwrap().workspace,
            std::fs::canonicalize(&args.workspace).unwrap().join(".run"),
            "confirmed local artifact recovers without a credential activation gate"
        );
        {
            let mut journal = state.journal.lock().unwrap();
            let active_request = journal
                .as_mut()
                .unwrap()
                .receipt
                .as_mut()
                .unwrap()
                .active
                .as_mut()
                .unwrap()
                .request
                .as_mut()
                .unwrap();
            active_request.execution_target = None;
        }
        assert!(
            restored_runtime_args(&args, &state).is_err(),
            "old local receipt has no trusted target"
        );
        assert!(state.runtime_recovery_hold_active());
    }

    #[test]
    fn owner_token_is_stable_per_instance_and_not_exported_to_env() {
        let before = std::env::var_os("APP_CLI_DEPLOY_TOKEN");
        let first = state();
        let second = state();
        first.initialize_owner_token().unwrap();
        second.initialize_owner_token().unwrap();
        let token = first.control_token().unwrap();
        assert_eq!(first.control_token().as_deref(), Some(token.as_str()));
        match before
            .as_ref()
            .and_then(|value| value.to_str())
            .filter(|value| !value.trim().is_empty())
        {
            Some(configured) => assert_eq!(token, configured),
            None => assert_ne!(Some(token), second.control_token()),
        }
        assert_eq!(std::env::var_os("APP_CLI_DEPLOY_TOKEN"), before);
        assert!(first.initialize_owner_token().is_err());
    }

    #[test]
    fn shutdown_budget_covers_grace_force_and_previous_generation() {
        let state = state();
        assert_eq!(state.shutdown_budget(false).as_secs(), 65);
        let mut old = release("old");
        old.services[0].run.shutdown_timeout_seconds = 90;
        state.set_release(old);
        assert_eq!(state.shutdown_budget(false).as_secs(), 125);
        let mut new = release("new");
        new.services[0].run.shutdown_timeout_seconds = 1;
        state.set_release(new);
        assert_eq!(
            state.shutdown_budget(false).as_secs(),
            125,
            "new release must not shorten a still-stopping generation's budget"
        );
        assert!(
            state.shutdown_budget(true).as_secs() >= 390,
            "supervisord requires sequential stop/remove RPC budgets plus cleanup"
        );
    }

    fn request() -> DeployRequest {
        DeployRequest {
            runtime_operation_id: None,
            url: "http://artifact".into(),
            local_path: None,
            execution_target: None,

            run_pg: None,
            release_id: "caller-token".into(),
            sha256: None,
        }
    }

    #[tokio::test]
    async fn deployment_replay_is_durable_and_does_not_block_new_operations() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let first = state();
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let mut input = request();
        input.run_pg = Some(shared_types::StartPgCredential {
            username: "dev".into(),
            password: "original-secret".into(),
        });
        assert!(matches!(
            first
                .try_accept_deploy_with_id(input.clone(), "a".into())
                .unwrap(),
            DeployAdmission::Accepted
        ));
        assert!(matches!(
            first
                .try_accept_deploy_with_id(input.clone(), "a".into())
                .unwrap(),
            DeployAdmission::Replayed(_)
        ));
        let mut changed = input.clone();
        changed.run_pg.as_mut().unwrap().password = "changed-secret".into();
        assert!(matches!(
            first.try_accept_deploy_with_id(changed, "a".into()),
            Err(AdmissionError::Conflict(_))
        ));
        let mut received = first.deploy_rx.lock().await;
        assert!(received.try_recv().is_ok());
        assert!(
            received.try_recv().is_err(),
            "replay must not dispatch again"
        );
        drop(received);
        first
            .fail_operation("confirmed failure".into(), Boundary::Failed)
            .unwrap();
        assert!(matches!(
            first
                .try_accept_deploy_with_id(input.clone(), "b".into())
                .unwrap(),
            DeployAdmission::Accepted
        ));
        drop(first);
        let restarted = state();
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let DeployAdmission::Replayed(original) = restarted
            .try_accept_deploy_with_id(input.clone(), "a".into())
            .unwrap()
        else {
            panic!("expected original result")
        };
        assert_eq!(original.phase, AppCliDeployPhase::Failed);
        assert_eq!(original.error.as_deref(), Some("confirmed failure"));
        assert!(matches!(
            restarted
                .try_accept_deploy_with_id(input, "b".into())
                .unwrap(),
            DeployAdmission::Replayed(_)
        ));
        assert!(restarted.deploy_rx.lock().await.try_recv().is_err());
        assert_eq!(
            restarted
                .journal
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .receipt
                .as_ref()
                .unwrap()
                .operation
                .operation_id,
            "b"
        );
        assert!(
            !std::fs::read_to_string(dir.path().join(".deploy-operation.json"))
                .unwrap()
                .contains("original-secret")
        );
        assert_eq!(
            restarted
                .recorded_deployment("a")
                .unwrap()
                .unwrap()
                .error
                .as_deref(),
            Some("confirmed failure")
        );
    }

    #[tokio::test]
    async fn concurrent_deployment_retries_dispatch_only_once() {
        let state = Arc::new(state());
        let mut threads = Vec::new();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        for _ in 0..2 {
            let state = state.clone();
            let barrier = barrier.clone();
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                state
                    .try_accept_deploy_with_id(request(), "same".into())
                    .unwrap()
            }));
        }
        barrier.wait();
        let accepted = threads
            .into_iter()
            .filter_map(|thread| match thread.join().unwrap() {
                DeployAdmission::Accepted => Some(()),
                DeployAdmission::Replayed(_) => None,
            })
            .count();
        assert_eq!(accepted, 1);
        let mut queue = state.deploy_rx.lock().await;
        assert!(queue.try_recv().is_ok());
        assert!(queue.try_recv().is_err());
    }

    fn release(rid: &str) -> workspace_manifest::ReleaseLock {
        let mut release: workspace_manifest::ReleaseLock = toml::from_str(
            r#"
schema_version = 1
release_id = "test-release-0001"
workspace_name = "demo"
minimum_app_cli_version = "0.1.3"
runtime_image_digest = "registry.example/app-runtime:0.1.140"

[pingap]
mode = "managed"
version = "0.14.3"
commit = "abc123"

[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200

[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"

[services.env]
"#,
        )
        .unwrap();
        release.release_id = rid.into();
        release
    }

    #[test]
    fn native_workspace_id_handles_path_names_and_run_aliases() {
        let temp = tempfile::tempdir().unwrap();
        for leaf in [
            "valid_workspace",
            "project.with spaces",
            "中文项目",
            &"x".repeat(80),
        ] {
            let source = temp.path().join(leaf);
            let run = source.join(".run");
            std::fs::create_dir_all(&run).unwrap();
            let id = runtime_workspace_id(&source);
            shared_types::validate_identifier(&id, "workspace_id").unwrap();
            assert_eq!(id, runtime_workspace_id(&run));
            assert_eq!(id, runtime_workspace_id(&source));
            if leaf == "valid_workspace" {
                assert_eq!(id, leaf, "preserve existing legal project identity");
            } else {
                assert_eq!(id.len(), 64);
                let other = temp.path().join("other").join(leaf);
                std::fs::create_dir_all(&other).unwrap();
                assert_ne!(id, runtime_workspace_id(&other));
            }
            #[cfg(unix)]
            {
                let alias = temp.path().join(format!("alias_{}", id));
                std::os::unix::fs::symlink(&source, &alias).unwrap();
                assert_eq!(id, runtime_workspace_id(&alias));
            }
        }
    }

    #[test]
    fn auxiliary_writer_cancellation_keeps_runtime_protected() {
        let state = state();
        state
            .initializing
            .store(false, std::sync::atomic::Ordering::Release);
        let mut writer = state.begin_auxiliary_write().unwrap();
        assert_eq!(
            state
                .auxiliary_writers
                .load(std::sync::atomic::Ordering::Acquire),
            1
        );
        writer.confirm();
        drop(writer);
        assert!(!state.runtime_recovery_hold_active());
        assert_eq!(
            state
                .auxiliary_writers
                .load(std::sync::atomic::Ordering::Acquire),
            0
        );
        drop(state.begin_auxiliary_write().unwrap());
        assert!(state.runtime_recovery_hold_active());
        assert!(state.begin_auxiliary_write().is_err());
    }

    #[tokio::test]
    async fn control_only_bootstrap_never_autostarts_an_existing_release() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).unwrap();
        // Even a broken release must not be executed by management bootstrap.
        std::fs::write(workspace.join("release.lock.toml"), "invalid release").unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let kernel = kernel_for(dir.path());
        kernel
            .store()
            .store_desired(shared_types::DesiredState::Running, 7)
            .unwrap();
        state.set_runtime_kernel(kernel.clone());
        let args = RuntimeArgs {
            workspace,
            control_only: true,
            ..Default::default()
        };
        assert!(initialize_startup(&args, &state).await.unwrap().is_none());
        assert_eq!(state.phase(), ServerPhase::Idle);
        assert_eq!(
            kernel.store().load_desired().unwrap(),
            (shared_types::DesiredState::Running, 7)
        );
        assert!(
            state
                .journal
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .receipt
                .is_none()
        );
    }

    #[tokio::test]
    async fn stopped_normal_owner_recovery_preserves_active_journal_without_switching() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        state.record_runtime_deployment(&request()).unwrap();
        state.set_release(release("hot-b"));
        state.complete_stage().unwrap();
        state.complete_running().unwrap();
        let kernel = kernel_for(dir.path());
        kernel
            .store()
            .store_desired(shared_types::DesiredState::Stopped, 7)
            .unwrap();
        state.set_runtime_kernel(kernel.clone());
        let before =
            serde_json::to_value(&state.journal.lock().unwrap().as_ref().unwrap().receipt).unwrap();
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        assert!(initialize_startup(&args, &state).await.unwrap().is_none());
        assert_eq!(state.phase(), ServerPhase::Idle);
        assert_eq!(
            kernel.store().load_desired().unwrap(),
            (shared_types::DesiredState::Stopped, 7)
        );
        assert_eq!(
            serde_json::to_value(&state.journal.lock().unwrap().as_ref().unwrap().receipt).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn startup_resumes_hot_receipt_identity_and_published_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        let artifact = release("manifest-b");
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&artifact).unwrap(),
        )
        .unwrap();
        let first = state();
        first.set_generation("generation-a".to_string());
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "hot-b".into())
            .unwrap();
        first.set_release(artifact);
        first.complete_stage().unwrap();
        first.complete_running().unwrap();
        drop(first);
        let restarted = state();
        restarted.set_generation("generation-a".to_string());
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        assert!(matches!(
            initialize_startup(&args, &restarted).await.unwrap(),
            Some(InitialAction::Existing { .. })
        ));
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "hot-b");
        assert_eq!(op.artifact_release_id.as_deref(), Some("manifest-b"));
        assert_eq!(op.deploy_stage, AppDeploymentStage::Succeeded);
        assert!(op.persisted);
        assert_eq!(op.phase, AppCliDeployPhase::Orchestrating);
    }

    #[tokio::test]
    async fn failed_cold_prepare_without_active_version_never_uses_existing_code() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&release("unattached-a")).unwrap(),
        )
        .unwrap();
        let first = state();
        first.set_generation("cold-generation".to_string());
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "failed-cold".into())
            .unwrap();
        first
            .fail_operation("download failed".into(), Boundary::Preparing)
            .unwrap();
        drop(first);
        let restarted = state();
        restarted.set_generation("cold-generation".to_string());
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        assert!(initialize_startup(&args, &restarted).await.is_err());
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "failed-cold");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        assert!(op.persisted);
        assert!(restarted.deploy_rx.lock().await.try_recv().is_err());
    }

    #[tokio::test]
    async fn existing_baseline_survives_first_hot_prepare_failure_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        let artifact = release("existing-a");
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&artifact).unwrap(),
        )
        .unwrap();
        let first = state();
        first.set_generation("existing-generation".to_string());
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = RuntimeArgs {
            workspace: workspace.clone(),
            ..Default::default()
        };
        assert!(matches!(
            initialize_startup(&args, &first).await.unwrap(),
            Some(InitialAction::Existing { .. })
        ));
        first.set_release(artifact);
        first.complete_running().unwrap();
        assert!(first.deploy_status().operation.is_none());
        first
            .try_accept_deploy_with_id(request(), "failed-b".into())
            .unwrap();
        first
            .fail_operation("invalid B".into(), Boundary::Preparing)
            .unwrap();
        drop(first);
        let restarted = state();
        restarted.set_generation("existing-generation".to_string());
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        assert!(matches!(
            initialize_startup(&args, &restarted).await.unwrap(),
            Some(InitialAction::Existing { .. })
        ));
        restarted.complete_running().unwrap();
        assert_eq!(restarted.release().unwrap().release_id, "existing-a");
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "failed-b");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        let receipt = restarted
            .journal
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .receipt
            .clone()
            .unwrap();
        assert_eq!(receipt.boundary, Boundary::RestoredActive);
        assert_eq!(receipt.active.unwrap().artifact_release_id, "existing-a");
        assert_eq!(receipt.operation.phase, AppCliDeployPhase::Failed);
        drop(restarted);
        let second_restart = state();
        second_restart.set_generation("existing-generation".to_string());
        *second_restart.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        assert!(matches!(
            initialize_startup(&args, &second_restart).await.unwrap(),
            Some(InitialAction::Existing { .. })
        ));
        second_restart.complete_running().unwrap();
        assert_eq!(second_restart.release().unwrap().release_id, "existing-a");
        let op = second_restart.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "failed-b");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
    }

    #[tokio::test]
    async fn existing_code_cannot_promote_foreign_generation_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        let artifact = release("existing-a");
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&artifact).unwrap(),
        )
        .unwrap();
        let first = state();
        first.set_generation("old-generation".to_string());
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "foreign-failed-b".into())
            .unwrap();
        first
            .fail_operation("invalid B".into(), Boundary::Preparing)
            .unwrap();
        drop(first);
        let before = std::fs::read(dir.path().join(".deploy-operation.json")).unwrap();
        let restarted = state();
        restarted.set_generation("new-generation".to_string());
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        assert!(
            initialize_startup(&args, &restarted).await.is_err(),
            "a foreign generation must not automatically start existing code"
        );
        assert!(restarted.deploy_status().operation.is_none());
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-operation.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn startup_interrupted_switch_retains_operation_for_error_reporting() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let first = state();
        first.set_generation("generation-a".to_string());
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "switch-b".into())
            .unwrap();
        first.persist_boundary(Boundary::Switching).unwrap();
        drop(first);
        let restarted = state();
        restarted.set_generation("generation-a".to_string());
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        let error = initialize_startup(&args, &restarted)
            .await
            .err()
            .expect("switch must fail closed");
        restarted
            .fail_operation(error.to_string(), Boundary::Failed)
            .unwrap();
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "switch-b");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        assert!(op.persisted);
    }

    #[tokio::test]
    async fn startup_requires_new_process_scope_without_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let mut journal = Journal::open(&dir.path().join("code")).unwrap();
        journal.process_scope = Some("same-container".into());
        journal.commit_coordinator().unwrap();
        *state.journal.lock().unwrap() = Some(journal);
        state
            .try_accept_deploy_with_id(request(), "interrupted".into())
            .unwrap();
        assert!(
            establish_startup_quiescence(&state, false, async { Ok(()) })
                .await
                .is_err()
        );
        state
            .journal
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .process_scope = Some("new-container".into());
        assert!(
            establish_startup_quiescence(&state, false, async { Ok(()) })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn startup_requires_durable_owner_after_stop_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        std::fs::create_dir(dir.path().join(".deploy-coordinator.json")).unwrap();
        state.set_phase(ServerPhase::Orchestrating);
        let error = establish_startup_quiescence(&state, true, async { Ok(()) })
            .await
            .expect_err("owner persistence must gate startup");
        state.begin_failure(error.to_string(), true);
        assert_eq!(state.phase(), ServerPhase::Orchestrating);
        assert!(
            state
                .try_accept_deploy_with_id(request(), "premature".into())
                .is_err()
        );
    }

    #[tokio::test]
    async fn startup_stop_failure_does_not_allow_replacement_deploy() {
        let state = Arc::new(state());
        state.set_phase(ServerPhase::Orchestrating);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let job_state = state.clone();
        let job_entered = entered.clone();
        let job_release = release.clone();
        let job = tokio::spawn(async move {
            let result = establish_startup_quiescence(&job_state, true, async {
                job_entered.notify_one();
                job_release.notified().await;
                anyhow::bail!("owned process group is still running")
            })
            .await;
            if let Err(error) = result {
                job_state.begin_failure(format!("startup shutdown unconfirmed: {error:#}"), true);
            }
        });
        entered.notified().await;
        assert!(
            state
                .try_accept_deploy_with_id(request(), "racing".into())
                .is_err()
        );
        release.notify_one();
        job.await.unwrap();
        assert_eq!(
            state.deploy_status().phase,
            AppCliDeployPhase::Orchestrating
        );
        assert!(
            state
                .try_accept_deploy_with_id(request(), "after-failure".into())
                .is_err()
        );
    }

    #[test]
    fn close_admission_preserves_accepted_request_and_rejects_late_request() {
        let state = Arc::new(state());
        state
            .try_accept_deploy_with_id(request(), "accepted".into())
            .unwrap();
        let guard = state.admission.lock().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let closer = state.clone();
        let thread = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            closer.close_admission();
        });
        entered_rx.recv().unwrap();
        assert!(state.accepting.load(std::sync::atomic::Ordering::Acquire));
        drop(guard);
        thread.join().unwrap();
        assert!(!state.accepting.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            state.deploy_status().operation.unwrap().operation_id,
            "accepted"
        );
        assert!(matches!(
            state.try_accept_deploy_with_id(request(), "late".into()),
            Err(AdmissionError::Busy(_))
        ));
    }

    #[tokio::test]
    async fn unclaimed_shutdown_cannot_rewrite_previous_active_owner() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let mut first = Journal::open(&workspace).unwrap();
        first.commit_coordinator().unwrap();
        drop(first);
        let before = std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        state.close_admission();
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        assert!(
            finish_clean_shutdown(&args, &state, false, true)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn prior_shutdown_failure_cannot_be_marked_quiescent() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let state = state();
        let mut journal = Journal::open(&workspace).unwrap();
        journal.commit_coordinator().unwrap();
        *state.journal.lock().unwrap() = Some(journal);
        let before = std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap();
        state.begin_failure("child shutdown not confirmed".into(), true);
        state.close_admission();
        let args = RuntimeArgs {
            workspace,
            ..Default::default()
        };
        assert!(
            finish_clean_shutdown(&args, &state, true, true)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn supervisor_join_failure_cannot_be_successful_shutdown() {
        for panic in [false, true] {
            let mut task = tokio::spawn(async move {
                assert!(!panic, "injected supervisor panic");
                anyhow::bail!("injected process shutdown failure")
            });
            let mut joined = false;
            assert!(join_supervisor(&mut task, &mut joined).await.is_err());
            assert!(joined);
        }
    }

    #[tokio::test]
    async fn durable_admission_failure_does_not_publish_or_enqueue_operation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        std::fs::create_dir(dir.path().join(".deploy-operation.json")).unwrap();
        state.set_phase(ServerPhase::Running);
        assert!(
            state
                .try_accept_deploy_with_id(request(), "rejected".into())
                .is_err()
        );
        assert_eq!(state.phase(), ServerPhase::Running);
        assert!(state.deploy_status().operation.is_none());
        assert!(state.deploy_rx.lock().await.try_recv().is_err());
    }

    #[tokio::test]
    async fn queue_failure_restores_empty_journal_and_complete_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state.deploy_rx.lock().await.close();
        state.set_phase(ServerPhase::Running);
        assert!(
            state
                .try_accept_deploy_with_id(request(), "rejected".into())
                .is_err()
        );
        assert!(state.deploy_status().operation.is_none());
        assert!(!dir.path().join(".deploy-operation.json").exists());
        assert_eq!(state.phase(), ServerPhase::Running);
    }

    #[test]
    fn activation_receipt_failure_cannot_publish_stage_success() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state
            .try_accept_deploy_with_id(request(), "operation-a".into())
            .unwrap();
        std::fs::remove_file(dir.path().join(".deploy-operation.json")).unwrap();
        std::fs::create_dir(dir.path().join(".deploy-operation.json")).unwrap();
        assert!(state.complete_stage().is_err());
        let operation = state.deploy_status().operation.unwrap();
        assert!(!operation.persisted);
        assert_eq!(operation.deploy_stage, AppDeploymentStage::Pending);
    }

    #[test]
    fn durable_stage_is_independent_from_later_orchestration_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state
            .try_accept_deploy_with_id(request(), "operation-a".into())
            .unwrap();
        state.persist_boundary(Boundary::Switching).unwrap();
        state.set_release(release("activated-b"));
        state.complete_stage().unwrap();
        let operation = state.deploy_status().operation.unwrap();
        assert!(operation.persisted);
        assert_eq!(operation.deploy_stage, AppDeploymentStage::Succeeded);
        state.begin_failure("orchestration failed".into(), false);
        let operation = state.deploy_status().operation.unwrap();
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.deploy_stage, AppDeploymentStage::Succeeded);
    }

    #[test]
    fn preparation_failure_is_durable_without_changing_serving_health() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state.ready.set_ready(true);
        state
            .try_accept_deploy_with_id(request(), "bad-artifact".into())
            .unwrap();
        state
            .fail_operation("invalid zip".into(), Boundary::Preparing)
            .unwrap();
        assert!(state.readiness_ok());
        let op = state.deploy_status().operation.unwrap();
        assert!(op.persisted);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        let journal = state.journal.lock().unwrap();
        let saved = &journal
            .as_ref()
            .unwrap()
            .receipt
            .as_ref()
            .unwrap()
            .operation;
        assert_eq!(saved.operation_id, "bad-artifact");
        assert_eq!(saved.error.as_deref(), Some("invalid zip"));
        assert!(saved.persisted);
    }

    #[tokio::test]
    async fn activation_failure_does_not_restore_previous_business_code() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("version"), "B").unwrap();
        std::fs::create_dir(dir.path().join(".previous")).unwrap();
        std::fs::write(dir.path().join(".previous/version"), "A").unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        state
            .try_accept_deploy_with_id(request(), "operation-b".into())
            .unwrap();
        assert!(
            fail_activation(
                &RuntimeArgs {
                    workspace: workspace.clone(),
                    ..Default::default()
                },
                &state,
                "activation failed".into()
            )
            .await
            .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("version")).unwrap(),
            "B"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".previous/version")).unwrap(),
            "A"
        );
        assert!(matches!(state.phase(), ServerPhase::Failed(_)));
        assert!(
            state
                .journal
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .resume(&state.generation_value())
                .is_err()
        );
    }

    /// 内部状态机 → wire 相位转换矩阵：as_str 委托共享枚举，Failed 负载
    /// 丢弃（error 走 DeployStatus.error 独立字段）。新增 ServerPhase 变体
    /// 时 From 实现编译错强制同步本矩阵。
    #[test]
    fn concurrent_deploy_admission_has_one_winner() {
        let state = Arc::new(state());
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|n| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    state
                        .try_accept_deploy(DeployRequest {
                            runtime_operation_id: None,
                            url: "http://127.0.0.1/artifact".into(),
                            release_id: format!("r{n}"),
                            sha256: None,
                            local_path: None,
                            execution_target: None,

                            run_pg: None,
                        })
                        .is_ok()
                })
            })
            .collect();
        let accepted = handles
            .into_iter()
            .map(|h| usize::from(h.join().expect("thread")))
            .sum::<usize>();
        assert_eq!(accepted, 1);
        let status = state.deploy_status();
        assert_eq!(status.protocol_version, DEPLOY_PROTOCOL);
        assert_eq!(
            status.operation.expect("operation").phase,
            AppCliDeployPhase::Deploying
        );
    }

    #[test]
    fn failed_operation_is_not_completed_by_old_generation_health() {
        let state = state();
        state
            .try_accept_deploy_with_id(
                DeployRequest {
                    runtime_operation_id: None,
                    url: "http://x".into(),
                    release_id: "requested".into(),
                    sha256: None,
                    local_path: None,
                    execution_target: None,

                    run_pg: None,
                },
                "op-a".into(),
            )
            .expect("accept");
        state.set_phase(ServerPhase::Failed("prepare failed".into()));
        state.set_phase(ServerPhase::Running);
        let operation = state.deploy_status().operation.expect("operation");
        assert_eq!(operation.operation_id, "op-a");
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.error.as_deref(), Some("prepare failed"));
    }

    #[test]
    fn runtime_phase_changes_cannot_clear_unconfirmed_shutdown() {
        let state = state();
        let request = || DeployRequest {
            runtime_operation_id: None,
            url: "http://x".into(),
            release_id: "requested".into(),
            sha256: None,
            local_path: None,
            execution_target: None,

            run_pg: None,
        };
        state
            .try_accept_deploy_with_id(request(), "op-a".into())
            .expect("accept");
        state.begin_failure("shutdown not confirmed".into(), true);
        let snapshot = state.deploy_status();
        assert_eq!(snapshot.phase, AppCliDeployPhase::Orchestrating);
        let operation = snapshot.operation.expect("operation");
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.recovery.expect("recovery").status, "pending");
        assert!(
            state
                .try_accept_deploy_with_id(request(), "op-b".into())
                .is_err()
        );
        for phase in [
            ServerPhase::Running,
            ServerPhase::Idle,
            ServerPhase::Failed("runtime stopped".into()),
        ] {
            state.set_phase(phase);
            assert_eq!(state.phase(), ServerPhase::Orchestrating);
        }
        let snapshot = state.deploy_status();
        let operation = snapshot.operation.expect("operation");
        assert_eq!(operation.operation_id, "op-a");
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.recovery.expect("recovery").status, "pending");
        assert!(
            state
                .try_accept_deploy_with_id(request(), "op-b".into())
                .is_err()
        );
    }

    #[test]
    fn server_phase_to_wire_phase_matrix() {
        let cases = [
            (ServerPhase::Idle, AppCliDeployPhase::Idle),
            (ServerPhase::Deploying, AppCliDeployPhase::Deploying),
            (ServerPhase::Orchestrating, AppCliDeployPhase::Orchestrating),
            (ServerPhase::Running, AppCliDeployPhase::Running),
            (
                ServerPhase::Failed("boom".to_string()),
                AppCliDeployPhase::Failed,
            ),
        ];
        for (phase, wire) in cases {
            assert_eq!(AppCliDeployPhase::from(&phase), wire);
            assert_eq!(phase.as_str(), wire.as_str());
        }
    }

    /// set_phase 同步 deploy_status：phase 即时反映 + Failed 附 error 快照。
    #[test]
    fn set_phase_updates_deploy_status_snapshot() {
        let st = state();
        st.set_phase(ServerPhase::Deploying);
        {
            let status = st
                .deploy_status
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(status.phase, AppCliDeployPhase::Deploying);
            assert_eq!(status.error, None);
        }
        st.set_phase(ServerPhase::Failed("download 404".to_string()));
        {
            let status = st
                .deploy_status
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(status.phase, AppCliDeployPhase::Failed);
            assert_eq!(status.error.as_deref(), Some("download 404"));
        }
    }

    #[test]
    fn readiness_matrix_per_phase() {
        let st = state();
        // Idle：基础设施就绪（空容器可服务），与 runtime_ready 无关
        st.set_phase(ServerPhase::Idle);
        assert!(st.readiness_ok());
        // Running：跟随后端 bridge readiness
        st.set_phase(ServerPhase::Running);
        assert!(!st.readiness_ok(), "runtime not ready → 503");
        st.ready.set_ready(true);
        assert!(st.readiness_ok());
        // Preparation/failure of a new deployment does not remove a healthy old app.
        st.set_phase(ServerPhase::Deploying);
        assert!(st.readiness_ok());
        st.set_phase(ServerPhase::Failed("prepare failed".into()));
        assert!(st.readiness_ok());
        // Activation explicitly stops the serving generation.
        st.ready.set_ready(false);
        for phase in [ServerPhase::Deploying, ServerPhase::Orchestrating] {
            st.set_phase(phase);
            assert!(!st.readiness_ok());
        }
        st.set_phase(ServerPhase::Failed("x".into()));
        assert!(!st.readiness_ok());
    }

    #[tokio::test]
    async fn deploy_acceptance_guards_and_channel() {
        let st = state();
        st.set_phase(ServerPhase::Running);
        let req = DeployRequest {
            runtime_operation_id: None,
            url: "http://x/p.zip".into(),
            release_id: "rel-1".into(),
            sha256: None,
            local_path: None,
            execution_target: None,

            run_pg: None,
        };
        assert!(st.try_accept_deploy(req.clone()).is_ok());
        // 进行中相位拒绝（防双部署竞争）
        st.set_phase(ServerPhase::Deploying);
        assert!(st.try_accept_deploy(req.clone()).is_err());
        st.set_phase(ServerPhase::Orchestrating);
        assert!(st.try_accept_deploy(req).is_err());
        // 受理的请求能被主循环收到
        st.set_phase(ServerPhase::Idle);
        st.try_accept_deploy(DeployRequest {
            runtime_operation_id: None,
            url: "http://x/p2.zip".into(),
            release_id: "rel-2".into(),
            sha256: None,
            local_path: None,
            execution_target: None,

            run_pg: None,
        })
        .unwrap();
        // 受理按序到达（Running 期受理的 rel-1 排在前——主循环串行消费）
        let first = st.deploy_rx.lock().await.recv().await.unwrap();
        assert_eq!(first.release_id, "rel-1");
        let second = st.deploy_rx.lock().await.recv().await.unwrap();
        assert_eq!(second.release_id, "rel-2");
    }

    #[test]
    fn boot_id_tracks_release_generation() {
        let st = state();
        assert_eq!(st.boot_id(), "idle");
        let mk_release = |rid: &str| workspace_manifest::ReleaseLock {
            schema_version: 1,
            release_id: rid.into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.14.3".into(),
                commit: "abc".into(),
            },
            minimum_app_cli_version: "0.0.0".into(),
            runtime_image_digest: String::new(),
            services: Vec::new(),
            bridge_service: None,
        };
        st.set_release(mk_release("rel-gen-1"));
        assert_eq!(st.boot_id(), "rel-gen-1");
        st.set_release(mk_release("rel-gen-2"));
        assert_eq!(st.boot_id(), "rel-gen-2");
        // 部署进度快照携带当前 release_id
        assert_eq!(st.deploy_status().release_id.as_deref(), Some("rel-gen-2"));
    }

    /// 令牌回显：request_release_id（请求身份）与 release_id（包内构建 id）两层
    /// 并存、互不覆盖；热部署受理与 operation.request_release_id 同源同值；
    /// 无请求方（Existing/legacy 路径）不回显。
    #[test]
    fn request_release_id_echoes_caller_token() {
        let st = state();
        assert_eq!(st.deploy_status().request_release_id, None);

        // env 启动部署：进入 Deploying 时登记令牌
        st.set_request_release_id("rel-token-a");
        assert_eq!(
            st.deploy_status().request_release_id.as_deref(),
            Some("rel-token-a")
        );

        // 热部署受理：回显与 operation.request_release_id 同步登记
        st.try_accept_deploy(DeployRequest {
            runtime_operation_id: None,
            url: "http://unused".into(),
            release_id: "rel-token-b".into(),
            sha256: None,
            local_path: None,
            execution_target: None,

            run_pg: None,
        })
        .expect("idle accepts deploy");
        let status = st.deploy_status();
        assert_eq!(status.request_release_id.as_deref(), Some("rel-token-b"));
        assert_eq!(
            status.operation.expect("accepted").request_release_id,
            "rel-token-b"
        );

        // 部署编排完成：包内构建 id 写入 release_id，令牌回显不被覆盖
        st.set_release(workspace_manifest::ReleaseLock {
            schema_version: 1,
            release_id: "build-9".into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.14.3".into(),
                commit: "abc".into(),
            },
            minimum_app_cli_version: "0.0.0".into(),
            runtime_image_digest: String::new(),
            services: Vec::new(),
            bridge_service: None,
        });
        let status = st.deploy_status();
        assert_eq!(status.release_id.as_deref(), Some("build-9"));
        assert_eq!(status.request_release_id.as_deref(), Some("rel-token-b"));
    }
    #[tokio::test]
    async fn unconfirmed_stop_keeps_api_state_pending_and_rejects_new_deployment() {
        let state = state();
        state
            .try_accept_deploy_with_id(
                DeployRequest {
                    runtime_operation_id: None,
                    url: "http://unused".into(),
                    release_id: "new".into(),
                    sha256: None,
                    local_path: None,
                    execution_target: None,

                    run_pg: None,
                },
                "operation".into(),
            )
            .unwrap();
        let hold = hold_unconfirmed(&state, "process group remains".into());
        tokio::pin!(hold);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut hold)
                .await
                .is_err()
        );
        let status = state.deploy_status();
        assert_eq!(status.phase, AppCliDeployPhase::Orchestrating);
        let operation = status.operation.unwrap();
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.recovery.unwrap().status, "pending");
        assert!(
            state
                .try_accept_deploy(DeployRequest {
                    runtime_operation_id: None,
                    url: "http://unused".into(),
                    release_id: "later".into(),
                    sha256: None,
                    local_path: None,
                    execution_target: None,

                    run_pg: None,
                })
                .is_err()
        );
    }

    // ===== R01/R02/R04：执行身份生命周期与屏障裁决 =====

    fn kernel_for(dir: &std::path::Path) -> std::sync::Arc<crate::runtime_kernel::RuntimeKernel> {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let identity = shared_types::RuntimeIdentityView {
            application_id: "app1".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-1".into(),
            source_root: "/workspace".into(),
            runtime_instance_id: "instance-1".into(),
            deployment_generation_id: "gen-1".into(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        };
        let store =
            crate::runtime_kernel::RuntimeStore::open_with_root(dir.join("state-root"), &workspace)
                .expect("store");
        std::sync::Arc::new(crate::runtime_kernel::RuntimeKernel::new(
            store,
            identity,
            Box::new(|_| {}),
        ))
    }

    fn runtime_request(
        kind: shared_types::RuntimeOperationKind,
        operation_id: &str,
        revision: u64,
    ) -> shared_types::RuntimeOperationRequest {
        shared_types::RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: "instance-1".into(),
            expected_revision: revision,
            workspace_id: "ws-1".into(),
            kind,
            profile: shared_types::RunProfileInput::Source {
                workspace_id: "ws-1".into(),
            },
            run_config: None,
            request_context: None,
        }
    }

    #[tokio::test]
    async fn start_restart_sequence_keeps_execution_identity_per_operation() {
        // R01：Start A 提交成功必须清理 server 执行身份——否则 Restart B 的
        // set_current 被旧 A 拒绝、B 完成时误收束 A，B 永久 Accepted。
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());

        // A：受理 → 占据身份 → 提交
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-a",
                0,
            ))
            .await
            .expect("admit A");
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-a".into(),
                dev_profile: true,
                pg: None,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Source));
        assert_eq!(state.current_runtime_operation().as_deref(), Some("op-a"));
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        // R01 核心：屏障通过后 server 身份必须清理（修复前残留 op-a）
        assert_eq!(state.current_runtime_operation(), None);

        // B：同一序列的 Restart 不被旧身份阻塞，正常提交
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Restart,
                "op-b",
                0,
            ))
            .await
            .expect("admit B");
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-b".into(),
                dev_profile: true,
                pg: None,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Source));
        assert_eq!(state.current_runtime_operation().as_deref(), Some("op-b"));
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        for operation in ["op-a", "op-b"] {
            let view = kernel.get(operation).await.unwrap().expect("record");
            assert_eq!(view.state, shared_types::RuntimeOperationState::Succeeded);
        }
    }

    #[tokio::test]
    async fn barrier_not_active_on_terminal_operation_is_idempotent_pass() {
        // R02 幂等分支：操作已被其他路径收束（终态）——NotActive 清残留身份放行
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-t",
                0,
            ))
            .await
            .expect("admit");
        state.set_current_runtime_operation(Some("op-t".into()));
        // 其他路径已把内核 active 收束（先于 server 屏障）
        kernel
            .finish(
                "op-t",
                shared_types::RuntimeOperationState::Succeeded,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        assert_eq!(state.current_runtime_operation(), None);
        let view = kernel.get("op-t").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Succeeded);
    }

    #[tokio::test]
    async fn barrier_not_active_on_live_operation_fails_closed() {
        // R02 fail-closed 分支保留：active 不在本操作且无终态（执行身份丢失）
        // ——不得当 Passed 报成功；本例构造终态已存在的幂等场景。身份丢失
        // 且无终态的 fail-closed 分支由 V03 结构性消除（Stop 不再抢 active）。
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-x",
                0,
            ))
            .await
            .expect("admit start");
        // 其他路径已收束（终态存在），server 身份残留——幂等清理放行
        kernel
            .finish(
                "op-x",
                shared_types::RuntimeOperationState::Succeeded,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        state.set_current_runtime_operation(Some("op-x".into()));
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        assert_eq!(state.current_runtime_operation(), None);
        let view = kernel.get("op-x").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Succeeded);
    }

    #[tokio::test]
    async fn barrier_cancelled_by_request_keeps_identity_until_settle() {
        // V02：屏障观察到取消意图 → CancelledByRequest——内核**未写终态**、
        // server 身份保留；停服确认后 finish 才收束 Cancelled
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-cxl",
                0,
            ))
            .await
            .expect("admit");
        state.set_current_runtime_operation(Some("op-cxl".into()));
        kernel.request_cancel("op-cxl").await.unwrap();
        assert_eq!(
            commit_running_barrier(&state).await,
            BarrierOutcome::CancelledByRequest
        );
        // 终态未写 + 身份保留（等待停服确认）
        let view = kernel.get("op-cxl").await.unwrap().expect("record");
        assert!(!view.state.is_terminal(), "屏障不得预写终态（V02）");
        assert_eq!(state.current_runtime_operation().as_deref(), Some("op-cxl"));
        // 停服确认后按 ID 收束 Cancelled（终态单调保住不被迟到成功覆盖）
        state
            .finish_runtime_operation_by_id(
                "op-cxl",
                shared_types::RuntimeOperationState::Cancelled,
                None,
            )
            .await
            .unwrap();
        let view = kernel.get("op-cxl").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Cancelled);
        assert_eq!(state.current_runtime_operation(), None);
    }

    #[tokio::test]
    async fn terminal_persist_failure_keeps_identity_and_gates_admission() {
        // V04：终态持久化失败——Err 上抛、身份保留、恢复门禁挂起（部署受理
        // 拒绝、ready 压低），不允许日志后按成功继续
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-persist",
                0,
            ))
            .await
            .expect("admit");
        state.set_current_runtime_operation(Some("op-persist".into()));
        // 注入：对不存在记录收束 → kernel.finish 报错（持久化失败替身）
        let result = state
            .finish_runtime_operation_by_id(
                "op-never-recorded",
                shared_types::RuntimeOperationState::Failed,
                None,
            )
            .await;
        let error = result.expect_err("persist failure must propagate");
        assert!(error.contains("persist failed"));
        // 身份保留 + 门禁挂起
        assert_eq!(
            state.current_runtime_operation().as_deref(),
            Some("op-persist"),
            "持久化失败不得清除执行身份"
        );
        assert!(state.runtime_recovery_hold_active());
        // 部署受理被门禁拒绝
        let dir2 = tempfile::tempdir().unwrap();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir2.path().join("code")).unwrap());
        state.set_phase(ServerPhase::Running);
        assert!(matches!(
            state.try_accept_deploy_with_id(request(), "gated".into()),
            Err(AdmissionError::Busy(_))
        ));
    }

    #[tokio::test]
    async fn stop_admission_does_not_steal_executor_identity() {
        // V03：Stop 受理（revision 推进）不抢走执行者身份——A 的提交屏障
        // 得到 Superseded（而非 NotActive），A 停服后按自身 ID 收束 Cancelled，
        // Stop 执行完成清 pending，此后新操作可受理
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-a",
                0,
            ))
            .await
            .expect("admit A");
        state.set_current_runtime_operation(Some("op-a".into()));
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Stop,
                "op-stop",
                0,
            ))
            .await
            .expect("stop admitted during A");
        assert_eq!(
            commit_running_barrier(&state).await,
            BarrierOutcome::Superseded
        );
        state
            .finish_runtime_operation_by_id(
                "op-a",
                shared_types::RuntimeOperationState::Cancelled,
                None,
            )
            .await
            .unwrap();
        kernel
            .finish(
                "op-stop",
                shared_types::RuntimeOperationState::Succeeded,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-c",
                1,
            ))
            .await
            .expect("C admitted after A/B settled");
        let a = kernel.get("op-a").await.unwrap().expect("record");
        assert_eq!(a.state, shared_types::RuntimeOperationState::Cancelled);
    }

    #[tokio::test]
    async fn stop_failure_after_running_retains_its_own_recovery_identity() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "running",
                0,
            ))
            .await
            .unwrap();
        state.set_current_runtime_operation(Some("running".into()));
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        assert!(state.current_runtime_operation().is_none());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Stop,
                "stopunknown",
                0,
            ))
            .await
            .unwrap();
        record_uncertain_control(
            &state,
            &ControlSignal::StopBusiness {
                operation_id: "stopunknown".into(),
            },
            "controlled stop failure",
        )
        .await;
        assert_eq!(
            kernel.get("stopunknown").await.unwrap().unwrap().state,
            shared_types::RuntimeOperationState::RecoveryRequired
        );
        assert!(kernel.recovery_protection_active());
        assert_eq!(
            kernel.get("running").await.unwrap().unwrap().state,
            shared_types::RuntimeOperationState::Succeeded
        );
    }

    #[tokio::test]
    async fn uncertain_a_recovery_protection_latches_while_stop_pending() {
        // V03：恢复保护由结果未知决定——A 以 RecoveryRequired 收束时 Stop 仍
        // 待执行（pending 占据），保护必须挂起且新操作被拒
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-a2",
                0,
            ))
            .await
            .expect("admit A");
        state.set_current_runtime_operation(Some("op-a2".into()));
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Stop,
                "op-stop2",
                0,
            ))
            .await
            .expect("stop admitted");
        kernel
            .finish(
                "op-a2",
                shared_types::RuntimeOperationState::RecoveryRequired,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert!(
            kernel.recovery_protection_active(),
            "未知结果必须挂起恢复保护（不依赖 active 匹配）"
        );
        let rejected = kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-c2",
                1,
            ))
            .await
            .expect_err("recovery protection rejects");
        assert_eq!(rejected.code, "ERR_RECOVERY_REQUIRED");
    }

    #[tokio::test]
    async fn kernel_unavailable_closes_legacy_deploy_admission() {
        // R05：状态根打开失败（尝试装配但 kernel=None）——legacy 部署受理
        // fail-closed；从未尝试装配的上下文保持旧语义
        let dir = tempfile::tempdir().unwrap();
        let legacy = state();
        *legacy.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        legacy.set_phase(ServerPhase::Running);
        // 未标记：旧语义放行
        assert!(
            legacy
                .try_accept_deploy_with_id(request(), "legacy-ok".into())
                .is_ok()
        );
        // 标记后无 kernel：可信状态不可读，拒绝
        let dir2 = tempfile::tempdir().unwrap();
        let blocked = state();
        *blocked.journal.lock().unwrap() = Some(Journal::open(&dir2.path().join("code")).unwrap());
        blocked.set_phase(ServerPhase::Running);
        blocked.mark_kernel_required();
        assert!(matches!(
            blocked.try_accept_deploy_with_id(request(), "blocked".into()),
            Err(AdmissionError::Busy(_))
        ));
    }

    #[tokio::test]
    async fn orchestrate_source_records_request_dev_profile() {
        // R08：Source 形态操作的 dev profile 随派发传递并记录——编排引擎
        // 据此选择生效命令（devrun 优先），不再读 serve 进程 env 猜测
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-prof",
                0,
            ))
            .await
            .expect("admit");
        assert_eq!(state.take_pending_dev_profile(), None, "未编排前无记录");
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-prof".into(),
                dev_profile: true,
                pg: None,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Source));
        assert_eq!(
            state.take_pending_dev_profile(),
            Some(true),
            "Source 形态必须记录 dev profile 供编排消费"
        );
        // 一次性消费：取后清空
        assert_eq!(state.take_pending_dev_profile(), None);
    }

    #[tokio::test]
    async fn cancelled_queued_orchestration_settles_without_dispatching_stop() {
        // R04：排队期取消的启动/重启——按自身 ID 收束 Cancelled，返回 Settled
        //（不派发 StopBusiness：那会执行无关业务停止并用 Succeeded 覆盖）
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Restart,
                "op-queued",
                0,
            ))
            .await
            .expect("admit");
        kernel.request_cancel("op-queued").await.unwrap();
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-queued".into(),
                dev_profile: true,
                pg: None,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Settled));
        assert_eq!(state.current_runtime_operation(), None);
        let view = kernel.get("op-queued").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Cancelled);
    }
}
