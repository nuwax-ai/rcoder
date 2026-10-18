//! The actual original executor owns an OS lock while its prepare RPC is held.
//! Exercise ordinary public retry and the scanner without a wall-clock race.
use super::*;
use std::path::PathBuf;
use tokio::sync::{Notify, Semaphore};

pub(super) struct PrepareGate {
    path: PathBuf,
    prepared: Notify,
    inspected: Notify,
    release: Semaphore,
    pause_before_lock: AtomicBool,
    pause_after_lock: AtomicBool,
    before_lock: Notify,
    allow_lock: Semaphore,
}

impl PrepareGate {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            prepared: Notify::new(),
            inspected: Notify::new(),
            release: Semaphore::new(0),
            pause_before_lock: AtomicBool::new(false),
            pause_after_lock: AtomicBool::new(true),
            before_lock: Notify::new(),
            allow_lock: Semaphore::new(0),
        }
    }

    pub(super) async fn acquire(
        &self,
        _: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<std::fs::File> {
        if self.pause_before_lock.swap(false, Ordering::SeqCst) {
            self.before_lock.notify_one();
            self.allow_lock.acquire().await.unwrap().forget();
        }
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&self.path)
            .map_err(|error| ContainerRuntimeError::Conflict(error.to_string()))?;
        file.try_lock()
            .map_err(|error| ContainerRuntimeError::Conflict(error.to_string()))?;
        self.prepared.notify_one();
        if self.pause_after_lock.swap(false, Ordering::SeqCst) {
            self.release.acquire().await.unwrap().forget();
        }
        Ok(file)
    }

    pub(super) fn handoff(&self) -> ContainerRuntimeResult<container_runtime_api::ComputeHandoff> {
        use container_runtime_api::ComputeHandoff;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ComputeHandoff::Vacant {
                    original_absent: true,
                });
            }
            Err(error) => return Err(ContainerRuntimeError::Conflict(error.to_string())),
        };
        let observed = match file.try_lock() {
            Ok(()) => ComputeHandoff::Acquired {
                guard: Box::new(HandoffFd { _file: file }),
                original_absent: false,
            },
            Err(std::fs::TryLockError::WouldBlock) => ComputeHandoff::Held,
            Err(error) => return Err(ContainerRuntimeError::Conflict(error.to_string())),
        };
        self.inspected.notify_one();
        Ok(observed)
    }

    pub(super) fn inspect(&self) -> ContainerRuntimeResult<ComputeLeaseInspection> {
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ComputeLeaseInspection::Absent);
            }
            Err(error) => return Err(ContainerRuntimeError::Conflict(error.to_string())),
        };
        let observation = match file.try_lock() {
            Ok(()) => ComputeLeaseInspection::Absent,
            Err(std::fs::TryLockError::WouldBlock) => ComputeLeaseInspection::Held,
            Err(error) => return Err(ContainerRuntimeError::Conflict(error.to_string())),
        };
        self.inspected.notify_one();
        Ok(observation)
    }
}

struct HandoffFd {
    _file: std::fs::File,
}
impl container_runtime_api::ComputeHandoffGuard for HandoffFd {}

enum Entry {
    DuplicateRestart,
    Scanner,
}

async fn running_prepared_executor_is_preserved(entry: Entry) {
    let (state, runtime, directory) = fresh_state().await;
    running_prepared_executor_on_state(entry, state, runtime, &directory, "live-prepare", None)
        .await;
}

