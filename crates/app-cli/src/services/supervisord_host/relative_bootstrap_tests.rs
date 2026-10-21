//! Controlled supervisor protocol tests; these do not deploy supervisord/Pingap.
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

struct Environment(Vec<(&'static str, Option<std::ffi::OsString>)>);

impl Environment {
    fn set(values: Vec<(&'static str, Option<std::ffi::OsString>)>) -> Self {
        let saved = values
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
        Self(saved)
    }
}

impl Drop for Environment {
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

struct ForeignChild(std::process::Child);

impl Drop for ForeignChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Authority {
    Current,
    ForeignOwner,
    ForeignSocket,
}

fn process_row(name: &str, state: &str, code: i64, pid: i64) -> String {
    format!(
        r#"<struct>
        <member><name>group</name><value><string>{name}</string></value></member>
        <member><name>name</name><value><string>{name}</string></value></member>
        <member><name>statename</name><value><string>{state}</string></value></member>
        <member><name>state</name><value><int>{code}</int></value></member>
        <member><name>pid</name><value><int>{pid}</int></value></member>
        <member><name>start</name><value><int>1</int></value></member>
        </struct>"#
    )
}

fn recovery_case(
    proxy: Option<(&str, i64, i64)>,
    configured: bool,
    authority: Authority,
    relative: bool,
    succeeds: bool,
) {
    recovery_case_for_release(
        proxy,
        configured,
        authority,
        relative,
        succeeds,
        crate::svc_spec::RESIDENT_SPEC_ID,
        false,
    );
}

fn recovery_case_for_release(
    proxy: Option<(&str, i64, i64)>,
    configured: bool,
    authority: Authority,
    relative: bool,
    succeeds: bool,
    release: &str,
    outside_resident: bool,
) {
    let _lock = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    let mut foreign = (authority == Authority::ForeignOwner).then(|| {
        ForeignChild(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        )
    });
    let owner_pid = foreign
        .as_ref()
        .map_or(std::process::id(), |child| child.0.id());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let temporary = tempfile::tempdir().unwrap();
            let root = temporary.path().canonicalize().unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir_all(&workspace).unwrap();
            let data = workspace.join("user-data.txt");
            std::fs::write(&data, b"user data must remain").unwrap();
            let socket = root.join("supervisor.sock");
            let _environment = Environment::set(vec![
                ("APP_CLI_SPEC_DIR", Some(root.join("specs").into_os_string())),
                ("APP_CLI_SUPERVISOR_SOCKET", Some(socket.clone().into_os_string())),
                ("APP_CLI_MANAGED", None),
                ("SERVICE_TYPE", None),
                ("PROJECT_ID", None),
                ("USERAPP_SINGLE_APP_ID", None),
                ("USERAPP_WORKSPACE_DIR", None),
                ("APP_CLI_STATE_ROOT", None),
                ("APP_CLI_RUNTIME_WORKSPACE", None),
                ("APP_CLI_PINGAP_RUNTIME_DIR", None),
            ]);
            let args = RuntimeArgs {
                workspace: workspace.clone(),
                log_dir: root.join("logs"),
                pingap_bin: root.join("pingap"),
                ..Default::default()
            };
            let (lease, scope) = owner_test_context::scope(&root, &workspace);
            supervisor::resident::bind_owner(scope, &args).unwrap();
            if authority == Authority::ForeignSocket {
                unsafe {
                    std::env::set_var(
                        "APP_CLI_SUPERVISOR_SOCKET",
                        root.join("another-supervisor.sock"),
                    );
                }
            }
            let endpoint = admin_probe::AdminEndpoint {
                addr: "127.0.0.1:19086".into(),
                user: "relative-fixture".into(),
                password: "relative-fixture".into(),
            };
            let mut spec = pingap_service_spec(&args, &endpoint).unwrap();
            spec.release_id = release.into();
            if relative {
                spec.argv[2] = ".local-logs/pingap/active/pingap.toml".into();
            }
            let path = spec.write().unwrap();
            // Preserve both opaque original bytes and forward-compatible data.
            let mut document = toml::Value::try_from(&spec).unwrap();
            document.as_table_mut().unwrap().insert(
                "future_extension".into(),
                toml::Value::String("keep this field".into()),
            );
            let original = format!(
                "# original launch diagnostics\n{}",
                toml::to_string_pretty(&document).unwrap()
            )
            .into_bytes();
            std::fs::write(&path, &original).unwrap();
            let foreign_directory=root.join("foreign-spec-owner");
            if outside_resident {
                assert_ne!(release,crate::svc_spec::RESIDENT_SPEC_ID);
                std::fs::create_dir_all(&foreign_directory).unwrap();
                std::fs::write(foreign_directory.join("marker.txt"),b"foreign original data").unwrap();
                std::os::unix::fs::symlink(
                    &foreign_directory,
                    crate::svc_spec::spec_root().join(crate::svc_spec::RESIDENT_SPEC_ID),
                ).unwrap();
            }
            let conf = root.join("50-app-services.conf");
            let conf_bytes = if configured {
                format!("[program:app-pingap]\ncommand=/usr/bin/app-cli run-service {release} pingap\n").into_bytes()
            } else {
                b"# app-cli dynamic services stopped\n".to_vec()
            };
            std::fs::write(&conf, &conf_bytes).unwrap();
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let observed = calls.clone();
            let proxy = proxy.map(|(state, code, pid)| process_row(PINGAP_PROGRAM, state, code, pid));
            let owner = process_row("app-cli", "RUNNING", 20, i64::from(owner_pid));
            let rpc = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        header.push(stream.read_u8().await.unwrap());
                    }
                    let header = String::from_utf8(header).unwrap();
                    let size = header
                        .lines()
                        .find_map(|line| line.strip_prefix("Content-Length: "))
                        .unwrap()
                        .parse::<usize>()
                        .unwrap();
                    let mut body = vec![0; size];
                    stream.read_exact(&mut body).await.unwrap();
                    let request = String::from_utf8(body).unwrap();
                    let method = request
                        .split("<methodName>supervisor.")
                        .nth(1)
                        .unwrap()
                        .split("</methodName>")
                        .next()
                        .unwrap()
                        .to_owned();
                    observed.lock().unwrap().push(method.clone());
                    let value = match method.as_str() {
                        "getAllProcessInfo" => {
                            let proxy = proxy
                                .as_ref()
                                .map(|row| format!("<value>{row}</value>"))
                                .unwrap_or_default();
                            format!("<array><data><value>{owner}</value>{proxy}</data></array>")
                        }
                        "getProcessInfo" => {
                            assert!(request.contains("<string>app-cli</string>"));
                            owner.clone()
                        }
                        other => panic!("bootstrap recovery must not mutate supervisor: {other}"),
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
            let host = SupervisordHost::protocol_fixture(socket, conf.clone());
            let result = host
                .recover_relative_bootstrap_until(
                    &args,
                    &tokio_util::sync::CancellationToken::new(),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                )
                .await;
            assert_eq!(result.is_ok(), succeeds, "recovery result: {result:?}");
            let archives = std::fs::read_dir(crate::svc_spec::spec_root().join(crate::svc_spec::RESIDENT_SPEC_ID))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("pingap.toml.stale-relative-")
                })
                .collect::<Vec<_>>();
            if succeeds && relative {
                assert_eq!(archives.len(), 1);
                assert_eq!(std::fs::read(&archives[0]).unwrap(), original);
                let current = ServiceSpecFile::load(release, "pingap").unwrap();
                assert!(Path::new(&current.argv[2]).is_absolute());
                assert_ne!(current.argv[2], spec.argv[2]);
                let current_document = toml::from_str::<toml::Value>(
                    &std::fs::read_to_string(&path).unwrap(),
                )
                .unwrap();
                assert_eq!(current_document["future_extension"].as_str(), Some("keep this field"));
                // The original request is idempotent after successful repair.
                host.recover_relative_bootstrap_until(
                    &args,
                    &tokio_util::sync::CancellationToken::new(),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(3),
                )
                .await
                .unwrap();
                assert_eq!(std::fs::read(&archives[0]).unwrap(), original);
                if release != crate::svc_spec::RESIDENT_SPEC_ID {
                    let current_directory=crate::svc_spec::spec_root().join("current");
                    std::fs::create_dir_all(&current_directory).unwrap();
                    let current_data=current_directory.join("current-service.txt");
                    std::fs::write(&current_data,b"keep current service").unwrap();
                    ServiceSpecFile::prune_other_generations("current");
                    assert!(!crate::svc_spec::spec_root().join(release).exists(),"the retired generation should actually be pruned");
                    assert_eq!(std::fs::read(&archives[0]).unwrap(),original,"history must survive generation pruning");
                    assert_eq!(std::fs::read(current_data).unwrap(),b"keep current service");
                }
            } else {
                assert!(archives.is_empty());
                assert_eq!(std::fs::read(&path).unwrap(), original);
            }
            assert_eq!(std::fs::read(&conf).unwrap(), conf_bytes);
            assert_eq!(std::fs::read(data).unwrap(), b"user data must remain");
            if outside_resident {
                assert!(std::fs::symlink_metadata(crate::svc_spec::spec_root().join(crate::svc_spec::RESIDENT_SPEC_ID)).unwrap().file_type().is_symlink());
                assert_eq!(std::fs::read(foreign_directory.join("marker.txt")).unwrap(),b"foreign original data");
                let foreign_entries=std::fs::read_dir(&foreign_directory).unwrap().map(|entry|entry.unwrap().file_name()).collect::<Vec<_>>();
                assert_eq!(foreign_entries,vec![std::ffi::OsString::from("marker.txt")],"foreign directory must not receive archive or replacement spec");
            }
            assert!(calls.lock().unwrap().iter().all(|method| {
                method == "getAllProcessInfo" || method == "getProcessInfo"
            }));
            if !relative {
                assert!(calls.lock().unwrap().is_empty(), "valid history needs no recovery");
            }
            rpc.abort();
            let _ = rpc.await;
            supervisor::resident::shutdown().await.unwrap();
            drop(lease);
        });
    if let Some(child) = foreign.as_mut() {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "foreign owner must remain alive"
        );
    }
}

