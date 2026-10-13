//! Admission/identity unit tests. Exact native Shutdown is covered by app-cli's
//! owner-upgrade process tests; these are not container/AI acceptance.
use super::*;

fn identity(path: &Path) -> RuntimeIdentityView {
    RuntimeIdentityView {
        application_id: "unknown-app".into(),
        service_family: "userapp-dev".into(),
        workspace_id: "workspace".into(),
        source_root: path.display().to_string(),
        runtime_instance_id: "observed-owner".into(),
        deployment_generation_id: "deployment".into(),
        protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
        capabilities: vec![],
    }
}

fn manager(root: &Path) -> DevServerManager {
    let mut config = crate::Config::from_env().unwrap();
    config.log_base_dir = root.join("logs");
    config.app_cli_bin = Some("a-bootstrap-that-must-not-execute".into());
    DevServerManager::new(Arc::new(config))
}

#[test]
fn same_uuid_is_not_sufficient_for_owner_upgrade_identity() {
    let root = tempfile::tempdir().unwrap();
    let original = identity(root.path());
    assert!(same_identity(&original, &original));
    for field in 0..8 {
        let mut foreign = original.clone();
        match field {
            0 => foreign.application_id = "other-app".into(),
            1 => foreign.service_family = "other-family".into(),
            2 => foreign.workspace_id = "other-workspace".into(),
            3 => foreign.source_root = root.path().join("other-root").display().to_string(),
            4 => foreign.runtime_instance_id = "new-owner".into(),
            5 => foreign.deployment_generation_id = "other-deployment".into(),
            6 => foreign.protocol_version += 1,
            7 => foreign.capabilities.push("another-contract".into()),
            _ => unreachable!("fixed test dimensions"),
        }
        assert!(
            !same_identity(&original, &foreign),
            "identity field {field} was ignored"
        );
    }
}

#[tokio::test]
async fn current_official_owner_is_observed_without_bootstrap_or_project_files() {
    let root = tempfile::tempdir().unwrap();
    let state = runtime_state_layout::ensure_state_root(root.path(), None, None).unwrap();
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("token"), b"existing-owner-token").unwrap();
    let manager = manager(root.path());
    let mut owner = identity(root.path());
    owner
        .capabilities
        .push(shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into());
    let available = AvailableOwner {
        address: "127.0.0.1:1".into(),
        identity: owner.clone(),
    };
    let observed = manager
        .ensure_official_owner_for_start(
            "project",
            root.path(),
            available,
            tokio::time::Instant::now() + Duration::from_millis(100),
            true,
        )
        .await
        .unwrap();
    assert!(same_identity(&observed.identity, &owner));
    assert!(lock(&manager.owner_children).unwrap().is_empty());
    assert!(!root.path().join("owner-bootstrap.lock").exists());
    assert!(!root.path().join("workspace.manifest.toml").exists());
}

#[tokio::test]
async fn a_captured_or_failed_build_expectation_prevents_late_owner_upgrade() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager(root.path());
    let owner = identity(root.path());
    for expectation in [
        super::super::super::types::OwnerExpectation::Captured {
            runtime_instance_id: owner.runtime_instance_id.clone(),
            revision: 7,
        },
        super::super::super::types::OwnerExpectation::ObservationFailed {
            reason: "original Stop barrier is unknown".into(),
        },
    ] {
        lock(&manager.owner_expectations)
            .unwrap()
            .insert("project".into(), expectation);
        let error = manager
            .ensure_official_owner_for_start(
                "project",
                root.path(),
                AvailableOwner {
                    address: "127.0.0.1:1".into(),
                    identity: owner.clone(),
                },
                tokio::time::Instant::now() + Duration::from_secs(1),
                true,
            )
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("after build admission"));
        assert!(lock(&manager.owner_children).unwrap().is_empty());
        assert!(
            lock(&manager.owner_expectations)
                .unwrap()
                .contains_key("project")
        );
    }
}