async fn running_prepared_executor_on_state(
    entry: Entry,
    state: Arc<AppState>,
    runtime: Arc<Runtime>,
    directory: &tempfile::TempDir,
    app: &str,
    peer: Option<Arc<AppState>>,
) {
    state.userapp_store.ensure_identity(app).await.unwrap();
    let gate = Arc::new(PrepareGate::new(directory.path().join("original.lock")));
    *runtime.prepare_gate.lock().unwrap() = Some(gate.clone());
    let request = || UserAppControlRequest {
        lifecycle_id: None,
        request_id: None,
    };
    let admitted = submit(
        &state,
        app.into(),
        UserAppOperationScope::Dev,
        ComputeControlAction::Restart,
        request(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gate.prepared.notified())
        .await
        .expect("original executor has physically acquired its prepare lock");
    let original = state
        .userapp_store
        .get_compute_control(&admitted.app_id, &admitted.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.state, ComputeControlState::Running);
    assert_eq!(original.stage, "draining_previous");
    assert!(
        original.lease.is_none(),
        "prepare has not returned for bind"
    );
    let peer = peer.as_ref().unwrap_or(&state);
    match entry {
        Entry::DuplicateRestart => {
            let joined = submit(
                peer,
                app.into(),
                UserAppOperationScope::Dev,
                ComputeControlAction::Restart,
                request(),
            )
            .await
            .unwrap();
            assert_eq!(joined.operation_id, original.operation_id);
            tokio::time::timeout(Duration::from_secs(5), gate.inspected.notified())
                .await
                .expect("duplicate examined the actual original lock");
        }
        Entry::Scanner => {
            crate::userapp_builder::recovery::run_compute_scan_for_test(peer)
                .await
                .unwrap();
        }
    }
    let observed = state
        .userapp_store
        .get_compute_control(&original.app_id, &original.operation_id)
        .await
        .unwrap()
        .unwrap();
    // Do not return early on assertion failure with the real executor blocked.
    gate.release.add_permits(1);
    assert_eq!(
        observed, original,
        "a held original FD must not revoke an otherwise normal Running executor"
    );
    let completed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = read(&state, &original).await;
            if current.state == ComputeControlState::Succeeded && current.lease.is_none() {
                break current;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the original executor completes without a replacement attempt");
    assert_eq!(completed.executor_id, original.executor_id);
    assert_eq!(completed.request_id, original.request_id);
    assert_eq!(completed.generation, original.generation);
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn duplicate_restart_preserves_running_executor_while_prepare_fd_is_held() {
    running_prepared_executor_is_preserved(Entry::DuplicateRestart).await;
}

#[tokio::test]
async fn scanner_preserves_running_executor_while_prepare_fd_is_held() {
    running_prepared_executor_is_preserved(Entry::Scanner).await;
}

async fn completed_original(
    state: &AppState,
    original: &ComputeControlRecord,
) -> ComputeControlRecord {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let record = read(state, original).await;
            if record.state == ComputeControlState::Succeeded && record.lease.is_none() {
                break record;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("same original compute operation completed")
}

#[tokio::test]
async fn late_prepare_after_early_handoff_is_fenced_without_a_second_stop_or_start() {
    let (state, runtime, directory) = fresh_state().await;
    state
        .userapp_store
        .ensure_identity("late-prepare")
        .await
        .unwrap();
    let gate = Arc::new(PrepareGate::new(directory.path().join("late.lock")));
    gate.pause_before_lock.store(true, Ordering::SeqCst);
    gate.pause_after_lock.store(false, Ordering::SeqCst);
    *runtime.prepare_gate.lock().unwrap() = Some(gate.clone());
    let request = || UserAppControlRequest {
        lifecycle_id: None,
        request_id: None,
    };
    let admitted = submit(
        &state,
        "late-prepare".into(),
        UserAppOperationScope::Dev,
        ComputeControlAction::Restart,
        request(),
    )
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), gate.before_lock.notified())
        .await
        .unwrap();
    let original = state
        .userapp_store
        .get_compute_control(&admitted.app_id, &admitted.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(original.state, ComputeControlState::Running);
    assert!(
        !gate.path.exists(),
        "original RPC has not acquired its mutex"
    );
    let joined = submit(
        &state,
        "late-prepare".into(),
        UserAppOperationScope::Dev,
        ComputeControlAction::Restart,
        request(),
    )
    .await
    .unwrap();
    assert_eq!(joined.operation_id, original.operation_id);
    let complete = completed_original(&state, &original).await;
    assert_ne!(complete.executor_id, original.executor_id);
    gate.allow_lock.add_permits(1);
    tokio::time::timeout(Duration::from_secs(5), async {
        while runtime.calls.releases.load(Ordering::SeqCst) < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("late old bind was rejected and its read-only lease exited");
    assert_eq!(read(&state, &original).await, complete);
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn handoff_cas_loser_drops_real_fd_without_clearing_original_marker() {
    let (state, runtime, pending, directory) = fixture().await;
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "original-executor".into(),
    };
    let original = state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let gate = Arc::new(PrepareGate::new(directory.path().join("cas.lock")));
    std::fs::write(&gate.path, b"original-executor").unwrap();
    *runtime.prepare_gate.lock().unwrap() = Some(gate.clone());
    *runtime.handoff_supersede.lock().unwrap() = Some((
        state.userapp_store.clone(),
        ComputeControlRequest {
            app_id: original.app_id.clone(),
            lifecycle_id: original.lifecycle_id.clone(),
            scope: original.scope,
            operation_id: "superseding-stop".into(),
            request_id: "stop-request".into(),
            request_fingerprint: "b".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
            builder_startup_mode: Default::default(),
        },
    ));
    assert!(
        crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &original)
            .await
            .unwrap()
            .is_none()
    );
    let superseded = read(&state, &original).await;
    assert_eq!(superseded.state, ComputeControlState::Superseded);
    assert_eq!(superseded.checkpoint, original.checkpoint);
    assert_eq!(superseded.lease, original.lease);
    assert_eq!(std::fs::read(&gate.path).unwrap(), b"original-executor");
    let observer = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&gate.path)
        .unwrap();
    observer.try_lock().unwrap();
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unknown_handoff_observation_preserves_running_record_for_the_original_retry() {
    let (state, runtime, pending, _directory) = fixture().await;
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "original-executor".into(),
    };
    let original = state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    runtime.handoff_error.store(true, Ordering::SeqCst);
    assert!(
        crate::userapp_builder::compute_drain_recovery::prepare_resume(&state, &original)
            .await
            .unwrap_err()
            .to_string()
            .contains("injected unknown handoff response")
    );
    assert_eq!(read(&state, &original).await, original);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn pg_peer_duplicate_and_scanner_preserve_the_actual_prepared_fd_holder() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|value| !value.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG tests require a disposable DSN"
    );
    let Some(dsn) = dsn else {
        eprintln!("[skip] explicit disposable PG DSN required for the compute handoff contract");
        return;
    };
    // The real scanner visits every application. Its controlled runtime must
    // never execute pending work belonging to a concurrently running fixture.
    let schema = format!("handoff_{}", uuid::Uuid::new_v4().simple());
    let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
    toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
        .exec(&mut admin)
        .await
        .unwrap();
    let mut scoped = url::Url::parse(&dsn).unwrap();
    let query: Vec<(String, String)> = scoped
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    let options = query
        .iter()
        .filter(|(key, _)| key == "options")
        .map(|(_, value)| value.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    {
        let mut params = scoped.query_pairs_mut();
        params.clear();
        params.extend_pairs(
            query
                .iter()
                .filter(|(key, _)| key != "options")
                .map(|(key, value)| (key.as_str(), value.as_str())),
        );
        params.append_pair("options", &format!("{options} -csearch_path={schema}"));
    }
    let config = rcoder_storage::config::PostgresConfig {
        url: Some(scoped.into()),
        ..Default::default()
    };
    let first = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(&config)
            .await
            .unwrap(),
    );
    let second = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(&config)
            .await
            .unwrap(),
    );
    for entry in [Entry::DuplicateRestart, Entry::Scanner] {
        let directory = tempfile::tempdir().unwrap();
        let runtime = Arc::new(Runtime::default());
        runtime.retry_allowed.store(true, Ordering::SeqCst);
        let state = runtime_state(runtime.clone(), first.clone(), None).await;
        let peer = runtime_state(runtime.clone(), second.clone(), None).await;
        let app = format!(
            "handoff{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        );
        running_prepared_executor_on_state(entry, state, runtime, &directory, &app, Some(peer))
            .await;
    }
    first.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
    toasty::sql::statement(format!("DROP SCHEMA {schema} CASCADE"))
        .exec(&mut admin)
        .await
        .unwrap();
}
