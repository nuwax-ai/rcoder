//! Real native sessions and held owner locks with a controlled HTTP identity
//! surface. These are handover protocol tests, not Pingap/image acceptance.
use super::*;
use std::{
    path::PathBuf,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::{Notify, Semaphore};
use tokio_util::sync::CancellationToken;

struct Business(CancellationToken);
#[async_trait::async_trait]
impl runtime_supervisor::WorkerControl for Business {
    async fn probe(&self) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        self.0.cancel();
        Ok(())
    }
}

struct NativeOwner {
    session: Weak<runtime_supervisor::OwnerSession>,
    task: tokio::task::JoinHandle<Result<i32>>,
}
impl NativeOwner {
    async fn start(state: &Path, source: &Path, allow_business: Arc<AtomicBool>) -> Self {
        let lease = runtime_supervisor::Owner::try_acquire(state)
            .unwrap()
            .unwrap();
        let session = Arc::new(
            runtime_supervisor::OwnerSession::start(
                lease,
                runtime_supervisor::SessionOptions {
                    binding: runtime_supervisor::Binding {
                        component: "app-cli".into(),
                        resource: source.into(),
                    },
                    policy: runtime_supervisor::Policy {
                        restart_limit: 1,
                        ..Default::default()
                    },
                    cleanup_adapter: None,
                    restart_on_exit: false,
                    shutdown: CancellationToken::new(),
                },
            )
            .await
            .unwrap(),
        );
        let weak = Arc::downgrade(&session);
        let task = tokio::spawn(session.run(Box::new(move |_| {
            if !allow_business.load(Ordering::SeqCst) {
                anyhow::bail!("controlled owner remains idle until an explicit business request");
            }
            let token = CancellationToken::new();
            let end = token.clone();
            Ok(runtime_supervisor::BusinessRun {
                control: Arc::new(Business(token)),
                end: Box::pin(async move {
                    end.cancelled().await;
                    Ok(None)
                }),
            })
        })));
        wait_snapshot(state, |snapshot| {
            snapshot.generation.is_none()
                && snapshot.phase == runtime_supervisor::Phase::RecoveryRequired
        })
        .await;
        Self {
            session: weak,
            task,
        }
    }

    async fn close(self, state: &Path) {
        let snapshot = runtime_supervisor::control(state, Request::new(Action::Status))
            .await
            .unwrap();
        let mut shutdown = Request::new(Action::Shutdown);
        shutdown.capture_generation(snapshot.generation.as_deref());
        runtime_supervisor::shutdown_captured_owner(
            state,
            &snapshot,
            &shutdown,
            Duration::from_secs(3),
        )
        .await
        .unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), self.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            0
        );
    }
}

