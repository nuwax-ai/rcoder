//! A supervisor can finish Stop after the caller deadline and retain a stopped
//! group after its configuration was already withdrawn. Original lifecycle
//! commands must reconcile that group rather than require manual file repair.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn rpc_request(stream: &mut tokio::net::UnixStream) -> String {
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(stream.read_u8().await.unwrap());
    }
    let header = String::from_utf8(header).unwrap();
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

async fn rpc_response(stream: &mut tokio::net::UnixStream, value: &str) {
    let body = format!(
        "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
    );
    let _ = stream
        .write_all(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
}

fn stopped_row(state: &str, code: i64, pid: Option<i64>) -> String {
    let pid = pid
        .map(|pid| format!("<member><name>pid</name><value><int>{pid}</int></value></member>"))
        .unwrap_or_default();
    format!(
        r#"<array><data><value><struct>
        <member><name>group</name><value><string>app-pingap</string></value></member>
        <member><name>name</name><value><string>app-pingap</string></value></member>
        <member><name>statename</name><value><string>{state}</string></value></member>
        <member><name>state</name><value><int>{code}</int></value></member>
        {pid}<member><name>start</name><value><int>0</int></value></member>
        </struct></value></data></array>"#
    )
}

#[test]
fn withdrawn_stopped_proxy_with_process_null_receipt_is_reconciled_by_original_drain() {
    let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let saved = std::env::var_os("APP_CLI_SPEC_DIR");
        owner_test_context::restore_spec_env(Some(root.join("specs").into_os_string()));
        let (lease, scope) = owner_test_context::scope(&root, &workspace);
        let identity = scope.identity().clone();
        let args = RuntimeArgs { workspace, log_dir: root.join("logs"), ..Default::default() };
        supervisor::resident::bind_owner(scope, &args).unwrap();
        let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir).unwrap();
        std::fs::create_dir_all(&runtime_root).unwrap();
        let active = crate::proxy::compiler::active_config_path(&runtime_root);
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(&active, b"retained old proxy graph").unwrap();
        let conf = root.join("dynamic.conf");
        let business = b"[program:app-svc-web]\ncommand=/usr/bin/app-cli run-service release web\n";
        std::fs::write(&conf, business).unwrap();
        let resident = root.join(RESIDENT_CONF_FILE);
        let withdrawn = b"# official proxy stopped\n";
        std::fs::write(&resident, withdrawn).unwrap();
        let receipt = ResidentEngineReceipt {
            version: 1, identity, runtime_root: runtime_root.canonicalize().unwrap(),
            socket: crate::xmlrpc::default_socket_path(), config: active.clone(), process: None,
        };
        let receipt_bytes = serde_json::to_vec(&receipt).unwrap();
        std::fs::write(runtime_root.join(RESIDENT_ENGINE_RECEIPT), &receipt_bytes).unwrap();
        let socket = root.join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let rpc = tokio::spawn(async move {
            let mut present = true;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = rpc_request(&mut stream).await;
                let method = request.split("<methodName>supervisor.").nth(1).unwrap()
                    .split("</methodName>").next().unwrap().to_string();
                seen.lock().unwrap().push(method.clone());
                let value = match method.as_str() {
                    "getAllProcessInfo" if present => stopped_row("STOPPED", 0, Some(0)),
                    "getAllProcessInfo" => "<array><data/></array>".into(),
                    "reloadConfig" => "<array><data><value><array><data><value><array><data/></array></value><value><array><data/></array></value><value><array><data/></array></value></data></array></value></data></array>".into(),
                    "stopProcessGroup" => {
                        assert!(request.contains("app-pingap"));
                        "<boolean>1</boolean>".into()
                    },
                    "removeProcessGroup" => {
                        assert!(request.contains("app-pingap"));
                        present = false;
                        "<boolean>1</boolean>".into()
                    },
                    other => panic!("unexpected mutation during original drain: {other}"),
                };
                assert!(!request.contains("app-svc-web"), "business group must remain untouched");
                rpc_response(&mut stream, &value).await;
            }
        });
        let host = SupervisordHost::protocol_fixture(socket, conf.clone());
        let result = host.drain_entry_until(&runtime_root,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2)).await;
        rpc.abort();
        let _ = rpc.await;
        supervisor::resident::shutdown().await.unwrap();
        drop(lease);
        owner_test_context::restore_spec_env(saved);
        assert!(result.is_ok(), "the original drain must finish a stopped, withdrawn group: {result:?}");
        let calls = calls.lock().unwrap();
        assert_eq!(calls.iter().filter(|method| method.as_str() == "removeProcessGroup").count(), 1,
            "a stopped group still needs exact removal: {calls:?}");
        assert!(!calls.iter().any(|method| method == "startProcess" || method == "addProcessGroup"));
        assert_eq!(std::fs::read(conf).unwrap(), business);
        assert_eq!(std::fs::read(resident).unwrap(), withdrawn);
        assert_eq!(std::fs::read(active).unwrap(), b"retained old proxy graph");
        assert_eq!(std::fs::read(runtime_root.join(RESIDENT_ENGINE_RECEIPT)).unwrap(), receipt_bytes);
    });
}

