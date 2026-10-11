//! Bounded original dispatch preparation, independently of compile/download.
use super::*;

#[tokio::test]
async fn expired_management_deadline_is_rejected_even_without_a_native_capture_hook() {
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    state.mark_initialized();
    let error = wait_business_management(&state, "original", None, tokio::time::Instant::now())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("deadline exceeded"));
    assert!(state.deploy_rx.lock().await.try_recv().is_err());
    assert!(state.control_rx.lock().await.try_recv().is_err());
}

#[tokio::test]
async fn original_dispatch_expiring_while_admission_is_held_cannot_commit() {
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    state.mark_initialized();
    let barrier = state.admission.lock().unwrap();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(40);
    let (entered, waiting) = std::sync::mpsc::channel();
    let attempted = state.clone();
    let thread = std::thread::spawn(move || {
        entered.send(()).unwrap();
        attempted.commit_business_relaunch("original", None, deadline)
    });
    waiting.recv().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(70));
    assert!(
        !thread.is_finished(),
        "the request must wait for actual admission ownership"
    );
    drop(barrier);
    let error = thread.join().unwrap().unwrap_err();
    assert!(error.to_string().contains("deadline exceeded"));
    assert!(state.deploy_rx.lock().await.try_recv().is_err());
    assert!(state.control_rx.lock().await.try_recv().is_err());
}

struct TestBusiness(CancellationToken);
#[async_trait::async_trait]
impl runtime_supervisor::WorkerControl for TestBusiness {
    async fn probe(&self) -> Result<()> {
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        self.0.cancel();
        Ok(())
    }
}

#[tokio::test]
async fn already_ready_native_management_cannot_bypass_an_expired_original_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let shutdown = CancellationToken::new();
    let session = Arc::new(
        runtime_supervisor::OwnerSession::start(
            runtime_supervisor::Owner::try_acquire(directory.path())
                .unwrap()
                .unwrap(),
            runtime_supervisor::SessionOptions {
                binding: runtime_supervisor::Binding {
                    component: "deadline-test".into(),
                    resource: directory.path().to_path_buf(),
                },
                policy: runtime_supervisor::Policy::default(),
                cleanup_adapter: None,
                restart_on_exit: false,
                shutdown: shutdown.clone(),
            },
        )
        .await
        .unwrap(),
    );
    let owner = session.clone();
    let ready_owner = session.clone();
    let running = tokio::spawn(async move {
        owner
            .run(Box::new(move |launch| {
                ready_owner.ready_guard(&launch.generation).mark_ready()?;
                let cancel = CancellationToken::new();
                Ok(runtime_supervisor::BusinessRun {
                    control: Arc::new(TestBusiness(cancel.clone())),
                    end: Box::pin(async move {
                        cancel.cancelled().await;
                        Ok(None)
                    }),
                })
            }))
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while !session.capture_business_start().unwrap().management_ready() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let state = Arc::new(ServerState::new(RuntimeStatusService::default()));
    let capture = session.clone();
    let commit = session.clone();
    state
        .set_business_relaunch(
            move || capture.capture_business_start(),
            move |permit, deadline| commit.request_explicit_business_start_before(permit, deadline),
            || {},
        )
        .unwrap();
    state.mark_initialized();
    let permit = state.capture_business_relaunch().unwrap();
    assert!(permit.as_ref().unwrap().management_ready());
    let error = wait_business_management(&state, "original", permit, tokio::time::Instant::now())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("deadline exceeded"));
    assert!(state.deploy_rx.lock().await.try_recv().is_err());
    assert!(state.control_rx.lock().await.try_recv().is_err());
    shutdown.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
