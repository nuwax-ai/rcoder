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
        *self.ticket.lock().await = Some(ticket.clone());
        self.entered.notify_one();
        self.acknowledge.notified().await;
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
async fn heartbeat_publication_before_start_acknowledgement_does_not_kill_the_same_process() {
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
        assert_eq!(healed.state, PreviewInstanceState::Ready);
        executor.acknowledge.notify_one();
        let result = start
            .await
            .unwrap()
            .expect("same execution already published by heartbeat must succeed");
        assert!(result.success);
        let current = store.get(&key).await.unwrap().unwrap();
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
    let healed = store.get(&key).await.unwrap().unwrap();
    assert_eq!(healed.state, PreviewInstanceState::Ready);
    assert_eq!(healed.instance_id, admitted.instance_id);
    assert_eq!(healed.operation_id, admitted.operation_id);
    assert_eq!(healed.base_path.as_deref(), Some("/late-base/"));
    assert!(
        coordinator
            .start_dev(PreviewStartRequest {
                identity,
                base_path: None
            })
            .await
            .unwrap()
            .success
    );
    assert_eq!(executor.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(executor.stops.load(std::sync::atomic::Ordering::SeqCst), 0);
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
    let ready = store.get(&key).await.unwrap().unwrap();
    coordinator.coordinated_stop(&ready).await.unwrap();
    let stopped = store.get(&key).await.unwrap().unwrap();
    assert_eq!(stopped.state, PreviewInstanceState::Stopped);
    executor.acknowledge.notify_one();
    assert!(
        start.await.unwrap().is_err(),
        "late acknowledgement must not undo an accepted Stop"
    );
    assert_eq!(store.get(&key).await.unwrap().unwrap(), stopped);
    assert_eq!(executor.starts.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(executor.stops.load(std::sync::atomic::Ordering::SeqCst), 1);
}