#[test]
fn relative_bootstrap_without_program_recovers_and_preserves_original() {
    recovery_case(None, false, Authority::Current, true, true);
}

#[test]
fn relative_bootstrap_stopped_zero_pid_recovers() {
    recovery_case(
        Some(("STOPPED", 0, 0)),
        true,
        Authority::Current,
        true,
        true,
    );
}

#[test]
fn relative_bootstrap_exited_zero_pid_recovers() {
    recovery_case(
        Some(("EXITED", 100, 0)),
        true,
        Authority::Current,
        true,
        true,
    );
}

#[test]
fn relative_bootstrap_fatal_zero_pid_recovers() {
    recovery_case(
        Some(("FATAL", 200, 0)),
        true,
        Authority::Current,
        true,
        true,
    );
}

#[test]
fn relative_bootstrap_stopped_nonzero_pid_retains_contract() {
    recovery_case(
        Some(("STOPPED", 0, 42)),
        true,
        Authority::Current,
        true,
        false,
    );
}

#[test]
fn relative_bootstrap_running_process_retains_contract() {
    recovery_case(
        Some(("RUNNING", 20, 42)),
        true,
        Authority::Current,
        true,
        false,
    );
}

#[test]
fn relative_bootstrap_unknown_process_retains_contract() {
    recovery_case(
        Some(("UNKNOWN", 1000, 0)),
        true,
        Authority::Current,
        true,
        false,
    );
}

#[test]
fn relative_bootstrap_foreign_live_owner_retains_contract() {
    recovery_case(None, false, Authority::ForeignOwner, true, false);
}

#[test]
fn relative_bootstrap_foreign_supervisor_socket_retains_contract() {
    recovery_case(None, false, Authority::ForeignSocket, true, false);
}

#[test]
fn relative_bootstrap_valid_absolute_spec_stays_byte_identical() {
    recovery_case(None, false, Authority::Current, false, true);
}

#[test]
fn relative_bootstrap_legacy_generation_history_survives_generation_pruning() {
    recovery_case_for_release(
        Some(("STOPPED", 0, 0)),
        true,
        Authority::Current,
        true,
        true,
        "legacy-generation",
        false,
    );
}

#[test]
fn relative_bootstrap_external_resident_symlink_preserves_foreign_directory() {
    recovery_case_for_release(
        Some(("STOPPED", 0, 0)),
        true,
        Authority::Current,
        true,
        false,
        "legacy-generation",
        true,
    );
}
