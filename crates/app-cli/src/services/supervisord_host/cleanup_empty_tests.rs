//! Read-only cleanup of a management-only generation via real Unix XML-RPC.
#[cfg(unix)]
use super::*;

#[cfg(unix)]
struct CleanupEnv {
    socket: Option<std::ffi::OsString>,
    conf: Option<std::ffi::OsString>,
}

#[cfg(unix)]
impl CleanupEnv {
    fn set(socket: &Path, conf: &Path) -> Self {
        let saved = Self {
            socket: std::env::var_os("APP_CLI_SUPERVISOR_SOCKET"),
            conf: std::env::var_os("APP_CLI_TEST_CLEANUP_CONF"),
        };
        // Each nextest case owns its process; the fixture uses a current-thread
        // runtime and retains the existing process-level environment test lock.
        unsafe {
            std::env::set_var("APP_CLI_SUPERVISOR_SOCKET", socket);
            std::env::set_var("APP_CLI_TEST_CLEANUP_CONF", conf);
        }
        saved
    }
}

#[cfg(unix)]
impl Drop for CleanupEnv {
    fn drop(&mut self) {
        unsafe {
            match &self.socket {
                Some(value) => std::env::set_var("APP_CLI_SUPERVISOR_SOCKET", value),
                None => std::env::remove_var("APP_CLI_SUPERVISOR_SOCKET"),
            }
            match &self.conf {
                Some(value) => std::env::set_var("APP_CLI_TEST_CLEANUP_CONF", value),
                None => std::env::remove_var("APP_CLI_TEST_CLEANUP_CONF"),
            }
        }
    }
}

#[cfg(unix)]
async fn rpc_fixture(
    socket: &Path,
    entries: String,
) -> (
    tokio::task::JoinHandle<()>,
    std::sync::Arc<std::sync::Mutex<Vec<String>>>,
) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = calls.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            let text = String::from_utf8(header).unwrap();
            let size: usize = text
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut bytes = vec![0; size];
            stream.read_exact(&mut bytes).await.unwrap();
            let request = String::from_utf8(bytes).unwrap();
            let method = request
                .split("<methodName>")
                .nth(1)
                .unwrap()
                .split("</methodName>")
                .next()
                .unwrap()
                .to_owned();
            recorded.lock().unwrap().push(method.clone());
            let value = match method.as_str() {
                "supervisor.getVersion" => "<string>4.2.5</string>".to_owned(),
                "supervisor.getAllProcessInfo" => format!("<array><data>{entries}</data></array>"),
                _ => panic!("management-only cleanup must not mutate supervisord: {method}"),
            };
            let body = format!(
                "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
            );
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
    });
    (server, calls)
}

#[cfg(unix)]
fn management_cleanup(entries: String) {
    let _lock = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let socket = root.join("supervisor.sock");
        let conf = root.join("50-app-services.conf");
        std::fs::write(&conf, "# app-cli dynamic services stopped\n").unwrap();
        let before = std::fs::read(&conf).unwrap();
        let _env = CleanupEnv::set(&socket, &conf);
        let (server, calls) = rpc_fixture(&socket, entries).await;
        let result = SupervisordHost::cleanup_generation(&root).await;
        server.abort();
        let _ = server.await;
        assert!(
            matches!(result, Ok(runtime_supervisor::CleanupOutcome::Empty)),
            "management-only supervisor scope must be observed empty: {result:?}"
        );
        assert!(
            !root.join(ENGINE_RECEIPT).exists(),
            "read-only cleanup must not register a business engine"
        );
        assert_eq!(std::fs::read(&conf).unwrap(), before);
        let calls = calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|method| method == "supervisor.getAllProcessInfo")
        );
        assert!(calls.iter().all(|method| matches!(
            method.as_str(),
            "supervisor.getVersion" | "supervisor.getAllProcessInfo"
        )));
    });
}

#[cfg(unix)]
#[test]
fn missing_receipt_empty_management_ipc_is_cleanup_empty() {
    management_cleanup(String::new());
}

#[cfg(unix)]
#[test]
fn missing_receipt_core_only_management_ipc_is_cleanup_empty() {
    let entries = ["postgres", "app-cli", "dbx-web"].into_iter().map(|name|
        format!("<value><struct><member><name>group</name><value><string>{name}</string></value></member><member><name>name</name><value><string>{name}</string></value></member><member><name>statename</name><value><string>RUNNING</string></value></member><member><name>state</name><value><int>20</int></value></member></struct></value>")
    ).collect();
    management_cleanup(entries);
}

