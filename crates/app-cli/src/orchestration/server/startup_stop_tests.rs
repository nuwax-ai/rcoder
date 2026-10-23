//! The real server loop and kernel talk to a controlled Unix XML-RPC engine.
//! This cold-entry fixture proves startup Stop scheduling and write barriers;
//! it does not stand in for real supervisord or official Pingap acceptance.
use super::*;
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

#[derive(Clone, Copy)]
enum Scenario {
    Normal,
    AuxiliaryWriter,
    LateStopReply,
    LostStartReply,
    StopFault,
    RemoveFault,
    AuxiliaryFailure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Group {
    Absent,
    Stopped,
    Running,
}

#[derive(Default)]
struct Gates {
    start_entered: tokio::sync::Notify,
    start_release: tokio::sync::Notify,
    stop_entered: tokio::sync::Notify,
    stop_release: tokio::sync::Notify,
    withdrawals: AtomicUsize,
    stops: AtomicUsize,
    removes: AtomicUsize,
}

struct Engine {
    task: tokio::task::JoinHandle<()>,
    group: Arc<Mutex<Group>>,
    gates: Arc<Gates>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn engine(socket: &std::path::Path, conf: std::path::PathBuf, scenario: Scenario) -> Engine {
    let listener = tokio::net::UnixListener::bind(socket).unwrap();
    let gates = Arc::new(Gates::default());
    let observed = gates.clone();
    let group = Arc::new(Mutex::new(Group::Absent));
    let physical = group.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = Vec::new();
            while !header.ends_with(b"\r\n\r\n") {
                header.push(stream.read_u8().await.unwrap());
            }
            let header = String::from_utf8(header).unwrap();
            let size: usize = header
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut request = vec![0; size];
            stream.read_exact(&mut request).await.unwrap();
            let request = String::from_utf8(request).unwrap();
            let method = request
                .split("<methodName>supervisor.")
                .nth(1)
                .unwrap()
                .split("</methodName>")
                .next()
                .unwrap();
            eprintln!("startup Stop fixture RPC: {method}");
            let value = match method {
                "getAllProcessInfo" => {
                    let group = *physical.lock().unwrap();
                    if group == Group::Absent {
                        "<array><data/></array>".into()
                    } else {
                        let (name, state, pid) = match group {
                            Group::Running => ("RUNNING", 20, 42),
                            Group::Stopped => ("STOPPED", 0, 0),
                            Group::Absent => unreachable!(),
                        };
                        format!(
                            "<array><data><value><struct><member><name>group</name><value><string>app-svc-worker</string></value></member><member><name>name</name><value><string>app-svc-worker</string></value></member><member><name>statename</name><value><string>{name}</string></value></member><member><name>state</name><value><int>{state}</int></value></member><member><name>pid</name><value><int>{pid}</int></value></member><member><name>start</name><value><int>123</int></value></member></struct></value></data></array>"
                        )
                    }
                }
                "reloadConfig" => {
                    if std::fs::read_to_string(&conf)
                        .unwrap()
                        .starts_with("# app-cli dynamic services stopped")
                    {
                        observed.withdrawals.fetch_add(1, Ordering::SeqCst);
                    }
                    "<array><data><value><array><data><value><array><data/></array></value><value><array><data/></array></value><value><array><data/></array></value></data></array></value></data></array>".into()
                }
                "addProcessGroup" => {
                    assert!(request.contains("app-svc-worker"));
                    *physical.lock().unwrap() = Group::Stopped;
                    "<boolean>1</boolean>".into()
                }
                "startProcess" => {
                    assert!(request.contains("app-svc-worker"));
                    *physical.lock().unwrap() = Group::Running;
                    observed.start_entered.notify_one();
                    observed.start_release.notified().await;
                    if matches!(scenario, Scenario::LostStartReply) {
                        // The mutation reached the engine; dropping this real
                        // socket leaves the original caller without its ACK.
                        continue;
                    }
                    "<boolean>1</boolean>".into()
                }
                "stopProcessGroup" => {
                    assert!(request.contains("app-svc-worker"));
                    observed.stops.fetch_add(1, Ordering::SeqCst);
                    if matches!(scenario, Scenario::StopFault) {
                        fault(&mut stream, "controlled Stop failure").await;
                        continue;
                    }
                    *physical.lock().unwrap() = Group::Stopped;
                    observed.stop_entered.notify_one();
                    if matches!(scenario, Scenario::LateStopReply) {
                        observed.stop_release.notified().await;
                    }
                    "<boolean>1</boolean>".into()
                }
                "removeProcessGroup" => {
                    assert!(request.contains("app-svc-worker"));
                    assert!(*physical.lock().unwrap() == Group::Stopped);
                    observed.removes.fetch_add(1, Ordering::SeqCst);
                    if matches!(scenario, Scenario::RemoveFault) {
                        fault(&mut stream, "controlled Remove failure").await;
                        continue;
                    }
                    *physical.lock().unwrap() = Group::Absent;
                    "<boolean>1</boolean>".into()
                }
                other => panic!("unexpected startup Stop RPC {other}: {request}"),
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
    Engine { task, group, gates }
}

async fn fault(stream: &mut tokio::net::UnixStream, message: &str) {
    let body = format!(
        "<methodResponse><fault><value><struct><member><name>faultCode</name><value><int>91</int></value></member><member><name>faultString</name><value><string>{message}</string></value></member></struct></value></fault></methodResponse>"
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

fn request(
    kind: shared_types::RuntimeOperationKind,
    id: &str,
) -> shared_types::RuntimeOperationRequest {
    shared_types::RuntimeOperationRequest {
        operation_id: id.into(),
        expected_runtime_instance_id: "startup-owner".into(),
        expected_revision: 0,
        workspace_id: "startup-workspace".into(),
        kind,
        profile: shared_types::RunProfileInput::Source {
            workspace_id: "startup-workspace".into(),
        },
        run_config: None,
        request_context: None,
    }
}

async fn run(scenario: Scenario) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let workspace = root.join("workspace");
    std::fs::create_dir_all(workspace.join("worker")).unwrap();
    let conf = root.join("dynamic.conf");
    let socket = root.join("rpc.sock");
    let engine = engine(&socket, conf.clone(), scenario).await;
    let args = RuntimeArgs {
        workspace: workspace.clone(),
        log_dir: root.join("logs"),
        ..Default::default()
    };
    // Lifecycle control requires a real retained owner capability even when no
    // proxy was launched yet. Use the same authority chain as the host tests.
    let owner_root = root.join("owner");
    let _owner_lease = process_utils::command_authority::OwnerLease::try_acquire(&owner_root)
        .unwrap()
        .unwrap();
    let identity = process_utils::command_authority::ResidentIdentity {
        application_id: "startup-app".into(),
        binding: serde_json::json!({"component":"app-cli","resource":workspace}),
        owner_instance: uuid::Uuid::new_v4().to_string(),
        physical_domain: None,
        process_epoch: None,
    };
    std::fs::write(
        owner_root.join("supervisor.json"),
        serde_json::to_vec(&serde_json::json!({
            "instance":identity.owner_instance,
            "snapshot":{"supervisor_id":identity.owner_instance,"binding":identity.binding}
        }))
        .unwrap(),
    )
    .unwrap();
    let scope = runtime_supervisor::ResidentScope::initialize(&_owner_lease, identity).unwrap();
    supervisor::resident::bind_owner(scope, &args).unwrap();
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    state.set_generation("startup-generation".into());
    *state.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
    let control = state.control_tx.clone();
    let kernel = Arc::new(crate::runtime_kernel::RuntimeKernel::new(
        crate::runtime_kernel::RuntimeStore::open_with_root(root.join("kernel"), &workspace)
            .unwrap(),
        shared_types::RuntimeIdentityView {
            application_id: "startup-app".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "startup-workspace".into(),
            source_root: workspace.to_string_lossy().into_owned(),
            runtime_instance_id: "startup-owner".into(),
            deployment_generation_id: "startup-generation".into(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        },
        Box::new(move |action| match action {
            crate::runtime_kernel::DispatchAction::StopBusiness { operation_id } => {
                control
                    .send(ControlSignal::StopBusiness { operation_id })
                    .unwrap();
            }
            crate::runtime_kernel::DispatchAction::OrchestrateSource { .. }
            | crate::runtime_kernel::DispatchAction::DeployLocalArtifact { .. }
            | crate::runtime_kernel::DispatchAction::DeployArtifact { .. }
            | crate::runtime_kernel::DispatchAction::RecoverArtifact { .. }
            | crate::runtime_kernel::DispatchAction::CancelSource { .. } => {}
        }),
    ));
    assert!(state.set_runtime_kernel(kernel.clone()));
    state.mark_initialized();
    kernel
        .admit(request(
            shared_types::RuntimeOperationKind::Start,
            "original-start",
        ))
        .await
        .unwrap();
    state.set_current_runtime_operation(Some("original-start".into()));
    state
        .record_runtime_deployment(&DeployRequest {
            runtime_operation_id: Some("original-start".into()),
            url: "source://workspace".into(),
            release_id: "original-start".into(),
            sha256: None,
            local_path: None,
            execution_target: Some(ExecutionTarget::Source),
            run_pg: None,
        })
        .unwrap();
    let release: ReleaseLock = toml::from_str("schema_version=1\nrelease_id='startup-release'\nworkspace_name='startup'\nminimum_app_cli_version='0.0.0'\nruntime_image_digest='fixture'\n[pingap]\nmode='managed'\nversion='0.15.0'\ncommit='fixture'\n[[services]]\nservice_id='worker'\nname='Worker'\ndir='worker'\ntype='node'\nkind='worker'\nenabled=true\nport=4100\nlogs=[]\n[services.run]\ncommand=['controlled-worker']\nmigrate=[]\ndepends_on=[]\nshutdown_timeout_seconds=1\n[services.health]\n[services.env]\n").unwrap();
    std::fs::write(
        workspace.join("release.lock.toml"),
        toml::to_string(&release).unwrap(),
    )
    .unwrap();
    let mut writer = matches!(scenario, Scenario::AuxiliaryWriter)
        .then(|| state.begin_auxiliary_write().unwrap());
    let prepared = InitialAction::ExistingPrepared(supervisor::PreparedProxy {
        workspace: workspace.clone(),
        release,
        dev_profile: false,
        outcome: crate::proxy::compiler::CompileOutcome {
            config_path: root.join("unpublished.toml"),
            expected_hash: "unpublished".into(),
            publication_id: uuid::Uuid::new_v4().to_string(),
            operation_id: Some("original-start".into()),
            serving_release_id: None,
            config_digest: "unpublished".into(),
            entry_probes: Vec::new(),
            business_probes: Vec::new(),
        },
    });
    let driver_state = state.clone();
    let driver = tokio::spawn(async move {
        server_loop(
            &args,
            &driver_state,
            Some(SupervisordHost::protocol_fixture(socket, conf)),
            Some(prepared),
            true,
        )
        .await
    });
    let entered = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        engine.gates.start_entered.notified(),
    )
    .await;
    assert!(
        entered.is_ok(),
        "startup did not reach the controlled mutation; driver finished={}",
        driver.is_finished()
    );
    if matches!(scenario, Scenario::AuxiliaryFailure) {
        let poisoned = state.clone();
        assert!(
            std::thread::spawn(move || {
                let _guard = poisoned.auxiliary_cancel.lock().unwrap();
                panic!("controlled current auxiliary cancellation observation failure");
            })
            .join()
            .is_err()
        );
    }
    kernel
        .admit(request(
            shared_types::RuntimeOperationKind::Stop,
            "original-stop",
        ))
        .await
        .unwrap();
    engine.gates.start_release.notify_one();

    if let Some(writer) = writer.as_mut() {
        within(
            writer.cancellation().cancelled(),
            &engine,
            &kernel,
            "auxiliary cancellation barrier",
        )
        .await;
        assert_eq!(
            engine.gates.stops.load(Ordering::SeqCst),
            0,
            "startup Stop must settle its configuration writer before physical cleanup"
        );
        assert_ne!(
            kernel.get("original-stop").await.unwrap().unwrap().state,
            shared_types::RuntimeOperationState::Succeeded
        );
        writer.confirm();
    }
    drop(writer);
    if matches!(scenario, Scenario::LateStopReply) {
        within(
            engine.gates.stop_entered.notified(),
            &engine,
            &kernel,
            "real Stop mutation reached the engine",
        )
        .await;
        assert!(!driver.is_finished());
        assert_eq!(engine.gates.removes.load(Ordering::SeqCst), 0);
        assert_ne!(
            kernel.get("original-stop").await.unwrap().unwrap().state,
            shared_types::RuntimeOperationState::Succeeded,
            "sent Stop without its real reply is not cleanup confirmation"
        );
        engine.gates.stop_release.notify_one();
    }
    if matches!(
        scenario,
        Scenario::LostStartReply
            | Scenario::StopFault
            | Scenario::RemoveFault
            | Scenario::AuxiliaryFailure
    ) {
        within(
            async {
                loop {
                    if kernel.get("original-stop").await.unwrap().unwrap().state
                        == shared_types::RuntimeOperationState::RecoveryRequired
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            },
            &engine,
            &kernel,
            "original Stop preserved unknown cleanup",
        )
        .await;
        assert!(kernel.recovery_protection_active());
        let stop = kernel.get("original-stop").await.unwrap().unwrap();
        assert_ne!(stop.state, shared_types::RuntimeOperationState::Succeeded);
        match scenario {
            Scenario::LostStartReply => {
                assert!(
                    stop.error_message
                        .as_deref()
                        .unwrap()
                        .contains("startup outcome")
                );
                assert!(*engine.group.lock().unwrap() == Group::Absent);
                assert_eq!(engine.gates.withdrawals.load(Ordering::SeqCst), 1);
                assert_eq!(engine.gates.stops.load(Ordering::SeqCst), 1);
                assert_eq!(engine.gates.removes.load(Ordering::SeqCst), 1);
            }
            Scenario::StopFault => {
                assert!(*engine.group.lock().unwrap() == Group::Running);
                assert_eq!(engine.gates.removes.load(Ordering::SeqCst), 0);
            }
            Scenario::RemoveFault => assert!(*engine.group.lock().unwrap() == Group::Stopped),
            Scenario::AuxiliaryFailure => {
                assert!(
                    stop.error_message
                        .as_deref()
                        .unwrap()
                        .contains("auxiliary cancellation lock poisoned")
                );
                assert!(*engine.group.lock().unwrap() == Group::Running);
                assert_eq!(engine.gates.withdrawals.load(Ordering::SeqCst), 0);
                assert_eq!(engine.gates.stops.load(Ordering::SeqCst), 0);
            }
            Scenario::Normal | Scenario::AuxiliaryWriter | Scenario::LateStopReply => {
                unreachable!()
            }
        }
        state.trigger_cancel();
        tokio::time::timeout(std::time::Duration::from_secs(3), driver)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_ne!(
            kernel.get("original-start").await.unwrap().unwrap().state,
            shared_types::RuntimeOperationState::Succeeded
        );
        return;
    }
    within(
        driver,
        &engine,
        &kernel,
        "original server loop Stop completion",
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        kernel.get("original-start").await.unwrap().unwrap().state,
        shared_types::RuntimeOperationState::Cancelled
    );
    let stopped = kernel.get("original-stop").await.unwrap().unwrap();
    assert_eq!(
        stopped.state,
        shared_types::RuntimeOperationState::Succeeded
    );
    assert_eq!(stopped.operation_id, "original-stop");
    assert!(*engine.group.lock().unwrap() == Group::Absent);
    assert_eq!(
        std::fs::read_to_string(root.join("dynamic.conf")).unwrap(),
        "# app-cli dynamic services stopped\n",
        "the stopped worker must have no supervisor restart source"
    );
    assert_eq!(engine.gates.stops.load(Ordering::SeqCst), 1);
    assert_eq!(engine.gates.removes.load(Ordering::SeqCst), 1);
    assert_eq!(
        engine.gates.withdrawals.load(Ordering::SeqCst),
        1,
        "the startup driver already confirmed business cleanup; Stop must not repeat it"
    );
    supervisor::resident::shutdown().await.unwrap();
}

async fn within<T>(
    future: impl std::future::Future<Output = T>,
    engine: &Engine,
    kernel: &crate::runtime_kernel::RuntimeKernel,
    label: &str,
) -> T {
    match tokio::time::timeout(std::time::Duration::from_secs(3), future).await {
        Ok(value) => value,
        Err(error) => {
            let group = *engine.group.lock().unwrap();
            let start = kernel.get("original-start").await.unwrap();
            let stop = kernel.get("original-stop").await.unwrap();
            panic!(
                "{label}: {error}; group={group:?}, withdrawals={}, stops={}, removes={}, original Start={start:?}, original Stop={stop:?}",
                engine.gates.withdrawals.load(Ordering::SeqCst),
                engine.gates.stops.load(Ordering::SeqCst),
                engine.gates.removes.load(Ordering::SeqCst),
            );
        }
    }
}

fn isolated(scenario: Scenario) {
    let _guard = crate::svc_spec::SPEC_ENV_LOCK.lock().unwrap();
    let specs = tempfile::tempdir().unwrap();
    let previous = std::env::var_os("APP_CLI_SPEC_DIR");
    set_spec_env(Some(specs.path().as_os_str().to_owned()));
    let result = std::panic::catch_unwind(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(run(scenario));
    });
    set_spec_env(previous);
    if let Err(error) = result {
        std::panic::resume_unwind(error);
    }
}

// The fixture is Unix-only and holds the same environment lock as the existing
// host tests. Production code never compiles this environment mutation helper.
#[allow(unsafe_code)]
fn set_spec_env(value: Option<std::ffi::OsString>) {
    unsafe {
        match value {
            Some(value) => std::env::set_var("APP_CLI_SPEC_DIR", value),
            None => std::env::remove_var("APP_CLI_SPEC_DIR"),
        }
    }
}

#[test]
fn startup_stop_reuses_its_confirmed_cleanup_without_a_second_withdrawal() {
    isolated(Scenario::Normal);
}
#[test]
fn startup_stop_settles_the_auxiliary_writer_before_cleanup() {
    isolated(Scenario::AuxiliaryWriter);
}
#[test]
fn startup_stop_does_not_finish_before_the_real_stop_reply() {
    isolated(Scenario::LateStopReply);
}
#[test]
fn startup_stop_preserves_an_unconfirmed_start_mutation() {
    isolated(Scenario::LostStartReply);
}
#[test]
fn startup_stop_retains_a_business_group_when_stop_fails() {
    isolated(Scenario::StopFault);
}
#[test]
fn startup_stop_does_not_confirm_a_group_that_failed_removal() {
    isolated(Scenario::RemoveFault);
}
#[test]
fn startup_stop_retains_protection_when_current_auxiliary_observation_fails() {
    isolated(Scenario::AuxiliaryFailure);
}
