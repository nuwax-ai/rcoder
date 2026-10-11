//! Slow real Unix XML-RPC responses must not enlarge the publication deadline.
use super::*;
use crate::proxy::publication::ProcessHost;

#[cfg(unix)]
#[tokio::test]
async fn proxy_stop_exact_consumes_the_absolute_deadline_at_read_and_reload() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for slow_method in ["supervisor.getAllProcessInfo", "supervisor.reloadConfig"] {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut request = vec![0; 8192];
                let length = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..length]);
                let reload = request.contains("supervisor.reloadConfig");
                if request.contains(slow_method) {
                    tokio::time::sleep(std::time::Duration::from_millis(180)).await;
                }
                let body = if reload {
                    "<methodResponse><params><param><value><array><data><value><array><data><value><array><data/></array></value><value><array><data/></array></value><value><array><data/></array></value></data></array></value></data></array></value></param></params></methodResponse>"
                } else {
                    "<methodResponse><params><param><value><array><data/></array></value></param></params></methodResponse>"
                };
                let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            }
        });
        let host = SupervisordHost::protocol_fixture(socket, root.path().join("dynamic.conf"));
        let mut process_host = resident::OfficialSupervisorHost {
            host: &host,
            runtime_root: root.path(),
            spec: ServiceSpecFile {
                release_id: "fixture".into(),
                service_id: "pingap".into(),
                cwd: "/".into(),
                argv: vec![
                    "pingap".into(),
                    "-c".into(),
                    root.path().join("candidate.toml").to_string_lossy().into(),
                ],
                env: Default::default(),
                port: None,
            },
            current: None,
            cancel: None,
            epoch: 0,
        };
        let start = tokio::time::Instant::now();
        let deadline = start + std::time::Duration::from_millis(40);
        let result = process_host.stop_exact(deadline).await;
        assert!(result.is_err(), "expired stop cannot commit: {slow_method}");
        assert!(
            start.elapsed() < std::time::Duration::from_millis(130),
            "{slow_method} escaped the same absolute deadline: {:?}",
            start.elapsed()
        );
        server.abort();
        let _ = server.await;
    }
}

#[cfg(unix)]
fn start_phase_deadline(slow: &'static str) {
    let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let saved = std::env::var_os("APP_CLI_SPEC_DIR");
            owner_test_context::restore_spec_env(Some(root.join("specs").into_os_string()));
            let args = RuntimeArgs { workspace: workspace.clone(), log_dir: root.join("logs"), ..Default::default() };
            let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir);
            std::fs::create_dir_all(&runtime_root).unwrap();
            let (lease, scope) = owner_test_context::scope(&root, &workspace);
            supervisor::resident::bind_owner(scope, &args).unwrap();
            let socket = root.join("rpc.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let called = calls.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") { header.push(stream.read_u8().await.unwrap()); }
                    let header = String::from_utf8(header).unwrap();
                    let size: usize = header.lines().find_map(|line| line.strip_prefix("Content-Length: ")).unwrap().parse().unwrap();
                    let mut request = vec![0; size];
                    stream.read_exact(&mut request).await.unwrap();
                    let request = String::from_utf8(request).unwrap();
                    let method = request.split("<methodName>").nth(1).unwrap().split("</methodName>").next().unwrap().to_string();
                    called.lock().unwrap().push(method.clone());
                    if method == slow { tokio::time::sleep(std::time::Duration::from_millis(1000)).await; }
                    let value = match method.as_str() {
                        "supervisor.reloadConfig" => "<array><data><value><array><data><value><array><data/></array></value><value><array><data/></array></value><value><array><data/></array></value></data></array></value></data></array>",
                        "supervisor.getProcessInfo" => "<struct><member><name>group</name><value><string>app-pingap</string></value></member><member><name>name</name><value><string>app-pingap</string></value></member><member><name>statename</name><value><string>RUNNING</string></value></member><member><name>state</name><value><int>20</int></value></member><member><name>pid</name><value><int>42</int></value></member><member><name>start</name><value><int>123</int></value></member></struct>",
                        "supervisor.addProcessGroup" | "supervisor.startProcess" => "<boolean>1</boolean>",
                        method => panic!("unexpected mutation after deadline: {method}"),
                    };
                    let body = format!("<methodResponse><params><param><value>{value}</value></param></params></methodResponse>");
                    let _ = stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                }
            });
            let host = SupervisordHost::protocol_fixture(socket, root.join("dynamic.conf"));
            let endpoint = admin_probe::AdminEndpoint { addr: "127.0.0.1:19086".into(), user: "fixture".into(), password: "fixture".into() };
            let candidate = CompileOutcome { config_path: runtime_root.join("candidate.toml"), expected_hash: "fixture".into(), publication_id: uuid::Uuid::new_v4().to_string(), operation_id: Some("original-operation".into()), serving_release_id: None, config_digest: "fixture".into(), entry_probes: vec![], business_probes: vec![] };
            let mut process_host = resident::OfficialSupervisorHost { host: &host, runtime_root: &runtime_root, spec: pingap_service_spec(&args, &endpoint), current: None, cancel: None, epoch: supervisor::resident::control_epoch().unwrap() };
            let start = tokio::time::Instant::now();
            let error = process_host.start_exact(&candidate, start + std::time::Duration::from_millis(500)).await.unwrap_err();
            assert!(start.elapsed() < std::time::Duration::from_millis(1000), "{slow}: {error:#}");
            assert!(process_host.current.is_none(), "late start cannot commit an incarnation");
            let expected = ["supervisor.reloadConfig", "supervisor.addProcessGroup", "supervisor.startProcess", "supervisor.getProcessInfo"];
            let through = expected.iter().position(|method| *method == slow).unwrap() + 1;
            assert_eq!(&*calls.lock().unwrap(), &expected[..through], "slow phase was not reached: {error:#}");
            host.client.wait_mutations_until(tokio::time::Instant::now() + std::time::Duration::from_secs(1)).await.unwrap();
            assert!(process_host.current.is_none(), "a late reply cannot alter request outcome");
            server.abort();
            let _ = server.await;
            supervisor::resident::shutdown().await.unwrap();
            drop(lease);
            owner_test_context::restore_spec_env(saved);
    });
}

// nextest starts each test in its own process. The resident owner intentionally
// remains a single process context after shutdown; do not clear that authority
// merely to allow a second fixture to bind into the same runtime.
#[cfg(unix)]
#[test]
fn proxy_start_each_rpc_phase_reload_consumes_deadline() {
    start_phase_deadline("supervisor.reloadConfig");
}
#[cfg(unix)]
#[test]
fn proxy_start_each_rpc_phase_add_consumes_deadline() {
    start_phase_deadline("supervisor.addProcessGroup");
}
#[cfg(unix)]
#[test]
fn proxy_start_each_rpc_phase_start_consumes_deadline() {
    start_phase_deadline("supervisor.startProcess");
}
#[cfg(unix)]
#[test]
fn proxy_start_each_rpc_phase_observe_consumes_deadline() {
    start_phase_deadline("supervisor.getProcessInfo");
}