#[cfg(unix)]
#[tokio::test]
async fn unregistered_managed_programs_and_foreign_named_programs_are_not_empty() {
    for (group, name, state) in [
        ("app-svc-web", "app-svc-web", "RUNNING"),
        ("app-svc-web", "app-svc-web", "STOPPED"),
        ("app-pingap", "app-pingap", "STOPPED"),
        ("foreign-group", "app-svc-web", "RUNNING"),
    ] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let socket = root.join("supervisor.sock");
        let conf = root.join("50-app-services.conf");
        let entries = format!(
            "<value><struct><member><name>group</name><value><string>{group}</string></value></member><member><name>name</name><value><string>{name}</string></value></member><member><name>statename</name><value><string>{state}</string></value></member></struct></value>"
        );
        let (server, calls) = rpc_fixture(&socket, entries).await;
        let error = cleanup::observe_unregistered_engine_empty_until(
            &socket,
            &conf,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("managed program"),
            "{error:#}"
        );
        assert!(!root.join(ENGINE_RECEIPT).exists());
        assert!(calls.lock().unwrap().iter().all(|method| matches!(
            method.as_str(),
            "supervisor.getVersion" | "supervisor.getAllProcessInfo"
        )));
        server.abort();
        let _ = server.await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unregistered_restart_configuration_and_unreadable_fragments_are_not_empty() {
    for resident in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let conf = root.join("50-app-services.conf");
        let retained = if resident {
            root.join(RESIDENT_CONF_FILE)
        } else {
            conf.clone()
        };
        let content = "[program:app-svc-web]\ncommand=/app/app-cli run-service old web\nautostart=false\nautorestart=true\n";
        std::fs::write(&retained, content).unwrap();
        let error = cleanup::observe_unregistered_engine_empty_until(
            &root.join("absent.sock"),
            &conf,
            tokio::time::Instant::now() + std::time::Duration::from_secs(1),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("configuration without engine ownership"),
            "{error:#}"
        );
        assert_eq!(std::fs::read_to_string(retained).unwrap(), content);
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let conf = root.join("50-app-services.conf");
    std::fs::create_dir(&conf).unwrap();
    let error = cleanup::observe_unregistered_engine_empty_until(
        &root.join("absent.sock"),
        &conf,
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("read unregistered supervisor fragment"),
        "{error:#}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn unregistered_rpc_failure_and_shared_deadline_never_confirm_empty() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for delayed in [false, true] {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let socket = root.join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 8192];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            if delayed {
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            let body = "<methodResponse><fault><value><struct><member><name>faultCode</name><value><int>1</int></value></member><member><name>faultString</name><value><string>observed-failure</string></value></member></struct></value></fault></methodResponse>";
            let _ = stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await;
        });
        let before = tokio::time::Instant::now();
        let error = cleanup::observe_unregistered_engine_empty_until(
            &socket,
            &root.join("50-app-services.conf"),
            before + std::time::Duration::from_millis(40),
        )
        .await
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("RPC unavailable"),
            "{error:#}"
        );
        if delayed {
            assert!(before.elapsed() < std::time::Duration::from_millis(140));
        }
        assert!(!root.join("cleanup-outcome.json").exists());
        server.abort();
        let _ = server.await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn unregistered_pending_write_does_not_become_empty_after_caller_timeout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let socket = root.join("supervisor.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let (seen, saw) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut header = Vec::new();
        while !header.ends_with(b"\r\n\r\n") {
            header.push(stream.read_u8().await.unwrap());
        }
        let header = String::from_utf8(header).unwrap();
        let size: usize = header
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let mut body = vec![0; size];
        stream.read_exact(&mut body).await.unwrap();
        assert!(
            String::from_utf8(body)
                .unwrap()
                .contains("supervisor.startProcess")
        );
        seen.send(()).unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        let body = "<methodResponse><params><param><value><boolean>1</boolean></value></param></params></methodResponse>";
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
    });
    let client = SupervisorClient::new(&socket);
    let mutation = tokio::spawn({
        let client = client.clone();
        async move {
            client
                .start_process_wait_until(
                    "app-svc-web",
                    tokio::time::Instant::now() + std::time::Duration::from_millis(20),
                )
                .await
        }
    });
    saw.await.unwrap();
    assert!(mutation.await.unwrap().is_err());
    let before = tokio::time::Instant::now();
    let error = cleanup::observe_unregistered_engine_empty_until(
        &socket,
        &root.join("50-app-services.conf"),
        before + std::time::Duration::from_millis(40),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("mutation")
            || format!("{error:#}").contains("supervisor writes"),
        "{error:#}"
    );
    assert!(before.elapsed() < std::time::Duration::from_millis(140));
    assert!(!root.join(ENGINE_RECEIPT).exists());
    server.await.unwrap();
    client
        .wait_mutations_until(tokio::time::Instant::now() + std::time::Duration::from_secs(1))
        .await
        .unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn unregistered_socket_replacement_between_empty_reads_cannot_confirm_cleanup() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().canonicalize().unwrap();
    let socket = root.join("supervisor.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let original = std::fs::metadata(&socket).unwrap();
    let (replaced, observed) = tokio::sync::oneshot::channel();
    let path = socket.clone();
    let server = tokio::spawn(async move {
        for method in ["supervisor.getVersion", "supervisor.getAllProcessInfo"] {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let size: usize = header
                .lines()
                .find_map(|line| line.strip_prefix("Content-Length: "))
                .unwrap()
                .parse()
                .unwrap();
            let mut body = vec![0; size];
            stream.read_exact(&mut body).await.unwrap();
            assert!(String::from_utf8(body).unwrap().contains(method));
            let value = if method == "supervisor.getVersion" {
                "<string>4.2.5</string>"
            } else {
                "<array><data/></array>"
            };
            let body = format!(
                "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
            );
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
        // The original daemon remains alive, while the pathname now selects a
        // different actual Unix socket. Both instances truthfully report empty.
        std::fs::remove_file(&path).unwrap();
        let replacement = tokio::net::UnixListener::bind(&path).unwrap();
        replaced.send(()).unwrap();
        loop {
            let (mut stream, _) = replacement.accept().await.unwrap();
            let mut request = [0; 8192];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let body = "<methodResponse><params><param><value><array><data/></array></value></param></params></methodResponse>";
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
    });
    let result = cleanup::observe_unregistered_engine_empty_until(
        &socket,
        &root.join("50-app-services.conf"),
        tokio::time::Instant::now() + std::time::Duration::from_secs(1),
    )
    .await;
    observed.await.unwrap();
    use std::os::unix::fs::MetadataExt;
    let current = std::fs::metadata(&socket).unwrap();
    assert_ne!(
        (original.dev(), original.ino()),
        (current.dev(), current.ino())
    );
    server.abort();
    let _ = server.await;
    assert!(
        result.is_err(),
        "two empty replies from replaced daemons cannot prove one scope empty"
    );
}
