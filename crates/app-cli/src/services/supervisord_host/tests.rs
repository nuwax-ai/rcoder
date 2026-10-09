// Included at the host module root to preserve existing test filter names.
#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_start_keeps_acknowledgement_but_lost_reply_stays_uncertain() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for reply in [true, false] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("supervisor.sock");
            let listener = tokio::net::UnixListener::bind(&path).unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            let stop = cancel.clone();
            let server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                // Stop is accepted after the mutation reached the server, before
                // its response. Closing the socket simulates a lost response.
                stop.cancel();
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                if reply {
                    let body = "<methodResponse><params><param><value><boolean>1</boolean></value></param></params></methodResponse>";
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
                }
            });
            let client = SupervisorClient::new(path);
            let result = settle_start_mutation(
                tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                &cancel,
                client.start_process_wait("app-svc-worker"),
            )
            .await;
            if reply {
                assert!(
                    result.is_ok(),
                    "acknowledged start must allow cleanup: {result:?}"
                );
            } else {
                assert!(result.unwrap_err().is::<supervisor::ShutdownUnconfirmed>());
            }
            server.await.unwrap();
        }
    }

    /// B08：supervisord 引擎与 builtin 同一生效命令选择——dev profile 且
    /// 配 [devrun] 时 devrun.command 优先（不再恒用 run.command）。
    #[test]
    fn effective_argv_prefers_devrun_in_dev_profile() {
        let mut spec = spec("web", "web", 30);
        spec.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["pnpm".into(), "dev".into()],
        });
        // 非 dev profile：run 兜底
        assert_eq!(
            crate::supervisor::effective_run_argv(&spec, false),
            &spec.run.command
        );
        // dev profile：devrun 优先
        assert_eq!(
            crate::supervisor::effective_run_argv(&spec, true),
            &["pnpm".to_string(), "dev".to_string()]
        );
    }

    /// B08：devrun-only 服务（run.command 为空）在 dev profile 下可启动；
    /// 非 dev profile 判空跳过（生效命令选择同源）。
    #[test]
    fn devrun_only_service_starts_only_in_dev_profile() {
        let mut spec = spec("web", "web", 30);
        spec.run.command = Vec::new();
        spec.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["vite".into(), "--host".into()],
        });
        assert!(
            crate::supervisor::effective_run_argv(&spec, true)
                == ["vite".to_string(), "--host".to_string()]
        );
        assert!(crate::supervisor::effective_run_argv(&spec, false).is_empty());
    }

    fn spec(id: &str, dir: &str, shutdown: u64) -> ServiceSpec {
        let mut s = toml::from_str::<ReleaseLock>(
            r#"
schema_version = 1
release_id = "rel-t"
workspace_name = "ws"
minimum_app_cli_version = "0.0.0"
runtime_image_digest = ""

[pingap]
mode = "managed"
version = "0.14.3"
commit = "abc"

[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200

[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "text"

[services.env]
NODE_ENV = "production"
"#,
        )
        .unwrap();
        let svc = &mut s.services[0];
        svc.service_id = id.into();
        svc.dir = dir.into();
        svc.run.shutdown_timeout_seconds = shutdown;
        s.services.remove(0)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn recorded_engine_loss_never_becomes_builtin_cleanup_success() {
        let root = tempfile::tempdir().unwrap();
        let receipt = EngineReceipt {
            generation: "original-generation".into(),
            supervisor_id: "original-owner".into(),
            socket: root.path().join("missing-supervisord.sock"),
        };
        std::fs::write(
            root.path().join(ENGINE_RECEIPT),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let generation = root.path().join("generation.json");
        std::fs::write(
            &generation,
            r#"{"id":"original-generation","supervisor":"original-owner"}"#,
        )
        .unwrap();
        let error = SupervisordHost::cleanup_generation(root.path())
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("engine is unavailable"));
        std::fs::write(
            generation,
            r#"{"id":"replacement","supervisor":"original-owner"}"#,
        )
        .unwrap();
        let error = SupervisordHost::cleanup_generation(root.path())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("identity differs"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stop_withdraws_dynamic_restart_source_and_preserves_fixed_programs() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("supervisor.sock");
        let conf = root.path().join("dynamic.conf");
        std::fs::write(&conf, "[program:app-svc-web]\nautorestart=true\n").unwrap();
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let group = |name: &str| {
            format!(
                "<value><struct><member><name>group</name><value><string>{name}</string></value></member></struct></value>"
            )
        };
        let array = |values: String| format!("<array><data>{values}</data></array>");
        let fixed = array(group("postgres"));
        // P1：app-pingap 为常驻组——停止序列只含业务组 app-svc-web，
        // 常驻组与固定组（postgres）同样不被触碰。
        let requests = vec![
            ("reloadConfig", "", "<array><data><value><array><data><value><array><data></data></array></value><value><array><data></data></array></value><value><array><data></data></array></value></data></array></value></data></array>".into()),
            (
                "getAllProcessInfo",
                "",
                array(group("postgres") + &group("app-pingap") + &group("app-svc-web")),
            ),
            (
                "stopProcessGroup",
                "app-svc-web",
                "<boolean>1</boolean>".into(),
            ),
            (
                "removeProcessGroup",
                "app-svc-web",
                "<boolean>1</boolean>".into(),
            ),
            ("getAllProcessInfo", "", fixed.clone()),
            ("reloadConfig", "", "<array><data><value><array><data><value><array><data></data></array></value><value><array><data></data></array></value><value><array><data></data></array></value></data></array></value></data></array>".into()),
            ("getAllProcessInfo", "", fixed.clone()),
            ("getAllProcessInfo", "", fixed),
        ];
        let checked_conf = conf.clone();
        let server = tokio::spawn(async move {
            let mut pending = requests;
            let mut stopped = std::collections::BTreeSet::new();
            while !pending.is_empty() {
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
                let mut bytes = vec![0; size];
                stream.read_exact(&mut bytes).await.unwrap();
                let request = String::from_utf8(bytes).unwrap();
                // Groups now stop concurrently. Preserve per-group stop/remove
                // ordering while allowing either group to make progress first.
                let index = if matches!(pending[0].0, "stopProcessGroup" | "removeProcessGroup") {
                    pending
                        .iter()
                        .position(|(method, target, _)| {
                            request
                                .contains(&format!("<methodName>supervisor.{method}</methodName>"))
                                && request.contains(&format!("<string>{target}</string>"))
                        })
                        .expect("expected an outstanding group request")
                } else {
                    0
                };
                let (method, target, value) = pending.remove(index);
                assert!(
                    request.contains(&format!("<methodName>supervisor.{method}</methodName>")),
                    "{request}"
                );
                if method == "stopProcessGroup" {
                    assert!(stopped.insert(target));
                } else if method == "removeProcessGroup" {
                    assert!(
                        stopped.remove(target),
                        "remove must follow stop for this group"
                    );
                }
                if !target.is_empty() {
                    assert!(request.contains(&format!("<string>{target}</string>")));
                }
                assert!(
                    !request.contains("<string>postgres</string>"),
                    "must not stop fixed PG"
                );
                assert!(
                    !matches!(
                        (method, target),
                        ("stopProcessGroup", "app-pingap") | ("removeProcessGroup", "app-pingap")
                    ),
                    "resident proxy group must never be stopped or removed"
                );
                assert!(
                    !std::fs::read_to_string(&checked_conf)
                        .unwrap()
                        .contains("[program:"),
                    "withdraw config before RPC"
                );
                let body = format!(
                    "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
                );
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let host = SupervisordHost {
            client: SupervisorClient::new(socket),
            conf_path: conf,
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            host.stop_business().await.unwrap();
            host.stop_business().await.unwrap();
            server.await.unwrap();
        })
        .await
        .unwrap();
    }

    #[test]
    #[cfg(unix)] // supervisord 引擎 Unix-only（xmlrpc 走 Unix socket）；断言含 Unix 路径字符串
    fn renders_service_and_pingap_programs() {
        let specs = vec![spec("web", "web", 45)];
        let release = ReleaseLock {
            schema_version: 1,
            release_id: "rel-t".into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.14.3".into(),
                commit: "abc".into(),
            },
            minimum_app_cli_version: "0.0.0".into(),
            runtime_image_digest: String::new(),
            services: specs.clone(),
            bridge_service: None,
        };
        let conf = render_programs_conf(
            &release,
            &specs,
            Path::new("/app/logs"),
            Path::new("/app/code"),
            false,
        );
        assert!(conf.contains("[program:app-svc-web]"));
        assert!(conf.contains("run-service rel-t web"));
        assert!(conf.contains("stopwaitsecs=3"));
        assert!(conf.contains("directory=/app/code/web"));
        assert!(conf.contains("stdout_logfile=/app/logs/services/web.log"));
        assert!(conf.contains("redirect_stderr=true"));
        // P1：pingap program 移入常驻分片——业务分片只含 app-svc-*
        assert!(!conf.contains("[program:app-pingap]"));
        let resident = render_resident_conf(Path::new("/app/logs"));
        assert!(resident.contains("[program:app-pingap]"));
        assert!(resident.contains("run-service resident pingap"));
        assert!(resident.contains("stdout_logfile=/app/logs/services/pingap.log"));
        // autostart=false：启动顺序由 server 显式控制（依赖序）
        assert_eq!(conf.matches("autostart=false").count(), 1);

        // Frontend templates are static in prod, but must get a real Vite
        // program in dev even though [run].command is empty.
        let mut frontend = specs[0].clone();
        frontend.r#type = workspace_manifest::ProjectType::Static;
        frontend.run.command.clear();
        frontend.devrun = Some(workspace_manifest::DevrunSection {
            command: vec!["pnpm".into(), "dev".into()],
        });
        let dev_conf = render_programs_conf(
            &release,
            std::slice::from_ref(&frontend),
            Path::new("/app/logs"),
            Path::new("/app/code"),
            true,
        );
        assert!(dev_conf.contains("[program:app-svc-web]"));
        assert!(dev_conf.contains("run-service rel-t web"));

        // The same service becomes an in-process static host in production,
        // even when a stale [run] command is present in the manifest.
        frontend.run.command = vec!["must-not-run".into()];
        let prod_conf = render_programs_conf(
            &release,
            std::slice::from_ref(&frontend),
            Path::new("/app/logs"),
            Path::new("/app/code"),
            false,
        );
        assert!(!prod_conf.contains("[program:app-svc-web]"));
        assert!(!prod_conf.contains("[program:app-pingap]"));

        // No devrun means static hosting in both profiles; do not invent a
        // process from the stale command either.
        frontend.devrun = None;
        assert!(!runs_as_process(&frontend, true));
        assert!(!runs_as_process(&frontend, false));
    }

    #[test]
    fn safe_token_rejects_injection() {
        assert_eq!(safe_program_token("web-1_2.3"), "web-1_2.3");
        assert_eq!(safe_program_token("a b"), "");
        assert_eq!(safe_program_token("a\nb"), "");
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn stop_group_fault_is_not_swallowed_as_success() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("supervisor.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for step in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0u8; 8192];
                let size = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                let body = if step == 0 {
                    assert!(request.contains("supervisor.reloadConfig"));
                    // An unrelated malformed config cannot skip the stop RPC.
                    r#"<methodResponse><fault><value><struct><member><name>faultString</name><value><string>invalid fixed supervisor config</string></value></member></struct></value></fault></methodResponse>"#
                } else if step == 2 {
                    assert!(request.contains("supervisor.stopProcessGroup"));
                    r#"<methodResponse><fault><value><struct><member><name>faultString</name><value><string>stop failed</string></value></member></struct></value></fault></methodResponse>"#
                } else {
                    assert!(request.contains("supervisor.getAllProcessInfo"));
                    r#"<methodResponse><params><param><value><array><data><value><struct><member><name>group</name><value><string>app-svc-web</string></value></member></struct></value></data></array></value></param></params></methodResponse>"#
                };
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let host = SupervisordHost {
            client: SupervisorClient::new(path),
            conf_path: root.path().join("unused.conf"),
        };
        let error = host.stop_business().await.unwrap_err().to_string();
        assert!(error.contains("did not stop"));
        assert!(error.contains("invalid fixed supervisor config"));
        assert!(error.contains("stop failed"));
        server.await.unwrap();
    }
}

#[cfg(test)]
mod transition_tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn transitional_resident_states_are_observed_until_settled_without_starting_again() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            for (state, code) in [
                ("STARTING", 10),
                ("BACKOFF", 30),
                ("STOPPING", 40),
                ("RUNNING", 20),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 8192];
                let size = stream.read(&mut request).await.unwrap();
                let request = String::from_utf8_lossy(&request[..size]);
                assert!(request.contains("supervisor.getAllProcessInfo"));
                assert!(!request.contains("supervisor.startProcess"));
                let body = format!(
                    r#"<methodResponse><params><param><value><array><data><value><struct>
                    <member><name>group</name><value><string>app-pingap</string></value></member>
                    <member><name>name</name><value><string>app-pingap</string></value></member>
                    <member><name>statename</name><value><string>{state}</string></value></member>
                    <member><name>state</name><value><int>{code}</int></value></member>
                    <member><name>pid</name><value><int>42</int></value></member>
                    <member><name>start</name><value><int>123</int></value></member>
                    </struct></value></data></array></value></param></params></methodResponse>"#
                );
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let host = SupervisordHost::protocol_fixture(socket, root.path().join("dynamic.conf"));
        let info = host
            .resident_entry_settled_with_budget(std::time::Duration::from_secs(2))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            info.checked_state(PINGAP_PROGRAM).unwrap(),
            SupervisorProcessState::Running
        );
        assert_eq!(
            info.running_identity().unwrap(),
            crate::xmlrpc::RunningProcess {
                pid: 42,
                started: 123
            }
        );
        server.await.unwrap();
    }

    #[test]
    fn legacy_binding_selects_explicit_wrapper_release_and_exact_configuration_path() {
        let fragment = "[program:app-svc-pingap]\ncommand=/app/app-cli run-service business pingap\n[program:app-pingap]\ncommand=/app/app-cli run-service legacy-a pingap\n";
        assert_eq!(
            resident_release_from_fragment(fragment).unwrap(),
            Some("legacy-a".into())
        );
        assert!(
            resident_release_from_fragment("[program:app-pingap]\ncommand=other --anything\n")
                .is_err()
        );
        let argv = vec![
            "/usr/bin/pingap".into(),
            "-c".into(),
            "/runtime/old/pingap.toml".into(),
            "--autoreload".into(),
        ];
        assert_eq!(
            entry_config_from_argv(&argv).unwrap(),
            PathBuf::from("/runtime/old/pingap.toml")
        );
        let mut ambiguous = argv.clone();
        ambiguous.extend(["-c".into(), "/runtime/active/pingap.toml".into()]);
        assert!(entry_config_from_argv(&ambiguous).is_err());
    }
}

#[cfg(test)]
mod resident_domain_tests {
    use super::*;

    #[test]
    fn resident_receipt_rejects_foreign_application_workspace_domain_and_incarnation() {
        let temporary = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temporary.path()).unwrap();
        let socket = root.join("supervisor.sock");
        let identity = process_utils::command_authority::ResidentIdentity {
            application_id: "app-a".into(),
            binding: serde_json::json!({"component":"app-cli","resource":root}),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            physical_domain: Some(
                serde_json::json!({"authority":"container","instance":"container-a","volume":"workspace-a"}),
            ),
            process_epoch: Some("pid-namespace-a".into()),
        };
        let receipt = ResidentEngineReceipt {
            version: 1,
            identity: identity.clone(),
            runtime_root: root.clone(),
            socket: socket.clone(),
            config: root.join("active/pingap.toml"),
            process: Some(crate::xmlrpc::RunningProcess {
                pid: 42,
                started: 123,
            }),
        };
        let bytes = serde_json::to_vec(&receipt).unwrap();
        std::fs::write(root.join(RESIDENT_ENGINE_RECEIPT), &bytes).unwrap();
        assert!(
            verify_resident_engine_receipt(&root, &identity, &socket)
                .unwrap()
                .is_some()
        );
        let mut foreign = identity.clone();
        foreign.application_id = "app-b".into();
        assert!(verify_resident_engine_receipt(&root, &foreign, &socket).is_err());
        foreign = identity.clone();
        foreign.binding["resource"] = "/different-workspace".into();
        assert!(verify_resident_engine_receipt(&root, &foreign, &socket).is_err());
        foreign = identity.clone();
        foreign.physical_domain.as_mut().unwrap()["instance"] = "container-b".into();
        assert!(verify_resident_engine_receipt(&root, &foreign, &socket).is_err());
        foreign = identity.clone();
        foreign.process_epoch = Some("pid-namespace-b".into());
        assert!(verify_resident_engine_receipt(&root, &foreign, &socket).is_err());
        assert!(
            verify_resident_engine_receipt(&root, &identity, &root.join("other.sock")).is_err()
        );
        assert_eq!(
            std::fs::read(root.join(RESIDENT_ENGINE_RECEIPT)).unwrap(),
            bytes,
            "observation cannot overwrite prior ownership evidence"
        );
        std::fs::write(root.join(RESIDENT_ENGINE_RECEIPT), b"{corrupt").unwrap();
        assert!(verify_resident_engine_receipt(&root, &identity, &socket).is_err());
    }
}

#[cfg(test)]
mod inactive_entry_ownership_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn foreign_or_corrupt_inactive_receipt_is_rejected_before_active_or_supervisor_mutation() {
        let _spec_guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let saved_spec_dir = std::env::var_os("APP_CLI_SPEC_DIR");
            let temporary = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temporary.path()).unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let owner_root = root.join("owner");
            let lease = process_utils::command_authority::OwnerLease::try_acquire(&owner_root)
                .unwrap()
                .unwrap();
            let identity = process_utils::command_authority::ResidentIdentity {
                application_id: "app-a".into(),
                binding: serde_json::json!({"component":"app-cli","resource":workspace}),
                owner_instance: uuid::Uuid::new_v4().to_string(),
                physical_domain: Some(
                    serde_json::json!({"authority":"container","instance":"container-a","volume":"workspace-a"}),
                ),
                process_epoch: Some("pid-namespace-a".into()),
            };
            std::fs::write(owner_root.join("supervisor.json"),serde_json::to_vec(&serde_json::json!({
                "instance":identity.owner_instance,"snapshot":{"supervisor_id":identity.owner_instance,"binding":identity.binding}
            })).unwrap()).unwrap();
            let scope =
                runtime_supervisor::ResidentScope::initialize(&lease, identity.clone()).unwrap();
            let args = RuntimeArgs {
                workspace,
                log_dir: root.join("logs"),
                ..Default::default()
            };
            supervisor::resident::bind_owner(scope, &args).unwrap();
            let runtime_root = crate::proxy::compiler::runtime_root(&args.log_dir);
            let active = crate::proxy::compiler::active_config_path(&runtime_root);
            std::fs::create_dir_all(active.parent().unwrap()).unwrap();
            let original_active = b"[servers.app]\naddr = \"127.0.0.1:19081\"\n";
            std::fs::write(&active, original_active).unwrap();
            unsafe {
                std::env::set_var("APP_CLI_SPEC_DIR", root.join("specs"));
            }
            ServiceSpecFile {
                release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(),
                service_id: "pingap".into(),
                cwd: "/".into(),
                argv: vec![
                    "/usr/bin/pingap".into(),
                    "-c".into(),
                    active.to_string_lossy().into_owned(),
                    "--autoreload".into(),
                ],
                env: std::collections::BTreeMap::from([
                    ("PINGAP_ADMIN_ADDR".into(), "127.0.0.1:3018".into()),
                    ("PINGAP_ADMIN_USER".into(), "test".into()),
                    ("PINGAP_ADMIN_PASSWORD".into(), "secret".into()),
                ]),
                port: None,
            }
            .write()
            .unwrap();
            let conf = root.join("dynamic.conf");
            let original_fragment =
                b"[program:app-pingap]\ncommand=/app/app-cli run-service resident pingap\n";
            std::fs::write(&conf, original_fragment).unwrap();
            let socket = root.join("rpc.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let counted = calls.clone();
            let (states, mut responses) = tokio::sync::mpsc::unbounded_channel::<(&'static str, i64)>();
            let server = tokio::spawn(async move {
                while let Some((state, code)) = responses.recv().await {
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
                    let request = String::from_utf8(body).unwrap();
                    assert!(request.contains("supervisor.getAllProcessInfo"));
                    assert!(!request.contains("supervisor.startProcess"));
                    counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let body = format!(
                        r#"<methodResponse><params><param><value><array><data><value><struct>
                        <member><name>group</name><value><string>app-pingap</string></value></member>
                        <member><name>name</name><value><string>app-pingap</string></value></member>
                        <member><name>statename</name><value><string>{state}</string></value></member>
                        <member><name>state</name><value><int>{code}</int></value></member>
                        <member><name>pid</name><value><int>0</int></value></member>
                        <member><name>start</name><value><int>0</int></value></member>
                        </struct></value></data></array></value></param></params></methodResponse>"#
                    );
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
            });
            let host = SupervisordHost::protocol_fixture(socket, conf.clone());
            for (index, (state, code, change)) in [
                ("STOPPED", 0, "application"),
                ("FATAL", 200, "domain"),
                ("EXITED", 100, "corrupt"),
            ]
            .into_iter()
            .enumerate()
            {
                let mut foreign = identity.clone();
                if change == "application" {
                    foreign.application_id = "foreign-app".into();
                }
                if change == "domain" {
                    foreign.physical_domain.as_mut().unwrap()["instance"] = "foreign-container".into();
                }
                let receipt = ResidentEngineReceipt {
                    version: 1,
                    identity: foreign,
                    runtime_root: std::fs::canonicalize(&runtime_root).unwrap(),
                    socket: crate::xmlrpc::default_socket_path(),
                    config: active.clone(),
                    process: None,
                };
                let bytes = if change == "corrupt" {
                    b"{corrupt".to_vec()
                } else {
                    serde_json::to_vec(&receipt).unwrap()
                };
                std::fs::write(runtime_root.join(RESIDENT_ENGINE_RECEIPT), &bytes).unwrap();
                states.send((state, code)).unwrap();
                let error = host
                    .drain_entry_until(
                        &runtime_root,
                        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
                    )
                    .await
                    .unwrap_err();
                assert!(
                    error.downcast_ref::<EntryOwnershipUnconfirmed>().is_some(),
                    "{error:#}"
                );
                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    index + 1,
                    "only the initial observation is allowed"
                );
                assert_eq!(std::fs::read(&active).unwrap(), original_active);
                assert_eq!(std::fs::read(&conf).unwrap(), original_fragment);
                assert_eq!(
                    std::fs::read(runtime_root.join(RESIDENT_ENGINE_RECEIPT)).unwrap(),
                    bytes
                );
                assert!(!root.join("51-app-cli-resident.conf").exists());
            }
            drop(states);
            server.await.unwrap();
            supervisor::resident::shutdown().await.unwrap();
            drop(lease);
            unsafe {
                match saved_spec_dir {
                    Some(path) => std::env::set_var("APP_CLI_SPEC_DIR", path),
                    None => std::env::remove_var("APP_CLI_SPEC_DIR"),
                };
            }
        });
    }
}

