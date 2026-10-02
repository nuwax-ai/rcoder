use super::*;

#[cfg(test)]
mod cases {
    use super::*;
    use shared_types::{RunProfileInput, RuntimeOperationState};
    use std::sync::{Arc, Mutex};

    fn manager(root: &Path) -> DevServerManager {
        let mut config = crate::Config::from_env().unwrap();
        config.log_base_dir = root.to_path_buf();
        DevServerManager::new(Arc::new(config))
    }
    fn request() -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: "original-operation".into(),
            expected_runtime_instance_id: "instance".into(),
            expected_revision: 7,
            workspace_id: "workspace".into(),
            kind: RuntimeOperationKind::Restart,
            profile: RunProfileInput::Source {
                workspace_id: "workspace".into(),
            },
            run_config: None,
            request_context: Some("build-task-one".into()),
        }
    }
    fn owner(address: &str) -> ExternalOwner {
        ExternalOwner {
            address: address.into(),
            token: "private-owner-token".into(),
            runtime_instance_id: "instance".into(),
        }
    }
    #[test]
    fn successor_retirement_keeps_history_and_cannot_erase_new_registration() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let request = request();
        let old = owner("127.0.0.1:1");
        manager
            .prepare_external_intent("project", dir.path(), &old, &request)
            .unwrap();
        let snapshot = manager.read_external_state().unwrap();
        manager
            .retire_replaced_registration("project", &snapshot, "successor")
            .unwrap();
        let retired = manager.read_external_state().unwrap();
        assert!(retired.owners.is_empty());
        assert!(retired.intents.is_empty());
        assert_eq!(
            retired.completed["project|original-operation"]
                .request
                .operation_id,
            request.operation_id
        );
        assert_eq!(retired.retired.len(), 1);
        assert!(!manager.processes.lock().unwrap().contains_key("project"));
        let mut next = request.clone();
        next.expected_runtime_instance_id = "successor".into();
        next.operation_id = "next".into();
        let mut successor = old;
        successor.runtime_instance_id = "successor".into();
        manager
            .prepare_external_intent("project", dir.path(), &successor, &next)
            .unwrap();
        assert!(
            manager
                .retire_replaced_registration("project", &snapshot, "successor")
                .is_err()
        );
        assert_eq!(
            manager.read_external_state().unwrap().owners["project"]
                .registration_operation_id
                .as_deref(),
            Some("next")
        );
    }

    #[test]
    fn new_build_replaces_transport_intent_and_late_completion_preserves_it() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let original = request();
        let owner = owner("127.0.0.1:1");
        manager
            .prepare_external_intent("project", dir.path(), &owner, &original)
            .unwrap();
        // Retrying the same task keeps its immutable request, including revision.
        let mut retry = original.clone();
        retry.operation_id = "discarded-new-id".into();
        retry.expected_revision = 100;
        let resumed = manager
            .prepare_external_intent("project", dir.path(), &owner, &retry)
            .unwrap();
        assert_eq!(resumed.request.operation_id, original.operation_id);
        assert_eq!(
            resumed.request.expected_revision,
            original.expected_revision
        );
        for (id, context) in [
            ("next-task", Some("another-task".into())),
            ("manual-start", None),
        ] {
            let mut next = original.clone();
            next.operation_id = id.into();
            next.request_context = context;
            let intent = manager
                .prepare_external_intent("project", dir.path(), &owner, &next)
                .unwrap();
            assert_eq!(intent.request.operation_id, id);
            manager
                .finish_external_intent("project", &original, true)
                .unwrap();
            let state = manager.read_external_state().unwrap();
            assert_eq!(
                state.intents[&key("project", next.kind)]
                    .request
                    .operation_id,
                id
            );
            assert_eq!(
                state.owners["project"].registration_operation_id.as_deref(),
                Some(id)
            );
            manager.ensure_new_build_admissible("project").unwrap();
        }
    }

    #[test]
    fn corrupt_transport_cache_is_preserved_and_rebuilt() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let path = manager.external_state_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"{truncated").unwrap();
        manager
            .prepare_external_intent("project", dir.path(), &owner("127.0.0.1:1"), &request())
            .unwrap();
        assert!(
            manager
                .read_external_state()
                .unwrap()
                .owners
                .contains_key("project")
        );
        let backups: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(std::fs::read(&backups[0]).unwrap(), b"{truncated");
    }

    #[test]
    fn stale_stop_completion_preserves_new_registration_on_same_instance() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let owner = owner("127.0.0.1:1");
        let mut stop = request();
        stop.kind = RuntimeOperationKind::Stop;
        stop.operation_id = "old-stop".into();
        manager
            .prepare_external_intent("project", dir.path(), &owner, &stop)
            .unwrap();
        let mut restart = request();
        restart.operation_id = "new-restart".into();
        manager
            .prepare_external_intent("project", dir.path(), &owner, &restart)
            .unwrap();
        manager
            .finish_external_intent("project", &restart, false)
            .unwrap();
        manager
            .finish_external_intent("project", &stop, true)
            .unwrap();
        assert_eq!(
            manager.read_external_state().unwrap().owners["project"]
                .registration_operation_id
                .as_deref(),
            Some("new-restart")
        );
        assert!(manager.processes.lock().unwrap().contains_key("project"));
        // Re-observing old completion also cannot clear newer registry state.
        manager
            .finish_external_intent("project", &stop, true)
            .unwrap();
        assert!(manager.processes.lock().unwrap().contains_key("project"));
        stop.operation_id = "current-stop".into();
        manager
            .prepare_external_intent("project", dir.path(), &owner, &stop)
            .unwrap();
        manager
            .finish_external_intent("project", &stop, true)
            .unwrap();
        assert!(
            !manager
                .read_external_state()
                .unwrap()
                .owners
                .contains_key("project")
        );
        assert!(!manager.processes.lock().unwrap().contains_key("project"));
    }

    #[tokio::test]
    async fn restarted_owner_only_closes_matching_terminal_history_without_replaying() {
        for scenario in [
            "succeeded",
            "failed",
            "cancelled",
            "missing",
            "unknown",
            "wrong_instance",
            "wrong_kind",
            "wrong_digest",
            "wrong_workspace",
            "new_registration",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            let application = std::env::var("PROJECT_ID")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            let token_root = dir.path().join(".app-cli-state").join(&application);
            std::fs::create_dir_all(&token_root).unwrap();
            std::fs::write(token_root.join("token"), "receipt-token").unwrap();
            let original = request();
            let mut receipt = RuntimeOperationView {
                operation_id: original.operation_id.clone(),
                kind: original.kind,
                state: RuntimeOperationState::Succeeded,
                request_digest: shared_types::runtime_request_digest(&original).unwrap(),
                revision: 8,
                runtime_instance_id: original.expected_runtime_instance_id.clone(),
                error_code: None,
                error_message: None,
                failure_detail: None,
            };
            match scenario {
                "failed" => receipt.state = RuntimeOperationState::Failed,
                "cancelled" => receipt.state = RuntimeOperationState::Cancelled,
                "unknown" => receipt.state = RuntimeOperationState::RecoveryRequired,
                "wrong_instance" => receipt.runtime_instance_id = "unrelated-owner".into(),
                "wrong_kind" => receipt.kind = RuntimeOperationKind::Stop,
                "wrong_digest" => receipt.request_digest = "wrong".into(),
                _ => {}
            }
            let identity = shared_types::RuntimeIdentityView {
                application_id: application,
                service_family: "userapp-dev".into(),
                workspace_id: if scenario == "wrong_workspace" {
                    "other-workspace"
                } else {
                    "workspace"
                }
                .into(),
                source_root: workspace.to_string_lossy().into(),
                runtime_instance_id: "replacement-owner".into(),
                deployment_generation_id: "generation".into(),
                protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
                capabilities: vec![],
            };
            let posts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let posted = posts.clone();
            let server = axum::Router::new()
                .route(
                    "/v1/runtime/identity",
                    axum::routing::get(move || {
                        let identity = identity.clone();
                        async move {
                            axum::Json(
                                serde_json::json!({"code":"0000","message":"ok","data":identity}),
                            )
                        }
                    }),
                )
                .route(
                    "/v1/runtime/operations/{id}",
                    axum::routing::get(move |headers: axum::http::HeaderMap| {
                        let receipt = receipt.clone();
                        async move {
                            assert_eq!(headers["x-deploy-token"], "receipt-token");
                            if scenario == "missing" {
                                return (
                                    axum::http::StatusCode::NOT_FOUND,
                                    axum::Json(serde_json::json!({})),
                                );
                            }
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({"data": receipt})),
                            )
                        }
                    }),
                )
                .route(
                    "/v1/runtime/operations",
                    axum::routing::post(move || {
                        let posted = posted.clone();
                        async move {
                            posted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(async move {
                axum::serve(listener, server).await.unwrap();
            });
            let manager = manager(dir.path());
            manager
                .prepare_external_intent("project", &workspace, &owner(&address), &original)
                .unwrap();
            if scenario == "new_registration" {
                // Another admitted operation has already registered the replacement instance.
                let mut stop = original.clone();
                stop.operation_id = "new-stop".into();
                stop.kind = RuntimeOperationKind::Stop;
                stop.expected_runtime_instance_id = "replacement-owner".into();
                let mut replacement = owner(&address);
                replacement.runtime_instance_id = "replacement-owner".into();
                manager
                    .prepare_external_intent("project", &workspace, &replacement, &stop)
                    .unwrap();
            }
            let result = manager
                .recover_external_operation("project", &workspace, &original.operation_id, None)
                .await;
            let success = matches!(
                scenario,
                "succeeded" | "failed" | "cancelled" | "new_registration"
            );
            assert_eq!(result.is_ok(), success, "scenario {scenario}: {result:?}");
            assert_eq!(
                posts.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "old writes must never be replayed"
            );
            let state = manager.read_external_state().unwrap();
            assert_eq!(
                state.intents.contains_key(&key("project", original.kind)),
                !success
            );
            if scenario == "new_registration" {
                assert_eq!(
                    state.owners["project"].registration_operation_id.as_deref(),
                    Some("new-stop")
                );
                assert_eq!(
                    manager.processes.lock().unwrap()["project"]
                        .external_owner
                        .as_ref()
                        .unwrap()
                        .runtime_instance_id,
                    "replacement-owner"
                );
            } else if success {
                assert!(!state.owners.contains_key("project"));
                assert!(!manager.processes.lock().unwrap().contains_key("project"));
                manager.ensure_new_build_admissible("project").unwrap();
                // Re-querying the completed receipt remains read-only and idempotent.
                assert!(
                    manager
                        .recover_external_operation(
                            "project",
                            &workspace,
                            &original.operation_id,
                            None
                        )
                        .await
                        .is_ok()
                );
            } else {
                // History remains unresolved, but it cannot veto a fresh build.
                manager.ensure_new_build_admissible("project").unwrap();
                assert_eq!(
                    state.intents[&key("project", original.kind)]
                        .request
                        .operation_id,
                    original.operation_id
                );
            }
            server.abort();
        }
    }

    #[derive(Default)]
    struct Wire {
        posts: Vec<RuntimeOperationRequest>,
        committed: bool,
    }
    async fn mock(
        root: std::path::PathBuf,
        fail_first: bool,
        accept_before_failure: bool,
    ) -> (String, Arc<Mutex<Wire>>, tokio::task::JoinHandle<()>) {
        let wire = Arc::new(Mutex::new(Wire::default()));
        let posted = wire.clone();
        let queried = wire.clone();
        let router = axum::Router::new()
            .route("/v1/runtime/operations", axum::routing::post(move |axum::Json(req): axum::Json<RuntimeOperationRequest>| {
                let posted = posted.clone(); let root = root.clone();
                async move {
                    let disk = read(&root.join("dev-server-external.json")).unwrap();
                    assert!(disk.intents.values().any(|intent| intent.request.operation_id == req.operation_id), "intent must exist before POST");
                    let mut wire = posted.lock().unwrap();
                    wire.posts.push(req.clone());
                    if fail_first && wire.posts.len() == 1 {
                        wire.committed = accept_before_failure;
                        return (axum::http::StatusCode::OK, "truncated response".to_string());
                    }
                    wire.committed = true;
                    (axum::http::StatusCode::ACCEPTED, serde_json::json!({"data": shared_types::RuntimeOperationAccepted {
                        operation_id: req.operation_id.clone(), state: RuntimeOperationState::Accepted,
                        poll: format!("/v1/runtime/operations/{}", req.operation_id),
                    }}).to_string())
                }
            }))
            .route("/v1/runtime/operations/{id}", axum::routing::get(move |axum::extract::Path(id): axum::extract::Path<String>| {
                let queried = queried.clone();
                async move {
                    let wire = queried.lock().unwrap();
                    if !wire.committed { return (axum::http::StatusCode::NOT_FOUND, axum::Json(serde_json::json!({}))); }
                    (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"data": RuntimeOperationView {
                        operation_id: id, kind: RuntimeOperationKind::Restart, state: RuntimeOperationState::Succeeded,
                        request_digest: "digest".into(), revision: 8, runtime_instance_id: "instance".into(),
                        error_code: None, error_message: None, failure_detail: None,
                    }})))
                }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (address, wire, task)
    }

    #[tokio::test]
    async fn lost_reply_then_restart_queries_original_or_replays_identical_request() {
        for accepted in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let (address, wire, server) = mock(dir.path().to_path_buf(), true, accepted).await;
            let external = owner(&address);
            let client = OwnerClient::new(&address, &external.token).unwrap();
            let first = manager(dir.path());
            let original = request();
            let intent = first
                .prepare_external_intent("project", dir.path(), &external, &original)
                .unwrap();
            assert!(
                first
                    .resume_external_intent("project", &client, &intent, &original)
                    .await
                    .is_err()
            );
            drop(first);
            let restarted = manager(dir.path());
            let mut later = original.clone();
            later.operation_id = "must-not-be-used".into();
            later.expected_revision = 99;
            let recovered = restarted
                .prepare_external_intent("project", dir.path(), &external, &later)
                .unwrap();
            let view = restarted
                .resume_external_intent("project", &client, &recovered, &later)
                .await
                .unwrap();
            assert_eq!(view.operation_id, original.operation_id);
            let posts = &wire.lock().unwrap().posts;
            assert_eq!(posts.len(), if accepted { 1 } else { 2 });
            for post in posts {
                assert_eq!(
                    serde_json::to_value(post).unwrap(),
                    serde_json::to_value(&original).unwrap()
                );
            }
            restarted
                .finish_external_intent("project", &recovered.request, false)
                .unwrap();
            assert!(restarted.read_external_state().unwrap().intents.is_empty());
            server.abort();
        }
    }

    #[tokio::test]
    async fn private_configuration_is_not_persisted_and_replay_requires_original() {
        let dir = tempfile::tempdir().unwrap();
        let (address, wire, server) = mock(dir.path().to_path_buf(), false, false).await;
        let external = owner(&address);
        let mut original = request();
        original.run_config = Some(shared_types::OperationRunConfig {
            pg: Some(shared_types::StartPgCredential {
                username: "private-runtime-user".into(),
                password: "private-runtime-password".into(),
            }),
        });
        let first = manager(dir.path());
        first
            .prepare_external_intent("project", dir.path(), &external, &original)
            .unwrap();
        let disk = std::fs::read_to_string(first.external_state_path()).unwrap();
        assert!(!disk.contains("private-runtime"));
        assert!(!disk.contains(&external.token));
        drop(first);
        let restarted = manager(dir.path());
        let mut missing = original.clone();
        missing.run_config = None;
        let intent = restarted
            .prepare_external_intent("project", dir.path(), &external, &missing)
            .unwrap();
        let client = OwnerClient::new(&address, &external.token).unwrap();
        assert!(
            restarted
                .resume_external_intent("project", &client, &intent, &missing)
                .await
                .is_err()
        );
        let mut wrong = original.clone();
        wrong
            .run_config
            .as_mut()
            .unwrap()
            .pg
            .as_mut()
            .unwrap()
            .password = "wrong".into();
        assert!(
            restarted
                .resume_external_intent("project", &client, &intent, &wrong)
                .await
                .is_err()
        );
        assert_eq!(wire.lock().unwrap().posts.len(), 0);
        restarted
            .resume_external_intent("project", &client, &intent, &original)
            .await
            .unwrap();
        assert_eq!(wire.lock().unwrap().posts.len(), 1);
        assert!(
            restarted
                .resume_external_intent("project", &client, &intent, &wrong)
                .await
                .is_err(),
            "different newly supplied config must be rejected even after owner committed"
        );
        server.abort();
    }

    #[tokio::test]
    async fn only_verified_preadmission_rejection_releases_local_intent() {
        for (code, cleared) in [
            (shared_types::ERR_REVISION_MISMATCH, true),
            (shared_types::ERR_OPERATION_IN_PROGRESS, true),
            (shared_types::ERR_OPERATION_ID_CONFLICT, false),
            (shared_types::ERR_RECOVERY_REQUIRED, false),
            ("ERR_BACKEND_ERROR", false),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let router = axum::Router::new()
                .route(
                    "/v1/runtime/operations/{id}",
                    axum::routing::get(|| async { axum::http::StatusCode::NOT_FOUND }),
                )
                .route(
                    "/v1/runtime/operations",
                    axum::routing::post(move || async move {
                        (
                            axum::http::StatusCode::CONFLICT,
                            axum::Json(serde_json::json!({"code": code, "message": "rejected"})),
                        )
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let server = tokio::spawn(async move {
                axum::serve(listener, router).await.unwrap();
            });
            let manager = manager(dir.path());
            let external = owner(&address);
            let request = request();
            let intent = manager
                .prepare_external_intent("project", dir.path(), &external, &request)
                .unwrap();
            let client = OwnerClient::new(&address, &external.token).unwrap();
            assert!(
                manager
                    .resume_external_intent("project", &client, &intent, &request)
                    .await
                    .is_err()
            );
            let state = manager.read_external_state().unwrap();
            assert_eq!(state.intents.is_empty(), cleared, "{code}");
            assert!(state.owners.contains_key("project"));
            server.abort();
        }
    }

    #[test]
    fn unwritable_and_contended_state_prevent_intent_creation() {
        let dir = tempfile::tempdir().unwrap();
        let manager = manager(dir.path());
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(manager.external_state_path().with_extension("lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(
            manager
                .prepare_external_intent("project", dir.path(), &owner("127.0.0.1:1"), &request())
                .is_err()
        );
        drop(lock);
        std::fs::create_dir(manager.external_state_path()).unwrap();
        assert!(
            manager
                .prepare_external_intent("project", dir.path(), &owner("127.0.0.1:1"), &request())
                .is_err()
        );
    }

    #[test]
    fn separate_managers_merge_disk_state_and_stale_completion_cannot_clear_intent() {
        let dir = tempfile::tempdir().unwrap();
        let a = manager(dir.path());
        let b = manager(dir.path());
        let original = request();
        let external = owner("127.0.0.1:1");
        a.prepare_external_intent("a", dir.path(), &external, &original)
            .unwrap();
        b.prepare_external_intent("b", dir.path(), &external, &original)
            .unwrap();
        assert_eq!(a.read_external_state().unwrap().intents.len(), 2);
        let mut stale = original.clone();
        stale.operation_id = "stale".into();
        assert!(a.finish_external_intent("a", &stale, false).is_err());
        let mut replacement = external.clone();
        replacement.runtime_instance_id = "new-owner".into();
        let mut replaced_request = original.clone();
        replaced_request.expected_runtime_instance_id = "new-owner".into();
        assert!(
            a.prepare_external_intent("a", dir.path(), &replacement, &replaced_request)
                .is_err()
        );
        assert_eq!(a.read_external_state().unwrap().intents.len(), 2);
    }
}
