//! Real local owner locks and authenticated control frames. These fixtures do
//! not run proxy business or claim that an unknown stop completed.
use super::*;
use runtime_supervisor::{Binding, Intent, Phase, Policy, Snapshot};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    sync::oneshot,
};

fn stopped_snapshot(root: &Path) -> Snapshot {
    Snapshot {
        version: 1,
        binding: Binding {
            component: "file-server-proxy".into(),
            resource: root.canonicalize().unwrap(),
        },
        supervisor_id: uuid::Uuid::new_v4().to_string(),
        generation: Some(uuid::Uuid::new_v4().to_string()),
        phase: Phase::Stopped,
        intent: Intent::Shutdown,
        operation_id: None,
        error: None,
        problem: None,
    }
}

fn save_discovery(root: &Path, address: std::net::SocketAddr, snapshot: &Snapshot) -> Vec<u8> {
    let bytes = serde_json::to_vec(&serde_json::json!({
        "version": 2, "instance": snapshot.supervisor_id, "address": address.to_string(),
        "token": "private-fixture-authentication-token", "snapshot": snapshot, "requests": []
    }))
    .unwrap();
    std::fs::write(root.join("supervisor.json"), &bytes).unwrap();
    bytes
}

async fn status_request(stream: &mut tokio::net::TcpStream, supervisor: &str) {
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await.unwrap();
    let value: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(value["version"], 2);
    assert_eq!(value["instance"], supervisor);
    assert_eq!(value["token"], "private-fixture-authentication-token");
    assert_eq!(
        value["request"]["action"], "status",
        "no write may be issued"
    );
}

#[tokio::test]
async fn offline_status_observer_lock_cannot_turn_original_start_into_stopped_reuse() {
    let directory = tempfile::tempdir().unwrap();
    let observer = runtime_supervisor::Owner::try_acquire(directory.path())
        .unwrap()
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let snapshot = stopped_snapshot(directory.path());
    let original = save_discovery(directory.path(), listener.local_addr().unwrap(), &snapshot);
    let supervisor = snapshot.supervisor_id.clone();
    let (received, request_seen) = oneshot::channel();
    let (release, may_close) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        status_request(&mut stream, &supervisor).await;
        received.send(()).unwrap();
        may_close.await.unwrap();
        // The observation ends without a live reply. The old native control
        // fallback would now acquire the free lock and project old Stopped.
        drop(stream);
    });
    let root = directory.path().to_owned();
    let start = tokio::spawn(async move { select_start_owner(&root, &Policy::default()).await });
    tokio::time::timeout(Duration::from_secs(5), request_seen)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !start.is_finished(),
        "the status barrier must precede the start result"
    );
    drop(observer);
    release.send(()).unwrap();
    match tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    {
        StartOwner::Owned {
            owner,
            remaining_initialization,
        } => {
            assert!(remaining_initialization > Duration::ZERO);
            assert!(remaining_initialization <= Policy::default().initialization_timeout);
            assert!(
                runtime_supervisor::Owner::try_acquire(directory.path())
                    .unwrap()
                    .is_none()
            );
            drop(owner);
        }
        StartOwner::Reused(reply) => panic!("offline observation is not a reusable owner: {reply}"),
    }
    server.await.unwrap();
    assert_eq!(
        std::fs::read(directory.path().join("supervisor.json")).unwrap(),
        original
    );
}

#[cfg(unix)]
#[tokio::test]
async fn start_retry_keeps_the_original_canonical_scope_when_an_alias_changes() {
    let directory = tempfile::tempdir().unwrap();
    let original_root = directory.path().join("original");
    let other_root = directory.path().join("other");
    let observer = runtime_supervisor::Owner::try_acquire(&original_root)
        .unwrap()
        .unwrap();
    let _other_owner = runtime_supervisor::Owner::try_acquire(&other_root)
        .unwrap()
        .unwrap();
    let alias = directory.path().join("alias");
    std::os::unix::fs::symlink(&original_root, &alias).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let snapshot = stopped_snapshot(&original_root);
    let original = save_discovery(&original_root, listener.local_addr().unwrap(), &snapshot);
    let (seen, request_seen) = oneshot::channel();
    let (release, may_close) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        status_request(&mut stream, &snapshot.supervisor_id).await;
        seen.send(()).unwrap();
        may_close.await.unwrap();
        drop(stream);
    });
    let requested = alias.clone();
    let start =
        tokio::spawn(async move { select_start_owner(&requested, &Policy::default()).await });
    tokio::time::timeout(Duration::from_secs(5), request_seen)
        .await
        .unwrap()
        .unwrap();
    std::fs::remove_file(&alias).unwrap();
    std::os::unix::fs::symlink(&other_root, &alias).unwrap();
    drop(observer);
    release.send(()).unwrap();
    match tokio::time::timeout(Duration::from_secs(5), start)
        .await
        .unwrap()
        .unwrap()
        .unwrap()
    {
        StartOwner::Owned { owner, .. } => {
            assert_eq!(owner.scope_root(), original_root.canonicalize().unwrap())
        }
        StartOwner::Reused(_) => {
            panic!("a changed alias cannot supply another scope's reuse authority")
        }
    }
    server.await.unwrap();
    assert_eq!(
        std::fs::read(original_root.join("supervisor.json")).unwrap(),
        original
    );
    assert!(!other_root.join("supervisor.json").exists());
}