async fn wait_snapshot(state: &Path, ready: impl Fn(&Snapshot) -> bool) -> Snapshot {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let snapshot = runtime_supervisor::control(state, Request::new(Action::Status))
                .await
                .unwrap();
            if ready(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("actual native state must reach the controlled stage")
}

struct Manager {
    _directory: tempfile::TempDir,
    state: PathBuf,
    source: PathBuf,
    address: String,
    identity: RuntimeIdentityView,
    native: Option<NativeOwner>,
    allow_business: Arc<AtomicBool>,
    identity_entered: Arc<Notify>,
    identity_release: Arc<Semaphore>,
    posts: Arc<AtomicUsize>,
    http: tokio::task::JoinHandle<()>,
}
impl Manager {
    async fn start(gated_identity: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let base = directory.path().canonicalize().unwrap();
        let source = base.join("source");
        let state = base.join("state");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(state.join("supervisor.json"), b"damaged native discovery").unwrap();
        std::fs::write(source.join("sentinel"), b"preserve user data").unwrap();
        let allow_business = Arc::new(AtomicBool::new(false));
        let native = NativeOwner::start(&state, &source, allow_business.clone()).await;
        let identity = RuntimeIdentityView {
            application_id: "credential-race-app".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "credential-race-workspace".into(),
            source_root: source.display().to_string(),
            runtime_instance_id: uuid::Uuid::new_v4().to_string(),
            deployment_generation_id: "credential-race-deployment".into(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![
                shared_types::OFFICIAL_PROXY_RESTART_CAPABILITY.into(),
                workspace_manifest::STARTUP_PROBE_CAPABILITY.into(),
            ],
        };
        std::fs::write(
            state.join("identity.json"),
            serde_json::to_vec(&identity).unwrap(),
        )
        .unwrap();
        std::fs::write(state.join("token"), b"wrong-control-token").unwrap();
        let identity_entered = Arc::new(Notify::new());
        let identity_release = Arc::new(Semaphore::new(0));
        let posts = Arc::new(AtomicUsize::new(0));
        let wire = identity.clone();
        let entered = identity_entered.clone();
        let release = identity_release.clone();
        let count = posts.clone();
        let router = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = wire.clone();
                    let entered = entered.clone();
                    let release = release.clone();
                    async move {
                        if gated_identity {
                            entered.notify_one();
                            release.acquire_owned().await.unwrap().forget();
                        }
                        axum::Json(serde_json::json!({"code":"OK","data":identity}))
                    }
                }),
            )
            .route(
                "/ready",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"status":"ready"})) }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(|| async { axum::http::StatusCode::UNAUTHORIZED }),
            )
            .fallback(move |request: axum::extract::Request| {
                let count = count.clone();
                async move {
                    if request.method() == axum::http::Method::POST {
                        count.fetch_add(1, Ordering::SeqCst);
                    }
                    axum::http::StatusCode::NOT_FOUND
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let http = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self {
            _directory: directory,
            state,
            source,
            address,
            identity,
            native: Some(native),
            allow_business,
            identity_entered,
            identity_release,
            posts,
            http,
        }
    }

    fn retire(&self) -> tokio::task::JoinHandle<Result<bool>> {
        let state = self.state.clone();
        let source = self.source.clone();
        let address = self.address.clone();
        let identity = self.identity.clone();
        tokio::spawn(async move {
            retire_observed_manager(
                &state,
                &source,
                &address,
                &identity,
                Instant::now() + Duration::from_secs(5),
            )
            .await
        })
    }

    async fn captured(&self) {
        tokio::time::timeout(Duration::from_secs(3), self.identity_entered.notified())
            .await
            .expect("HTTP barrier follows the original native Status capture");
    }

    fn assert_preserved(&self, snapshot: &Snapshot) {
        assert_ne!(snapshot.intent, runtime_supervisor::Intent::Shutdown);
        assert!(
            runtime_supervisor::completed_shutdown_for(&self.state, &snapshot.supervisor_id)
                .unwrap()
                .is_none(),
            "a stale credential observation must not create a successor Shutdown"
        );
        assert!(
            runtime_supervisor::Owner::try_acquire(&self.state)
                .unwrap()
                .is_none(),
            "the successor must retain its actual owner lock"
        );
        assert_eq!(
            std::fs::read(self.source.join("sentinel")).unwrap(),
            b"preserve user data"
        );
        assert_eq!(self.posts.load(Ordering::SeqCst), 0);
    }

    async fn close(mut self) {
        if let Some(owner) = self.native.take() {
            owner.close(&self.state).await;
        }
    }
}
impl Drop for Manager {
    fn drop(&mut self) {
        self.http.abort();
        if let Some(owner) = &self.native {
            owner.task.abort();
        }
    }
}

#[tokio::test]
async fn credential_retirement_does_not_shutdown_a_replacement_native_owner() {
    let mut manager = Manager::start(true).await;
    let original = runtime_supervisor::last_snapshot(&manager.state).unwrap();
    let retirement = manager.retire();
    manager.captured().await;
    manager.native.take().unwrap().close(&manager.state).await;
    manager.native = Some(
        NativeOwner::start(
            &manager.state,
            &manager.source,
            manager.allow_business.clone(),
        )
        .await,
    );
    let successor = runtime_supervisor::last_snapshot(&manager.state).unwrap();
    assert_ne!(successor.supervisor_id, original.supervisor_id);
    // Even the same stale HTTP/saved identity cannot authorize the new native
    // scope: its actual control identity changed during the HTTP observation.
    manager.identity_release.add_permits(1);
    assert!(!retirement.await.unwrap().unwrap());
    let after = runtime_supervisor::control(&manager.state, Request::new(Action::Status))
        .await
        .unwrap();
    assert_eq!(after.supervisor_id, successor.supervisor_id);
    manager.assert_preserved(&after);
    manager.close().await;
}

#[tokio::test]
async fn credential_retirement_preserves_a_generation_started_after_idle_capture() {
    let manager = Manager::start(true).await;
    let original = runtime_supervisor::last_snapshot(&manager.state).unwrap();
    assert!(original.generation.is_none());
    let retirement = manager.retire();
    manager.captured().await;
    manager.allow_business.store(true, Ordering::SeqCst);
    {
        let session = manager.native.as_ref().unwrap().session.upgrade().unwrap();
        let permit = session.capture_business_start().unwrap();
        session.request_explicit_business_start(&permit).unwrap();
        session.request_relaunch();
    }
    let running = wait_snapshot(&manager.state, |snapshot| snapshot.generation.is_some()).await;
    manager.identity_release.add_permits(1);
    assert!(!retirement.await.unwrap().unwrap());
    let after = runtime_supervisor::control(&manager.state, Request::new(Action::Status))
        .await
        .unwrap();
    assert_eq!(after.supervisor_id, original.supervisor_id);
    assert_eq!(after.generation, running.generation);
    manager.assert_preserved(&after);
    manager.close().await;
}

#[tokio::test]
async fn source_dispatch_still_retires_the_same_owner_for_missing_or_invalid_token() {
    for missing in [false, true] {
        let mut manager = Manager::start(false).await;
        if missing {
            std::fs::remove_file(manager.state.join("token")).unwrap();
        }
        let original = runtime_supervisor::last_snapshot(&manager.state).unwrap();
        let result = super::super::owner_dispatch::dispatch_to_owner(
            &manager.address,
            &manager.source,
            &manager.state,
            &manager.identity.application_id,
        )
        .await
        .unwrap();
        assert!(matches!(
            result,
            super::super::owner_dispatch::OwnerDispatch::NoOwner
        ));
        let owner = manager.native.take().unwrap();
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(3), owner.task)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
            0
        );
        assert!(
            runtime_supervisor::completed_shutdown_for(&manager.state, &original.supervisor_id)
                .unwrap()
                .is_some(),
            "credential repair requires the original real Shutdown receipt"
        );
        assert!(
            runtime_supervisor::Owner::try_acquire(&manager.state)
                .unwrap()
                .is_some()
        );
        assert_eq!(manager.posts.load(Ordering::SeqCst), 0);
        assert_eq!(
            std::fs::read(manager.source.join("sentinel")).unwrap(),
            b"preserve user data"
        );
    }
}
