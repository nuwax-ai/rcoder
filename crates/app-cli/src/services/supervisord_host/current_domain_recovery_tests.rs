//! Historical metadata cannot revoke the current application's inert engine.
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

#[derive(Clone, Copy)]
pub(super) enum NativeOwner {
    None,
    Current,
    #[cfg(target_os = "linux")]
    Ancestor,
    Foreign,
}

#[allow(clippy::too_many_arguments)]
fn recovery_case(
    claim: &str,
    managed: bool,
    native: NativeOwner,
    state: &str,
    code: i64,
    pid: i64,
    fresh_only: bool,
    succeeds: bool,
) {
    recovery_case_with_context(
        claim, managed, native, state, code, pid, fresh_only, succeeds, "normal",
    );
}

#[allow(clippy::too_many_arguments)]
pub(super) fn recovery_case_with_context(
    claim: &str,
    managed: bool,
    native: NativeOwner,
    state: &str,
    code: i64,
    pid: i64,
    fresh_only: bool,
    succeeds: bool,
    mode: &str,
) {
    let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    let mut foreign = matches!(native, NativeOwner::Foreign).then(|| {
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap()
    });
    let owner_pid = match native {
        NativeOwner::None => None,
        NativeOwner::Current => Some(std::process::id()),
        NativeOwner::Foreign => Some(foreign.as_ref().unwrap().id()),
        #[cfg(target_os = "linux")]
        NativeOwner::Ancestor => {
            #[cfg(target_os = "linux")]
            {
                let text = std::fs::read_to_string("/proc/self/stat").unwrap();
                let fields = text.rsplit_once(") ").unwrap().1;
                Some(fields.split_whitespace().nth(1).unwrap().parse().unwrap())
            }
            #[cfg(not(target_os = "linux"))]
            panic!("ancestor fixture is Linux-only")
        }
    };
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        let temporary = tempfile::tempdir().unwrap();
        let base = temporary.path().canonicalize().unwrap();
        let workspace = base.join("workspaces/app-a");
        let owner_root = workspace.join("state/app-a");
        std::fs::create_dir_all(&owner_root).unwrap();
        let env = Environment::set(vec![
            ("APP_CLI_SPEC_DIR", Some(base.join("specs").into_os_string())),
            ("APP_CLI_MANAGED", managed.then(|| "1".into())),
            ("SERVICE_TYPE", managed.then(|| "userapp-builder".into())),
            ("PROJECT_ID", managed.then(|| "app-a".into())),
            ("USERAPP_SINGLE_APP_ID", managed.then(|| "app-a".into())),
            ("USERAPP_WORKSPACE_DIR", managed.then(|| workspace.clone().into_os_string())),
            ("APP_CLI_STATE_ROOT", managed.then(|| owner_root.clone().into_os_string())),
            ("APP_CLI_RUNTIME_WORKSPACE", None),
            ("APP_CLI_SUPERVISOR_SOCKET", Some(base.join("rpc.sock").into_os_string())),
        ]);
        let lease = process_utils::command_authority::OwnerLease::try_acquire(&owner_root)
            .unwrap().unwrap();
        let identity = process_utils::command_authority::ResidentIdentity {
            application_id: "app-a".into(),
            binding: serde_json::json!({"component":"app-cli","resource":workspace}),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            physical_domain: None, process_epoch: None,
        };
        std::fs::write(owner_root.join("supervisor.json"), serde_json::to_vec(&serde_json::json!({
            "instance":identity.owner_instance,
            "snapshot":{"supervisor_id":identity.owner_instance,"binding":identity.binding}
        })).unwrap()).unwrap();
        let scope = runtime_supervisor::ResidentScope::initialize(&lease, identity.clone()).unwrap();
        let args = RuntimeArgs { workspace: workspace.clone(), log_dir: workspace.join("logs"),
            ..Default::default() };
        supervisor::resident::bind_owner(scope.clone(), &args).unwrap();
        // Establish the real captured owner first, then simulate a drifted old
        // declaration. Initial ownership validation remains unchanged.
        unsafe { match mode {
            "legacy-layout" => std::env::set_var("APP_CLI_STATE_ROOT", base.join("old-state-layout")),
            "foreign-project" => std::env::set_var("PROJECT_ID", "different-current-app"),
            "foreign-socket" => std::env::set_var("APP_CLI_SUPERVISOR_SOCKET", base.join("foreign-current.sock")),
            _ => {},
        }}

        let runtime = crate::proxy::compiler::runtime_root(&args.log_dir).unwrap();
        std::fs::create_dir_all(&runtime).unwrap();
        let current = runtime.join(RESIDENT_ENGINE_RECEIPT);
        let foreign_config = base.join("foreign-app/config.toml");
        std::fs::create_dir_all(foreign_config.parent().unwrap()).unwrap();
        std::fs::write(&foreign_config, b"foreign resource must stay intact").unwrap();
        let old = match claim {
            "missing" => None,
            "corrupt" => Some(b"{original unreadable historical receipt".to_vec()),
            "foreign" => {
                let mut old = identity.clone(); old.application_id = "foreign-app".into();
                Some(serde_json::to_vec(&ResidentEngineReceipt {
                    version: 1, identity: old, runtime_root: base.join("foreign-app"),
                    socket: "/foreign/supervisor.sock".into(), config: foreign_config.clone(),
                    process: Some(crate::xmlrpc::RunningProcess { pid: i64::from(std::process::id()), started: 1 }),
                }).unwrap())
            }
            _ => panic!("unknown historical fixture"),
        };
        if let Some(bytes) = &old { std::fs::write(&current, bytes).unwrap(); }
        let conf = base.join("50-app-services.conf");
        std::fs::write(&conf, b"# no business restart sources
").unwrap();
        let configured = matches!(mode,"configured"|"live"|"live-foreign-path");
        let actual_config = if mode=="live-foreign-path" { foreign_config.clone() } else { runtime.join("fresh.toml") };
        if configured {
            ServiceSpecFile { release_id: crate::svc_spec::RESIDENT_SPEC_ID.into(), service_id: "pingap".into(),
                cwd: "/".into(), argv: vec!["/usr/bin/pingap".into(),"-c".into(),actual_config.to_string_lossy().into_owned()],
                env: std::collections::BTreeMap::from([("PINGAP_ADMIN_ADDR".into(),"127.0.0.1:19086".into()),
                    ("PINGAP_ADMIN_USER".into(),"fixture".into()),("PINGAP_ADMIN_PASSWORD".into(),"fixture".into())]),port:None }.write().unwrap();
            std::fs::write(&conf,"[program:app-pingap]\ncommand=/usr/bin/app-cli run-service resident pingap\n").unwrap();
        }
        let original_conf = std::fs::read(&conf).unwrap();

        std::fs::write(base.join(RESIDENT_CONF_FILE), b"# official proxy stopped
").unwrap();
        let socket = base.join("rpc.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let calls = observed.clone();
        let state = state.to_owned();
        let rpc = tokio::spawn(async move {
            let mut present = !fresh_only || state == "RUNNING" || pid != 0;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut header = Vec::new();
                while !header.ends_with(b"\r\n\r\n") { header.push(stream.read_u8().await.unwrap()); }
                let header = String::from_utf8(header).unwrap();
                let size = header.lines().find_map(|line| line.strip_prefix("Content-Length: "))
                    .unwrap().parse::<usize>().unwrap();
                let mut body = vec![0; size]; stream.read_exact(&mut body).await.unwrap();
                let request = String::from_utf8(body).unwrap();
                let method = request.split("<methodName>supervisor.").nth(1).unwrap()
                    .split("</methodName>").next().unwrap().to_owned();
                calls.lock().unwrap().push(method.clone());
                let owner = owner_pid.map(|owner| format!(r#"<struct>
                    <member><name>group</name><value><string>app-cli</string></value></member>
                    <member><name>name</name><value><string>app-cli</string></value></member>
                    <member><name>statename</name><value><string>RUNNING</string></value></member>
                    <member><name>state</name><value><int>20</int></value></member>
                    <member><name>pid</name><value><int>{owner}</int></value></member>
                    <member><name>start</name><value><int>1</int></value></member></struct>"#));
                let value = match method.as_str() {
                    "getAllProcessInfo" => {
                        let mut rows = String::new();
                        if present { rows.push_str(&format!(r#"<value><struct>
                            <member><name>group</name><value><string>app-pingap</string></value></member>
                            <member><name>name</name><value><string>app-pingap</string></value></member>
                            <member><name>statename</name><value><string>{state}</string></value></member>
                            <member><name>state</name><value><int>{code}</int></value></member>
                            <member><name>pid</name><value><int>{pid}</int></value></member>
                            <member><name>start</name><value><int>1</int></value></member></struct></value>"#)); }
                        if let Some(owner) = &owner { rows.push_str(&format!("<value>{owner}</value>")); }
                        format!("<array><data>{rows}</data></array>")
                    }
                    "getProcessInfo" => {
                        if request.contains("<string>app-cli</string>") { owner.unwrap() }
                        else { format!(r#"<struct><member><name>group</name><value><string>app-pingap</string></value></member>
                            <member><name>name</name><value><string>app-pingap</string></value></member>
                            <member><name>statename</name><value><string>RUNNING</string></value></member>
                            <member><name>state</name><value><int>20</int></value></member>
                            <member><name>pid</name><value><int>{pid}</int></value></member>
                            <member><name>start</name><value><int>1</int></value></member></struct>"#) }
                    }
                    "reloadConfig" => "<array><data><value><array><data><value><array><data/></array></value><value><array><data/></array></value><value><array><data/></array></value></data></array></value></data></array>".into(),
                    "stopProcessGroup" => { assert!(request.contains("<string>app-pingap</string>")); "<boolean>1</boolean>".into() }
                    "removeProcessGroup" => {
                        assert!(request.contains("<string>app-pingap</string>"));
                        assert!(present); present = false; "<boolean>1</boolean>".into()
                    }
                    _ => panic!("recovery must not stop/start foreign or business execution: {method}"),
                };
                let body = format!("<methodResponse><params><param><value>{value}</value></param></params></methodResponse>");
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            }
        });
        let mut host = SupervisordHost::protocol_fixture(socket, conf.clone());
        if configured {
            let argv=vec!["/usr/bin/pingap".into(),"-c".into(),actual_config.to_string_lossy().into_owned()];
            host.fixture_argv=Some(std::sync::Arc::new(move |_pid| Ok(argv.clone())));
        }
        let config = runtime.join("fresh.toml");
        std::fs::write(&config, b"fresh original source candidate").unwrap();
        let deadline = || tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        let result = if mode=="configured" {
            host.authorize_inactive_entry_until(&runtime, &actual_config, deadline()).await
        } else if mode=="live" || mode=="live-foreign-path" {
            let info=host.resident_entry_settled_until(deadline()).await.unwrap().unwrap();
            let spec=ServiceSpecFile::load(crate::svc_spec::RESIDENT_SPEC_ID,"pingap").unwrap();
            host.authorize_entry_domain_until(&runtime, &actual_config, &spec, &info, deadline()).await
        } else if fresh_only { host.record_entry_domain_until(&runtime, &config, None, deadline()).await }
        else { host.drain_entry_until(&runtime, deadline()).await };
        assert_eq!(result.is_ok(), succeeds, "{claim}/managed={managed}: {result:?}");
        if succeeds {
            if configured {
                if let Some(bytes)=&old { assert_eq!(std::fs::read(&current).unwrap(),*bytes,
                    "retirement authority must not rebrand the running process or old claim"); }
                assert!(!observed.lock().unwrap().iter().any(|method| method=="startProcess" || method=="addProcessGroup"));
                host.close_entry_until(deadline()).await.unwrap();
                assert!(host.resident_entry_settled_until(deadline()).await.unwrap().is_none());
            }
            if !fresh_only { host.record_entry_domain_until(&runtime, &config, None, deadline()).await.unwrap(); }
            let claim = read_resident_engine_receipt(&runtime).unwrap().unwrap();
            assert_eq!(claim.identity, identity); assert_eq!(claim.config, config); assert!(claim.process.is_none());
            if let Some(bytes) = &old {
                let history: Vec<_> = std::fs::read_dir(runtime.join("resident-supervisord-claims")).unwrap()
                    .map(|entry| entry.unwrap().path()).filter(|path| path.file_name().unwrap().to_string_lossy().starts_with("historical-")).collect();
                assert!(!history.is_empty());
                assert!(history.iter().all(|path| std::fs::read(path).unwrap() == *bytes));
            }
        } else {
            assert!(observed.lock().unwrap().iter().all(|method| method == "getAllProcessInfo" || method == "getProcessInfo"));
            if let Some(bytes) = &old { assert_eq!(std::fs::read(&current).unwrap(), *bytes); }
        }
        assert_eq!(std::fs::read(&foreign_config).unwrap(), b"foreign resource must stay intact");
        assert_eq!(std::fs::read(&config).unwrap(), b"fresh original source candidate");
        if !configured || !succeeds { assert_eq!(std::fs::read(&conf).unwrap(), original_conf); }
        rpc.abort(); let _ = rpc.await;
        supervisor::resident::shutdown().await.unwrap(); drop(lease); drop(env);
    });
    if let Some(child) = foreign.as_mut() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "foreign live owner must remain running"
        );
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[test]
fn current_private_builder_recovers_missing_receipt_without_platform_stamp() {
    recovery_case(
        "missing",
        true,
        NativeOwner::None,
        "STOPPED",
        0,
        0,
        false,
        true,
    );
}
#[test]
fn current_private_builder_preserves_corrupt_history_and_publishes_fresh_claim() {
    recovery_case(
        "corrupt",
        true,
        NativeOwner::None,
        "STOPPED",
        0,
        0,
        false,
        true,
    );
}
#[test]
fn current_private_builder_recovers_foreign_old_record_without_touching_its_resources() {
    recovery_case(
        "foreign",
        true,
        NativeOwner::None,
        "STOPPED",
        0,
        0,
        false,
        true,
    );
}
#[test]
fn fresh_source_claim_can_replace_corrupt_history_without_a_cleanup_shortcut() {
    recovery_case(
        "corrupt",
        true,
        NativeOwner::None,
        "STOPPED",
        0,
        0,
        true,
        true,
    );
}
#[test]
fn native_self_owner_recovers_missing_history_from_current_supervisor_fact() {
    recovery_case(
        "missing",
        false,
        NativeOwner::Current,
        "STOPPED",
        0,
        0,
        false,
        true,
    );
}
#[cfg(target_os = "linux")]
#[test]
fn native_worker_recovers_through_its_live_supervised_owner_ancestor() {
    recovery_case(
        "corrupt",
        false,
        NativeOwner::Ancestor,
        "STOPPED",
        0,
        0,
        false,
        true,
    );
}
#[test]
fn missing_history_without_current_owner_fact_retains_unknown_registration() {
    recovery_case(
        "missing",
        false,
        NativeOwner::None,
        "STOPPED",
        0,
        0,
        false,
        false,
    );
}
#[test]
fn another_live_native_owner_is_not_adopted_from_foreign_old_metadata() {
    recovery_case(
        "foreign",
        false,
        NativeOwner::Foreign,
        "STOPPED",
        0,
        0,
        false,
        false,
    );
}
#[test]
fn current_private_builder_does_not_rewrite_a_running_proxy_claim() {
    recovery_case(
        "corrupt",
        true,
        NativeOwner::None,
        "RUNNING",
        20,
        42,
        true,
        false,
    );
}
#[test]
fn current_private_builder_keeps_unknown_actual_proxy_state_protected() {
    recovery_case(
        "corrupt",
        true,
        NativeOwner::None,
        "UNKNOWN",
        1000,
        0,
        false,
        false,
    );
}
#[test]
fn current_private_builder_keeps_nonzero_actual_proxy_pid_protected() {
    recovery_case(
        "corrupt",
        true,
        NativeOwner::None,
        "STOPPED",
        0,
        42,
        false,
        false,
    );
}

#[test]
fn legacy_managed_layout_uses_actual_supervised_owner_to_repair_old_history() {
    recovery_case_with_context(
        "corrupt",
        true,
        NativeOwner::Current,
        "STOPPED",
        0,
        0,
        false,
        true,
        "legacy-layout",
    );
}
#[test]
fn explicit_current_foreign_project_remains_protected_despite_self_owner() {
    recovery_case_with_context(
        "corrupt",
        true,
        NativeOwner::Current,
        "STOPPED",
        0,
        0,
        false,
        false,
        "foreign-project",
    );
}
#[test]
fn explicit_current_foreign_socket_remains_protected_despite_self_owner() {
    recovery_case_with_context(
        "corrupt",
        true,
        NativeOwner::Current,
        "STOPPED",
        0,
        0,
        false,
        false,
        "foreign-socket",
    );
}
#[test]
fn configured_stopped_proxy_corrupt_history_allows_controlled_retire_then_fresh_claim() {
    recovery_case_with_context(
        "corrupt",
        true,
        NativeOwner::Current,
        "STOPPED",
        0,
        0,
        false,
        true,
        "configured",
    );
}
#[test]
fn configured_stopped_proxy_nonzero_pid_cannot_be_retired_from_history() {
    recovery_case_with_context(
        "corrupt",
        true,
        NativeOwner::Current,
        "STOPPED",
        0,
        42,
        false,
        false,
        "configured",
    );
}
#[test]
fn live_proxy_corrupt_history_authorizes_only_exact_retirement_before_new_claim() {
    recovery_case_with_context(
        "corrupt",
        true,
        NativeOwner::Current,
        "RUNNING",
        20,
        i64::from(std::process::id()),
        false,
        true,
        "live",
    );
}
#[test]
fn live_proxy_actual_foreign_configuration_cannot_be_retired_from_current_env() {
    recovery_case_with_context(
        "corrupt",
        true,
        NativeOwner::Current,
        "RUNNING",
        20,
        i64::from(std::process::id()),
        false,
        false,
        "live-foreign-path",
    );
}
