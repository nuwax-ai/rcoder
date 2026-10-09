//! Controlled HTTP protocol fixtures, not evidence of real Pingap application.
use super::{admin_probe::AdminEndpoint, apply_status::*, compiler, publication};
use pingap_config::PingapConfig;
use std::{
    io::{Read, Write},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct Fixture {
    endpoint: AdminEndpoint,
    address: std::net::SocketAddr,
    reject: Arc<Mutex<Option<String>>>,
    foreign: Arc<AtomicBool>,
    business_fails: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl Fixture {
    fn new(active: std::path::PathBuf) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let reject = Arc::new(Mutex::new(None::<String>));
        let foreign = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let business_fails = Arc::new(AtomicBool::new(false));
        let unhealthy = business_fails.clone();
        let (rejected, changed, cancelled) = (reject.clone(), foreign.clone(), stop.clone());
        let worker = std::thread::spawn(move || {
            let instance = uuid::Uuid::new_v4().to_string();
            let other = uuid::Uuid::new_v4().to_string();
            let mut status = ApplyStatus {
                schema_version: 1,
                process_id: std::process::id(),
                process_instance_id: instance.clone(),
                applied: None,
                last_attempt: None,
            };
            let mut attempt = 0;
            while !cancelled.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(2));
                        continue;
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0u8; 2048];
                let count = match stream.read(&mut request) {
                    Ok(0) | Err(_) => continue,
                    Ok(count) => count,
                };
                let request = String::from_utf8_lossy(&request[..count]);
                let config = std::fs::read(&active)
                    .ok()
                    .and_then(|bytes| PingapConfig::new(&bytes, true).ok());
                if let Some(config) = config {
                    let id = config.plugins[compiler::PUBLICATION_OBJECT]["data"]
                        .as_str()
                        .unwrap()
                        .to_owned();
                    let digest = compiler::configuration_digest(&config).unwrap();
                    let hash = config.hash().unwrap();
                    if status
                        .last_attempt
                        .as_ref()
                        .and_then(|value| value.operation_id.as_deref())
                        != Some(&id)
                    {
                        attempt += 1;
                        let failed = rejected.lock().unwrap().as_deref() == Some(&id);
                        status.last_attempt = Some(ApplyAttempt {
                            attempt_id: attempt,
                            operation_id: Some(id.clone()),
                            config_hash: Some(hash.clone()),
                            config_digest: Some(digest.clone()),
                            state: if failed {
                                ApplyState::Failed
                            } else {
                                ApplyState::Applied
                            },
                            failure: failed.then(|| ApplyFailure {
                                category: "fixture_rejection".into(),
                                message: "candidate rejected".into(),
                            }),
                        });
                        if !failed {
                            status.applied = Some(AppliedPublication {
                                attempt_id: attempt,
                                operation_id: Some(id),
                                config_hash: hash,
                                config_digest: digest,
                                applied_at_unix_ms: 1,
                            });
                        }
                    }
                }
                status.process_instance_id = if changed.load(Ordering::Acquire) {
                    other.clone()
                } else {
                    instance.clone()
                };
                let (code, headers, body) = if request.starts_with("GET /api/apply-status ") {
                    assert!(request.to_ascii_lowercase().contains("authorization:"));
                    (200, String::new(), serde_json::to_string(&status).unwrap())
                } else if request.starts_with("GET /business/health ") {
                    (
                        if unhealthy.load(Ordering::Acquire) {
                            503
                        } else {
                            200
                        },
                        String::new(),
                        "business".into(),
                    )
                } else {
                    let id = status
                        .applied
                        .as_ref()
                        .unwrap()
                        .operation_id
                        .as_deref()
                        .unwrap();
                    let code = if std::fs::read_to_string(&active)
                        .unwrap()
                        .contains("status = 503")
                    {
                        503
                    } else {
                        200
                    };
                    (
                        code,
                        format!("X-Rcoder-Publication: {id}\r\n"),
                        id.to_owned(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {code} Test\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                // A client may cancel a bounded observation after connecting.
                // The transaction's behavior assertions still require replies.
                let _ = stream.write_all(response.as_bytes());
            }
        });
        Self {
            endpoint: AdminEndpoint {
                addr: address.to_string(),
                user: "fixture".into(),
                password: "fixture".into(),
            },
            address,
            reject,
            foreign,
            business_fails,
            stop,
            worker: Some(worker),
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let result = self.worker.take().unwrap().join();
        if !std::thread::panicking() {
            result.unwrap();
        }
    }
}

async fn candidate(
    root: &std::path::Path,
    address: std::net::SocketAddr,
    status: u16,
) -> compiler::CompileOutcome {
    let mut config = PingapConfig::default();
    config.basic.auto_restart_check_interval = Some(std::time::Duration::from_secs(2));
    config.servers.insert(
        "fixture".into(),
        pingap_config::ServerConf {
            addr: address.to_string(),
            ..Default::default()
        },
    );
    let id = uuid::Uuid::new_v4().to_string();
    let config = compiler::publication_config(config, &id, status).unwrap();
    compiler::persist_candidate(root, &config, &id, status)
        .await
        .unwrap()
}

#[tokio::test]
async fn rejected_publication_restores_new_correlated_old_graph() {
    let root = tempfile::tempdir().unwrap();
    let active = compiler::active_config_path(root.path());
    let fixture = Fixture::new(active.clone());
    let mut original = candidate(root.path(), fixture.address, 200).await;
    original.business_probes.push(compiler::ServiceProbeTarget {
        service_id: "backend".into(),
        address: fixture.address,
        path: "/business/health".into(),
    });
    compiler::publish_active(root.path(), &original.config_path)
        .await
        .unwrap();
    let original_receipt = super::admin_probe::wait_for_publication(
        &fixture.endpoint,
        &original,
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    compiler::record_confirmed_publication(&original, &original_receipt).unwrap();
    let rejected = candidate(root.path(), fixture.address, 200).await;
    *fixture.reject.lock().unwrap() = Some(rejected.publication_id.clone());
    let error = publication::publish_confirmed(root.path(), &active, &rejected, &fixture.endpoint)
        .await
        .unwrap_err();
    assert!(
        !error.is::<crate::supervisor::ShutdownUnconfirmed>(),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains("previous graph restored"));
    let (restored, receipt) = compiler::confirmed_publication().unwrap();
    assert_ne!(restored.publication_id, rejected.publication_id);
    assert_ne!(restored.publication_id, original.publication_id);
    assert_eq!(receipt.instance_id, original_receipt.instance_id);
    assert_eq!(
        std::fs::read(active).unwrap(),
        std::fs::read(&restored.config_path).unwrap()
    );
    assert_eq!(
        restored.business_probes.len(),
        1,
        "serving rollback must preserve the HTTP contract"
    );
    fixture.business_fails.store(true, Ordering::Release);
    assert!(
        super::admin_probe::wait_for_publication(
            &fixture.endpoint,
            &restored,
            std::time::Duration::from_secs(2)
        )
        .await
        .is_err(),
        "marker alone must not hide a stopped backend after rollback"
    );
}

#[tokio::test]
async fn foreign_process_cannot_authorize_rollback_write() {
    let root = tempfile::tempdir().unwrap();
    let active = compiler::active_config_path(root.path());
    let fixture = Fixture::new(active.clone());
    let original = candidate(root.path(), fixture.address, 200).await;
    compiler::publish_active(root.path(), &original.config_path)
        .await
        .unwrap();
    let receipt = super::admin_probe::wait_for_publication(
        &fixture.endpoint,
        &original,
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    let bytes = std::fs::read(&active).unwrap();
    let previous = PingapConfig::new(&bytes, true).unwrap();
    fixture.foreign.store(true, Ordering::Release);
    let result = publication::rollback_publication(
        root.path(),
        &active,
        previous,
        &fixture.endpoint,
        &receipt,
        &original.business_probes,
        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(std::fs::read(active).unwrap(), bytes);
}

#[test]
fn digest_matches_upstream_canonical_fixture() {
    let bytes = include_bytes!("../../tests/fixtures/applied-config.toml");
    let config = PingapConfig::new(bytes, true).unwrap();
    assert_eq!(
        compiler::configuration_digest(&config).unwrap(),
        include_str!("../../tests/fixtures/applied-config.sha256").trim()
    );
}

#[tokio::test]
async fn applied_marker_does_not_confirm_an_unavailable_business() {
    let root = tempfile::tempdir().unwrap();
    let active = compiler::active_config_path(root.path());
    let fixture = Fixture::new(active);
    let mut outcome = candidate(root.path(), fixture.address, 200).await;
    let unavailable = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = unavailable.local_addr().unwrap();
    drop(unavailable);
    outcome.business_probes.push(compiler::ServiceProbeTarget {
        service_id: "backend".into(),
        address,
        path: "/health".into(),
    });
    compiler::publish_active(root.path(), &outcome.config_path)
        .await
        .unwrap();
    let error = super::admin_probe::wait_for_publication(
        &fixture.endpoint,
        &outcome,
        std::time::Duration::from_secs(2),
    )
    .await
    .unwrap_err();
    assert!(format!("{error:#}").contains("verify business service backend"));
}