#[derive(Clone, Copy)]
enum DelayedReply {
    None,
    Stop,
    Remove,
}

fn withdrawn_case(
    state: &'static str,
    code: i64,
    pid: Option<i64>,
    claim: &'static str,
    delayed: DelayedReply,
    expected_success: bool,
) {
    let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let saved = std::env::var_os("APP_CLI_SPEC_DIR");
        owner_test_context::restore_spec_env(Some(root.join("specs").into_os_string()));
        let (lease, scope) = owner_test_context::scope(&root, &workspace);
        let mut identity = scope.identity().clone();
        let args = RuntimeArgs { workspace, log_dir: root.join("logs"), ..Default::default() };
        supervisor::resident::bind_owner(scope, &args).unwrap();
        let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir).unwrap();
        let active = crate::proxy::compiler::active_config_path(&runtime_root);
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(&active, b"retained old proxy graph").unwrap();
        let conf = root.join("dynamic.conf");
        let business = "[program:app-svc-web]\ncommand=/usr/bin/app-cli run-service release web\n";
        let resident = root.join(RESIDENT_CONF_FILE);
        std::fs::write(&resident, b"# official proxy stopped\n").unwrap();
        let initial = if matches!(delayed, DelayedReply::Stop) {
            ServiceSpecFile {
                release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(), service_id: "pingap".into(), cwd: "/".into(),
                argv: vec!["/usr/bin/pingap".into(), "-c".into(), active.to_string_lossy().into_owned()],
                env: std::collections::BTreeMap::from([
                    ("PINGAP_ADMIN_ADDR".into(), "127.0.0.1:19086".into()),
                    ("PINGAP_ADMIN_USER".into(), "fixture".into()),
                    ("PINGAP_ADMIN_PASSWORD".into(), "fixture".into()),
                ]), port: None,
            }.write().unwrap();
            format!("{business}[program:app-pingap]\ncommand=/usr/bin/app-cli run-service resident pingap\n")
        } else { business.to_owned() };
        std::fs::write(&conf, initial).unwrap();
        match claim {
            "historical" => {
                identity.owner_instance = "previous-owner-instance".into();
                identity.physical_domain.as_mut().unwrap()["instance"] = "previous-container".into();
                identity.process_epoch = Some("previous-pid-namespace".into());
            },
            "foreign-application" => identity.application_id = "another-app".into(),
            "foreign-workspace" => identity.binding["resource"] = serde_json::json!(root.join("another-workspace")),
            "owned" => {},
            other => panic!("unknown fixture claim {other}"),
        }
        let receipt = ResidentEngineReceipt {
            version: 1, identity, runtime_root: runtime_root.canonicalize().unwrap(),
            socket: crate::xmlrpc::default_socket_path(), config: active.clone(), process: None,
        };
        let receipt_bytes = serde_json::to_vec(&receipt).unwrap();
        std::fs::write(runtime_root.join(RESIDENT_ENGINE_RECEIPT), &receipt_bytes).unwrap();
        let socket = root.join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let release = std::sync::Arc::new(tokio::sync::Notify::new());
        let rpc_release = release.clone();
        let fault_held = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let rpc_fault_held = fault_held.clone();
        let rpc = tokio::spawn(async move {
            let mut present = true;
            let mut held = false;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = rpc_request(&mut stream).await;
                let method = request.split("<methodName>supervisor.").nth(1).unwrap()
                    .split("</methodName>").next().unwrap().to_string();
                seen.lock().unwrap().push(method.clone());
                let value = match method.as_str() {
                    "getAllProcessInfo" if present => stopped_row(state, code, pid),
                    "getAllProcessInfo" => "<array><data/></array>".into(),
                    "reloadConfig" => "<array><data><value><array><data><value><array><data/></array></value><value><array><data/></array></value><value><array><data/></array></value></data></array></value></data></array>".into(),
                    "stopProcessGroup" => "<boolean>1</boolean>".into(),
                    "removeProcessGroup" => { present = false; "<boolean>1</boolean>".into() },
                    other => panic!("unexpected mutation during withdrawn cleanup: {other}"),
                };
                assert!(!request.contains("app-svc-web"), "business group must remain untouched");
                let hold = matches!((delayed, method.as_str()),
                    (DelayedReply::Stop, "stopProcessGroup") | (DelayedReply::Remove, "removeProcessGroup"));
                if hold && !held {
                    held = true;
                    rpc_fault_held.store(true, std::sync::atomic::Ordering::Release);
                    rpc_release.notified().await;
                }
                rpc_response(&mut stream, &value).await;
            }
        });
        let host = SupervisordHost::protocol_fixture(socket, conf.clone());
        let deadline = || tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut expired = None;
        let mut mutations_at_timeout = Vec::new();
        let mut held_at_timeout = false;
        if !matches!(delayed, DelayedReply::None) {
            // Allow the real fragment withdrawal/fsync and RPC dispatch to run
            // under concurrent build load. Only the deliberately retained reply
            // must consume this bounded fixture deadline. Production keeps 25s.
            let fault_deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
            expired = Some(if matches!(delayed, DelayedReply::Stop) {
                host.close_entry_until(fault_deadline).await
            } else {
                host.drain_entry_until(&runtime_root, fault_deadline).await
            });
            mutations_at_timeout = calls.lock().unwrap().clone();
            held_at_timeout = fault_held.load(std::sync::atomic::Ordering::Acquire);
            release.notify_one();
            host.client.wait_mutations_until(deadline()).await.unwrap();
        }
        let result = host.drain_entry_until(&runtime_root, deadline()).await;
        let repeated = if expected_success { Some(host.drain_entry_until(&runtime_root, deadline()).await) } else { None };
        let observed = calls.lock().unwrap().clone();
        rpc.abort(); let _ = rpc.await;
        supervisor::resident::shutdown().await.unwrap(); drop(lease);
        owner_test_context::restore_spec_env(saved);
        assert_eq!(result.is_ok(), expected_success, "{claim}/{state}/{pid:?}: {result:?}");
        if let Some(result) = expired {
            assert!(held_at_timeout,
                "the timeout must occur after the selected RPC reply was actually held: {mutations_at_timeout:?}");
            assert!(result.is_err(), "the original deadline must retain a failed outcome");
            assert!(result.unwrap_err().is::<supervisor::ShutdownUnconfirmed>());
            if matches!(delayed, DelayedReply::Stop) {
                assert!(mutations_at_timeout.iter().any(|method| method == "stopProcessGroup"));
                assert!(!mutations_at_timeout.iter().any(|method| method == "removeProcessGroup"),
                    "remove must not escape the original stop deadline");
            }
        }
        if expected_success {
            assert!(repeated.unwrap().is_ok(), "repeat original drain must be idempotent");
            assert_eq!(observed.iter().filter(|method| method.as_str() == "removeProcessGroup").count(), 1,
                "late completion must not remove a successor or replay remove: {observed:?}");
            let expected_stops = usize::from(matches!(delayed, DelayedReply::Stop));
            assert_eq!(observed.iter().filter(|method| method.as_str() == "stopProcessGroup").count(), expected_stops,
                "a verified stopped group needs removal, not another stop: {observed:?}");
        } else {
            assert!(observed.iter().all(|method| method == "getAllProcessInfo"),
                "unconfirmed ownership/state must not authorize any mutation: {observed:?}");
        }
        assert_eq!(std::fs::read(conf).unwrap(), business.as_bytes());
        assert_eq!(std::fs::read(resident).unwrap(), b"# official proxy stopped\n");
        assert_eq!(std::fs::read(active).unwrap(), b"retained old proxy graph");
        assert_eq!(std::fs::read(runtime_root.join(RESIDENT_ENGINE_RECEIPT)).unwrap(), receipt_bytes);
    });
}

