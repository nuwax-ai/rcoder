//! DRAFT: isolated Docker HTTP protocol fixtures using the real Bollard client.
//! This is not real Docker/paired-120s acceptance. Register from the parent module.
use super::*;
use shared_types::{AppResourceIdentity, AppResourceKind, BuilderRestartStopKind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const SELF_TEST: &str = "runtime::docker_builder_control::restart_stop_observation_tests::original_restart_stop_observation_protocol_contract";

fn captured_target() -> BuilderControlTarget {
    let context = UserAppExecutionContext {
        app_id: format!("stopobserve{}", uuid::Uuid::new_v4().simple()),
        lifecycle_id: "originallife".into(),
        operation_id: "originalrestart".into(),
        executor_id: "originalexecutor".into(),
        request_fingerprint: "a".repeat(64),
    };
    BuilderControlTarget {
        resource_binding: Some(shared_types::UserAppResourceBinding {
            app_id: context.app_id.clone(),
            lifecycle_id: context.lifecycle_id.clone(),
            service_type: ServiceType::UserappBuilder,
            physical_uid: "a".repeat(64),
            adopted_by_operation: "originaladoption".into(),
        }),
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: format!("rcoder-app-builder-{}", context.app_id),
            uid: "a".repeat(64),
            resource_version: None,
        }),
        context,
        pod: None,
        restart_image: None,
        restart_runtime_workspace: None,
        builder_startup_mode: shared_types::BuilderStartupMode::default(),
    }
}

struct ProtocolFixture {
    runtime: DockerRuntime,
    requests: Arc<Mutex<Vec<String>>>,
    force_running: Arc<AtomicBool>,
    server: tokio::task::JoinHandle<()>,
    actor: tokio::task::JoinHandle<()>,
}

impl Drop for ProtocolFixture {
    fn drop(&mut self) {
        self.server.abort();
        self.actor.abort();
    }
}

