use super::*;
use std::path::Path;

async fn stopped_owner(root: &Path) -> Arc<OwnerSession> {
    let owner = Owner::try_acquire(root).unwrap().unwrap();
    let session = Arc::new(
        OwnerSession::start(
            owner,
            SessionOptions {
                binding: control::Binding {
                    component: "test".into(),
                    resource: root.to_path_buf(),
                },
                policy: crate::Policy::default(),
                cleanup_adapter: None,
                restart_on_exit: false,
                shutdown: CancellationToken::new(),
            },
        )
        .await
        .unwrap(),
    );
    session
        .core
        .with_discovery(|discovery| {
            discovery.snapshot.phase = Phase::Stopped;
            discovery.snapshot.intent = Intent::Stopped;
            discovery.snapshot.generation = None;
            Ok(())
        })
        .unwrap();
    session
}

#[tokio::test]
async fn new_explicit_business_intent_survives_a_new_owner_without_replaying_stop() {
    let directory = tempfile::tempdir().unwrap();
    let session = stopped_owner(directory.path()).await;
    let captured = session.capture_business_start().unwrap();
    session.request_explicit_business_start(&captured).unwrap();
    assert_eq!(session.snapshot().intent, Intent::Run);
    let original = session.snapshot().supervisor_id;
    drop(session);
    let owner = Owner::try_acquire(directory.path()).unwrap().unwrap();
    let recovered = OwnerSession::start(
        owner,
        SessionOptions {
            binding: control::Binding {
                component: "test".into(),
                resource: directory.path().to_path_buf(),
            },
            policy: crate::Policy::default(),
            cleanup_adapter: None,
            restart_on_exit: false,
            shutdown: CancellationToken::new(),
        },
    )
    .await
    .unwrap();
    assert_ne!(recovered.snapshot().supervisor_id, original);
    assert_eq!(recovered.snapshot().intent, Intent::Run);
}

#[tokio::test]
async fn completed_stop_invalidates_old_source_callback_even_when_generation_remains_none() {
    let directory = tempfile::tempdir().unwrap();
    let session = stopped_owner(directory.path()).await;
    let captured = session.capture_business_start().unwrap();
    let mut driver = session.run.lock().unwrap().take().unwrap();
    driver.seed_fence().await.unwrap();
    let discovery = session.core.discovery.lock().unwrap().clone();
    let stopped = driver
        .handle_request(&Envelope {
            version: control::CONTROL_VERSION,
            instance: discovery.instance,
            token: discovery.token,
            request: control::Request::new(control::Action::StopWork),
        })
        .unwrap();
    assert_eq!(stopped.phase, Phase::Stopped);
    let before = std::fs::read(directory.path().join("supervisor.json")).unwrap();
    assert!(session.request_explicit_business_start(&captured).is_err());
    assert_eq!(session.snapshot().intent, Intent::Stopped);
    assert_eq!(
        std::fs::read(directory.path().join("supervisor.json")).unwrap(),
        before
    );
    let current = session.capture_business_start().unwrap();
    session.request_explicit_business_start(&current).unwrap();
    assert_eq!(session.snapshot().intent, Intent::Run);
}

#[tokio::test]
async fn pending_stop_shutdown_and_changed_generation_never_accept_old_business_callback() {
    let directory = tempfile::tempdir().unwrap();
    let session = stopped_owner(directory.path()).await;
    for phase in [Phase::Stopping, Phase::CleanupPending] {
        let captured = session.capture_business_start().unwrap();
        session
            .core
            .with_discovery(|discovery| {
                discovery.snapshot.phase = phase;
                Ok(())
            })
            .unwrap();
        assert!(session.request_explicit_business_start(&captured).is_err());
        assert_eq!(session.snapshot().intent, Intent::Stopped);
        session
            .core
            .with_discovery(|discovery| {
                discovery.snapshot.phase = Phase::Stopped;
                Ok(())
            })
            .unwrap();
    }
    let captured = session.capture_business_start().unwrap();
    session
        .core
        .with_discovery(|discovery| {
            discovery.snapshot.generation = Some(uuid::Uuid::new_v4().to_string());
            Ok(())
        })
        .unwrap();
    assert!(session.request_explicit_business_start(&captured).is_err());
    let captured = session.capture_business_start().unwrap();
    session
        .core
        .with_discovery(|discovery| {
            discovery.snapshot.intent = Intent::Shutdown;
            Ok(())
        })
        .unwrap();
    assert!(session.request_explicit_business_start(&captured).is_err());
    assert_eq!(session.snapshot().intent, Intent::Shutdown);
}

