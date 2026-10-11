//! Real Unix RPC fault injection, not in-memory timer helpers.
#![cfg(unix)]
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::time::{Duration, Instant};

const TRUE: &str = "<methodResponse><params><param><value><boolean>1</boolean></value></param></params></methodResponse>";

async fn request(stream: &mut tokio::net::UnixStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
    }
    let header = String::from_utf8(bytes).unwrap();
    let length: usize = header
        .lines()
        .find_map(|line| line.strip_prefix("Content-Length: "))
        .unwrap()
        .parse()
        .unwrap();
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    String::from_utf8(body).unwrap()
}

async fn response(stream: &mut tokio::net::UnixStream, body: &str) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn slow_rpc_reads_and_each_mutation_consume_the_original_absolute_deadline() {
    for (method, access) in [
        ("supervisor.getAllProcessInfo", RpcAccess::Observation),
        ("supervisor.getProcessInfo", RpcAccess::Observation),
        ("supervisor.reloadConfig", RpcAccess::Mutation),
        ("supervisor.addProcessGroup", RpcAccess::Mutation),
        ("supervisor.startProcess", RpcAccess::Mutation),
        ("supervisor.stopProcessGroup", RpcAccess::Cleanup),
        ("supervisor.removeProcessGroup", RpcAccess::Cleanup),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let socket = temporary.path().join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let (received, reached) = tokio::sync::oneshot::channel();
        let (reply, release) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(request(&mut stream).await.contains(method));
            received.send(()).unwrap();
            release.await.unwrap();
            // An observation may cancel its socket; a dispatched write retains it.
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{TRUE}",
                        TRUE.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let client = SupervisorClient::new(socket);
        let start = Instant::now();
        let result = client
            .call_until(
                method,
                &[PINGAP_NAME.into()],
                start + Duration::from_millis(50),
                access,
            )
            .await;
        reached.await.unwrap();
        let error = result.unwrap_err();
        assert!(
            start.elapsed() < Duration::from_millis(150),
            "{method} escaped deadline: {error:#}"
        );
        match access {
            RpcAccess::Observation => {
                assert!(!error.is::<crate::supervisor::ShutdownUnconfirmed>())
            }
            RpcAccess::Mutation | RpcAccess::Cleanup => assert!(
                error.is::<crate::supervisor::ShutdownUnconfirmed>(),
                "{method}: {error:#}"
            ),
        }
        reply.send(()).unwrap();
        server.await.unwrap();
        client
            .wait_mutations_until(Instant::now() + Duration::from_secs(1))
            .await
            .unwrap();
    }
}

const PINGAP_NAME: &str = "app-pingap";