#[cfg(test)]
mod owner_test_context {
    use super::*;

    pub(super) fn scope(
        root: &Path,
        workspace: &Path,
    ) -> (
        process_utils::command_authority::OwnerLease,
        runtime_supervisor::ResidentScope,
    ) {
        let owner_root = root.join("owner");
        let lease = process_utils::command_authority::OwnerLease::try_acquire(&owner_root)
            .unwrap()
            .unwrap();
        let identity = process_utils::command_authority::ResidentIdentity {
            application_id: "app-a".into(),
            binding: serde_json::json!({"component":"app-cli","resource":workspace}),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            physical_domain: Some(
                serde_json::json!({"authority":"container","instance":"container-a","volume":"workspace-a"}),
            ),
            process_epoch: Some("pid-namespace-a".into()),
        };
        std::fs::write(owner_root.join("supervisor.json"),serde_json::to_vec(&serde_json::json!({
            "instance":identity.owner_instance,"snapshot":{"supervisor_id":identity.owner_instance,"binding":identity.binding}
        })).unwrap()).unwrap();
        let scope = runtime_supervisor::ResidentScope::initialize(&lease, identity).unwrap();
        (lease, scope)
    }

    pub(super) fn restore_spec_env(value: Option<std::ffi::OsString>) {
        unsafe {
            match value {
                Some(value) => std::env::set_var("APP_CLI_SPEC_DIR", value),
                None => std::env::remove_var("APP_CLI_SPEC_DIR"),
            };
        }
    }
}