async fn retained_live_status(snapshot: Snapshot, root: &Path) -> Result<StartOwner, String> {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let original = save_discovery(root, listener.local_addr().unwrap(), &snapshot);
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            status_request(&mut stream, &snapshot.supervisor_id).await;
            let mut reply = serde_json::to_vec(&serde_json::json!({
                "instance": snapshot.supervisor_id, "snapshot": snapshot, "error": null
            }))
            .unwrap();
            reply.push(b'\n');
            stream.write_all(&reply).await.unwrap();
        }
    });
    let policy = Policy {
        initialization_timeout: Duration::from_millis(50),
        ..Policy::default()
    };
    let result = select_start_owner(root, &policy).await;
    server.abort();
    drop(server.await);
    assert_eq!(
        std::fs::read(root.join("supervisor.json")).unwrap(),
        original,
        "neither current control nor unknown stop evidence may be rewritten"
    );
    assert!(
        runtime_supervisor::Owner::try_acquire(root)
            .unwrap()
            .is_none(),
        "a live holder is never displaced"
    );
    result
}

#[tokio::test]
async fn live_stop_or_cleanup_is_neither_successful_reuse_nor_authority_to_replace() {
    for phase in [
        Phase::Stopping,
        Phase::CleanupPending,
        Phase::RecoveryRequired,
        Phase::Stopped,
    ] {
        let directory = tempfile::tempdir().unwrap();
        let _holder = runtime_supervisor::Owner::try_acquire(directory.path())
            .unwrap()
            .unwrap();
        let mut snapshot = stopped_snapshot(directory.path());
        snapshot.phase = phase;
        snapshot.operation_id = Some("original-pending-stop".into());
        assert!(
            retained_live_status(snapshot, directory.path())
                .await
                .is_err(),
            "phase {phase}"
        );
    }
}

#[tokio::test]
async fn a_live_foreign_binding_never_authorizes_reuse_or_replacement() {
    for changed in ["component", "resource"] {
        let directory = tempfile::tempdir().unwrap();
        let _holder = runtime_supervisor::Owner::try_acquire(directory.path())
            .unwrap()
            .unwrap();
        let mut snapshot = stopped_snapshot(directory.path());
        match changed {
            "component" => snapshot.binding.component = "another-component".into(),
            "resource" => snapshot.binding.resource = directory.path().join("another-scope"),
            _ => unreachable!(),
        }
        match retained_live_status(snapshot, directory.path()).await {
            Err(error) => assert!(
                error.contains("Live native owner belongs to another component or scope"),
                "foreign {changed} must fail for its real binding, not a broken frame or deadline: {error}"
            ),
            Ok(StartOwner::Owned { .. }) => panic!("foreign {changed} cannot be replaced"),
            Ok(StartOwner::Reused(_)) => panic!("foreign {changed} cannot be reused"),
        }
    }
}