#[tokio::test]
async fn late_start_keeps_the_shared_fence_and_expired_stop_never_targets_the_new_instance() {
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (received, reached) = tokio::sync::oneshot::channel();
    let (release, late_start) = tokio::sync::oneshot::channel();
    let executed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let actual = executed.clone();
    let server = tokio::spawn(async move {
        let (mut old, _) = listener.accept().await.unwrap();
        assert!(request(&mut old).await.contains("supervisor.startProcess"));
        received.send(()).unwrap();
        late_start.await.unwrap();
        actual.lock().unwrap().push("start-old");
        response(&mut old, TRUE).await;
        drop(old);
        for (method, action) in [
            ("supervisor.stopProcessGroup", "stop-old"),
            ("supervisor.removeProcessGroup", "remove-old"),
            ("supervisor.startProcess", "start-new"),
        ] {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(request(&mut stream).await.contains(method));
            actual.lock().unwrap().push(action);
            response(&mut stream, TRUE).await;
        }
    });
    let client = SupervisorClient::new(socket.clone());
    let old = client
        .start_process_wait_until(PINGAP_NAME, Instant::now() + Duration::from_millis(40))
        .await
        .unwrap_err();
    reached.await.unwrap();
    assert!(old.is::<crate::supervisor::ShutdownUnconfirmed>());
    // A brand-new client and the old, unbounded API cannot bypass the same fence.
    let second = SupervisorClient::new(socket);
    let blocked = second.start_process_wait(PINGAP_NAME).await.unwrap_err();
    assert!(
        is_before_dispatch(&blocked),
        "normal writes must reject, not queue: {blocked:#}"
    );
    let stop = second
        .stop_remove_group_until(PINGAP_NAME, Instant::now() + Duration::from_millis(30))
        .await
        .unwrap_err();
    assert!(stop.is::<crate::supervisor::ShutdownUnconfirmed>());
    assert!(executed.lock().unwrap().is_empty());
    release.send(()).unwrap();
    client
        .wait_mutations_until(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    assert_eq!(&*executed.lock().unwrap(), &["start-old"]);
    second
        .stop_remove_group_until(PINGAP_NAME, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    second
        .start_process_wait_until(PINGAP_NAME, Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(
        &*executed.lock().unwrap(),
        &["start-old", "stop-old", "remove-old", "start-new"]
    );
}

#[tokio::test]
async fn cancelled_caller_retains_dispatched_request_until_actual_response() {
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (received, reached) = tokio::sync::oneshot::channel();
    let (release, reply) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        assert!(
            request(&mut stream)
                .await
                .contains("supervisor.startProcess")
        );
        received.send(()).unwrap();
        reply.await.unwrap();
        response(&mut stream, TRUE).await;
    });
    let path = socket.clone();
    let caller = tokio::spawn(async move {
        SupervisorClient::new(path)
            .start_process_wait_until(PINGAP_NAME, Instant::now() + Duration::from_secs(1))
            .await
    });
    reached.await.unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    let client = SupervisorClient::new(socket);
    assert!(
        client
            .wait_mutations_until(Instant::now() + Duration::from_millis(30))
            .await
            .unwrap_err()
            .is::<crate::supervisor::ShutdownUnconfirmed>()
    );
    release.send(()).unwrap();
    server.await.unwrap();
    client
        .wait_mutations_until(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
}

#[tokio::test]
async fn different_group_cleanup_is_concurrent_but_excludes_start_and_owner_retirement() {
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let arrived = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let release = std::sync::Arc::new(tokio::sync::Barrier::new(3));
    let server_arrived = arrived.clone();
    let server_release = release.clone();
    let server = tokio::spawn(async move {
        let mut handlers = Vec::new();
        for _ in 0..4 {
            let (mut stream, _) = listener.accept().await.unwrap();
            let arrived = server_arrived.clone();
            let release = server_release.clone();
            handlers.push(tokio::spawn(async move {
                let body = request(&mut stream).await;
                assert!(body.contains("app-svc-one") || body.contains("app-svc-two"));
                if body.contains("supervisor.stopProcessGroup") {
                    arrived.wait().await;
                    release.wait().await;
                } else {
                    assert!(body.contains("supervisor.removeProcessGroup"));
                }
                response(&mut stream, TRUE).await;
            }));
        }
        for handler in handlers {
            handler.await.unwrap();
        }
    });
    let first_path = socket.clone();
    let second_path = socket.clone();
    let deadline = Instant::now() + Duration::from_millis(700);
    let first = tokio::spawn(async move {
        SupervisorClient::new(first_path)
            .stop_remove_group_until("app-svc-one", deadline)
            .await
    });
    let second = tokio::spawn(async move {
        SupervisorClient::new(second_path)
            .stop_remove_group_until("app-svc-two", deadline)
            .await
    });
    tokio::time::timeout(Duration::from_millis(400), arrived.wait()).await
        .expect("two different business groups must begin physical Stop together, not multiply the shutdown grace");
    let client = SupervisorClient::new(socket);
    let blocked = client.start_process_wait(PINGAP_NAME).await.unwrap_err();
    assert!(
        is_before_dispatch(&blocked),
        "Start cannot overlap any pending cleanup: {blocked:#}"
    );
    assert!(
        client
            .wait_mutations_until(Instant::now() + Duration::from_millis(25))
            .await
            .unwrap_err()
            .is::<crate::supervisor::ShutdownUnconfirmed>()
    );
    release.wait().await;
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    server.await.unwrap();
    client
        .wait_mutations_until(Instant::now() + Duration::from_millis(100))
        .await
        .unwrap();
}

#[tokio::test]
async fn expired_before_dispatch_has_no_socket_side_effect_and_is_not_unknown() {
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let client = SupervisorClient::new(socket);
    let error = client
        .start_process_wait_until(PINGAP_NAME, Instant::now())
        .await
        .unwrap_err();
    assert!(is_before_dispatch(&error));
    assert!(!error.is::<crate::supervisor::ShutdownUnconfirmed>());
    assert!(
        tokio::time::timeout(Duration::from_millis(30), listener.accept())
            .await
            .is_err()
    );
    assert!(
        client
            .call_until(
                "supervisor.getAllProcessInfo",
                &[],
                Instant::now(),
                RpcAccess::Observation
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn deadline_before_attempt_atomically_revokes_the_retained_socket_task() {
    dispatch_interleaving(DispatchHookPhase::BeforeAttempt, false).await;
}

#[tokio::test]
async fn deadline_after_attempt_reports_unknown_without_committing_a_late_start() {
    dispatch_interleaving(DispatchHookPhase::AfterAttempt, true).await;
}

async fn dispatch_interleaving(phase: DispatchHookPhase, attempted: bool) {
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        // The test never cancels the server. Releasing the retained task must
        // close an unissued connection, rather than send an expired Start frame.
        loop {
            let mut chunk = [0; 8192];
            let read = stream.read(&mut chunk).await.unwrap();
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(split) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let header = String::from_utf8_lossy(&bytes[..split]);
                let length: usize = header
                    .lines()
                    .find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                if bytes.len() >= split + 4 + length {
                    break;
                }
            }
        }
        bytes
    });
    let hook = std::sync::Arc::new(TestDispatchHook {
        phase,
        reached: tokio::sync::Barrier::new(2),
        release: tokio::sync::Barrier::new(2),
    });
    let mut client = SupervisorClient::new(socket.clone());
    client.dispatch_hook = Some(hook.clone());
    let caller = tokio::spawn(async move {
        client
            .start_process_wait_until(PINGAP_NAME, Instant::now() + Duration::from_millis(80))
            .await
    });
    tokio::time::timeout(Duration::from_secs(1), hook.reached.wait())
        .await
        .unwrap();
    let error = caller.await.unwrap().unwrap_err();
    if attempted {
        assert!(
            error.is::<crate::supervisor::ShutdownUnconfirmed>(),
            "an admitted attempt must retain uncertainty: {error:#}"
        );
    } else {
        assert!(
            is_before_dispatch(&error),
            "caller won before dispatch: {error:#}"
        );
        assert!(!error.is::<crate::supervisor::ShutdownUnconfirmed>());
    }
    hook.release.wait().await;
    let bytes = tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        bytes.is_empty(),
        "expired old Start wrote a request after its caller completed: {} bytes",
        bytes.len()
    );
    SupervisorClient::new(socket)
        .wait_mutations_until(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    if attempted {
        assert!(error.is::<crate::supervisor::ShutdownUnconfirmed>());
    } else {
        assert!(is_before_dispatch(&error));
    }
}

#[tokio::test]
async fn partial_request_timeout_signals_eof_to_a_healthy_server_without_force_close() {
    let temporary = tempfile::tempdir().unwrap();
    let socket = temporary.path().join("rpc.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (accepted, connected) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
        }
        let header = String::from_utf8(header).unwrap();
        let expected: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        accepted.send(()).unwrap();
        // A real healthy HTTP server waits for Content-Length bytes. Delayed
        // consumption fills the send buffer, but it never force-closes a client.
        tokio::time::sleep(Duration::from_millis(650)).await;
        let mut actual = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), stream.read_to_end(&mut actual)).await
            .expect("partial write must signal EOF; server and client cannot wait for each other forever")
            .unwrap();
        assert!(
            actual.len() < expected,
            "the fixture must send a genuinely incomplete frame"
        );
        let fault = "<methodResponse><fault><value><struct><member><name>faultCode</name><value><int>1</int></value></member><member><name>faultString</name><value><string>Incomplete HTTP request; no command executed</string></value></member></struct></value></fault></methodResponse>";
        response(&mut stream, fault).await;
        actual.len()
    });
    let client = SupervisorClient::new(socket);
    let error = client
        .call_until(
            "supervisor.startProcess",
            &["x".repeat(4 * 1024 * 1024).into()],
            Instant::now() + Duration::from_millis(500),
            RpcAccess::Mutation,
        )
        .await
        .unwrap_err();
    connected.await.unwrap();
    assert!(
        error.is::<crate::supervisor::ShutdownUnconfirmed>(),
        "partial write is not a before-dispatch failure: {error:#}"
    );
    let blocked = client.start_process_wait(PINGAP_NAME).await.unwrap_err();
    assert!(is_before_dispatch(&blocked));
    assert!(
        client
            .wait_mutations_until(Instant::now() + Duration::from_millis(30))
            .await
            .unwrap_err()
            .is::<crate::supervisor::ShutdownUnconfirmed>()
    );
    assert!(server.await.unwrap() > 0);
    client
        .wait_mutations_until(Instant::now() + Duration::from_secs(1))
        .await
        .unwrap();
    // The late rejection resolves only this transport. The original caller
    // retains its timeout/Unknown result and no start was retried.
    assert!(error.is::<crate::supervisor::ShutdownUnconfirmed>());
}
