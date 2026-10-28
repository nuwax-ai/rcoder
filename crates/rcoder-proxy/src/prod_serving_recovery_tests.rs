//! Real Pingora requests verify that only authorized application traffic uses
//! the reconstruction capability. Runtime creation itself is tested by AppService.
use super::*;
use shared_types::{RemoteWakeState, WakeFailure, WakeOutcome};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const APP_ID: &str = "serving-recovery";
const APP_PATH: &str = "/api/v1/userapp/proxy/app/prod/user/serving-recovery/";
const TEST_KEY: &str = "isolated-proxy-recovery-key";

#[derive(Clone, Copy)]
enum ProbeMode {
    ColdMissing,
    MissingAfterDispatch,
    MissingAfterConnect,
    ColdUnavailable,
    FreshUnavailable,
    LegacyStopped,
}

struct ServingControl {
    mode: ProbeMode,
    service: Arc<PingoraProxyService>,
    backend_port: u16,
    backend_host: String,
    stale_port: Option<u16>,
    serving_calls: AtomicUsize,
    legacy_calls: AtomicUsize,
    serving_probes: AtomicUsize,
    fresh_probes: AtomicUsize,
}

#[async_trait::async_trait]
impl shared_types::AppWakeControl for ServingControl {
    fn is_stopped(&self, _: &str) -> bool {
        matches!(self.mode, ProbeMode::LegacyStopped)
    }

    fn wake_timeout(&self) -> Duration {
        Duration::from_secs(3)
    }

    async fn remote_wake_pending_result(&self, _: &str) -> Result<bool, WakeFailure> {
        Err(WakeFailure::new(
            shared_types::ERR_CONTAINER_NOT_FOUND,
            "wake_runtime_probe",
            "missing workload does not authorize tool reconstruction",
        ))
    }

    async fn remote_serving_pending_result(&self, _: &str) -> Result<bool, WakeFailure> {
        self.serving_probes.fetch_add(1, Ordering::SeqCst);
        match self.mode {
            ProbeMode::ColdMissing => Ok(true),
            ProbeMode::ColdUnavailable => Err(WakeFailure::new(
                shared_types::ERR_RUNTIME_UNAVAILABLE,
                "wake_runtime_probe",
                "runtime query unavailable",
            )),
            _ => Ok(false),
        }
    }

    async fn remote_wake_state_fresh(&self, _: &str) -> RemoteWakeState {
        self.fresh_probes.fetch_add(1, Ordering::SeqCst);
        if matches!(self.mode, ProbeMode::FreshUnavailable) {
            RemoteWakeState::Unavailable
        } else {
            RemoteWakeState::Missing
        }
    }

    async fn ensure_running(&self, _: &str) -> WakeOutcome {
        self.legacy_calls.fetch_add(1, Ordering::SeqCst);
        WakeOutcome::AlreadyRunning
    }

    async fn ensure_serving(&self, app_id: &str) -> WakeOutcome {
        self.serving_calls.fetch_add(1, Ordering::SeqCst);
        if let Some(stale_port) = self.stale_port {
            self.service.remove_app_backend(app_id, stale_port);
        }
        self.service
            .add_app_backend(app_id, self.backend_port, self.backend_host.clone());
        WakeOutcome::Ready
    }
}

#[derive(Default)]
struct AccessCounter(AtomicUsize);

#[async_trait::async_trait]
impl shared_types::AppAccessTracker for AccessCounter {
    async fn touch(&self, _: &str) -> Option<chrono::DateTime<chrono::Utc>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        None
    }
}

struct Harness {
    proxy_port: u16,
    control: Arc<ServingControl>,
    accesses: Arc<AccessCounter>,
    requests: Arc<AtomicUsize>,
    stop: oneshot::Sender<()>,
    proxy: tokio::task::JoinHandle<Result<(), ProxyError>>,
    backend: tokio::task::JoinHandle<()>,
    #[cfg(feature = "deploy-host")]
    registered_hosts: Vec<String>,
}

