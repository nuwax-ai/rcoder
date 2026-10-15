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
        single_application: false,
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
    exercise_native_retirement(has_generation, None).await;
}

#[cfg(unix)]
#[derive(Clone, Copy)]
enum MetadataRecovery {
    ProtocolV4,
    MissingToken,
    EmptyToken,
}

#[cfg(unix)]
mod scoped_environment {
    #![allow(unsafe_code)] // nextest isolates these process-global fixture values.

    pub(super) struct Guard(Vec<(&'static str, Option<std::ffi::OsString>)>);
    impl Guard {
        pub(super) fn install(managed: &runtime_state_layout::ManagedWorkspace) -> Self {
            let values = [
                ("APP_CLI_MANAGED", Some("1".into())),
                ("SERVICE_TYPE", Some("user-app-builder".into())),
                ("PROJECT_ID", Some(managed.application_id.clone().into())),
                (
                    "USERAPP_WORKSPACE_DIR",
                    managed
                        .source_root
                        .parent()
                        .map(|p| p.as_os_str().to_owned()),
                ),
                (
                    "APP_CLI_STATE_ROOT",
                    Some(managed.state_root.as_os_str().to_owned()),
                ),
                ("APP_CLI_RUNTIME_WORKSPACE", None),
                ("USERAPP_SINGLE_APP_ID", None),
            ];
            let original = values
                .iter()
                .map(|(key, _)| (*key, std::env::var_os(key)))
                .collect();
            for (key, value) in values {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
            Self(original)
        }
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            for (key, value) in &self.0 {
                unsafe {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn source_build_retires_known_v4_owner_before_current_management_bootstrap() {
    exercise_native_retirement(true, Some(MetadataRecovery::ProtocolV4)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn source_build_retires_tokenless_live_owner_without_rotating_its_live_token() {
    exercise_native_retirement(true, Some(MetadataRecovery::MissingToken)).await;
}

#[cfg(unix)]
#[tokio::test]
async fn source_build_retires_empty_token_owner_before_restoring_current_credentials() {
    exercise_native_retirement(true, Some(MetadataRecovery::EmptyToken)).await;
}

#[cfg(unix)]
async fn exercise_native_retirement(has_generation: bool, metadata: Option<MetadataRecovery>) {
    use std::os::unix::fs::PermissionsExt;
    let (temp, managed, mut identity) = fixture();
    let _environment = metadata.map(|_| scoped_environment::Guard::install(&managed));
    if let Some(kind) = metadata {
        identity.source_root = managed.source_root.display().to_string();
        identity.workspace_id = "11".into();
        match kind {
            MetadataRecovery::ProtocolV4 => identity.protocol_version = 4,
            MetadataRecovery::MissingToken | MetadataRecovery::EmptyToken => identity
                .capabilities
                .push(shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into()),
        }
        save(
            &managed.state_root.join("identity.json"),
            &serde_json::to_value(&identity).unwrap(),
        );
    }
    let root = managed.state_root.clone();
    if matches!(metadata, Some(MetadataRecovery::ProtocolV4)) {
        std::fs::write(root.join("token"), b"original-owner-token").unwrap();
    } else if matches!(metadata, Some(MetadataRecovery::EmptyToken)) {
        std::fs::write(root.join("token"), b" \n").unwrap();
    }
    let original_token = std::fs::read(root.join("token")).ok();
    let preserved_data = managed.source_root.join("user-data.txt");
    std::fs::write(&preserved_data, b"retained application data").unwrap();
    let migration = root.join("original-unknown-migration.json");
    std::fs::write(
        &migration,
        b"{\"state\":\"unknown\",\"operation_id\":\"original-migration\"}",
    )
    .unwrap();
    let original_migration = std::fs::read(&migration).unwrap();
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
    successor.protocol_version = shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION;
    if metadata.is_some()
        && !successor
            .capabilities
            .iter()
            .any(|c| c == shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY)
    {
        successor
            .capabilities
            .push(shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into());
    }
    let expected_successor = successor.clone();
    let wire_calls_before_retirement = Arc::new(AtomicUsize::new(0));
    let http_root = root.clone();
    let http_launch = launch.clone();
    let http_retired = retired.clone();
    let wire_launch = launch.clone();
    let recovery_launch = launch.clone();
    let premature_recovery = wire_calls_before_retirement.clone();
    let premature_status = wire_calls_before_retirement.clone();
    let new_recovery_id = successor.runtime_instance_id.clone();
    let new_status_id = successor.runtime_instance_id.clone();
    let http = axum::Router::new().route(
        "/v1/runtime/identity",
        axum::routing::get(move || {
            let identity = if http_launch.exists() {
                std::fs::write(http_root.join("token"), b"successor-owner-token").unwrap();
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
    ).route("/v1/runtime/recovery", axum::routing::get(move || {
        let (launch, premature, id) = (recovery_launch.clone(), premature_recovery.clone(), new_recovery_id.clone());
        async move {
            if !launch.exists() { premature.fetch_add(1, Ordering::SeqCst); }
            axum::Json(shared_types::HttpResult::success(shared_types::RuntimeRecoveryView {
                runtime_instance_id:id, deployment_generation_id:"deployment".into(), revision:7,
                kernel_protected:false, owner_protected:false, operation_id:None, boundary:None,
                generation_matches:None, credentials_required:false,
                migrations:shared_types::RuntimeMigrationRecoveryState::Unconfirmed,
            }))
        }
    })).route("/v1/runtime/status", axum::routing::get(move || {
        let (launch, premature, id) = (wire_launch.clone(), premature_status.clone(), new_status_id.clone());
        async move {
            if !launch.exists() { premature.fetch_add(1, Ordering::SeqCst); }
            axum::Json(shared_types::HttpResult::success(shared_types::RuntimeStatusView {
                desired:shared_types::DesiredState::Stopped, observed:shared_types::ObservedHealth::Stopped,
                active_target:None, revision:7, active_operation_id:None, recovery_protection:false, runtime_instance_id:id,
            }))
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
    let mut original_process = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let original_pid = original_process.id().unwrap();
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
            assert!(
                !native_launch.exists(),
                "new management must wait for the old owner to exit"
            );
            assert_eq!(
                std::fs::read(root.join("token")).ok(),
                original_token,
                "a live owner's credentials must never be rotated during retirement"
            );
            assert!(process_utils::process_exists(original_pid).unwrap());
            original_process.kill().await.unwrap();
            original_process.wait().await.unwrap();
            assert!(!process_utils::process_exists(original_pid).unwrap());
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
        let original_shutdown =
            runtime_supervisor::completed_shutdown_for(&root, &current.supervisor_id)
                .unwrap()
                .unwrap();
        current.supervisor_id = "successor-supervisor".into();
        current.binding.resource = successor_root;
        current.phase = Phase::Ready;
        current.intent = Intent::Stopped;
        current.generation = Some(uuid::Uuid::new_v4().to_string());
        discovery(&root, &next_address, &current, &[original_shutdown]);
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
    config.app_cli_admin_probe_addr = address.clone();
    let manager = DevServerManager::new(Arc::new(config));
    let outcome = tokio::time::timeout(Duration::from_secs(6), async {
        if metadata.is_some() {
            manager
                .preflight_userapp_build("11", &managed.source_root)
                .await
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            match owner_client::observe_owner(&address).await? {
                OwnerProbe::Ready(identity) => Ok(identity),
                _ => anyhow::bail!("source preflight did not restore current management"),
            }
        } else {
            manager
                .handover_managed_owner("11", &address, identity, &managed)
                .await
        }
    })
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
    assert_eq!(
        std::fs::read(&preserved_data).unwrap(),
        b"retained application data"
    );
    assert_eq!(
        std::fs::read(&migration).unwrap(),
        original_migration,
        "unknown migration is never replayed or rewritten"
    );
    assert_eq!(
        wire_calls_before_retirement.load(Ordering::SeqCst),
        0,
        "old wire control cannot be reused for metadata retirement"
    );
    assert!(!process_utils::process_exists(original_pid).unwrap());
    if metadata.is_some() {
        assert!(
            matches!(
                lock(&manager.owner_expectations).unwrap().get("11"),
                Some(super::super::super::types::OwnerExpectation::Captured { revision: 7, .. })
            ),
            "the original Source preflight must continue with its current management owner"
        );
    }
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

#[cfg(unix)]
#[tokio::test]
async fn source_metadata_recovery_preserves_foreign_and_future_protocol_owners() {
    for changed in ["application", "workspace", "future_protocol"] {
        let (_temp, managed, mut identity) = fixture();
        let _environment = scoped_environment::Guard::install(&managed);
        identity.source_root = managed.source_root.display().to_string();
        identity.protocol_version = 4;
        match changed {
            "application" => identity.application_id = "another-application".into(),
            "workspace" => {
                identity.source_root = managed
                    .source_root
                    .with_file_name("foreign")
                    .display()
                    .to_string()
            }
            "future_protocol" => {
                identity.protocol_version = shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION + 1
            }
            _ => unreachable!(),
        }
        save(
            &managed.state_root.join("identity.json"),
            &serde_json::to_value(&identity).unwrap(),
        );
        let before = snapshot(&managed, &identity);
        discovery(&managed.state_root, "127.0.0.1:1", &before, &[]);
        let original = std::fs::read(managed.state_root.join("supervisor.json")).unwrap();
        let owner = identity.clone();
        let http = axum::Router::new().route(
            "/v1/runtime/identity",
            axum::routing::get(move || {
                let owner = owner.clone();
                async move { axum::Json(shared_types::HttpResult::success(owner)) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move { axum::serve(listener, http).await.unwrap() });
        let mut config = crate::Config::from_env().unwrap();
        config.log_base_dir = managed.source_root.join("logs");
        config.app_cli_admin_probe_addr = address;
        config.app_cli_bin = Some("a-foreign-owner-bootstrap-must-not-run".into());
        let manager = DevServerManager::new(Arc::new(config));
        let result = manager
            .preflight_userapp_build("11", &managed.source_root)
            .await;
        server.abort();
        assert!(result.is_err(), "{changed}");
        assert!(
            lock(&manager.owner_children).unwrap().is_empty(),
            "{changed}"
        );
        assert_eq!(
            std::fs::read(managed.state_root.join("supervisor.json")).unwrap(),
            original,
            "{changed}"
        );
        assert!(
            !managed.state_root.join("owner-bootstrap.lock").exists(),
            "{changed}"
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn source_build_bootstraps_after_legacy_completed_shutdown_without_rewriting_its_request() {
    exercise_legacy_completion(false).await;
}

#[cfg(unix)]
#[tokio::test]
async fn source_build_observes_concurrent_successor_without_replaying_legacy_shutdown() {
    exercise_legacy_completion(true).await;
}

#[cfg(unix)]
async fn exercise_legacy_completion(replacement: bool) {
    use std::os::unix::fs::PermissionsExt;
    let (temp, managed, mut identity) = fixture();
    let _environment = scoped_environment::Guard::install(&managed);
    identity.source_root = managed.source_root.display().to_string();
    identity.workspace_id = "11".into();
    let root = managed.state_root.clone();
    let launch = temp.path().join("legacy-bootstrap-args");
    let program = temp.path().join("legacy-management-fixture.sh");
    std::fs::write(
        &program,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{}'\nsleep 2\n",
            launch.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let data = managed.source_root.join("application-data.bin");
    std::fs::write(&data, b"keep the source and application data").unwrap();
    let migration = root.join("unknown-migration.json");
    std::fs::write(
        &migration,
        b"{\"state\":\"unknown\",\"operation_id\":\"original\"}",
    )
    .unwrap();
    let original_migration = std::fs::read(&migration).unwrap();
    let mut terminal = snapshot(&managed, &identity);
    terminal.phase = Phase::Stopped;
    terminal.intent = Intent::Shutdown;
    terminal.generation = None;
    let request = Request::new(Action::Shutdown);
    discovery(
        &root,
        "127.0.0.1:1",
        &terminal,
        &[(request.clone(), terminal.clone())],
    );
    let original_shutdown = runtime_supervisor::saved_request_snapshot(&root, &request)
        .unwrap()
        .unwrap();
    let owner_lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("owner.lock"))
        .unwrap();
    owner_lock.try_lock().unwrap();
    let worker_root = root.join("work").join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&worker_root).unwrap();
    let worker_lock = std::fs::File::create(worker_root.join("generation.lock")).unwrap();
    worker_lock.try_lock().unwrap();
    let mut worker = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let old_pid = worker.id().unwrap();
    let mut successor = identity.clone();
    successor.runtime_instance_id = "current-legacy-successor".into();
    successor
        .capabilities
        .push(shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into());
    let next_id = successor.runtime_instance_id.clone();
    let status_id = next_id.clone();
    let recovery_id = next_id.clone();
    let observed = Arc::new(AtomicBool::new(false));
    let http_observed = observed.clone();
    let http_launch = launch.clone();
    let http_root = root.clone();
    let router = axum::Router::new()
        .route("/v1/runtime/identity", axum::routing::get(move || {
            let (identity, launch, root, observed) = (successor.clone(), http_launch.clone(), http_root.clone(), http_observed.clone());
            async move {
                observed.store(true, Ordering::SeqCst);
                if !launch.exists() {
                    return (axum::http::StatusCode::SERVICE_UNAVAILABLE, axum::Json(serde_json::json!({"code":"ERR_INITIALIZING","message":"retiring old process","data":null})));
                }
                save(&root.join("identity.json"), &serde_json::to_value(&identity).unwrap());
                std::fs::write(root.join("token"), b"new-management-token").unwrap();
                (axum::http::StatusCode::OK, axum::Json(serde_json::to_value(shared_types::HttpResult::success(identity)).unwrap()))
            }
        }))
        .route("/v1/runtime/recovery", axum::routing::get(move || {
            let id = recovery_id.clone();
            async move { axum::Json(shared_types::HttpResult::success(shared_types::RuntimeRecoveryView {
                runtime_instance_id:id, deployment_generation_id:"deployment".into(), revision:7,
                kernel_protected:false, owner_protected:false, operation_id:None, boundary:None,
                generation_matches:None, credentials_required:false,
                migrations:shared_types::RuntimeMigrationRecoveryState::Unconfirmed,
            })) }
        }))
        .route("/v1/runtime/status", axum::routing::get(move || {
            let id = status_id.clone();
            async move { axum::Json(shared_types::HttpResult::success(shared_types::RuntimeStatusView {
                desired:shared_types::DesiredState::Stopped, observed:shared_types::ObservedHealth::Stopped,
                active_target:None, revision:7, active_operation_id:None, recovery_protection:false, runtime_instance_id:id,
            })) }
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let http_task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let next_native = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let next_address = next_native.local_addr().unwrap().to_string();
    let native_root = root.clone();
    let native_launch = launch.clone();
    let source_root = managed.source_root.clone();
    let native_request = request.clone();
    let mut current = terminal.clone();
    let native_observed = observed.clone();
    let native_statuses = Arc::new(AtomicUsize::new(0));
    let recorded_statuses = native_statuses.clone();
    let native_task = tokio::spawn(async move {
        while !native_observed.load(Ordering::SeqCst) {
            tokio::task::yield_now().await;
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !native_launch.exists(),
            "terminal metadata is insufficient while the old owner lease is held"
        );
        drop(owner_lock);
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !native_launch.exists(),
            "the real old worker must exit before management bootstrap"
        );
        assert!(process_utils::process_exists(old_pid).unwrap());
        worker.kill().await.unwrap();
        worker.wait().await.unwrap();
        assert!(!process_utils::process_exists(old_pid).unwrap());
        let successor_lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(native_root.join("owner.lock"))
            .unwrap();
        if !replacement {
            drop(worker_lock);
            while !native_launch.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            successor_lock.try_lock().unwrap();
        } else {
            loop {
                match successor_lock.try_lock() {
                    Ok(()) => break,
                    Err(std::fs::TryLockError::WouldBlock) => tokio::task::yield_now().await,
                    Err(error) => panic!("replacement owner lease: {error}"),
                }
            }
            drop(worker_lock);
        }
        current.supervisor_id = "new-legacy-native-supervisor".into();
        current.binding.resource = source_root;
        current.phase = Phase::Ready;
        current.intent = Intent::Stopped;
        current.operation_id = None;
        discovery(
            &native_root,
            &next_address,
            &current,
            &[(native_request, terminal)],
        );
        if replacement {
            std::fs::write(&native_launch, b"existing successor; no bootstrap").unwrap();
        }
        // The successor remains the live native owner throughout both initial
        // discovery and business preflight revalidation. A one-response server
        // would release its listener/lease and correctly be treated as absent.
        loop {
            let (mut stream, _) = next_native.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stream)
                .read_line(&mut line)
                .await
                .unwrap();
            let frame: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(
                frame["request"]["action"], "status",
                "a legacy wildcard Shutdown must never be sent to the successor"
            );
            recorded_statuses.fetch_add(1, Ordering::SeqCst);
            stream.write_all(format!("{}\n", serde_json::json!({"instance":current.supervisor_id,"snapshot":current,"error":null})).as_bytes()).await.unwrap();
        }
    });
    let mut config = crate::Config::from_env().unwrap();
    config.app_cli_admin_probe_addr = address;
    config.app_cli_bin = Some(program.display().to_string());
    config.log_base_dir = temp.path().join("logs");
    let manager = DevServerManager::new(Arc::new(config));
    let candidate = tokio::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let tracked = super::super::super::SupervisedChild::adopt(
        candidate,
        Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
    );
    assert!(tracked.wait_exit(Duration::from_secs(2)).await.is_some());
    lock(&manager.owner_children)
        .unwrap()
        .insert("11".into(), tracked);
    let outcome = tokio::time::timeout(
        Duration::from_secs(6),
        manager.preflight_userapp_build("11", &managed.source_root),
    )
    .await;
    // Only fixture-owned protocol resources are retired after the complete
    // operation; cleanup also runs before reporting a real product failure.
    native_task.abort();
    drop(native_task.await);
    http_task.abort();
    outcome
        .expect("legacy recovery deadline")
        .expect("the original source preflight must complete");
    assert!(
        native_statuses.load(Ordering::SeqCst) >= 2,
        "the same successor must remain observable for preflight revalidation"
    );
    assert_eq!(
        std::fs::read(&data).unwrap(),
        b"keep the source and application data"
    );
    assert_eq!(std::fs::read(&migration).unwrap(), original_migration);
    assert_eq!(
        runtime_supervisor::saved_request_snapshot(&root, &request)
            .unwrap()
            .unwrap()
            .supervisor_id,
        original_shutdown.supervisor_id
    );
    let discovery_bytes: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("supervisor.json")).unwrap()).unwrap();
    let old_request = &discovery_bytes["requests"][0][0];
    assert!(
        old_request["expected_generation"].is_null(),
        "the historical request must keep its original parameters"
    );
    assert_eq!(old_request["request_id"], request.request_id);
    let launched = std::fs::read_to_string(launch).unwrap();
    if replacement {
        assert_eq!(launched, "existing successor; no bootstrap");
        assert!(
            lock(&manager.owner_children)
                .unwrap()
                .get("11")
                .unwrap()
                .exited()
                .is_some(),
            "a concurrent successor must not spawn another management owner"
        );
    } else {
        assert!(launched.starts_with("serve\n--control-only\n--workspace\n"));
    }
    assert!(
        matches!(lock(&manager.owner_expectations).unwrap().get("11"), Some(super::super::super::types::OwnerExpectation::Captured { runtime_instance_id, .. }) if runtime_instance_id == &next_id)
    );
}