#[tokio::test]
async fn missing_capability_does_not_authorize_foreign_application_bootstrap() {
    let root = tempfile::tempdir().unwrap();
    let manager = manager(root.path());
    let mut owner = identity(root.path());
    owner.application_id = "foreign-app".into();
    let error = manager
        .ensure_official_owner_for_start(
            "project",
            root.path(),
            AvailableOwner {
                address: "127.0.0.1:1".into(),
                identity: owner,
            },
            tokio::time::Instant::now() + Duration::from_secs(1),
            false,
        )
        .await
        .err()
        .unwrap();
    assert!(error.to_string().contains("identity mismatch"));
    assert!(lock(&manager.owner_children).unwrap().is_empty());
}

#[tokio::test]
async fn authenticated_owner_client_does_not_follow_a_redirect_with_its_token() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let received = Arc::new(AtomicUsize::new(0));
    let count = received.clone();
    let target = axum::Router::new().fallback(move || {
        let count = count.clone();
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            axum::http::StatusCode::OK
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let destination = format!("http://{}/forwarded", listener.local_addr().unwrap());
    let target = tokio::spawn(async move { axum::serve(listener, target).await.unwrap() });
    let origin = axum::Router::new().fallback(move || {
        let destination = destination.clone();
        async move { axum::response::Redirect::temporary(&destination) }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let origin = tokio::spawn(async move { axum::serve(listener, origin).await.unwrap() });
    assert!(
        OwnerClient::new(&address, "private-test-token")
            .unwrap()
            .status()
            .await
            .is_err()
    );
    assert_eq!(received.load(Ordering::SeqCst), 0);
    origin.abort();
    target.abort();
}

/// Real subprocess exit + real loopback control + held lock, with protocol
/// fixtures for the two owners. The fixture is not an actual app-cli binary.
#[tokio::test]
#[cfg(unix)]
async fn exited_upgrade_candidate_observes_a_different_initializing_native_winner() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let temporary = tempfile::tempdir().unwrap();
    let source = temporary.path().canonicalize().unwrap().join("source");
    std::fs::create_dir_all(&source).unwrap();
    let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
    std::fs::create_dir_all(&root).unwrap();
    let owner_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("owner.lock"))
        .unwrap();
    owner_lock.try_lock().unwrap();
    std::fs::write(root.join("token"), b"fixture-token").unwrap();
    let old_native = uuid::Uuid::new_v4().to_string();
    let new_native = uuid::Uuid::new_v4().to_string();
    let transitioning = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicBool::new(false));
    let observed_winner = Arc::new(AtomicUsize::new(0));
    let old = identity(&source);
    let mut successor = old.clone();
    successor.runtime_instance_id = "different-official-winner".into();
    successor
        .capabilities
        .push(shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into());
    let control_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let control_addr = control_listener.local_addr().unwrap().to_string();
    let initial = runtime_supervisor::Snapshot {
        version: 1,
        binding: runtime_supervisor::Binding {
            component: "app-cli".into(),
            resource: source.clone(),
        },
        supervisor_id: old_native.clone(),
        generation: None,
        phase: runtime_supervisor::Phase::Stopped,
        intent: runtime_supervisor::Intent::Stopped,
        operation_id: None,
        error: None,
        problem: None,
    };
    let discovery = |id: &str, snapshot: &runtime_supervisor::Snapshot| {
        serde_json::to_vec(
        &serde_json::json!({"version":2,"instance":id,"address":control_addr,"token":"native-token","snapshot":snapshot,"requests":[]})).unwrap()
    };
    std::fs::write(
        root.join("supervisor.json"),
        discovery(&old_native, &initial),
    )
    .unwrap();
    let control_transition = transitioning.clone();
    let control_ready = ready.clone();
    let winner_counter = observed_winner.clone();
    let control_initial = initial.clone();
    let control_winner = new_native.clone();
    let control_task = tokio::spawn(async move {
        loop {
            let (stream, _) = control_listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut line = String::new();
            stream.read_line(&mut line).await.unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                request["request"]["action"], "status",
                "file-server must not issue another Shutdown"
            );
            let changed = control_transition.load(Ordering::SeqCst);
            let mut snapshot = control_initial.clone();
            if changed {
                snapshot.supervisor_id = control_winner.clone();
                snapshot.phase = if control_ready.load(Ordering::SeqCst) {
                    runtime_supervisor::Phase::Ready
                } else {
                    runtime_supervisor::Phase::Reconciling
                };
                winner_counter.fetch_add(1, Ordering::SeqCst);
            }
            let mut reply = serde_json::to_vec(&serde_json::json!({"instance":snapshot.supervisor_id,"snapshot":snapshot,"error":null})).unwrap();
            reply.push(b'\n');
            stream.get_mut().write_all(&reply).await.unwrap();
        }
    });
    let old_wire = old.clone();
    let new_wire = successor.clone();
    let identity_transition = transitioning.clone();
    let identity_ready = ready.clone();
    let status_transition = transitioning.clone();
    let new_status = successor.runtime_instance_id.clone();
    let old_status = old.runtime_instance_id.clone();
    let signal_transition = transitioning.clone();
    let signal_ready = ready.clone();
    let signal_root = root.clone();
    let signal_native = new_native.clone();
    let signal_snapshot = initial.clone();
    let signal_control = control_addr.clone();
    let router = axum::Router::new()
        .route("/v1/runtime/identity", axum::routing::get(move || {
            let (old, new, changed, ready) = (old_wire.clone(), new_wire.clone(), identity_transition.clone(), identity_ready.clone());
            async move {
                if changed.load(Ordering::SeqCst) && !ready.load(Ordering::SeqCst) {
                    return (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"code":"ERR_INITIALIZING","message":"booting","data":null})));
                }
                (axum::http::StatusCode::OK, axum::Json(serde_json::to_value(shared_types::HttpResult::success(
                    if changed.load(Ordering::SeqCst) { new } else { old })).unwrap()))
            }
        }))
        .route("/v1/runtime/status", axum::routing::get(move || {
            let (changed, old, new) = (status_transition.clone(), old_status.clone(), new_status.clone());
            async move { axum::Json(shared_types::HttpResult::success(shared_types::RuntimeStatusView {
                desired: shared_types::DesiredState::Stopped, observed: shared_types::ObservedHealth::Stopped,
                active_target: None, revision: 7, active_operation_id: None, recovery_protection: false,
                runtime_instance_id: if changed.load(Ordering::SeqCst) { new } else { old },
            })) }
        }))
        .route("/fixture-upgrade", axum::routing::post(move || {
            let (changed, ready, root, native, mut snapshot, address) = (signal_transition.clone(), signal_ready.clone(), signal_root.clone(), signal_native.clone(), signal_snapshot.clone(), signal_control.clone());
            async move {
                changed.store(true, Ordering::SeqCst);
                snapshot.supervisor_id = native.clone();
                snapshot.phase = runtime_supervisor::Phase::Reconciling;
                std::fs::write(root.join("supervisor.json"), serde_json::to_vec(&serde_json::json!({"version":2,"instance":native,"address":address,"token":"native-token","snapshot":snapshot,"requests":[]})).unwrap()).unwrap();
                tokio::spawn(async move { tokio::time::sleep(Duration::from_millis(400)).await; ready.store(true, Ordering::SeqCst); });
                axum::http::StatusCode::OK
            }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let http_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let script = temporary.path().join("bootstrap-fixture.py");
    std::fs::write(&script, format!("#!/usr/bin/env python3\nimport urllib.request\nopener=urllib.request.build_opener(urllib.request.ProxyHandler({{}}))\nopener.open(urllib.request.Request('http://{address}/fixture-upgrade',data=b''),timeout=5).read()\n")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut config = crate::Config::from_env().unwrap();
    config.log_base_dir = temporary.path().join("logs");
    config.app_cli_bin = Some(script.display().to_string());
    config.app_cli_admin_probe_addr = address.clone();
    let manager = DevServerManager::new(Arc::new(config));
    let observed = manager
        .ensure_official_owner_for_start(
            "project",
            &source,
            AvailableOwner {
                address,
                identity: old,
            },
            tokio::time::Instant::now() + Duration::from_secs(3),
            false,
        )
        .await
        .unwrap();
    assert!(same_identity(&observed.identity, &successor));
    assert!(
        observed_winner.load(Ordering::SeqCst) > 0,
        "initializing native winner was never observed after candidate exit"
    );
    control_task.abort();
    http_task.abort();
}