#[cfg(test)]
mod owner_shutdown_domain_tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn inactive_owner_shutdown_rejects_foreign_missing_and_corrupt_claim_before_any_rpc_or_write() {
        let _env_guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let saved = std::env::var_os("APP_CLI_SPEC_DIR");
            let temporary = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temporary.path()).unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir(&workspace).unwrap();
            let (_lease, scope) = owner_test_context::scope(&root, &workspace);
            let runtime_root = root.join("runtime");
            let active = crate::proxy::compiler::active_config_path(&runtime_root);
            std::fs::create_dir_all(active.parent().unwrap()).unwrap();
            let original_active = b"original active bytes";
            std::fs::write(&active, original_active).unwrap();
            unsafe {
                std::env::set_var("APP_CLI_SPEC_DIR", root.join("specs"));
            }
            ServiceSpecFile {
                release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(),
                service_id: "pingap".into(),
                cwd: "/".into(),
                argv: vec![
                    "/usr/bin/pingap".into(),
                    "-c".into(),
                    active.to_string_lossy().into_owned(),
                    "--autoreload".into(),
                ],
                env: std::collections::BTreeMap::from([
                    ("PINGAP_ADMIN_ADDR".into(), "127.0.0.1:3018".into()),
                    ("PINGAP_ADMIN_USER".into(), "test".into()),
                    ("PINGAP_ADMIN_PASSWORD".into(), "secret".into()),
                ]),
                port: None,
            }
            .write()
            .unwrap();
            let conf = root.join("dynamic.conf");
            let fragment = b"[program:app-pingap]\ncommand=/app/app-cli run-service resident pingap\n";
            std::fs::write(&conf, fragment).unwrap();
            let standalone = root.join(RESIDENT_CONF_FILE);
            let standalone_bytes = b"# unrelated preserved fragment\n";
            std::fs::write(&standalone, standalone_bytes).unwrap();
            let socket = root.join("rpc.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let observed = calls.clone();
            let state = std::sync::Arc::new(std::sync::Mutex::new("STOPPED"));
            let state_rpc = state.clone();
            let server = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    observed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
                    let mut request = vec![0; size];
                    stream.read_exact(&mut request).await.unwrap();
                    let request = String::from_utf8(request).unwrap();
                    let state = *state_rpc.lock().unwrap();
                    let value = if request.contains("getAllProcessInfo") && state == "NONE" {
                        "<array><data></data></array>".to_string()
                    } else if request.contains("getAllProcessInfo") {
                        let code = if state == "STOPPED" { 0 } else { 200 };
                        format!(
                            r#"<array><data><value><struct><member><name>group</name><value><string>app-pingap</string></value></member><member><name>name</name><value><string>app-pingap</string></value></member><member><name>statename</name><value><string>{state}</string></value></member><member><name>state</name><value><int>{code}</int></value></member><member><name>pid</name><value><int>0</int></value></member><member><name>start</name><value><int>0</int></value></member></struct></value></data></array>"#
                        )
                    } else if request.contains("reloadConfig") {
                        "<array><data><value><array><data><value><array><data></data></array></value><value><array><data></data></array></value><value><array><data></data></array></value></data></array></value></data></array>".into()
                    } else {
                        "<boolean>1</boolean>".into()
                    };
                    let body = format!(
                        "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
                    );
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
            });
            let host = SupervisordHost::protocol_fixture(socket, conf.clone());
            for (current, change) in [
                ("STOPPED", "application"),
                ("FATAL", "missing"),
                ("NONE", "domain"),
                ("STOPPED", "corrupt"),
            ] {
                *state.lock().unwrap() = current;
                let receipt_path = runtime_root.join(RESIDENT_ENGINE_RECEIPT);
                let mut identity = scope.identity().clone();
                if change == "application" {
                    identity.application_id = "foreign-app".into();
                }
                if change == "domain" {
                    identity.physical_domain = None;
                }
                let receipt = ResidentEngineReceipt {
                    version: 1,
                    identity,
                    runtime_root: std::fs::canonicalize(&runtime_root).unwrap(),
                    socket: crate::xmlrpc::default_socket_path(),
                    config: active.clone(),
                    process: None,
                };
                if change == "missing" {
                    if receipt_path.exists() {
                        std::fs::remove_file(&receipt_path).unwrap();
                    }
                } else {
                    std::fs::write(
                        &receipt_path,
                        if change == "corrupt" {
                            b"{bad".to_vec()
                        } else {
                            serde_json::to_vec(&receipt).unwrap()
                        },
                    )
                    .unwrap();
                }
                let error = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    host.shutdown_owner_entry(&runtime_root, &scope),
                )
                .await
                .unwrap()
                .unwrap_err();
                assert!(
                    error.to_string().contains("receipt")
                        || error.to_string().contains("another application"),
                    "{error:#}"
                );
                assert_eq!(
                    calls.load(std::sync::atomic::Ordering::SeqCst),
                    0,
                    "domain rejection must precede every RPC"
                );
                assert_eq!(std::fs::read(&active).unwrap(), original_active);
                assert_eq!(std::fs::read(&conf).unwrap(), fragment);
                assert_eq!(std::fs::read(&standalone).unwrap(), standalone_bytes);
            }
            server.abort();
            let _ = server.await;
            scope.close().unwrap();
            scope.record_quiescent().unwrap();
            owner_test_context::restore_spec_env(saved);
        });
    }
}

