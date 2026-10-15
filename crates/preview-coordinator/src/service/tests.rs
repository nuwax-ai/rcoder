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
