use super::*;
use runtime_supervisor::{FailureCode, Intent, Phase};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn fixture() -> (tempfile::TempDir, ManagedWorkspace, RuntimeIdentityView) {
    let temp = tempfile::tempdir().unwrap();
    let source_root = std::fs::canonicalize(temp.path()).unwrap().join("11");
    let state_root = source_root.join("state/11");
    std::fs::create_dir_all(&state_root).unwrap();
    let managed = ManagedWorkspace {
        application_id: "11".into(),
        source_root: source_root.clone(),
        state_root,
    };
    let identity = RuntimeIdentityView {
        application_id: "11".into(),
        service_family: "userapp-dev".into(),
        workspace_id: "code".into(),
        source_root: source_root.join("code").display().to_string(),
        runtime_instance_id: "old-http-owner".into(),
        deployment_generation_id: "deployment".into(),
        protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    save(
        &managed.state_root.join("identity.json"),
        &serde_json::to_value(&identity).unwrap(),
    );
    (temp, managed, identity)
}

fn save(path: &Path, value: &serde_json::Value) {
    let temporary = path.with_extension("test-tmp");
    std::fs::write(&temporary, serde_json::to_vec(value).unwrap()).unwrap();
    std::fs::rename(temporary, path).unwrap();
}

fn snapshot(managed: &ManagedWorkspace, identity: &RuntimeIdentityView) -> Snapshot {
    Snapshot {
        version: 1,
        binding: runtime_supervisor::Binding {
            component: "app-cli".into(),
            resource: PathBuf::from(&identity.source_root),
        },
        supervisor_id: format!("supervisor-{}", managed.application_id),
        generation: Some(uuid::Uuid::new_v4().to_string()),
        phase: Phase::Ready,
        intent: Intent::Run,
        operation_id: None,
        error: None,
        problem: None,
    }
}

fn discovery(root: &Path, address: &str, snapshot: &Snapshot, requests: &[(Request, Snapshot)]) {
    save(
        &root.join("supervisor.json"),
        &serde_json::json!({
            "version": 2, "instance": snapshot.supervisor_id, "token": "fixture-native-token",
            "address": address, "snapshot": snapshot, "requests": requests,
        }),
    );
}

fn generation_receipt(root: &Path, generation: &str, supervisor: &str, phase: &str) {
    let work = root.join("work").join(generation);
    std::fs::create_dir_all(&work).unwrap();
    save(
        &work.join("generation.json"),
        &serde_json::json!({
            "version":1,"id":generation,"supervisor":supervisor,"token":"generation-token",
            "intent":"shutdown","phase":phase,"worker_pid":null,"exit_code":0,"error":null,
        }),
    );
}

#[test]
fn managed_handover_refuses_foreign_app_component_protocol_or_state_authority() {
    let (_temp, managed, identity) = fixture();
    assert!(
        verify_managed_identity(&identity, &managed).is_ok(),
        "a deleted child can be retired"
    );
    for field in ["application", "service", "protocol", "root", "instance"] {
        let mut foreign = identity.clone();
        match field {
            "application" => foreign.application_id = "12".into(),
            "service" => foreign.service_family = "other-service".into(),
            "protocol" => foreign.protocol_version += 1,
            "root" => {
                foreign.source_root = managed
                    .source_root
                    .with_file_name("12")
                    .display()
                    .to_string()
            }
            "instance" => foreign.runtime_instance_id = "different-state-owner".into(),
            _ => unreachable!(),
        }
        assert!(
            verify_managed_identity(&foreign, &managed).is_err(),
            "{field}"
        );
    }
    let mut native = snapshot(&managed, &identity);
    assert!(verify_supervisor_binding(&native, &identity, &managed).is_ok());
    native.binding.component = "foreign-component".into();
    assert!(verify_supervisor_binding(&native, &identity, &managed).is_err());
}

#[test]
fn completed_shutdown_lookup_preserves_parameters_and_refuses_ambiguity() {
    let (_temp, managed, identity) = fixture();
    let mut terminal = snapshot(&managed, &identity);
    terminal.phase = Phase::Stopped;
    terminal.intent = Intent::Shutdown;
    terminal.generation = None;
    terminal.operation_id = None;
    for generation in [Some("captured-generation"), None] {
        let mut request = Request::new(Action::Shutdown);
        request.capture_generation(generation);
        discovery(
            &managed.state_root,
            "127.0.0.1:1",
            &terminal,
            &[(request.clone(), terminal.clone())],
        );
        let original = std::fs::read(managed.state_root.join("supervisor.json")).unwrap();
        let (restored, receipt) = runtime_supervisor::completed_shutdown_for(
            &managed.state_root,
            &terminal.supervisor_id,
        )
        .unwrap()
        .unwrap();
        assert_eq!(restored, request);
        assert_eq!(
            receipt.operation_id.as_deref(),
            Some(request.request_id.as_str())
        );
        assert_eq!(
            std::fs::read(managed.state_root.join("supervisor.json")).unwrap(),
            original,
            "lookup is read-only"
        );
        assert!(
            runtime_supervisor::completed_shutdown_for(&managed.state_root, "another-supervisor")
                .unwrap()
                .is_none()
        );
        let mut duplicate = request.clone();
        duplicate.request_id = "ambiguous-second-shutdown".into();
        discovery(
            &managed.state_root,
            "127.0.0.1:1",
            &terminal,
            &[(request, terminal.clone()), (duplicate, terminal.clone())],
        );
        assert!(
            runtime_supervisor::completed_shutdown_for(
                &managed.state_root,
                &terminal.supervisor_id
            )
            .is_err()
        );
    }
}

#[test]
fn completed_shutdown_lookup_does_not_invent_legacy_generation() {
    let (_temp, managed, identity) = fixture();
    let mut terminal = snapshot(&managed, &identity);
    terminal.phase = Phase::Stopped;
    terminal.intent = Intent::Shutdown;
    terminal.generation = None;
    let legacy = Request::new(Action::Shutdown);
    discovery(
        &managed.state_root,
        "127.0.0.1:1",
        &terminal,
        &[(legacy.clone(), terminal.clone())],
    );
    let (restored, _) =
        runtime_supervisor::completed_shutdown_for(&managed.state_root, &terminal.supervisor_id)
            .unwrap()
            .unwrap();
    assert_eq!(
        restored, legacy,
        "lookup must not manufacture a captured generation for legacy requests"
    );
    assert!(restored.expected_generation.is_none());
}

#[cfg(unix)]
#[tokio::test]
async fn completed_bootstrap_shutdown_with_cleared_live_slot_resumes_current_request() {
    use std::os::unix::fs::PermissionsExt;
    let (temp, managed, mut identity) = fixture();
    identity.source_root = managed.source_root.display().to_string();
    identity.workspace_id = "11".into();
    identity.runtime_instance_id = "resumed-owner".into();
    let root = managed.state_root.clone();
    let launch = temp.path().join("resumed-args");
    let program = temp.path().join("resumed-app-cli.sh");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nsleep 2\n",
            launch.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let http_launch = launch.clone();
    let http_root = root.clone();
    let http=axum::Router::new().route("/v1/runtime/identity",axum::routing::get(move || {
        let ready=http_launch.exists();
        let identity=identity.clone();
        if ready { save(&http_root.join("identity.json"),&serde_json::to_value(&identity).unwrap()); }
        async move {
            if ready { (axum::http::StatusCode::OK,axum::Json(serde_json::json!({"code":"0000","message":"ok","data":identity}))) }
            else { (axum::http::StatusCode::SERVICE_UNAVAILABLE,axum::Json(serde_json::json!({"code":"ERR_INITIALIZING","message":"finishing retained shutdown"}))) }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let http_task = tokio::spawn(async move { axum::serve(listener, http).await.unwrap() });
    let native = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let native_address = native.local_addr().unwrap().to_string();
    let next_native = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let next_address = next_native.local_addr().unwrap().to_string();
    let owner_lock = std::fs::File::create(root.join("owner.lock")).unwrap();
    owner_lock.try_lock().unwrap();
    let generation = uuid::Uuid::new_v4().to_string();
    let mut request = Request::new(Action::Shutdown);
    request.capture_generation(Some(&generation));
    let original = request.clone();
    let mut retained = Snapshot {
        version: 1,
        binding: runtime_supervisor::Binding {
            component: "app-cli".into(),
            resource: managed.source_root.clone(),
        },
        supervisor_id: "finishing-bootstrap".into(),
        generation: Some(generation.clone()),
        phase: Phase::CleanupPending,
        intent: Intent::Shutdown,
        operation_id: Some(request.request_id.clone()),
        error: None,
        problem: None,
    };
    discovery(
        &root,
        &native_address,
        &retained,
        &[(request.clone(), retained.clone())],
    );
    let work = root.join("work").join(&generation);
    std::fs::create_dir_all(&work).unwrap();
    let generation_lock = std::fs::File::create(work.join("generation.lock")).unwrap();
    generation_lock.try_lock().unwrap();
    save(
        &work.join("generation.json"),
        &serde_json::json!({"version":1,"id":generation,"supervisor":"original-execution-owner","token":"generation-token","intent":"shutdown","phase":"Draining","worker_pid":null,"exit_code":null,"error":null}),
    );
    let mut child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "read completion; exit 0"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut completion = child.stdin.take().unwrap();
    let tracked = super::super::super::SupervisedChild::adopt(
        child,
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    );
    let mut config = crate::Config::from_env().unwrap();
    config.app_cli_bin = Some(program.display().to_string());
    config.log_base_dir = temp.path().join("logs");
    let manager = DevServerManager::new(Arc::new(config));
    lock(&manager.owner_children)
        .unwrap()
        .insert("11".into(), tracked.clone());
    let native_launch = launch.clone();
    let native_task = tokio::spawn(async move {
        let (mut stream, _) = native.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(frame["request"]["action"], "status");
        stream.write_all(format!("{}\n",serde_json::json!({"instance":retained.supervisor_id,"snapshot":retained,"error":null})).as_bytes()).await.unwrap();
        save(
            &work.join("generation.json"),
            &serde_json::json!({"version":1,"id":generation,"supervisor":"original-execution-owner","token":"generation-token","intent":"shutdown","phase":"Quiescent","worker_pid":null,"exit_code":0,"error":null}),
        );
        drop(generation_lock);
        retained.phase = Phase::Stopped;
        retained.generation = None;
        retained.operation_id = None;
        discovery(
            &root,
            &native_address,
            &retained,
            &[(request.clone(), retained.clone())],
        );
        completion.write_all(b"finished\n").await.unwrap();
        drop(completion);
        assert!(
            matches!(tracked.wait_exit(Duration::from_secs(2)).await,Some(super::super::super::ChildExit::Exited(status)) if status.success())
        );
        drop(owner_lock);
        drop(stream);
        drop(native);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !native_launch.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let next_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("owner.lock"))
            .unwrap();
        next_lock.try_lock().unwrap();
        let mut successor = retained.clone();
        successor.supervisor_id = "resumed-native-owner".into();
        successor.phase = Phase::Ready;
        successor.intent = Intent::Stopped;
        discovery(&root, &next_address, &successor, &[(request, retained)]);
        let (mut stream, _) = next_native.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            frame["request"]["action"], "status",
            "resumption must not resend Shutdown to the successor"
        );
        stream.write_all(format!("{}\n",serde_json::json!({"instance":successor.supervisor_id,"snapshot":successor,"error":null})).as_bytes()).await.unwrap();
    });
    let outcome = tokio::time::timeout(
        Duration::from_secs(6),
        manager.wait_for_recovery_owner_with_context(
            "11",
            &managed.source_root,
            &address,
            &managed.state_root,
            Some(&managed),
        ),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), native_task)
        .await
        .unwrap()
        .unwrap();
    http_task.abort();
    assert_eq!(
        outcome.unwrap().unwrap().runtime_instance_id,
        "resumed-owner"
    );
    let (restored, _) =
        runtime_supervisor::completed_shutdown_for(&managed.state_root, "finishing-bootstrap")
            .unwrap()
            .unwrap();
    assert_eq!(
        restored, original,
        "request identity and captured generation remain unchanged"
    );
    let arguments = std::fs::read_to_string(launch).unwrap();
    assert_eq!(
        arguments.lines().filter(|line| *line == "serve").count(),
        1,
        "only one control-only successor is launched"
    );
    assert!(arguments.starts_with("serve\n--control-only\n--workspace\n"));
}

#[tokio::test]
async fn late_shutdown_never_retargets_replacement_at_the_same_state_root() {
    let (_temp, managed, identity) = fixture();
    let old = snapshot(&managed, &identity);
    generation_receipt(
        &managed.state_root,
        old.generation.as_deref().unwrap(),
        &old.supervisor_id,
        "Quiescent",
    );
    let mut replacement = old.clone();
    replacement.supervisor_id = "replacement-supervisor".into();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let owner_lock = std::fs::File::create(managed.state_root.join("owner.lock")).unwrap();
    owner_lock.try_lock().unwrap();
    discovery(&managed.state_root, &address, &replacement, &[]);
    let mut request = Request::new(Action::Shutdown);
    request.capture_generation(old.generation.as_deref());
    let error = runtime_supervisor::shutdown_captured_owner(
        &managed.state_root,
        &old,
        &request,
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(
        error
            .downcast_ref::<runtime_supervisor::Problem>()
            .is_some_and(|problem| problem.code == FailureCode::IdentityChanged)
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err(),
        "no stale stop may reach the replacement endpoint"
    );
    assert_eq!(
        runtime_supervisor::last_snapshot(&managed.state_root)
            .unwrap()
            .supervisor_id,
        replacement.supervisor_id
    );
}

#[tokio::test]
async fn shutdown_rejects_execution_owner_change_after_capturing_generation() {
    let (_temp, managed, identity) = fixture();
    let before = snapshot(&managed, &identity);
    let generation = before.generation.clone().unwrap();
    generation_receipt(
        &managed.state_root,
        &generation,
        "original-execution-owner",
        "Draining",
    );
    let owner_lock = std::fs::File::create(managed.state_root.join("owner.lock")).unwrap();
    owner_lock.try_lock().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    discovery(&managed.state_root, &address, &before, &[]);
    let root = managed.state_root.clone();
    let mut terminal = before.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .unwrap();
        let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
        let request: Request = serde_json::from_value(frame["request"].clone()).unwrap();
        assert_eq!(request.action, Action::Shutdown);
        assert_eq!(
            request.expected_generation.as_deref(),
            Some(generation.as_str())
        );
        generation_receipt(&root, &generation, "changed-execution-owner", "Quiescent");
        terminal.phase = Phase::Stopped;
        terminal.intent = Intent::Shutdown;
        terminal.generation = None;
        terminal.operation_id = Some(request.request_id.clone());
        let mut live = terminal.clone();
        live.operation_id = None;
        discovery(&root, &address, &live, &[(request, terminal.clone())]);
        drop(owner_lock);
        stream.write_all(format!("{}\n", serde_json::json!({"instance":terminal.supervisor_id,"snapshot":terminal,"error":null})).as_bytes()).await.unwrap();
    });
    let mut request = Request::new(Action::Shutdown);
    request.capture_generation(before.generation.as_deref());
    let error = runtime_supervisor::shutdown_captured_owner(
        &managed.state_root,
        &before,
        &request,
        Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    server.await.unwrap();
    assert!(
        error
            .downcast_ref::<runtime_supervisor::Problem>()
            .is_some_and(|problem| problem.code == FailureCode::IdentityChanged)
    );
    assert!(
        error
            .to_string()
            .contains("execution generation identity changed")
    );
}

#[cfg(unix)]
#[tokio::test]
async fn live_wrong_root_owner_shutdown_retries_same_identity_waits_locks_and_continues() {
    exercise_handover(true).await;
}

#[cfg(unix)]
#[tokio::test]
async fn idle_wrong_root_owner_without_generation_can_be_retired_and_restarted() {
    exercise_handover(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn exited_bootstrap_candidate_waits_for_verified_owner_reconciliation() {
    let (_temp, managed, mut identity) = fixture();
    identity.source_root = managed.source_root.display().to_string();
    identity.workspace_id = "11".into();
    let ready = Arc::new(AtomicBool::new(false));
    let http_ready = ready.clone();
    let http = axum::Router::new().route("/v1/runtime/identity", axum::routing::get(move || {
        let ready = http_ready.load(Ordering::SeqCst);
        let identity = identity.clone();
        async move {
            if ready {
                (axum::http::StatusCode::OK, axum::Json(serde_json::json!({"code":"0000","message":"ok","data":identity})))
            } else {
                (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"code":"ERR_INITIALIZING","message":"reconciling old binding"})))
            }
        }
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let http_task = tokio::spawn(async move { axum::serve(listener, http).await.unwrap() });
    let native = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let root = managed.state_root.clone();
    let owner_lock = std::fs::File::create(root.join("owner.lock")).unwrap();
    owner_lock.try_lock().unwrap();
    let mut native_snapshot = Snapshot {
        version: 1,
        binding: runtime_supervisor::Binding {
            component: "app-cli".into(),
            resource: managed.source_root.join("gone/code"),
        },
        supervisor_id: "winning-supervisor".into(),
        generation: None,
        phase: Phase::Reconciling,
        intent: Intent::Stopped,
        operation_id: None,
        error: None,
        problem: None,
    };
    discovery(
        &root,
        &native.local_addr().unwrap().to_string(),
        &native_snapshot,
        &[],
    );
    let source = managed.source_root.clone();
    let (observed_tx, observed_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let native_task = tokio::spawn(async move {
        let mut first_observed = Some(observed_tx);
        let mut finish = Some(finish_rx);
        loop {
            let (mut stream, _) = native.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(frame["request"]["action"], "status");
            stream.write_all(format!("{}\n",serde_json::json!({"instance":native_snapshot.supervisor_id,"snapshot":native_snapshot,"error":null})).as_bytes()).await.unwrap();
            if let Some(observed) = first_observed.take() {
                observed.send(()).unwrap();
                finish.take().unwrap().await.unwrap();
                native_snapshot.binding.resource = source.clone();
                native_snapshot.phase = Phase::Ready;
                ready.store(true, Ordering::SeqCst);
            }
        }
    });
    let manager = Arc::new(DevServerManager::new(Arc::new(
        crate::Config::from_env().unwrap(),
    )));
    let child = tokio::process::Command::new("/bin/sh")
        .args(["-c", "exit 4"])
        .spawn()
        .unwrap();
    let tracked = super::super::super::SupervisedChild::adopt(
        child,
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    );
    assert!(tracked.wait_exit(Duration::from_secs(2)).await.is_some());
    lock(&manager.owner_children)
        .unwrap()
        .insert("11".into(), tracked);
    let waiter = tokio::spawn({
        let manager = manager.clone();
        let managed = managed.clone();
        async move {
            manager
                .wait_for_recovery_owner_with_context(
                    "11",
                    &managed.source_root,
                    &address,
                    &managed.state_root,
                    Some(&managed),
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(2), observed_rx)
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !waiter.is_finished(),
        "candidate exit must not invalidate the owner proven to be reconciling"
    );
    finish_tx.send(()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(3), waiter)
        .await
        .unwrap()
        .unwrap();
    http_task.abort();
    native_task.abort();
    let identity = outcome.unwrap();
    assert_eq!(
        identity.source_root,
        managed.source_root.display().to_string()
    );
}

#[cfg(unix)]
async fn exercise_handover(has_generation: bool) {
    use std::os::unix::fs::PermissionsExt;
    let (temp, managed, identity) = fixture();
    let root = managed.state_root.clone();
    let launch = temp.path().join("launched-args");
    let program = temp.path().join("app-cli-fixture.sh");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nsleep 2\n",
            launch.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let retired = Arc::new(AtomicBool::new(false));
    let old_http_identity = identity.clone();
    let mut successor = identity.clone();
    successor.source_root = managed.source_root.display().to_string();
    successor.workspace_id = "11".into();
    successor.runtime_instance_id = "new-http-owner".into();
    let expected_successor = successor.clone();
    let http_root = root.clone();
    let http_launch = launch.clone();
    let http_retired = retired.clone();
    let http = axum::Router::new().route(
        "/v1/runtime/identity",
        axum::routing::get(move || {
            let identity = if http_launch.exists() {
                save(
                    &http_root.join("identity.json"),
                    &serde_json::to_value(&successor).unwrap(),
                );
                Some(successor.clone())
            } else if http_retired.load(Ordering::SeqCst) {
                None
            } else {
                Some(old_http_identity.clone())
            };
            async move {
                if let Some(identity) = identity {
                    (
                        axum::http::StatusCode::OK,
                        axum::Json(
                            serde_json::json!({"code":"0000", "message":"ok", "data":identity}),
                        ),
                    )
                } else {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(
                            serde_json::json!({"code":"ERR_INITIALIZING", "message":"transition"}),
                        ),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let http_task = tokio::spawn(async move { axum::serve(listener, http).await.unwrap() });
    let native = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let native_address = native.local_addr().unwrap().to_string();
    let next_native = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let next_address = next_native.local_addr().unwrap().to_string();
    let owner_lock = std::fs::File::create(root.join("owner.lock")).unwrap();
    owner_lock.try_lock().unwrap();
    let mut before = snapshot(&managed, &identity);
    if !has_generation {
        before.generation = None;
    }
    discovery(&root, &native_address, &before, &[]);
    let generation = before.generation.clone();
    let work = generation
        .as_ref()
        .map(|generation| root.join("work").join(generation));
    let generation_lock = work.as_ref().map(|work| {
        std::fs::create_dir_all(work).unwrap();
        let lock = std::fs::File::create(work.join("generation.lock")).unwrap();
        lock.try_lock().unwrap();
        save(&work.join("generation.json"),
        &serde_json::json!({
            "version":1,"id":generation,"supervisor":before.supervisor_id,"token":"generation-token",
            "intent":"shutdown","phase":"Draining","worker_pid":null,"exit_code":null,"error":null,
        }));
        lock
    });
    let calls = Arc::new(AtomicUsize::new(0));
    let observed_calls = calls.clone();
    let native_launch = launch.clone();
    let successor_root = managed.source_root.clone();
    let native_task = tokio::spawn(async move {
        let mut request_identity = None;
        let mut current = before;
        loop {
            let (mut stream, _) = native.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            let envelope: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(envelope["instance"], current.supervisor_id);
            let request: Request = serde_json::from_value(envelope["request"].clone()).unwrap();
            if request.action == Action::Status {
                stream.write_all(format!("{}\n", serde_json::json!({"instance":current.supervisor_id,"snapshot":current,"error":null})).as_bytes()).await.unwrap();
                continue;
            }
            assert_eq!(
                request.action,
                Action::Shutdown,
                "StopWork would retain the wrong root"
            );
            assert_eq!(
                request.expected_generation,
                Some(current.generation.clone().unwrap_or_default()),
                "None must be captured explicitly, not the legacy wildcard"
            );
            let count = observed_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(first) = &request_identity {
                assert_eq!(&request, first);
            } else {
                request_identity = Some(request.clone());
            }
            current.operation_id = Some(request.request_id.clone());
            current.intent = Intent::Shutdown;
            current.phase = Phase::CleanupPending;
            discovery(
                &root,
                &native_address,
                &current,
                &[(request.clone(), current.clone())],
            );
            if count == 0 {
                continue;
            } // Lost first reply after durable admission.
            retired.store(true, Ordering::SeqCst);
            if let Some(work) = &work {
                save(
                    &work.join("generation.json"),
                    &serde_json::json!({
                        "version":1,"id":generation,"supervisor":current.supervisor_id,"token":"generation-token",
                        "intent":"shutdown","phase":"Quiescent","worker_pid":null,"exit_code":0,"error":null,
                    }),
                );
            }
            drop(generation_lock);
            current.phase = Phase::Stopped;
            current.generation = None;
            let terminal = current.clone();
            current.operation_id = None;
            discovery(
                &root,
                &native_address,
                &current,
                &[(request, terminal.clone())],
            );
            stream.write_all(format!("{}\n", serde_json::json!({"instance":current.supervisor_id,"snapshot":terminal,"error":null})).as_bytes()).await.unwrap();
            tokio::time::sleep(Duration::from_millis(250)).await;
            assert!(
                !native_launch.exists(),
                "terminal receipt alone cannot authorize spawn while owner lock is held"
            );
            drop(owner_lock);
            break;
        }
        drop(native);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !native_launch.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let successor_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(root.join("owner.lock"))
            .unwrap();
        successor_lock.try_lock().unwrap();
        current.supervisor_id = "successor-supervisor".into();
        current.binding.resource = successor_root;
        current.phase = Phase::Ready;
        current.intent = Intent::Stopped;
        current.generation = Some(uuid::Uuid::new_v4().to_string());
        discovery(&root, &next_address, &current, &[]);
        let (mut stream, _) = next_native.accept().await.unwrap();
        let mut line = String::new();
        BufReader::new(&mut stream)
            .read_line(&mut line)
            .await
            .unwrap();
        let envelope: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(envelope["instance"], current.supervisor_id);
        let request: Request = serde_json::from_value(envelope["request"].clone()).unwrap();
        assert_eq!(
            request.action,
            Action::Status,
            "late shutdown must never reach the successor"
        );
        stream.write_all(format!("{}\n", serde_json::json!({"instance":current.supervisor_id,"snapshot":current,"error":null})).as_bytes()).await.unwrap();
    });
    let mut config = crate::Config::from_env().unwrap();
    config.app_cli_bin = Some(program.display().to_string());
    config.log_base_dir = temp.path().join("logs");
    let manager = DevServerManager::new(Arc::new(config));
    let outcome = tokio::time::timeout(
        Duration::from_secs(6),
        manager.handover_managed_owner("11", &address, identity, &managed),
    )
    .await;
    tokio::time::timeout(Duration::from_secs(1), native_task)
        .await
        .unwrap()
        .unwrap();
    http_task.abort();
    let actual = outcome
        .expect("handover must continue inside the current request")
        .unwrap();
    assert_eq!(
        actual.runtime_instance_id,
        expected_successor.runtime_instance_id
    );
    assert_eq!(actual.source_root, expected_successor.source_root);
    if !has_generation {
        assert!(
            !managed.state_root.join("work").exists(),
            "idle retirement must not invent generation cleanup receipts"
        );
    }
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "lost replies resume the same Shutdown"
    );
    let args = std::fs::read_to_string(launch).unwrap();
    assert!(args.starts_with("serve\n--control-only\n--workspace\n"));
    assert!(
        args.lines()
            .any(|arg| arg == managed.source_root.to_string_lossy())
    );
    assert!(
        !args
            .lines()
            .any(|arg| arg == managed.source_root.join("code").to_string_lossy())
    );
}