#[cfg(test)]
mod publication_receipt_tests {
    use super::*;

    /// Complete host orchestration with controlled supervisor/admin/entry RPC
    /// peers and a real in-process static backend. This is protocol evidence.
    #[cfg(unix)]
    #[test]
    fn successful_orchestration_keeps_publication_confirmed_after_owner_receipt_writes() {
        let _env_guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            use std::os::unix::fs::PermissionsExt;
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let saved_spec = std::env::var_os("APP_CLI_SPEC_DIR");
            let saved_pg = std::env::var_os("APP_CLI_REQUIRE_PG");
            let saved_runtime = std::env::var_os("APP_CLI_PINGAP_RUNTIME_DIR");
            let temporary = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temporary.path()).unwrap();
            let workspace = root.join("workspace");
            std::fs::create_dir_all(workspace.join("web/dist")).unwrap();
            std::fs::write(
                workspace.join("web/dist/index.html"),
                b"real static backend",
            )
            .unwrap();
            let backend = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let backend_port = backend.local_addr().unwrap().port();
            drop(backend);
            let public = tokio::net::TcpListener::bind("0.0.0.0:9080").await.unwrap();
            let secondary = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let secondary_address = secondary.local_addr().unwrap();
            let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let admin_address = admin.local_addr().unwrap();
            let release: ReleaseLock = toml::from_str(&format!(
                r#"
                schema_version=1
                release_id="publication-fixture"
                workspace_name="fixture"
                minimum_app_cli_version="0.0.0"
                runtime_image_digest="fixture"
                [pingap]
                mode="custom"
                config="proxy.toml"
                version="0.15.0"
                commit="fixture"
                [[services]]
                service_id="web"
                name="Web"
                dir="web"
                type="static"
                kind="web"
                enabled=true
                port={backend_port}
                static_content_dir="dist"
                logs=[]
                [services.run]
                command=[]
                migrate=[]
                depends_on=[]
                shutdown_timeout_seconds=1
                [services.health]
                startup_path="/"
                readiness_path="/"
                liveness_path="/"
                [services.proxy]
                path="/"
                strip_prefix=false
                [services.env]
            "#
            ))
            .unwrap();
            std::fs::write(workspace.join("proxy.toml"),format!("[servers.app]\naddr=\"0.0.0.0:9080,{secondary_address}\"\nlocations=[\"business\"]\n[locations.business]\npath=\"/\"\nupstream=\"web\"\n[upstreams.web]\naddrs=[\"127.0.0.1:{backend_port}\"]\n")).unwrap();
            let pingap = root.join("pingap");
            std::fs::write(
                &pingap,
                "#!/bin/sh\nif [ \"$1\" = '--apply-protocol-version' ]; then echo 1; fi\nexit 0\n",
            )
            .unwrap();
            std::fs::set_permissions(&pingap, std::fs::Permissions::from_mode(0o755)).unwrap();
            let args = RuntimeArgs {
                workspace: workspace.clone(),
                log_dir: root.join("logs"),
                pingap_bin: pingap,
                ..Default::default()
            };
            let runtime_root = args.log_dir.join("pingap");
            unsafe {
                std::env::set_var("APP_CLI_SPEC_DIR", root.join("specs"));
                std::env::remove_var("APP_CLI_REQUIRE_PG");
                std::env::set_var("APP_CLI_PINGAP_RUNTIME_DIR", &runtime_root);
            }
            let outcome = crate::proxy::compiler::compile_and_validate(
                &workspace,
                &runtime_root,
                &args.pingap_bin,
                &release,
                false,
            )
            .await
            .unwrap();
            let active = crate::proxy::compiler::active_config_path(&runtime_root);
            ServiceSpecFile {
                release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(),
                service_id: "pingap".into(),
                cwd: "/".into(),
                argv: vec![
                    args.pingap_bin.to_string_lossy().into_owned(),
                    "-c".into(),
                    active.to_string_lossy().into_owned(),
                    "--autoreload".into(),
                ],
                env: std::collections::BTreeMap::from([
                    ("PINGAP_ADMIN_ADDR".into(), admin_address.to_string()),
                    ("PINGAP_ADMIN_USER".into(), "protocol".into()),
                    ("PINGAP_ADMIN_PASSWORD".into(), "protocol-password".into()),
                ]),
                port: None,
            }
            .write()
            .unwrap();
            let (lease, scope) = owner_test_context::scope(&root, &workspace);
            let identity = scope.identity().clone();
            supervisor::resident::bind_owner(scope, &args).unwrap();
            let report = crate::proxy::apply_status::ApplyStatus {
                schema_version: 1,
                process_id: std::process::id(),
                process_instance_id: uuid::Uuid::new_v4().to_string(),
                applied: Some(crate::proxy::apply_status::AppliedPublication {
                    attempt_id: 1,
                    operation_id: Some(outcome.publication_id.clone()),
                    config_hash: outcome.expected_hash.clone(),
                    config_digest: outcome.config_digest.clone(),
                    applied_at_unix_ms: 1,
                }),
                last_attempt: None,
            };
            assert_eq!(outcome.entry_probes.len(), 2, "both the public and secondary listeners must be verified");
            assert!(outcome.entry_probes.iter().any(|target| target.address == "127.0.0.1:9080".parse().unwrap()));
            assert!(outcome.entry_probes.iter().any(|target| target.address == secondary_address));
            assert!(!outcome.business_probes.is_empty(), "the real static backend remains part of serving confirmation");
            let mut http_servers = Vec::new();
            for (listener, is_admin) in [(public, false), (secondary, false), (admin, true)] {
                let publication = outcome.publication_id.clone();
                let report = report.clone();
                http_servers.push(tokio::spawn(async move {
                    loop {
                        let (mut stream, _) = listener.accept().await.unwrap();
                        let mut header = Vec::new();
                        while !header.ends_with(b"\r\n\r\n") { header.push(stream.read_u8().await.unwrap()); }
                        let request = String::from_utf8(header).unwrap();
                        let (body, extra) = if is_admin {
                            assert!(request.starts_with("GET /api/apply-status "));
                            let authorization = request.lines().find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("authorization").then(|| value.trim())
                            }).unwrap();
                            let (_, timestamp) = authorization.rsplit_once(':').unwrap();
                            let timestamp: u64 = timestamp.parse().unwrap();
                            assert_eq!(authorization, admin_probe::authorization_header("protocol", "protocol-password", timestamp));
                            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
                            assert!(now.abs_diff(timestamp) <= 60, "admin authentication timestamp is stale");
                            (serde_json::to_string(&report).unwrap(), String::new())
                        } else {
                            assert!(request.starts_with(&format!("GET /_pub/{publication} ")));
                            (publication.clone(), format!("X-Rcoder-Publication: {publication}\r\n"))
                        };
                        stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{extra}Connection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                    }
                }));
            }
            let socket = root.join("rpc.sock");
            let listener = tokio::net::UnixListener::bind(&socket).unwrap();
            let pid = std::process::id();
            let supervisor_server = tokio::spawn(async move {
                for method in [
                    "getAllProcessInfo",
                    "getAllProcessInfo",
                    "reloadConfig",
                    "getAllProcessInfo",
                    "addProcessGroup",
                    "startProcess",
                    "getProcessInfo",
                    "getProcessInfo",
                ] {
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
                    let mut bytes = vec![0; size];
                    stream.read_exact(&mut bytes).await.unwrap();
                    let request = String::from_utf8(bytes).unwrap();
                    assert!(
                        request.contains(&format!("<methodName>supervisor.{method}</methodName>")),
                        "{request}"
                    );
                    let value=match method {
                        "getAllProcessInfo"=>"<array><data></data></array>".to_string(),
                        "reloadConfig"=>"<array><data><value><array><data><value><array><data><value><string>app-pingap</string></value></data></array></value><value><array><data></data></array></value><value><array><data></data></array></value></data></array></value></data></array>".into(),
                        "getProcessInfo"=>format!(r#"<struct><member><name>group</name><value><string>app-pingap</string></value></member><member><name>name</name><value><string>app-pingap</string></value></member><member><name>statename</name><value><string>RUNNING</string></value></member><member><name>state</name><value><int>20</int></value></member><member><name>pid</name><value><int>{pid}</int></value></member><member><name>start</name><value><int>123</int></value></member></struct>"#),
                        _=>"<boolean>1</boolean>".into(),
                    };
                    let body = format!(
                        "<methodResponse><params><param><value>{value}</value></param></params></methodResponse>"
                    );
                    stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
                }
            });
            let host = SupervisordHost::protocol_fixture(socket, root.join("dynamic.conf"));
            let status = RuntimeStatusService::default();
            host.orchestrate(
                &args,
                &release,
                &status,
                supervisor::RunProfile {
                    run_migrations: false,
                    dev_profile: false,
                    pg: None,
                    prepared_proxy: Some(supervisor::PreparedProxy {
                        workspace: workspace.clone(),
                        release: release.clone(),
                        dev_profile: false,
                        outcome: outcome.clone(),
                    }),
                },
                &tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
            supervisor_server.await.unwrap();
            assert!(status.is_ready());
            assert!(
                !crate::proxy::compiler::publication_uncertain(),
                "receipt JSON writes must not invalidate an already Applied publication"
            );
            let (confirmed, receipt) = crate::proxy::compiler::confirmed_publication().unwrap();
            assert_eq!(confirmed.publication_id, outcome.publication_id);
            assert_eq!(receipt.process_id, pid);
            let owner_receipt = read_resident_engine_receipt(&runtime_root)
                .unwrap()
                .unwrap();
            assert_eq!(owner_receipt.identity, identity);
            assert_eq!(owner_receipt.process.unwrap().pid, i64::from(pid));
            assert_eq!(
                std::fs::read(&active).unwrap(),
                std::fs::read(&outcome.config_path).unwrap()
            );
            assert!(host.resident_conf_path().unwrap().exists());
            let body = reqwest::get(format!("http://127.0.0.1:{backend_port}/"))
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert_eq!(body, "real static backend");
            crate::static_hosting::reconcile(&[], &workspace, false)
                .await
                .unwrap();
            supervisor::resident::shutdown().await.unwrap();
            drop(lease);
            for server in http_servers { server.abort(); let _ = server.await; }
            owner_test_context::restore_spec_env(saved_spec);
            unsafe {
                match saved_pg {
                    Some(value) => std::env::set_var("APP_CLI_REQUIRE_PG", value),
                    None => std::env::remove_var("APP_CLI_REQUIRE_PG"),
                };
                match saved_runtime {
                    Some(value) => std::env::set_var("APP_CLI_PINGAP_RUNTIME_DIR", value),
                    None => std::env::remove_var("APP_CLI_PINGAP_RUNTIME_DIR"),
                };
            }
        });
    }
}
