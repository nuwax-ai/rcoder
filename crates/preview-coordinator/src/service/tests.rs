use shared_types::{PreviewExecutor, PreviewExecutorError, PreviewLifecycleStore};

use super::*;
use crate::{InProcessPreviewStore, SingleInstanceEvidence};

struct TestExecutor {
    fail_stop: bool,
}

impl TestExecutor {
    fn new(fail_stop: bool) -> Self {
        Self { fail_stop }
    }
}

#[async_trait::async_trait]
impl PreviewExecutor for TestExecutor {
    async fn start_local(
        &self,
        ticket: &ExecutorStartTicket,
    ) -> Result<(i64, u16), PreviewExecutorError> {
        Ok((42, ticket.port))
    }

    async fn stop_local(
        &self,
        _preview_key: &str,
        _instance_id: &str,
    ) -> Result<ExecutorStopOutcome, PreviewExecutorError> {
        if self.fail_stop {
            Err(PreviewExecutorError::Failed(
                "injected stop failure".to_string(),
            ))
        } else {
            Ok(ExecutorStopOutcome::NotRegistered)
        }
    }

    async fn verify_local(
        &self,
        _preview_key: &str,
        _instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewExecutorError> {
        Ok(ExecutorVerifyReport {
            identity_match: true,
            alive: true,
            pid: Some(1),
            port: Some(PREVIEW_PORT_MIN),
        })
    }

    async fn registration_matches(
        &self,
        _preview_key: &str,
        _instance_id: &str,
    ) -> Result<bool, PreviewExecutorError> {
        Ok(true)
    }

    async fn read_log_local(
        &self,
        _log_key: &str,
        _log_type: &str,
        _start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewExecutorError> {
        Ok(ExecutorLogChunk {
            logs: Vec::new(),
            total_lines: 0,
            log_file_name: String::new(),
        })
    }
}

fn identity(project_id: &str) -> PreviewProjectIdentity {
    PreviewProjectIdentity {
        project_id: project_id.to_string(),
        tenant_id: None,
        space_id: None,
        isolation_type: None,
        resolved_path: format!("/tmp/{project_id}"),
    }
}

fn coordinator(
    store: Arc<InProcessPreviewStore>,
    executor: Arc<TestExecutor>,
) -> PreviewCoordinator {
    let config = CoordinatorConfig {
        start_budget_secs: 1,
        ..CoordinatorConfig::default()
    };
    PreviewCoordinator::new(
        store,
        executor,
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".to_string(),
        config,
    )
}

async fn seed_ready(
    store: &InProcessPreviewStore,
    identity: &PreviewProjectIdentity,
    host: PreviewHostIdentity,
    operation_id: &str,
    instance_id: &str,
) -> PreviewInstanceRecord {
    let key = compute_key(identity);
    let accepted = store
        .accept_start(AcceptStartInput {
            preview_key: key.clone(),
            project_id: identity.project_id.clone(),
            project_path: identity.resolved_path.clone(),
            host,
            operation_id: operation_id.to_string(),
            instance_id: instance_id.to_string(),
            requested_port: None,
            base_path: None,
            recover_unknown_evidence: None,
        })
        .await
        .expect("seed start admission");
    let AcceptStartOutcome::Admitted(starting) = accepted else {
        panic!("seed start must be admitted");
    };
    let port = starting.port.expect("seed start allocates port");
    store
        .publish_running(&key, operation_id, starting.revision, 42, port, None)
        .await
        .expect("seed ready instance")
}

#[tokio::test]
async fn restart_recovers_stopping_instance_owned_by_deleted_host() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(TestExecutor::new(false));
    let coordinator = coordinator(Arc::clone(&store), Arc::clone(&executor));
    let identity = identity("orphaned-preview");
    let ready = seed_ready(
        store.as_ref(),
        &identity,
        PreviewHostIdentity {
            host_id: "deleted-pod-uid:old-boot".to_string(),
            pod_name: Some("deleted-rcoder-pod".to_string()),
            pod_ip: Some("10.0.0.8".to_string()),
        },
        "old-start-operation",
        "old-instance",
    )
    .await;
    let stopping = store
        .accept_stop(&ready.preview_key, "abandoned-stop-operation")
        .await
        .expect("seed abandoned stop");
    assert_eq!(stopping.state, PreviewInstanceState::Stopping);

    let restarted = coordinator
        .restart_dev(PreviewRestartRequest {
            identity,
            base_path: None,
        })
        .await
        .expect("restart must recover deleted host and start a replacement");

    assert!(restarted.success);
    let current = store
        .get(&ready.preview_key)
        .await
        .expect("read current preview")
        .expect("replacement preview exists");
    assert_eq!(current.state, PreviewInstanceState::Ready);
    assert_eq!(current.host_id, coordinator.host().host_id);
    assert_ne!(current.instance_id, ready.instance_id);
}

#[tokio::test]
async fn start_does_not_reuse_fresh_ready_record_from_deleted_host() {
    let store = Arc::new(InProcessPreviewStore::new());
    let coordinator = coordinator(Arc::clone(&store), Arc::new(TestExecutor::new(false)));
    let identity = identity("orphaned-ready-preview");
    let old = seed_ready(
        store.as_ref(),
        &identity,
        PreviewHostIdentity {
            host_id: "deleted-pod-uid:old-boot".to_string(),
            pod_name: Some("deleted-rcoder-pod".to_string()),
            pod_ip: Some("10.0.0.8".to_string()),
        },
        "old-start-operation",
        "old-instance",
    )
    .await;
    assert!(coordinator.heartbeat_fresh(&old));

    let started = coordinator
        .start_dev(PreviewStartRequest {
            identity,
            base_path: None,
        })
        .await
        .expect("deleted host must be replaced despite a fresh heartbeat");

    assert!(started.success);
    let current = store
        .get(&old.preview_key)
        .await
        .expect("read current preview")
        .expect("replacement preview exists");
    assert_eq!(current.state, PreviewInstanceState::Ready);
    assert_eq!(current.host_id, coordinator.host().host_id);
    assert_ne!(current.instance_id, old.instance_id);
}

#[tokio::test]
async fn keep_alive_rebuilds_stale_ready_record_from_deleted_host() {
    let store = Arc::new(InProcessPreviewStore::new());
    let config = CoordinatorConfig {
        heartbeat_ttl_secs: 0,
        start_budget_secs: 1,
        ..CoordinatorConfig::default()
    };
    let coordinator = PreviewCoordinator::new(
        Arc::clone(&store) as Arc<dyn PreviewLifecycleStore>,
        Arc::new(TestExecutor::new(false)),
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".to_string(),
        config,
    );
    let identity = identity("stale-orphaned-ready-preview");
    let old = seed_ready(
        store.as_ref(),
        &identity,
        PreviewHostIdentity {
            host_id: "deleted-pod-uid:old-boot".to_string(),
            pod_name: Some("deleted-rcoder-pod".to_string()),
            pod_ip: None,
        },
        "old-start-operation",
        "old-instance",
    )
    .await;

    let kept_alive = coordinator
        .keep_alive_dev(&PreviewKeepAliveRequest {
            identity,
            port: old.port.expect("old port"),
            pid: old.pid,
            base_path: None,
        })
        .await
        .expect("deleted host must be replaced after verify is unavailable");

    assert!(kept_alive.success);
    let current = store
        .get(&old.preview_key)
        .await
        .expect("read current preview")
        .expect("replacement preview exists");
    assert_eq!(current.state, PreviewInstanceState::Ready);
    assert_eq!(current.host_id, coordinator.host().host_id);
    assert_ne!(current.instance_id, old.instance_id);
}

#[tokio::test]
async fn stop_dispatch_failure_does_not_leave_instance_stopping() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(TestExecutor::new(true));
    let coordinator = coordinator(Arc::clone(&store), executor);
    let identity = identity("failed-stop-preview");
    let ready = seed_ready(
        store.as_ref(),
        &identity,
        coordinator.host().clone(),
        "start-operation",
        "local-instance",
    )
    .await;