#[tokio::test]
async fn before_source_preparation_refreshes_initial_ready_generation_without_losing_stop_history()
{
    let directory = tempfile::tempdir().unwrap();
    let session = stopped_owner(directory.path()).await;
    session
        .core
        .with_discovery(|discovery| {
            discovery.snapshot.phase = Phase::Ready;
            discovery.snapshot.intent = Intent::Run;
            discovery.snapshot.generation = Some("previous-cleaned-generation".into());
            Ok(())
        })
        .unwrap();
    let before_preparation = session.capture_business_start().unwrap();
    session
        .core
        .with_discovery(|discovery| {
            discovery.snapshot.generation = Some("initialized-current-generation".into());
            Ok(())
        })
        .unwrap();
    let observed = session.capture_business_start().unwrap();
    let prepared = before_preparation.after_initialization(&observed).unwrap();
    session.request_explicit_business_start(&prepared).unwrap();
    assert_eq!(
        session.snapshot().generation.as_deref(),
        Some("initialized-current-generation")
    );

    // Once preparation has captured the current generation, a later generation
    // still invalidates publication. This does not relax the commit fence.
    session
        .core
        .with_discovery(|discovery| {
            discovery.snapshot.generation = Some("later-generation".into());
            Ok(())
        })
        .unwrap();
    assert!(session.request_explicit_business_start(&prepared).is_err());

    let before_stop = session.capture_business_start().unwrap();
    let mut driver = session.run.lock().unwrap().take().unwrap();
    driver.seed_fence().await.unwrap();
    let discovery = session.core.discovery.lock().unwrap().clone();
    driver
        .handle_request(&Envelope {
            version: control::CONTROL_VERSION,
            instance: discovery.instance,
            token: discovery.token,
            request: control::Request::new(control::Action::StopWork),
        })
        .unwrap();
    let after_stop = session.capture_business_start().unwrap();
    assert!(before_stop.after_initialization(&after_stop).is_err());
    assert_eq!(session.snapshot().intent, Intent::Stopped);
}

#[tokio::test]
async fn expired_dispatch_waiting_for_native_commit_lock_never_changes_stopped_intent() {
    let directory = tempfile::tempdir().unwrap();
    let session = stopped_owner(directory.path()).await;
    let captured = session.capture_business_start().unwrap();
    let before = std::fs::read(directory.path().join("supervisor.json")).unwrap();
    let original = session.snapshot();
    let barrier = session.core.discovery_commit.lock().unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_millis(40);
    let (entered, waiting) = std::sync::mpsc::channel();
    let attempted = session.clone();
    let thread = std::thread::spawn(move || {
        entered.send(()).unwrap();
        attempted.request_explicit_business_start_before(&captured, deadline)
    });
    waiting.recv().unwrap();
    std::thread::sleep(Duration::from_millis(70));
    assert!(
        !thread.is_finished(),
        "the original native commit must still be waiting for its real lock"
    );
    drop(barrier);
    let error = thread.join().unwrap().unwrap_err();
    assert!(error.to_string().contains("deadline exceeded"), "{error:#}");
    assert_eq!(
        serde_json::to_value(session.snapshot()).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(session.snapshot().intent, Intent::Stopped);
    assert_eq!(
        std::fs::read(directory.path().join("supervisor.json")).unwrap(),
        before
    );
}