impl ProtocolFixture {
    async fn start(target: &BuilderControlTarget, case: &'static str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = requests.clone();
        let force_running = Arc::new(AtomicBool::new(false));
        let observed_running = force_running.clone();
        let target = target.clone();
        let server = tokio::spawn(async move {
            let mut stopped = false;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 2048];
                loop {
                    let count = stream.read(&mut buffer).await.unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&buffer[..count]);
                    assert!(bytes.len() < 32768, "fixture request header is bounded");
                    if bytes.windows(4).any(|part| part == b"\r\n\r\n") {
                        break;
                    }
                }
                let line = String::from_utf8_lossy(&bytes)
                    .lines()
                    .next()
                    .unwrap()
                    .to_owned();
                recorded.lock().unwrap().push(line.clone());
                let method = line.split_whitespace().next().unwrap();
                let path = line.split_whitespace().nth(1).unwrap();
                let expected =
                    format!("/containers/{}/json", target.workload.as_ref().unwrap().uid);
                let (status, body) = if method == "POST"
                    && path.split('?').next().unwrap().ends_with("/stop")
                    && matches!(case, "ack-stopped" | "ack-absent" | "ack-then-running")
                {
                    assert!(path.contains(&format!(
                        "/containers/{}/stop",
                        target.workload.as_ref().unwrap().uid
                    )));
                    stopped = true;
                    (204, String::new())
                } else if method == "GET" && path.ends_with(&expected) {
                    let status = match case {
                        "missing" => 404,
                        "server-error" => 500,
                        "forbidden" => 403,
                        "ack-absent" if stopped => 404,
                        _ => 200,
                    };
                    if status == 200 {
                        let mut labels = target.context.resource_metadata();
                        labels.insert(
                            "service-type".into(),
                            ServiceType::UserappBuilder.to_string(),
                        );
                        labels.insert("identifier".into(), target.context.app_id.clone());
                        if case == "foreign-app" {
                            labels.insert("identifier".into(), "foreignapp".into());
                            labels.insert("rcoder.io/application-id".into(), "foreignapp".into());
                        }
                        if case == "foreign-life" {
                            labels.insert("rcoder.io/lifecycle-id".into(), "foreignlife".into());
                        }
                        let running = case == "running-no-ack"
                            || (case.starts_with("ack-") && !stopped)
                            || observed_running.load(Ordering::Acquire);
                        let uid = if case == "wrong-response-cid" {
                            "b".repeat(64)
                        } else {
                            target.workload.as_ref().unwrap().uid.clone()
                        };
                        (200, serde_json::json!({
                            "Id": uid,
                            "Created": "2026-01-01T00:00:00Z",
                            "Config": {"Labels": labels},
                            "State": {"Running":running, "Status":if running {"running"} else {"exited"},
                                "Restarting":false,"Dead":false}
                        }).to_string())
                    } else {
                        (
                            status,
                            serde_json::json!({"message":"isolated protocol observation response"})
                                .to_string(),
                        )
                    }
                } else {
                    // Unexpected writes remain visible in requests; no fake
                    // successful mutation can hide an observer regression.
                    (
                        403,
                        serde_json::json!({"message":"unexpected protocol fixture request"})
                            .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let (actor, containers) = crate::container_state_actor::ContainerStateActor::new();
        let actor = tokio::spawn(actor.run());
        let manager = Arc::new(crate::DockerManager {
            docker: bollard::Docker::connect_with_http(
                &format!("http://{address}"),
                1,
                bollard::API_DEFAULT_VERSION,
            )
            .unwrap(),
            config: crate::DockerManagerConfig::default(),
            containers,
            main_network_name: Arc::new(tokio::sync::RwLock::new("isolated-fixture".into())),
            api_cache: Arc::new(crate::api_cache::DockerApiCache::new(600, 600, 100)),
        });
        Self {
            runtime: DockerRuntime::new(manager),
            requests,
            force_running,
            server,
            actor,
        }
    }

    fn lines(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

async fn protocol_contract() {
    for case in [
        "missing",
        "stopped-no-ack",
        "running-no-ack",
        "wrong-response-cid",
        "foreign-app",
        "foreign-life",
        "server-error",
        "forbidden",
    ] {
        let target = captured_target();
        let fixture = ProtocolFixture::start(&target, case).await;
        assert!(
            !super::super::docker_compute_receipt::matches_stop(&target)
                .await
                .unwrap()
        );
        let observed = fixture
            .runtime
            .observe_original_builder_restart_stop(&target)
            .await;
        match case {
            "missing" => assert_eq!(
                observed.unwrap(),
                Some(BuilderRestartStopKind::CapturedContainerAbsent)
            ),
            "stopped-no-ack" | "running-no-ack" => assert_eq!(
                observed.unwrap(),
                None,
                "container state cannot acknowledge the old Stop"
            ),
            "wrong-response-cid" | "foreign-app" | "foreign-life" => assert!(
                matches!(observed, Err(Error::Conflict(_))),
                "{case}: foreign observations grant no continuation: {observed:?}"
            ),
            "server-error" | "forbidden" => assert!(
                matches!(observed, Err(Error::DockerError(_))),
                "{case}: errors cannot be absence: {observed:?}"
            ),
            _ => unreachable!(),
        }
        let lines = fixture.lines();
        assert_eq!(lines.len(), 1, "{case}: one actual protocol inspect");
        assert!(lines[0].starts_with("GET "));
        assert!(lines[0].contains(&format!(
            "/containers/{}/json",
            target.workload.as_ref().unwrap().uid
        )));
        assert!(
            !super::super::docker_compute_receipt::matches_stop(&target)
                .await
                .unwrap(),
            "{case}: the observer cannot manufacture a Stop receipt"
        );
    }
    for case in ["ack-stopped", "ack-absent", "ack-then-running"] {
        let target = captured_target();
        let fixture = ProtocolFixture::start(&target, case).await;
        // The production stop path sends the fake daemon's exact-CID Stop,
        // consumes its 204, confirms its result, and writes its own atomic
        // receipt. This fixture does not claim a real Docker process was stopped.
        fixture
            .runtime
            .apply_builder_compute(&target, false)
            .await
            .unwrap();
        assert!(
            super::super::docker_compute_receipt::matches_stop(&target)
                .await
                .unwrap()
        );
        let before = fixture.lines();
        assert_eq!(
            before
                .iter()
                .filter(|line| line.starts_with("POST ") && line.contains("/stop"))
                .count(),
            1
        );
        assert!(before.iter().all(|line| line.starts_with("GET ")
            || (line.starts_with("POST ") && line.contains("/stop"))));
        if case == "ack-then-running" {
            fixture.force_running.store(true, Ordering::Release);
        }
        let offset = before.len();
        let observed = fixture
            .runtime
            .observe_original_builder_restart_stop(&target)
            .await
            .unwrap();
        assert_eq!(
            observed,
            match case {
                "ack-then-running" => None,
                "ack-absent" => Some(BuilderRestartStopKind::CapturedContainerAbsent),
                "ack-stopped" => Some(BuilderRestartStopKind::StopReceiptAcknowledged),
                _ => unreachable!(),
            }
        );
        let lines = fixture.lines();
        let observations = &lines[offset..];
        assert_eq!(observations.len(), if case == "ack-absent" { 1 } else { 2 });
        assert!(
            observations.iter().all(|line| line.starts_with("GET ")
                && line.contains(&format!(
                    "/containers/{}/json",
                    target.workload.as_ref().unwrap().uid
                ))),
            "receipt-backed recovery still performs only exact-CID read observation"
        );
    }

    let mut short = captured_target();
    short.resource_binding = None;
    short.workload.as_mut().unwrap().uid = "a".repeat(12);
    let fixture = ProtocolFixture::start(&short, "missing").await;
    assert_eq!(
        fixture
            .runtime
            .observe_original_builder_restart_stop(&short)
            .await
            .unwrap(),
        None
    );
    assert!(
        fixture.lines().is_empty(),
        "a short alias is not even sent as an absence probe"
    );
}

#[test]
fn original_restart_stop_observation_protocol_contract() {
    if std::env::var_os("RCODER_DRAFT_STOP_OBSERVER_CHILD").is_none() {
        // Child env setup avoids process-global mutation/races with other receipt tests.
        let receipts = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", SELF_TEST, "--nocapture"])
            .env("RCODER_DRAFT_STOP_OBSERVER_CHILD", "1")
            .env("RCODER_OPERATION_RECEIPT_ROOT", receipts.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(
        std::env::var_os("RCODER_OPERATION_RECEIPT_ROOT").is_some(),
        "private receipt root required"
    );
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(protocol_contract());
}