    let error = coordinator
        .coordinated_stop(&ready)
        .await
        .expect_err("injected stop failure must propagate");
    assert!(error.to_string().contains("injected stop failure"));

    let current = store
        .get(&ready.preview_key)
        .await
        .expect("read failed stop state")
        .expect("preview remains recorded");
    assert_eq!(current.state, PreviewInstanceState::Unknown);
    assert!(
        current
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("injected stop failure"))
    );
}

/// Holds the start acknowledgement after the executor has registered its process.
/// Heartbeat can therefore observe the actual admission before the caller publishes.
#[derive(Default)]
struct StartBarrierExecutor {
    ticket: tokio::sync::Mutex<Option<ExecutorStartTicket>>,
    entered: tokio::sync::Notify,
    acknowledge: tokio::sync::Notify,
    starts: std::sync::atomic::AtomicUsize,
    port_conflicts: std::sync::atomic::AtomicUsize,
    port_conflict_cleanups: std::sync::atomic::AtomicUsize,
    start_tickets: std::sync::Mutex<Vec<ExecutorStartTicket>>,
    stops: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl PreviewExecutor for StartBarrierExecutor {
    async fn start_local(
        &self,
        ticket: &ExecutorStartTicket,
    ) -> Result<(i64, u16), PreviewExecutorError> {
        self.starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.start_tickets.lock().unwrap().push(ticket.clone());
        *self.ticket.lock().await = Some(ticket.clone());
        self.entered.notify_one();
        self.acknowledge.notified().await;
        if self
            .port_conflicts
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |count| count.checked_sub(1),
            )
            .is_ok()
        {
            // The real executor can discover a readiness/strict-port conflict
            // after registration. PortInUse is returned only after cleanup.
            *self.ticket.lock().await = None;
            self.port_conflict_cleanups
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            return Err(PreviewExecutorError::PortInUse(
                "registered attempt cleaned after controlled port conflict".into(),
            ));
        }
        Ok((42, ticket.port))
    }