#[test]
fn withdrawn_stopped_proxy_accepts_historical_process_epoch_only_after_exit() {
    withdrawn_case(
        "STOPPED",
        0,
        Some(0),
        "historical",
        DelayedReply::None,
        true,
    );
}
#[test]
fn withdrawn_exited_proxy_group_is_removed_without_replaying_stop() {
    withdrawn_case("EXITED", 100, Some(0), "owned", DelayedReply::None, true);
}
#[test]
fn withdrawn_fatal_proxy_group_is_removed_without_replaying_stop() {
    withdrawn_case("FATAL", 200, Some(0), "owned", DelayedReply::None, true);
}
#[test]
fn withdrawn_stopped_proxy_rejects_foreign_application_without_mutation() {
    withdrawn_case(
        "STOPPED",
        0,
        Some(0),
        "foreign-application",
        DelayedReply::None,
        false,
    );
}
#[test]
fn withdrawn_stopped_proxy_rejects_foreign_workspace_without_mutation() {
    withdrawn_case(
        "STOPPED",
        0,
        Some(0),
        "foreign-workspace",
        DelayedReply::None,
        false,
    );
}
#[test]
fn withdrawn_stopped_proxy_rejects_nonzero_pid_without_mutation() {
    withdrawn_case("STOPPED", 0, Some(42), "owned", DelayedReply::None, false);
}
#[test]
fn withdrawn_stopped_proxy_rejects_missing_pid_without_mutation() {
    withdrawn_case("STOPPED", 0, None, "owned", DelayedReply::None, false);
}
#[test]
fn withdrawn_unknown_proxy_state_never_authorizes_mutation() {
    withdrawn_case("UNKNOWN", 1000, Some(0), "owned", DelayedReply::None, false);
}
#[test]
fn late_proxy_stop_reply_recovers_the_original_drain_after_configuration_withdrawal() {
    withdrawn_case("STOPPED", 0, Some(0), "owned", DelayedReply::Stop, true);
}
#[test]
fn late_proxy_remove_reply_is_reconciled_once_and_original_drain_is_idempotent() {
    withdrawn_case("STOPPED", 0, Some(0), "owned", DelayedReply::Remove, true);
}