#[tokio::test]
async fn unreadable_discovery_under_a_real_holder_preserves_the_original_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let _holder = runtime_supervisor::Owner::try_acquire(directory.path())
        .unwrap()
        .unwrap();
    let original = b"{original unreadable supervisor record";
    std::fs::write(directory.path().join("supervisor.json"), original).unwrap();
    let policy = Policy {
        initialization_timeout: Duration::from_millis(30),
        ..Policy::default()
    };
    assert!(select_start_owner(directory.path(), &policy).await.is_err());
    assert_eq!(
        std::fs::read(directory.path().join("supervisor.json")).unwrap(),
        original
    );
    assert!(
        runtime_supervisor::Owner::try_acquire(directory.path())
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_free_scope_returns_its_real_lock_without_inventing_a_running_result() {
    let directory = tempfile::tempdir().unwrap();
    match select_start_owner(directory.path(), &Policy::default())
        .await
        .unwrap()
    {
        StartOwner::Owned {
            owner,
            remaining_initialization,
        } => {
            assert!(remaining_initialization > Duration::ZERO);
            assert!(
                runtime_supervisor::Owner::try_acquire(directory.path())
                    .unwrap()
                    .is_none()
            );
            assert!(!directory.path().join("supervisor.json").exists());
            drop(owner);
        }
        StartOwner::Reused(_) => {
            panic!("a free owner must be acquired, never fabricated as Running")
        }
    }
}

mod legacy_start_tests {
    //! Compatibility with a live version-one direct owner. Real scope locks and
    //! authenticated loopback frames; no proxy business readiness is fabricated.
    use super::*;
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };

    const TOKEN: &str = "private-legacy-owner-token-at-least-32-bytes";

    fn lock_scope(root: &Path) -> File {
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join("owner.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        lock
    }

    fn receipt(root: &Path, address: std::net::SocketAddr, instance: &str, phase: &str) -> Vec<u8> {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version":1,"instance_id":instance,"token":TOKEN,
            "control_address":address.to_string(),"address":address.to_string(),"phase":phase
        }))
        .unwrap();
        std::fs::write(root.join("owner.json"), &bytes).unwrap();
        bytes
    }

    async fn read_status(stream: &mut tokio::net::TcpStream, instance: &str) {
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).await.unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["version"], 1);
        assert_eq!(value["instance_id"], instance);
        assert_eq!(value["token"], TOKEN);
        assert_eq!(
            value["action"], "status",
            "legacy compatibility grants no write"
        );
    }

    #[tokio::test]
    async fn authenticated_live_legacy_owner_is_reused_without_a_supervisor_record() {
        for historical_phase in ["Running", "Stopped"] {
            for legacy_supervisor in [None, Some(uuid::Uuid::new_v4().to_string())] {
                let directory = tempfile::tempdir().unwrap();
                let _owner = lock_scope(directory.path());
                let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = listener.local_addr().unwrap();
                let instance = uuid::Uuid::new_v4().to_string();
                let mut original = receipt(directory.path(), address, &instance, historical_phase);
                if let Some(supervisor) = legacy_supervisor {
                    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
                    value["supervisor_id"] = serde_json::json!(supervisor);
                    original = serde_json::to_vec(&value).unwrap();
                    std::fs::write(directory.path().join("owner.json"), &original).unwrap();
                }
                let expected = instance.clone();
                let server = tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        read_status(&mut stream, &expected).await;
                        let mut bytes = serde_json::to_vec(&serde_json::json!({
                            "version":1,"instance_id":expected,"phase":"Running",
                            "address":address.to_string(),"error":null
                        }))
                        .unwrap();
                        bytes.push(b'\n');
                        stream.write_all(&bytes).await.unwrap();
                    }
                });
                let policy = Policy {
                    initialization_timeout: Duration::from_secs(1),
                    ..Default::default()
                };
                let result = select_start_owner(directory.path(), &policy).await;
                // npm waitRunning polls this public Rust control path rather than
                // consuming the detached launcher's stdout. Both must see the
                // actual live owner despite a stale historical Stopped phase.
                let polled = if result.is_ok() {
                    Some(
                        crate::native_control::control_with_request(
                            directory.path(),
                            "status",
                            None,
                            None,
                            None,
                        )
                        .await,
                    )
                } else {
                    None
                };
                server.abort();
                drop(server.await);
                match result {
                    Ok(StartOwner::Reused(body)) => {
                        let status: serde_json::Value = serde_json::from_str(&body).unwrap();
                        assert_eq!(status["instance_id"], instance);
                        assert_eq!(status["phase"], "Running");
                        assert_eq!(status["address"], address.to_string());
                        let status: serde_json::Value = serde_json::from_str(&polled.unwrap().expect(
                        "npm status polling must observe the authenticated live legacy owner",
                    ))
                    .unwrap();
                        assert_eq!(status["phase"], "Running");
                        assert_eq!(status["instance_id"], instance);
                    }
                    Ok(StartOwner::Owned { .. }) => {
                        panic!("a live legacy owner cannot be displaced")
                    }
                    Err(error) => panic!(
                        "actual legacy Running must be reused despite historical {historical_phase}: {error}"
                    ),
                }
                assert_eq!(
                    std::fs::read(directory.path().join("owner.json")).unwrap(),
                    original
                );
                assert!(!directory.path().join("supervisor.json").exists());
                assert!(
                    runtime_supervisor::Owner::try_acquire(directory.path())
                        .unwrap()
                        .is_none()
                );
            }
        }
    }

    async fn legacy_with_stale_supervisor(action: &str) {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        };
        // Windows reports a refused loopback connect after roughly two seconds.
        // Keep the same live protocol and assertions; only the fixture deadlines
        // must allow the OS to return its actual typed result.
        let observation_budget = if cfg!(windows) {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(1)
        };
        let control_budget = if cfg!(windows) {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(2)
        };
        let directory = tempfile::tempdir().unwrap();
        let owner = Arc::new(Mutex::new(Some(lock_scope(directory.path()))));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let original = receipt(directory.path(), address, &instance, "Stopped");
        let previous = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dead_address = previous.local_addr().unwrap();
        drop(previous);
        let refused = tokio::time::timeout(
            observation_budget,
            tokio::net::TcpStream::connect(dead_address),
        )
        .await
        .unwrap()
        .unwrap_err();
        assert_eq!(
            refused.kind(),
            std::io::ErrorKind::ConnectionRefused,
            "this fixture requires typed refusal, not an unknown or timed-out RPC"
        );
        let old = stopped_snapshot(directory.path());
        let discovery = save_discovery(directory.path(), dead_address, &old);
        let captured = instance.clone();
        let held = owner.clone();
        let status_count = Arc::new(AtomicUsize::new(0));
        let stop_count = Arc::new(AtomicUsize::new(0));
        let statuses = status_count.clone();
        let stops = stop_count.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut line = String::new();
                BufReader::new(&mut stream)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["version"], 1);
                assert_eq!(request["instance_id"], captured);
                assert_eq!(request["token"], TOKEN);
                let phase = match request["action"].as_str().unwrap() {
                    "status" => {
                        statuses.fetch_add(1, Ordering::SeqCst);
                        "Running"
                    }
                    "stop" => {
                        stops.fetch_add(1, Ordering::SeqCst);
                        drop(held.lock().unwrap().take());
                        "Stopped"
                    }
                    other => panic!("unexpected legacy side effect: {other}"),
                };
                let mut reply = serde_json::to_vec(&serde_json::json!({
                    "version":1,"instance_id":captured,"phase":phase,
                    "address":address.to_string(),"error":null
                }))
                .unwrap();
                reply.push(b'\n');
                stream.write_all(&reply).await.unwrap();
            }
        });
        let result = if action == "start" {
            match select_start_owner(
                directory.path(),
                &Policy {
                    initialization_timeout: observation_budget,
                    ..Default::default()
                },
            )
            .await
            {
                Ok(StartOwner::Reused(body)) => Ok(body),
                Ok(StartOwner::Owned { .. }) => Err("live legacy owner was displaced".into()),
                Err(error) => Err(error),
            }
        } else {
            tokio::time::timeout(
                control_budget,
                crate::native_control::control_with_request(
                    directory.path(),
                    action,
                    Some(&instance),
                    None,
                    None,
                ),
            )
            .await
            .map_err(|_| "test control deadline elapsed".to_owned())
            .and_then(|result| result)
        };
        server.abort();
        drop(server.await);
        assert_eq!(
            std::fs::read(directory.path().join("supervisor.json")).unwrap(),
            discovery
        );
        assert_eq!(
            std::fs::read(directory.path().join("owner.json")).unwrap(),
            original
        );
        let value: serde_json::Value = serde_json::from_str(&result.unwrap_or_else(|error|
            panic!("{action} must reach the authenticated legacy owner despite stale v2 locator: {error}"))).unwrap();
        assert_eq!(value["instance_id"], instance);
        assert_eq!(
            value["phase"],
            if action == "stop" {
                "Stopped"
            } else {
                "Running"
            }
        );
        assert!(
            status_count.load(Ordering::SeqCst) > 0,
            "actual authenticated Status must be observed before reuse or Stop"
        );
        assert_eq!(
            stop_count.load(Ordering::SeqCst),
            usize::from(action == "stop")
        );
        assert_eq!(
            runtime_supervisor::Owner::try_acquire(directory.path())
                .unwrap()
                .is_some(),
            action == "stop"
        );
    }

    #[tokio::test]
    async fn stale_supervisor_locator_does_not_hide_live_legacy_start() {
        legacy_with_stale_supervisor("start").await;
    }

    #[tokio::test]
    async fn stale_supervisor_locator_does_not_hide_live_legacy_status() {
        legacy_with_stale_supervisor("status").await;
    }

    #[tokio::test]
    async fn stale_supervisor_locator_does_not_hide_live_legacy_stop() {
        legacy_with_stale_supervisor("stop").await;
    }

    #[tokio::test]
    async fn live_supervisor_always_wins_over_an_available_legacy_endpoint() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for variant in ["ready", "pending-stop", "foreign"] {
            for entry in ["start", "status", "stop"] {
                let directory = tempfile::tempdir().unwrap();
                let _owner = lock_scope(directory.path());
                let legacy = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address = legacy.local_addr().unwrap();
                let mut snapshot = stopped_snapshot(directory.path());
                snapshot.phase = if variant == "pending-stop" {
                    Phase::Stopping
                } else {
                    Phase::Ready
                };
                if variant == "foreign" {
                    snapshot.binding.component = "another-component".into();
                }
                let instance = snapshot.generation.clone().unwrap();
                let original = receipt(directory.path(), address, &instance, "Running");
                let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
                value["supervisor_id"] = serde_json::json!(snapshot.supervisor_id);
                let original = serde_json::to_vec(&value).unwrap();
                std::fs::write(directory.path().join("owner.json"), &original).unwrap();
                let modern = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let discovery =
                    save_discovery(directory.path(), modern.local_addr().unwrap(), &snapshot);
                let legacy_count = Arc::new(AtomicUsize::new(0));
                let seen = legacy_count.clone();
                let legacy_server = tokio::spawn(async move {
                    loop {
                        let _stream = legacy.accept().await.unwrap();
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                });
                let modern_status = Arc::new(AtomicUsize::new(0));
                let modern_stop = Arc::new(AtomicUsize::new(0));
                let statuses = modern_status.clone();
                let stops = modern_stop.clone();
                let captured = snapshot.clone();
                let modern_server = tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = modern.accept().await.unwrap();
                        let mut line = String::new();
                        BufReader::new(&mut stream)
                            .read_line(&mut line)
                            .await
                            .unwrap();
                        let request: serde_json::Value = serde_json::from_str(&line).unwrap();
                        assert_eq!(request["version"], 2);
                        assert_eq!(request["instance"], captured.supervisor_id);
                        assert_eq!(request["token"], "private-fixture-authentication-token");
                        let error = match request["request"]["action"].as_str().unwrap() {
                            "status" => {
                                statuses.fetch_add(1, Ordering::SeqCst);
                                serde_json::Value::Null
                            }
                            "shutdown" => {
                                stops.fetch_add(1, Ordering::SeqCst);
                                assert_eq!(
                                    request["request"]["expected_generation"],
                                    captured.generation.as_deref().unwrap()
                                );
                                serde_json::json!({"code":"busy","message":"original supervisor Stop remains pending"})
                            }
                            other => panic!("unexpected modern side effect: {other}"),
                        };
                        let mut reply = serde_json::to_vec(&serde_json::json!({"instance":captured.supervisor_id,"snapshot":captured,"error":error})).unwrap();
                        reply.push(b'\n');
                        stream.write_all(&reply).await.unwrap();
                    }
                });
                let result = if entry == "start" {
                    match select_start_owner(
                        directory.path(),
                        &Policy {
                            initialization_timeout: Duration::from_millis(50),
                            ..Default::default()
                        },
                    )
                    .await
                    {
                        Ok(StartOwner::Reused(reply)) => Ok(reply),
                        Ok(StartOwner::Owned { .. }) => Err("a live owner was displaced".into()),
                        Err(error) => Err(error),
                    }
                } else {
                    crate::native_control::control_with_request(
                        directory.path(),
                        entry,
                        None,
                        None,
                        None,
                    )
                    .await
                };
                legacy_server.abort();
                modern_server.abort();
                drop(legacy_server.await);
                drop(modern_server.await);
                assert!(modern_status.load(Ordering::SeqCst) > 0);
                assert_eq!(
                    legacy_count.load(Ordering::SeqCst),
                    0,
                    "{variant}/{entry}: live modern authority cannot downgrade"
                );
                assert_eq!(
                    modern_stop.load(Ordering::SeqCst),
                    usize::from(entry == "stop" && variant != "foreign")
                );
                if variant == "foreign" {
                    assert!(
                        result
                            .unwrap_err()
                            .contains("Live native owner belongs to another component or scope")
                    );
                } else if entry == "stop" {
                    assert!(
                        result
                            .unwrap_err()
                            .contains("original supervisor Stop remains pending")
                    );
                } else if entry == "start" && variant == "pending-stop" {
                    assert!(result.is_err());
                } else {
                    let reply: serde_json::Value = serde_json::from_str(&result.unwrap()).unwrap();
                    assert_eq!(reply["version"], 2);
                    assert_eq!(reply["supervisor_id"], snapshot.supervisor_id);
                    assert_eq!(
                        reply["phase"],
                        if variant == "pending-stop" {
                            "Stopping"
                        } else {
                            "Running"
                        }
                    );
                }
                assert_eq!(
                    std::fs::read(directory.path().join("supervisor.json")).unwrap(),
                    discovery
                );
                assert_eq!(
                    std::fs::read(directory.path().join("owner.json")).unwrap(),
                    original
                );
            }
        }
    }

    #[tokio::test]
    async fn unknown_supervisor_reply_never_selects_legacy_or_sends_stop() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for variant in ["no-reply", "malformed"] {
            for entry in ["start", "status", "stop"] {
                let directory = tempfile::tempdir().unwrap();
                let _owner = lock_scope(directory.path());
                let legacy = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let original = receipt(
                    directory.path(),
                    legacy.local_addr().unwrap(),
                    &uuid::Uuid::new_v4().to_string(),
                    "Running",
                );
                let modern = TcpListener::bind("127.0.0.1:0").await.unwrap();
                let snapshot = stopped_snapshot(directory.path());
                let discovery =
                    save_discovery(directory.path(), modern.local_addr().unwrap(), &snapshot);
                let legacy_count = Arc::new(AtomicUsize::new(0));
                let seen = legacy_count.clone();
                let legacy_server = tokio::spawn(async move {
                    loop {
                        let _stream = legacy.accept().await.unwrap();
                        seen.fetch_add(1, Ordering::SeqCst);
                    }
                });
                let modern_count = Arc::new(AtomicUsize::new(0));
                let seen = modern_count.clone();
                let modern_server = tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = modern.accept().await.unwrap();
                        status_request(&mut stream, &snapshot.supervisor_id).await;
                        seen.fetch_add(1, Ordering::SeqCst);
                        if variant == "no-reply" {
                            tokio::time::sleep(Duration::from_secs(10)).await;
                        } else {
                            stream
                                .write_all(b"not-a-valid-supervisor-frame\n")
                                .await
                                .unwrap();
                        }
                    }
                });
                let result = if entry == "start" {
                    match select_start_owner(
                        directory.path(),
                        &Policy {
                            initialization_timeout: Duration::from_millis(50),
                            ..Default::default()
                        },
                    )
                    .await
                    {
                        Ok(StartOwner::Reused(reply)) => Ok(reply),
                        Ok(StartOwner::Owned { .. }) => Err("a live owner was displaced".into()),
                        Err(error) => Err(error),
                    }
                } else {
                    // Cancelling this read-only pre-admission observation cannot
                    // authorize a second protocol or deliver either Stop write.
                    tokio::time::timeout(
                        Duration::from_millis(100),
                        crate::native_control::control_with_request(
                            directory.path(),
                            entry,
                            None,
                            None,
                            None,
                        ),
                    )
                    .await
                    .map_err(|_| "test cancelled an unknown live observation".to_owned())
                    .and_then(|result| result)
                };
                legacy_server.abort();
                modern_server.abort();
                drop(legacy_server.await);
                drop(modern_server.await);
                assert!(result.is_err(), "{variant}/{entry}");
                assert!(modern_count.load(Ordering::SeqCst) > 0);
                assert_eq!(
                    legacy_count.load(Ordering::SeqCst),
                    0,
                    "unknown modern reply is not connection refusal"
                );
                assert_eq!(
                    std::fs::read(directory.path().join("supervisor.json")).unwrap(),
                    discovery
                );
                assert_eq!(
                    std::fs::read(directory.path().join("owner.json")).unwrap(),
                    original
                );
            }
        }
    }

    #[tokio::test]
    async fn captured_control_never_downgrades_to_legacy_after_supervisor_refusal() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        for captured in ["supervisor", "request"] {
            let directory = tempfile::tempdir().unwrap();
            let _owner = lock_scope(directory.path());
            let legacy = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let original = receipt(
                directory.path(),
                legacy.local_addr().unwrap(),
                &uuid::Uuid::new_v4().to_string(),
                "Running",
            );
            let dead = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let dead_address = dead.local_addr().unwrap();
            drop(dead);
            let snapshot = stopped_snapshot(directory.path());
            let discovery = save_discovery(directory.path(), dead_address, &snapshot);
            let count = Arc::new(AtomicUsize::new(0));
            let seen = count.clone();
            let server = tokio::spawn(async move {
                loop {
                    let _stream = legacy.accept().await.unwrap();
                    seen.fetch_add(1, Ordering::SeqCst);
                }
            });
            let result = crate::native_control::control_with_request(
                directory.path(),
                "stop",
                None,
                (captured == "request").then_some("original-control-request"),
                (captured == "supervisor").then_some(snapshot.supervisor_id.as_str()),
            )
            .await;
            server.abort();
            drop(server.await);
            assert!(result.is_err());
            assert_eq!(count.load(Ordering::SeqCst), 0);
            assert_eq!(
                std::fs::read(directory.path().join("supervisor.json")).unwrap(),
                discovery
            );
            assert_eq!(
                std::fs::read(directory.path().join("owner.json")).unwrap(),
                original
            );
        }
    }

    #[tokio::test]
    async fn an_unknown_legacy_stop_ack_keeps_the_original_target_and_is_not_replayed() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let directory = tempfile::tempdir().unwrap();
        let _owner = lock_scope(directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let original = receipt(directory.path(), address, &instance, "Stopped");
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        let captured = instance.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_status(&mut stream, &captured).await;
            let mut reply = serde_json::to_vec(&serde_json::json!({"version":1,"instance_id":captured,"phase":"Running","address":address.to_string(),"error":null})).unwrap();
            reply.push(b'\n');
            stream.write_all(&reply).await.unwrap();
            let (mut stop, _) = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stop)
                .read_line(&mut line)
                .await
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["action"], "stop");
            assert_eq!(request["instance_id"], captured);
            assert_eq!(request["token"], TOKEN);
            seen.fetch_add(1, Ordering::SeqCst);
            drop(stop);
            // Keep the endpoint available to detect any accidental retry.
            while let Ok((mut retry, _)) = listener.accept().await {
                let mut line = String::new();
                BufReader::new(&mut retry)
                    .read_line(&mut line)
                    .await
                    .unwrap();
                seen.fetch_add(1, Ordering::SeqCst);
            }
        });
        let result = crate::native_control::control_with_request(
            directory.path(),
            "stop",
            Some(&instance),
            None,
            None,
        )
        .await;
        assert!(result.unwrap_err().contains("invalid owner response"));
        tokio::task::yield_now().await;
        server.abort();
        drop(server.await);
        assert_eq!(
            count.load(Ordering::SeqCst),
            1,
            "an unknown Stop cannot select or mutate another target"
        );
        assert_eq!(
            std::fs::read(directory.path().join("owner.json")).unwrap(),
            original
        );
        assert!(
            runtime_supervisor::Owner::try_acquire(directory.path())
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn legacy_probe_keeps_replacement_stop_and_real_lock_protections() {
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        };
        for variant in [
            "reply-identity",
            "replacement",
            "token",
            "control",
            "supervisor",
            "supervisor-locator",
            "stopped",
            "unbound",
            "lock-released",
        ] {
            let directory = tempfile::tempdir().unwrap();
            let owner = Arc::new(Mutex::new(Some(lock_scope(directory.path()))));
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let instance = uuid::Uuid::new_v4().to_string();
            let original = receipt(directory.path(), address, &instance, "Running");
            let root = directory.path().to_owned();
            let captured = instance.clone();
            let held = owner.clone();
            let delivered = Arc::new(AtomicBool::new(false));
            let seen = delivered.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                read_status(&mut stream, &captured).await;
                seen.store(true, Ordering::SeqCst);
                if variant == "replacement" {
                    receipt(&root, address, &uuid::Uuid::new_v4().to_string(), "Running");
                }
                if ["token", "control", "supervisor"].contains(&variant) {
                    let mut value: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(root.join("owner.json")).unwrap())
                            .unwrap();
                    match variant {
                        "token" => {
                            value["token"] = serde_json::json!(
                                "a-different-current-private-token-at-least-32-bytes"
                            )
                        }
                        "control" => value["control_address"] = serde_json::json!("127.0.0.1:1"),
                        "supervisor" => {
                            value["supervisor_id"] =
                                serde_json::json!(uuid::Uuid::new_v4().to_string())
                        }
                        _ => unreachable!(),
                    }
                    std::fs::write(root.join("owner.json"), serde_json::to_vec(&value).unwrap())
                        .unwrap();
                }
                if variant == "supervisor-locator" {
                    std::fs::write(root.join("supervisor.json"), b"a replacement locator").unwrap();
                }
                if variant == "lock-released" {
                    drop(held.lock().unwrap().take());
                }
                let mut reply = serde_json::to_vec(&serde_json::json!({
                "version":1,
                "instance_id":if variant == "reply-identity" { uuid::Uuid::new_v4().to_string() } else { captured },
                "phase":if variant == "stopped" { "Stopped" } else { "Running" },
                "address":if variant == "unbound" { "127.0.0.1:0".into() } else { address.to_string() },
                "error":null
            })).unwrap();
                reply.push(b'\n');
                stream.write_all(&reply).await.unwrap();
            });
            let result = crate::native_control::observe_live_legacy_owner(directory.path()).await;
            server.abort();
            drop(server.await);
            assert!(
                delivered.load(Ordering::SeqCst),
                "{variant} must reach the real authenticated frame"
            );
            let error = result.unwrap_err();
            let expected = match variant {
                "reply-identity" => "owner response identity mismatch",
                "replacement" | "token" | "control" | "supervisor" => "receipt identity changed",
                "supervisor-locator" => "supervisor locator changed",
                "stopped" => "live legacy owner is Stopped",
                "unbound" => "address is unbound",
                "lock-released" => "no longer holds its physical scope",
                _ => unreachable!(),
            };
            assert!(error.contains(expected), "{variant}: {error}");
            if !["replacement", "token", "control", "supervisor"].contains(&variant) {
                assert_eq!(
                    std::fs::read(directory.path().join("owner.json")).unwrap(),
                    original
                );
            }
        }
    }

    #[tokio::test]
    async fn unreadable_supervisor_discovery_is_not_legacy_protocol_absence() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        };
        let directory = tempfile::tempdir().unwrap();
        let _owner = lock_scope(directory.path());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let original = receipt(
            directory.path(),
            listener.local_addr().unwrap(),
            &uuid::Uuid::new_v4().to_string(),
            "Running",
        );
        std::fs::create_dir(directory.path().join("supervisor.json")).unwrap();
        let delivered = Arc::new(AtomicBool::new(false));
        let observed = delivered.clone();
        let server = tokio::spawn(async move {
            let _stream = listener.accept().await.unwrap();
            observed.store(true, Ordering::SeqCst);
        });
        let policy = Policy {
            initialization_timeout: Duration::from_millis(30),
            ..Default::default()
        };
        assert!(select_start_owner(directory.path(), &policy).await.is_err());
        server.abort();
        drop(server.await);
        assert!(
            !delivered.load(Ordering::SeqCst),
            "IO failure cannot silently downgrade to legacy"
        );
        assert_eq!(
            std::fs::read(directory.path().join("owner.json")).unwrap(),
            original
        );
        assert!(directory.path().join("supervisor.json").is_dir());
    }

    #[tokio::test]
    async fn explicit_stop_reaches_the_exact_live_legacy_owner_despite_old_stopped_phase() {
        use std::sync::{Arc, Mutex};
        let directory = tempfile::tempdir().unwrap();
        let owner = Arc::new(Mutex::new(Some(lock_scope(directory.path()))));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        let original = receipt(directory.path(), address, &instance, "Stopped");
        let captured = instance.clone();
        let held = owner.clone();
        let server = tokio::spawn(async move {
            let (mut status, _) = listener.accept().await.unwrap();
            read_status(&mut status, &captured).await;
            let mut reply = serde_json::to_vec(&serde_json::json!({
                "version":1,"instance_id":captured,"phase":"Running",
                "address":address.to_string(),"error":null
            }))
            .unwrap();
            reply.push(b'\n');
            status.write_all(&reply).await.unwrap();
            let (mut stop, _) = listener.accept().await.unwrap();
            let mut line = String::new();
            BufReader::new(&mut stop)
                .read_line(&mut line)
                .await
                .unwrap();
            let request: serde_json::Value = serde_json::from_str(&line).unwrap();
            assert_eq!(request["version"], 1);
            assert_eq!(request["action"], "stop");
            assert_eq!(request["instance_id"], captured);
            assert_eq!(request["token"], TOKEN);
            drop(held.lock().unwrap().take());
            let mut reply = serde_json::to_vec(&serde_json::json!({
                "version":1,"instance_id":captured,"phase":"Stopped","address":"","error":null
            }))
            .unwrap();
            reply.push(b'\n');
            stop.write_all(&reply).await.unwrap();
        });
        let result = crate::native_control::control_with_request(
            directory.path(),
            "stop",
            Some(&instance),
            None,
            None,
        )
        .await;
        if result.is_err() {
            server.abort();
        }
        let settled = server.await;
        let status: serde_json::Value = serde_json::from_str(
            &result.expect("an explicit current Stop cannot be rejected by old phase"),
        )
        .unwrap();
        settled.unwrap();
        assert_eq!(status["phase"], "Stopped");
        assert_eq!(status["instance_id"], instance);
        assert_eq!(
            std::fs::read(directory.path().join("owner.json")).unwrap(),
            original
        );
        assert!(
            runtime_supervisor::Owner::try_acquire(directory.path())
                .unwrap()
                .is_some()
        );
    }
}