    async fn stop_local(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<ExecutorStopOutcome, PreviewExecutorError> {
        let mut ticket = self.ticket.lock().await;
        if ticket
            .as_ref()
            .is_some_and(|t| t.preview_key == key && t.instance_id == instance)
        {
            self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            *ticket = None;
            Ok(ExecutorStopOutcome::Stopped)
        } else {
            Ok(ExecutorStopOutcome::NotRegistered)
        }
    }

    async fn verify_local(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<ExecutorVerifyReport, PreviewExecutorError> {
        let ticket = self.ticket.lock().await;
        let matching = ticket
            .as_ref()
            .filter(|t| t.preview_key == key && t.instance_id == instance);
        Ok(ExecutorVerifyReport {
            identity_match: matching.is_some(),
            alive: matching.is_some(),
            pid: matching.map(|_| 42),
            port: matching.map(|t| t.port),
        })
    }

    async fn registration_matches(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<bool, PreviewExecutorError> {
        Ok(self.verify_local(key, instance).await?.identity_match)
    }

    async fn read_log_local(
        &self,
        log: &str,
        kind: &str,
        start: usize,
    ) -> Result<ExecutorLogChunk, PreviewExecutorError> {
        TestExecutor::new(false)
            .read_log_local(log, kind, start)
            .await
    }
}

fn barrier_coordinator(
    store: Arc<InProcessPreviewStore>,
    executor: Arc<StartBarrierExecutor>,
) -> Arc<PreviewCoordinator> {
    Arc::new(PreviewCoordinator::new(
        store,
        executor,
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".into(),
        CoordinatorConfig {
            start_budget_secs: 1,
            ..CoordinatorConfig::default()
        },
    ))
}

#[tokio::test]
async fn heartbeat_defers_publication_to_the_live_start_owner_until_acknowledgement() {
    for base in [None, Some(" custom-preview/// ".to_string())] {
        let store = Arc::new(InProcessPreviewStore::new());
        let executor = Arc::new(StartBarrierExecutor::default());
        let coordinator = barrier_coordinator(store.clone(), executor.clone());
        let identity = identity("heartbeat-before-ack");
        let key = compute_key(&identity);
        let start = tokio::spawn({
            let coordinator = coordinator.clone();
            let identity = identity.clone();
            let base = base.clone();
            async move {
                coordinator
                    .start_dev(PreviewStartRequest {
                        identity,
                        base_path: base,
                    })
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), executor.entered.notified())
            .await
            .unwrap();
        coordinator.run_heartbeat_sweep().await;
        let healed = store.get(&key).await.unwrap().unwrap();
        let state_before_ack = healed.state;
        let pid_before_ack = healed.pid;
        executor.acknowledge.notify_one();
        let result = start
            .await
            .unwrap()
            .expect("the original owner must publish only after its native acknowledgement");
        assert!(result.success);
        assert_eq!(pid_before_ack, None);
        assert_eq!(
            state_before_ack,
            PreviewInstanceState::Starting,
            "heartbeat must leave publication to a live exact startup owner"
        );
        let current = store.get(&key).await.unwrap().unwrap();
        assert_eq!(current.state, PreviewInstanceState::Ready);
        assert_eq!(current.instance_id, healed.instance_id);
        assert_eq!(current.pid, Some(42));
        assert_eq!(
            current.base_path,
            Some(
                base.as_deref()
                    .map(normalize_base_path)
                    .unwrap_or_else(|| default_base(result.port))
            )
        );
        assert_eq!(executor.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(executor.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(
            coordinator
                .start_dev(PreviewStartRequest {
                    identity,
                    base_path: base
                })
                .await
                .unwrap()
                .success
        );
        assert_eq!(executor.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
        coordinator.coordinated_stop(&current).await.unwrap();
    }
}

#[tokio::test]
async fn start_timeout_keeps_original_admission_for_late_heartbeat_without_replaying_start() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(StartBarrierExecutor::default());
    let coordinator = barrier_coordinator(store.clone(), executor.clone());
    let identity = identity("late-start-after-timeout");
    let key = compute_key(&identity);
    let start = tokio::spawn({
        let coordinator = coordinator.clone();
        let identity = identity.clone();
        async move {
            coordinator
                .start_dev(PreviewStartRequest {
                    identity,
                    base_path: Some("late-base".into()),
                })
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), executor.entered.notified())
        .await
        .unwrap();
    let admitted = store.get(&key).await.unwrap().unwrap();
    let error = tokio::time::timeout(Duration::from_secs(3), start)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("budget exceeded"));
    let pending = store.get(&key).await.unwrap().unwrap();
    assert_eq!(
        pending.state,
        PreviewInstanceState::Starting,
        "unknown acknowledgement is not proof of failed execution"
    );
    assert_eq!(pending.operation_id, admitted.operation_id);
    coordinator.run_heartbeat_sweep().await;
    let observed = store.get(&key).await.unwrap().unwrap();
    let state_before_ack = observed.state;
    assert_eq!(observed.instance_id, admitted.instance_id);
    assert_eq!(observed.operation_id, admitted.operation_id);
    assert_eq!(observed.base_path.as_deref(), Some("/late-base/"));
    let owner = coordinator
        .startup_owners
        .get(&key, &admitted.instance_id)
        .unwrap()
        .unwrap();
    let initial_waiters = owner.observer_count();
    let follower = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            coordinator
                .start_dev(PreviewStartRequest {
                    identity,
                    base_path: None,
                })
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.observer_count() < initial_waiters + 1 {
            // Before the fix the Ready fast path returns immediately; retain
            // that concrete result and drain the original ACK before failing.
            if follower.is_finished() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let joined_before_ack = owner.observer_count() > initial_waiters;
    executor.acknowledge.notify_one();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(3), owner.wait_result())
            .await
            .unwrap()
            .unwrap(),
        startup::StartupResult::Ready(_)
    ));
    assert!(follower.await.unwrap().unwrap().success);
    let current = store.get(&key).await.unwrap().unwrap();
    coordinator.coordinated_stop(&current).await.unwrap();
    assert_eq!(
        state_before_ack,
        PreviewInstanceState::Starting,
        "request timeout must retain publication ownership"
    );
    assert!(
        joined_before_ack,
        "the repeated caller must wait on the exact owner, not a premature Ready fast path"
    );
    assert_eq!(current.state, PreviewInstanceState::Ready);
    assert_eq!(executor.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(executor.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[tokio::test]
async fn accepted_stop_wins_over_late_start_acknowledgement_after_heartbeat() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(StartBarrierExecutor::default());
    let coordinator = barrier_coordinator(store.clone(), executor.clone());
    let identity = identity("stop-before-start-ack");
    let key = compute_key(&identity);
    let start = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            coordinator
                .start_dev(PreviewStartRequest {
                    identity,
                    base_path: None,
                })
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), executor.entered.notified())
        .await
        .unwrap();
    coordinator.run_heartbeat_sweep().await;
    let pending = store.get(&key).await.unwrap().unwrap();
    let state_before_stop = pending.state;
    let stop = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { coordinator.coordinated_stop(&pending).await }
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if store.get(&key).await.unwrap().unwrap().state == PreviewInstanceState::Stopping {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if executor.stops.load(std::sync::atomic::Ordering::SeqCst) == 1 { break; }
            tokio::task::yield_now().await;
        }
    }).await.expect("an already registered process must be stopped before waiting for the late start acknowledgement");
    assert!(
        !stop.is_finished(),
        "Stop cannot acknowledge while the original start can still complete"
    );
    executor.acknowledge.notify_one();
    stop.await.unwrap().unwrap();
    let stopped = store.get(&key).await.unwrap().unwrap();
    assert_eq!(stopped.state, PreviewInstanceState::Stopped);
    assert!(
        start.await.unwrap().is_err(),
        "late acknowledgement must not undo an accepted Stop"
    );
    assert_eq!(store.get(&key).await.unwrap().unwrap(), stopped);
    assert_eq!(executor.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(executor.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(state_before_stop, PreviewInstanceState::Starting);
}

/// This fixture blocks before any native registration exists. A timed-out
/// caller cannot make the remaining preparation/start/publish sequence run.
#[derive(Default)]
struct PreparationBarrierExecutor {
    // 0: success; 1: confirmed source rejection; 2: unknown cleanup; 3: port in use.
    failure: std::sync::atomic::AtomicUsize,
    port_conflicts: std::sync::atomic::AtomicUsize,
    start_tickets: std::sync::Mutex<Vec<ExecutorStartTicket>>,
    deny_stop: std::sync::atomic::AtomicBool,
    stop_inputs: std::sync::Mutex<Vec<(String, String)>>,
    registered: tokio::sync::Mutex<Option<ExecutorStartTicket>>,
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
    starts: std::sync::atomic::AtomicUsize,
    stops: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl PreviewExecutor for PreparationBarrierExecutor {
    async fn start_local(
        &self,
        ticket: &ExecutorStartTicket,
    ) -> Result<(i64, u16), PreviewExecutorError> {
        self.starts
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.start_tickets.lock().unwrap().push(ticket.clone());
        self.entered.notify_one();
        self.release.notified().await;
        if self
            .port_conflicts
            .try_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |count| count.checked_sub(1),
            )
            .is_ok()
        {
            return Err(PreviewExecutorError::PortInUse(
                "controlled repeated strict-port conflict".into(),
            ));
        }
        match self.failure.swap(0, std::sync::atomic::Ordering::SeqCst) {
            0 => {}
            1 => {
                return Err(PreviewExecutorError::Business {
                    code: "TYPECHECK_FAILED".into(),
                    message: "confirmed preparation source failure".into(),
                });
            }
            2 => {
                return Err(PreviewExecutorError::Failed(
                    "cleanup result unknown".into(),
                ));
            }
            3 => {
                return Err(PreviewExecutorError::PortInUse(
                    "controlled strict-port conflict".into(),
                ));
            }
            value => panic!("unknown fixture outcome {value}"),
        }
        *self.registered.lock().await = Some(ticket.clone());
        Ok((42, ticket.port))
    }

    async fn stop_local(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<ExecutorStopOutcome, PreviewExecutorError> {
        self.stop_inputs
            .lock()
            .unwrap()
            .push((key.into(), instance.into()));
        if self.deny_stop.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(PreviewExecutorError::Failed(
                "original cleanup currently unavailable".into(),
            ));
        }
        let mut registered = self.registered.lock().await;
        match registered.as_ref() {
            Some(ticket) if ticket.preview_key == key && ticket.instance_id == instance => {
                self.stops.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                *registered = None;
                Ok(ExecutorStopOutcome::Stopped)
            }
            Some(_) => Ok(ExecutorStopOutcome::IdentityMismatch),
            None => Ok(ExecutorStopOutcome::NotRegistered),
        }
    }

    async fn verify_local(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<ExecutorVerifyReport, PreviewExecutorError> {
        let registered = self.registered.lock().await;
        let matching = registered
            .as_ref()
            .filter(|ticket| ticket.preview_key == key && ticket.instance_id == instance);
        Ok(ExecutorVerifyReport {
            identity_match: matching.is_some(),
            alive: matching.is_some(),
            pid: matching.map(|_| 42),
            port: matching.map(|ticket| ticket.port),
        })
    }

    async fn registration_matches(
        &self,
        key: &str,
        instance: &str,
    ) -> Result<bool, PreviewExecutorError> {
        Ok(self.verify_local(key, instance).await?.identity_match)
    }

    async fn read_log_local(
        &self,
        log: &str,
        kind: &str,
        start: usize,
    ) -> Result<ExecutorLogChunk, PreviewExecutorError> {
        TestExecutor::new(false)
            .read_log_local(log, kind, start)
            .await
    }
}

#[tokio::test]
async fn pre_registration_timeout_keeps_the_original_start_running_until_ready() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = Arc::new(PreviewCoordinator::new(
        store.clone(),
        executor.clone(),
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".into(),
        CoordinatorConfig {
            start_budget_secs: 1,
            ..CoordinatorConfig::default()
        },
    ));
    let identity = identity("pre-registration-timeout");
    let key = compute_key(&identity);
    let start = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            coordinator
                .start_dev(PreviewStartRequest {
                    identity,
                    base_path: Some("prepared-base".into()),
                })
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(3), executor.entered.notified())
        .await
        .unwrap();
    let admitted = store.get(&key).await.unwrap().unwrap();
    assert_eq!(admitted.state, PreviewInstanceState::Starting);
    assert!(
        executor.registered.lock().await.is_none(),
        "fixture must still be before registration"
    );
    let error = tokio::time::timeout(Duration::from_secs(3), start)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(error.to_string().contains("budget exceeded"));
    executor.release.notify_one();
    let ready = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let current = store.get(&key).await.unwrap().unwrap();
            if current.state == PreviewInstanceState::Ready {
                break current;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("accepted preparation must continue to publish Ready after the caller times out");
    assert_eq!(ready.instance_id, admitted.instance_id);
    assert_eq!(ready.operation_id, admitted.operation_id);
    assert_eq!(ready.base_path.as_deref(), Some("/prepared-base/"));
    assert_eq!(executor.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(executor.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[path = "startup_tests.rs"]
mod startup_owner_tests;