fn withdrawn_owner_shutdown_case(
    proxy_present: bool,
    business_present: bool,
    claim: &'static str,
    expected_success: bool,
) {
    let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let workspace = root.join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let saved = std::env::var_os("APP_CLI_SPEC_DIR");
        owner_test_context::restore_spec_env(Some(root.join("specs").into_os_string()));
        let (lease, scope) = owner_test_context::scope(&root, &workspace);
        let runtime_root = root.join("runtime");
        let active = crate::proxy::compiler::active_config_path(&runtime_root);
        std::fs::create_dir_all(active.parent().unwrap()).unwrap();
        std::fs::write(&active, b"old owned graph and data sentinel").unwrap();
        let conf = root.join("dynamic.conf");
        let original_fragment: &[u8] = if business_present {
            b"[program:app-svc-web]\ncommand=/usr/bin/app-cli run-service release web\n"
        } else { b"# no owned restart sources\n" };
        std::fs::write(&conf, original_fragment).unwrap();
        let resident = root.join(RESIDENT_CONF_FILE);
        std::fs::write(&resident, b"# official proxy stopped\n").unwrap();
        let receipt_path = runtime_root.join(RESIDENT_ENGINE_RECEIPT);
        let receipt_bytes = match claim {
            "old-pid" => {
                let mut identity = scope.identity().clone();
                identity.owner_instance = "previous-owner".into();
                identity.physical_domain.as_mut().unwrap()["instance"] = "previous-container".into();
                identity.process_epoch = Some("previous-pid-namespace".into());
                serde_json::to_vec(&ResidentEngineReceipt {
                    version: 1, identity, runtime_root: runtime_root.canonicalize().unwrap(),
                    socket: crate::xmlrpc::default_socket_path(), config: active.clone(),
                    process: Some(crate::xmlrpc::RunningProcess { pid: 424242, started: 123456 }),
                }).unwrap()
            },
            "corrupt" | "foreign-business" => b"{retained-corrupt-historical-receipt".to_vec(),
            "missing" | "missing-conf" | "missing-spec" => Vec::new(),
            other => panic!("unknown owner shutdown claim {other}"),
        };
        if !matches!(claim, "missing" | "missing-conf" | "missing-spec") { std::fs::write(&receipt_path, &receipt_bytes).unwrap(); }
        let socket = root.join("rpc.sock");
        let self_owner=!proxy_present && business_present;
        let actual_cwd=if claim=="foreign-business" {root.join("foreign-current-workspace")} else {workspace.clone()};
        std::fs::create_dir_all(&actual_cwd).unwrap();
        let real_business=self_owner.then(||std::sync::Arc::new(std::sync::Mutex::new(
            std::process::Command::new("sleep").arg("30").current_dir(&actual_cwd).spawn().unwrap())));
        let actual_pid=real_business.as_ref().map(|child|child.lock().unwrap().id()).unwrap_or(42);
        if self_owner {
            ServiceSpecFile{release_id:"release".into(),service_id:"web".into(),cwd:actual_cwd.to_string_lossy().into_owned(),
                argv:vec!["sleep".into(),"30".into()],env:Default::default(),port:None}.write().unwrap();
        }
        let saved_socket=std::env::var_os("APP_CLI_SUPERVISOR_SOCKET");
        if self_owner {unsafe{std::env::set_var("APP_CLI_SUPERVISOR_SOCKET",&socket);}}
        let rpc_business=real_business.clone();
        if claim == "missing-conf" { std::fs::remove_file(&conf).unwrap(); }
        if claim == "missing-spec" {
            std::fs::remove_file(crate::svc_spec::spec_root().join("release/web.toml")).unwrap();
        }


        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen = calls.clone();
        let rpc = tokio::spawn(async move {
            let mut proxy = proxy_present;
            let mut business = business_present;
            let mut business_running = business_present;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request = rpc_request(&mut stream).await;
                let method = request.split("<methodName>supervisor.").nth(1).unwrap()
                    .split("</methodName>").next().unwrap().to_string();
                let target = if request.contains("<string>app-pingap</string>") { Some("app-pingap") }
                    else if request.contains("<string>app-svc-web</string>") { Some("app-svc-web") }
                    else { None };
                seen.lock().unwrap().push((method.clone(), target));
                let value = match method.as_str() {
                    "getAllProcessInfo" => {
                        let mut rows = String::new();
                        if self_owner {rows.push_str(&format!(r#"<value><struct>
                            <member><name>group</name><value><string>app-cli</string></value></member>
                            <member><name>name</name><value><string>app-cli</string></value></member>
                            <member><name>statename</name><value><string>RUNNING</string></value></member>
                            <member><name>state</name><value><int>20</int></value></member>
                            <member><name>pid</name><value><int>{}</int></value></member>
                            <member><name>start</name><value><int>1</int></value></member></struct></value>"#,std::process::id()));}

                        if proxy {
                            let row = stopped_row("STOPPED", 0, Some(0));
                            rows.push_str(row.strip_prefix("<array><data>").unwrap()
                                .strip_suffix("</data></array>").unwrap());
                        }
                        if business {
                            let (state, code, pid) = if business_running { ("RUNNING",20,i64::from(actual_pid)) }
                                else { ("STOPPED", 0, 0) };
                            rows.push_str(&format!(r#"<value><struct>
                                <member><name>group</name><value><string>app-svc-web</string></value></member>
                                <member><name>name</name><value><string>app-svc-web</string></value></member>
                                <member><name>statename</name><value><string>{state}</string></value></member>
                                <member><name>state</name><value><int>{code}</int></value></member>
                                <member><name>pid</name><value><int>{pid}</int></value></member>
                                <member><name>start</name><value><int>1</int></value></member>
                                </struct></value>"#));
                        }
                        format!("<array><data>{rows}</data></array>")
                    },
                    "getProcessInfo" => {
                        let (name,pid)=if request.contains("<string>app-cli</string>") {("app-cli",std::process::id())}
                            else {assert!(request.contains("<string>app-svc-web</string>"));("app-svc-web",actual_pid)};
                        format!(r#"<struct><member><name>group</name><value><string>{name}</string></value></member>
                            <member><name>name</name><value><string>{name}</string></value></member>
                            <member><name>statename</name><value><string>RUNNING</string></value></member>
                            <member><name>state</name><value><int>20</int></value></member>
                            <member><name>pid</name><value><int>{pid}</int></value></member>
                            <member><name>start</name><value><int>1</int></value></member></struct>"#)
                    },
                    "reloadConfig" => "<array><data><value><array><data><value><array><data/></array></value><value><array><data/></array></value><value><array><data/></array></value></data></array></value></data></array>".into(),
                    "stopProcessGroup" => {
                        assert_eq!(target, Some("app-svc-web"), "the stopped proxy must not receive another stop");
                        if let Some(child)=rpc_business.as_ref(){let mut child=child.lock().unwrap();child.kill().unwrap();child.wait().unwrap();}
                        business_running = false;
                        "<boolean>1</boolean>".into()
                    },
                    "removeProcessGroup" => {
                        match target {
                            Some("app-pingap") => proxy = false,
                            Some("app-svc-web") => { assert!(!business_running); business = false; },
                            _ => panic!("cleanup escaped the owned programs"),
                        }
                        "<boolean>1</boolean>".into()
                    },
                    other => panic!("unexpected owner shutdown mutation {other}"),
                };
                rpc_response(&mut stream, &value).await;
            }
        });
        let host = SupervisordHost::protocol_fixture(socket, conf.clone());
        let result = host.shutdown_owner_entry_until(&runtime_root, &scope,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2)).await;
        let repeated = if expected_success { Some(host.shutdown_owner_entry_until(&runtime_root, &scope,
            tokio::time::Instant::now() + std::time::Duration::from_secs(2)).await) } else { None };
        let observed = calls.lock().unwrap().clone();
        rpc.abort(); let _ = rpc.await;
        scope.close().unwrap(); scope.record_quiescent().unwrap(); drop(lease);
        owner_test_context::restore_spec_env(saved);
        if self_owner {unsafe{match saved_socket{Some(value)=>std::env::set_var("APP_CLI_SUPERVISOR_SOCKET",value),None=>std::env::remove_var("APP_CLI_SUPERVISOR_SOCKET")}}}
        if let Some(child)=real_business.as_ref(){let mut child=child.lock().unwrap();
            if expected_success{assert!(child.try_wait().unwrap().is_some(),"owned real business must have exited");}
            else{assert!(child.try_wait().unwrap().is_none(),"foreign real business must remain alive");child.kill().unwrap();child.wait().unwrap();}}

        assert_eq!(result.is_ok(), expected_success, "owner shutdown {claim}: {result:?}");
        if expected_success {
            assert!(repeated.unwrap().is_ok(), "owner shutdown must be idempotent after full cleanup");
        }
        if expected_success && proxy_present {
            assert_eq!(observed.iter().filter(|(method, group)| method == "removeProcessGroup" && *group == Some("app-pingap")).count(), 1);
            assert_eq!(observed.iter().filter(|(method, group)| method == "stopProcessGroup" && *group == Some("app-svc-web")).count(), usize::from(business_present));
            assert_eq!(observed.iter().filter(|(method, group)| method == "removeProcessGroup" && *group == Some("app-svc-web")).count(), usize::from(business_present));
            let proxy_remove = observed.iter().position(|(method, group)| method == "removeProcessGroup" && *group == Some("app-pingap")).unwrap();
            let business_stop = observed.iter().position(|(method, group)| method == "stopProcessGroup" && *group == Some("app-svc-web")).unwrap();
            assert!(proxy_remove < business_stop, "business cleanup must follow confirmed proxy removal: {observed:?}");
            assert_eq!(std::fs::read(&conf).unwrap(), b"# app-cli dynamic services stopped\n");
        } else if expected_success && business_present {
            assert!(!observed.iter().any(|(_,group)|*group==Some("app-pingap")));
            assert_eq!(observed.iter().filter(|(method,group)|method=="stopProcessGroup" && *group==Some("app-svc-web")).count(),1);
            assert_eq!(observed.iter().filter(|(method,group)|method=="removeProcessGroup" && *group==Some("app-svc-web")).count(),1);
            assert_eq!(std::fs::read(&conf).unwrap(),b"# app-cli dynamic services stopped\n");
        } else {
            assert!(observed.iter().all(|(method, _)| method == "getAllProcessInfo" || method=="getProcessInfo"),
                "empty or unconfirmed owner must not issue any mutation: {observed:?}");
            assert_eq!(std::fs::read(&conf).unwrap(), original_fragment);
        }
        assert_eq!(std::fs::read(active).unwrap(), b"old owned graph and data sentinel");
        assert_eq!(std::fs::read(resident).unwrap(), b"# official proxy stopped\n");
        if matches!(claim, "missing" | "missing-conf" | "missing-spec") { assert!(!receipt_path.exists()); }
        else { assert_eq!(std::fs::read(receipt_path).unwrap(), receipt_bytes); }
    });
}

#[test]
fn owner_shutdown_reconciles_withdrawn_proxy_with_historical_pid_before_stopping_business() {
    withdrawn_owner_shutdown_case(true, true, "old-pid", true);
}
#[test]
fn empty_owner_shutdown_retains_corrupt_history_without_blocking_or_mutating() {
    withdrawn_owner_shutdown_case(false, false, "corrupt", true);
}
#[test]
fn owner_shutdown_withdrawn_terminal_proxy_without_receipt_rejects_all_mutations() {
    withdrawn_owner_shutdown_case(true, true, "missing", false);
}
#[test]
fn owner_shutdown_withdrawn_terminal_proxy_with_corrupt_receipt_rejects_all_mutations() {
    withdrawn_owner_shutdown_case(true, true, "corrupt", false);
}

#[test]
fn owner_shutdown_absent_proxy_reconciles_real_owned_business_despite_corrupt_history() {
    withdrawn_owner_shutdown_case(false, true, "corrupt", true);
}
#[test]
fn owner_shutdown_absent_proxy_reconciles_real_owned_business_without_history() {
    withdrawn_owner_shutdown_case(false, true, "missing", true);
}
#[test]
fn owner_shutdown_absent_proxy_preserves_foreign_running_business_in_shared_engine() {
    withdrawn_owner_shutdown_case(false, true, "foreign-business", false);
}

#[cfg(target_os = "linux")]
#[test]
fn owner_shutdown_absent_configuration_uses_actual_owned_business_workspace() {
    withdrawn_owner_shutdown_case(false, true, "missing-conf", true);
}
#[cfg(target_os = "linux")]
#[test]
fn owner_shutdown_absent_spec_uses_actual_owned_business_workspace() {
    withdrawn_owner_shutdown_case(false, true, "missing-spec", true);
}