impl Harness {
    async fn start(mode: ProbeMode) -> Self {
        let backend = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("backend listener");
        let backend_port = backend.local_addr().unwrap().port();
        let requests = Arc::new(AtomicUsize::new(0));
        let received = requests.clone();
        let backend = tokio::spawn(async move {
            let app = axum::Router::new().fallback(move || {
                let received = received.clone();
                async move {
                    received.fetch_add(1, Ordering::SeqCst);
                    "restored"
                }
            });
            axum::serve(backend, app).await.expect("backend serve");
        });
        #[cfg(feature = "deploy-host")]
        let backend_host = {
            let host = format!("serving-backend-{backend_port}");
            shared_types::published::register_port(&host, backend_port, backend_port);
            host
        };
        #[cfg(not(feature = "deploy-host"))]
        let backend_host = "127.0.0.1".to_string();
        let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let mut manager = PingoraServerManager::new(ProxyConfig::with_listen_port(proxy_port))
            .with_api_key_config(Arc::new(ArcSwap::from_pointee(
                shared_types::ApiKeyAuthConfig {
                    enabled: true,
                    api_key: TEST_KEY.to_string(),
                },
            )));
        let service = manager.service();
        service.set_error_pages(Arc::new(crate::error_page::ErrorPageRenderer::new(None)));
        let stale_port = if matches!(
            mode,
            ProbeMode::MissingAfterConnect | ProbeMode::FreshUnavailable
        ) {
            let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = reservation.local_addr().unwrap().port();
            drop(reservation);
            #[cfg(feature = "deploy-host")]
            let stale_host = {
                let host = format!("serving-stale-{port}");
                shared_types::published::register_port(&host, port, port);
                host
            };
            #[cfg(not(feature = "deploy-host"))]
            let stale_host = "127.0.0.1".to_string();
            service.add_app_backend(APP_ID, port, stale_host);
            Some(port)
        } else {
            None
        };
        let control = Arc::new(ServingControl {
            mode,
            service,
            backend_port,
            backend_host: backend_host.clone(),
            stale_port,
            serving_calls: AtomicUsize::new(0),
            legacy_calls: AtomicUsize::new(0),
            serving_probes: AtomicUsize::new(0),
            fresh_probes: AtomicUsize::new(0),
        });
        let accesses = Arc::new(AccessCounter::default());
        manager = manager
            .with_wake_control(control.clone())
            .with_access_tracker(accesses.clone());
        let (stop, stopped) = oneshot::channel();
        let proxy = tokio::spawn(async move { manager.start(stopped).await });
        let mut listening = false;
        for _ in 0..100 {
            if tokio::net::TcpStream::connect(("127.0.0.1", proxy_port))
                .await
                .is_ok()
            {
                listening = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(listening, "isolated proxy did not listen");
        #[cfg(feature = "deploy-host")]
        let registered_hosts = {
            let mut hosts = vec![backend_host];
            if let Some(port) = stale_port {
                hosts.push(format!("serving-stale-{port}"));
            }
            hosts
        };
        Self {
            proxy_port,
            control,
            accesses,
            requests,
            stop,
            proxy,
            backend,
            #[cfg(feature = "deploy-host")]
            registered_hosts,
        }
    }

    async fn get(&self, path: &str, key: Option<&str>) -> String {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", self.proxy_port))
            .await
            .expect("proxy connect");
        let key = key
            .map(|key| format!("X-Api-Key: {key}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: localhost\r\n{key}Accept: application/json\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        tokio::time::timeout(Duration::from_secs(6), stream.read_to_end(&mut response))
            .await
            .expect("response deadline")
            .expect("proxy response");
        String::from_utf8(response).unwrap()
    }

    async fn finish(self) {
        self.stop.send(()).unwrap();
        self.proxy.await.unwrap().unwrap();
        self.backend.abort();
        #[cfg(feature = "deploy-host")]
        for host in self.registered_hosts {
            shared_types::published::unregister(&host);
        }
    }
}

#[tokio::test]
async fn authorized_prod_app_restores_cold_or_disappeared_workload() {
    for mode in [
        ProbeMode::ColdMissing,
        ProbeMode::MissingAfterDispatch,
        ProbeMode::MissingAfterConnect,
    ] {
        let harness = Harness::start(mode).await;
        let response = harness.get(APP_PATH, Some(TEST_KEY)).await;
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("restored"), "{response}");
        assert_eq!(harness.control.serving_calls.load(Ordering::SeqCst), 1);
        assert_eq!(harness.control.legacy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.requests.load(Ordering::SeqCst), 1);
        if !matches!(mode, ProbeMode::ColdMissing) {
            assert_eq!(harness.control.fresh_probes.load(Ordering::SeqCst), 1);
        }
        harness.finish().await;
    }
}

#[tokio::test]
async fn missing_or_invalid_key_cannot_touch_probe_or_restore_prod() {
    let harness = Harness::start(ProbeMode::ColdMissing).await;
    for key in [None, Some("incorrect-key")] {
        let response = harness.get(APP_PATH, key).await;
        assert!(response.starts_with("HTTP/1.1 401"), "{response}");
    }
    assert_eq!(harness.accesses.0.load(Ordering::SeqCst), 0);
    assert_eq!(harness.control.serving_probes.load(Ordering::SeqCst), 0);
    assert_eq!(harness.control.serving_calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.control.legacy_calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.requests.load(Ordering::SeqCst), 0);
    harness.finish().await;
}

#[tokio::test]
async fn tool_and_observation_routes_do_not_authorize_missing_reconstruction() {
    let harness = Harness::start(ProbeMode::ColdMissing).await;
    for path in [
        "/api/v1/userapp/proxy/ttyd/prod/user/serving-recovery/",
        "/api/v1/userapp/proxy/dbx/prod/user/serving-recovery/",
        "/api/v1/userapp/serving-recovery/prod/readiness",
    ] {
        let response = harness.get(path, Some(TEST_KEY)).await;
        assert!(response.starts_with("HTTP/1.1 404"), "{path}: {response}");
    }
    assert_eq!(harness.accesses.0.load(Ordering::SeqCst), 0);
    assert_eq!(harness.control.serving_probes.load(Ordering::SeqCst), 0);
    assert_eq!(harness.control.serving_calls.load(Ordering::SeqCst), 0);
    assert_eq!(harness.control.legacy_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
}

#[tokio::test]
async fn stopped_tools_keep_the_existing_workload_wake_capability() {
    let harness = Harness::start(ProbeMode::LegacyStopped).await;
    let response = harness
        .get(
            "/api/v1/userapp/proxy/ttyd/prod/user/serving-recovery/",
            Some(TEST_KEY),
        )
        .await;
    assert!(!response.starts_with("HTTP/1.1 200"), "{response}");
    assert_eq!(harness.control.legacy_calls.load(Ordering::SeqCst), 1);
    assert_eq!(harness.control.serving_calls.load(Ordering::SeqCst), 0);
    harness.finish().await;
}

#[tokio::test]
async fn runtime_probe_failures_never_authorize_reconstruction() {
    for mode in [ProbeMode::ColdUnavailable, ProbeMode::FreshUnavailable] {
        let harness = Harness::start(mode).await;
        let response = harness.get(APP_PATH, Some(TEST_KEY)).await;
        assert!(!response.starts_with("HTTP/1.1 200"), "{response}");
        assert_eq!(harness.control.serving_calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.control.legacy_calls.load(Ordering::SeqCst), 0);
        assert_eq!(harness.requests.load(Ordering::SeqCst), 0);
        harness.finish().await;
    }
}
