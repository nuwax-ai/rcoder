//! Deterministic ownership tests: admission, preparation and durable completion
//! each have their own barrier. Protocol substitutes never stand in for real AI.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

type StoreResult<T> = Result<T, PreviewStoreError>;
#[derive(Default)]
struct StartupStore {
    inner: InProcessPreviewStore,
    hold_admission_reply: AtomicBool,
    hold_before_admission: AtomicBool,
    deny_admission: AtomicBool,
    hold_retry_reply: AtomicBool,
    retry_committed: tokio::sync::Notify,
    retry_reply_release: tokio::sync::Notify,
    lost_retry_reply: AtomicUsize,
    retries: std::sync::Mutex<Vec<shared_types::PreviewStartPortRetry>>,
    admission_entered: tokio::sync::Notify,
    admission_release: tokio::sync::Notify,
    lost_admission_reply: AtomicUsize,
    deny_publication: AtomicBool,
    deny_failure: AtomicBool,
    retire_on_failure: AtomicBool,
    retired: AtomicBool,
    admissions: std::sync::Mutex<Vec<(String, String)>>,
    publications: std::sync::Mutex<Vec<(String, i64, i64, u16)>>,
    failures: std::sync::Mutex<Vec<(String, i64)>>,
}

#[async_trait::async_trait]
impl PreviewLifecycleStore for StartupStore {
    async fn accept_start(&self, input: AcceptStartInput) -> StoreResult<AcceptStartOutcome> {
        self.admissions
            .lock()
            .unwrap()
            .push((input.operation_id.clone(), input.instance_id.clone()));
        if self.deny_admission.load(Ordering::SeqCst) {
            self.admission_entered.notify_one();
            return Err(PreviewStoreError::Unavailable(
                "admission service unavailable".into(),
            ));
        }
        if self.hold_before_admission.swap(false, Ordering::SeqCst) {
            self.admission_entered.notify_one();
            self.admission_release.notified().await;
        }
        let result = self.inner.accept_start(input).await;
        if self.hold_admission_reply.swap(false, Ordering::SeqCst) {
            // This models PG's owner: the mutation committed before the caller
            // receives its acknowledgement; dropping the caller cannot undo it.
            self.admission_entered.notify_one();
            self.admission_release.notified().await;
        }
        if self
            .lost_admission_reply
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            return Err(PreviewStoreError::Unavailable(
                "committed admission reply lost".into(),
            ));
        }
        result
    }
    async fn retry_start_port(
        &self,
        input: shared_types::PreviewStartPortRetry,
    ) -> StoreResult<PreviewInstanceRecord> {
        self.retries.lock().unwrap().push(input.clone());
        let result = self.inner.retry_start_port(input).await;
        if result.is_ok() && self.hold_retry_reply.swap(false, Ordering::SeqCst) {
            // The original row is still Starting; its exact port CAS committed
            // before acknowledgement. Stop must fence the entire logical start.
            self.retry_committed.notify_one();
            self.retry_reply_release.notified().await;
        }
        if result.is_ok()
            && self
                .lost_retry_reply
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
        {
            return Err(PreviewStoreError::Unavailable(
                "committed port CAS reply lost".into(),
            ));
        }
        result
    }
    async fn publish_running(
        &self,
        key: &str,
        operation: &str,
        revision: i64,
        pid: i64,
        port: u16,
        base: Option<&str>,
    ) -> StoreResult<PreviewInstanceRecord> {
        self.publications
            .lock()
            .unwrap()
            .push((operation.into(), revision, pid, port));
        if self.deny_publication.load(Ordering::SeqCst) {
            return Err(PreviewStoreError::Unavailable(
                "publication storage unavailable".into(),
            ));
        }
        self.inner
            .publish_running(key, operation, revision, pid, port, base)
            .await
    }
    async fn accept_stop(&self, key: &str, operation: &str) -> StoreResult<PreviewInstanceRecord> {
        self.inner.accept_stop(key, operation).await
    }
    async fn mark_stopped(
        &self,
        key: &str,
        operation: &str,
        revision: i64,
    ) -> StoreResult<PreviewInstanceRecord> {
        self.inner.mark_stopped(key, operation, revision).await
    }
    async fn mark_failed(
        &self,
        key: &str,
        instance: &str,
        revision: i64,
        detail: &str,
    ) -> StoreResult<PreviewInstanceRecord> {
        self.failures
            .lock()
            .unwrap()
            .push((instance.into(), revision));
        if self.retire_on_failure.load(Ordering::SeqCst) {
            self.retired.store(true, Ordering::SeqCst);
            return Err(PreviewStoreError::Invalid(
                "authority row already retired".into(),
            ));
        }
        if self.deny_failure.load(Ordering::SeqCst) {
            return Err(PreviewStoreError::Unavailable(
                "failure terminal storage unavailable".into(),
            ));
        }
        self.inner
            .mark_failed(key, instance, revision, detail)
            .await
    }
    async fn mark_unknown(
        &self,
        key: &str,
        instance: &str,
        detail: &str,
    ) -> StoreResult<PreviewInstanceRecord> {
        self.inner.mark_unknown(key, instance, detail).await
    }
    async fn resolve_unknown_stopped(
        &self,
        key: &str,
        instance: &str,
        evidence: &str,
    ) -> StoreResult<PreviewInstanceRecord> {
        self.inner
            .resolve_unknown_stopped(key, instance, evidence)
            .await
    }
    async fn refresh_heartbeat(
        &self,
        key: &str,
        instance: &str,
    ) -> StoreResult<Option<PreviewInstanceRecord>> {
        self.inner.refresh_heartbeat(key, instance).await
    }
    async fn flush_activity(
        &self,
        entries: &[shared_types::ActivityFlushEntry],
    ) -> StoreResult<usize> {
        self.inner.flush_activity(entries).await
    }
    async fn get(&self, key: &str) -> StoreResult<Option<PreviewInstanceRecord>> {
        if self.retired.load(Ordering::SeqCst) {
            return Ok(None);
        }
        self.inner.get(key).await
    }
    async fn find_active_by_port(&self, port: u16) -> StoreResult<Option<PreviewInstanceRecord>> {
        self.inner.find_active_by_port(port).await
    }
    async fn active_ports(&self) -> StoreResult<Vec<u16>> {
        self.inner.active_ports().await
    }
    async fn list_by_host(
        &self,
        host: &str,
        active: bool,
    ) -> StoreResult<Vec<PreviewInstanceRecord>> {
        self.inner.list_by_host(host, active).await
    }
    async fn list_active(&self) -> StoreResult<Vec<PreviewInstanceRecord>> {
        self.inner.list_active().await
    }
    async fn reconcile_host_reboot(
        &self,
        pod: &str,
        boot: &str,
    ) -> StoreResult<Vec<PreviewInstanceRecord>> {
        self.inner.reconcile_host_reboot(pod, boot).await
    }
}

fn owned_coordinator(
    store: Arc<dyn PreviewLifecycleStore>,
    executor: Arc<PreparationBarrierExecutor>,
) -> Arc<PreviewCoordinator> {
    Arc::new(PreviewCoordinator::new(
        store,
        executor,
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".into(),
        CoordinatorConfig {
            start_budget_secs: 1,
            remote_dispatch_timeout_secs: 1,
            ..CoordinatorConfig::default()
        },
    ))
}
async fn wait_state(
    store: &dyn PreviewLifecycleStore,
    key: &str,
    state: PreviewInstanceState,
) -> PreviewInstanceRecord {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(row) = store.get(key).await.unwrap()
                && row.state == state
            {
                break row;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("owned startup did not reach its expected durable state")
}
async fn entered(executor: &PreparationBarrierExecutor) {
    tokio::time::timeout(Duration::from_secs(3), executor.entered.notified())
        .await
        .unwrap();
}
fn start_request(
    coordinator: &Arc<PreviewCoordinator>,
    project: &str,
) -> tokio::task::JoinHandle<Result<PreviewStartEnvelope, PreviewCoordinationError>> {
    let coordinator = coordinator.clone();
    let identity = identity(project);
    tokio::spawn(async move {
        coordinator
            .start_dev(PreviewStartRequest {
                identity,
                base_path: None,
            })
            .await
    })
}

#[tokio::test]
async fn caller_cancel_during_preparation_keeps_exact_owner_until_ready() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("cancel-preparation"));
    let request = start_request(&coordinator, "cancel-preparation");
    entered(&executor).await;
    let admitted = store.get(&key).await.unwrap().unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    assert!(executor.registered.lock().await.is_none());
    executor.release.notify_one();
    let ready = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    assert_eq!(ready.operation_id, admitted.operation_id);
    assert_eq!(ready.instance_id, admitted.instance_id);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn caller_cancel_during_committed_admission_keeps_original_start_owner() {
    let store = Arc::new(StartupStore::default());
    store.hold_admission_reply.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("cancel-admission"));
    let request = start_request(&coordinator, "cancel-admission");
    tokio::time::timeout(Duration::from_secs(3), store.admission_entered.notified())
        .await
        .unwrap();
    let admitted = store.get(&key).await.unwrap().unwrap();
    request.abort();
    assert!(request.await.unwrap_err().is_cancelled());
    store.admission_release.notify_one();
    entered(&executor).await;
    executor.release.notify_one();
    let ready = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    assert_eq!(ready.operation_id, admitted.operation_id);
    assert_eq!(ready.instance_id, admitted.instance_id);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    assert_eq!(store.admissions.lock().unwrap().len(), 1);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn lost_admission_reply_reconciles_same_operation_without_replaying_native_start() {
    let store = Arc::new(StartupStore::default());
    store.lost_admission_reply.store(1, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let request = start_request(&coordinator, "lost-admission");
    entered(&executor).await;
    executor.release.notify_one();
    assert!(request.await.unwrap().unwrap().success);
    let admissions = store.admissions.lock().unwrap().clone();
    assert_eq!(admissions.len(), 2);
    assert_eq!(
        admissions[0], admissions[1],
        "retry must retain the original operation and instance"
    );
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    let key = compute_key(&identity("lost-admission"));
    let ready = store.get(&key).await.unwrap().unwrap();
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn confirmed_preparation_failure_retries_original_terminal_write_then_allows_entry_retry() {
    let store = Arc::new(StartupStore::default());
    store.deny_failure.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.failure.store(1, Ordering::SeqCst);
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("failed-preparation"));
    let request = start_request(&coordinator, "failed-preparation");
    entered(&executor).await;
    let admitted = store.get(&key).await.unwrap().unwrap();
    executor.release.notify_one();
    assert!(
        request
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("budget exceeded")
    );
    assert_eq!(
        store.get(&key).await.unwrap().unwrap().state,
        PreviewInstanceState::Starting
    );
    assert!(!store.failures.lock().unwrap().is_empty());
    store.deny_failure.store(false, Ordering::SeqCst);
    let failed = wait_state(store.as_ref(), &key, PreviewInstanceState::Failed).await;
    assert_eq!(failed.operation_id, admitted.operation_id);
    assert_eq!(failed.instance_id, admitted.instance_id);
    assert!(
        store
            .failures
            .lock()
            .unwrap()
            .iter()
            .all(|(instance, revision)| instance == &admitted.instance_id
                && *revision == admitted.revision)
    );
    let retry = start_request(&coordinator, "failed-preparation");
    entered(&executor).await;
    executor.release.notify_one();
    assert!(retry.await.unwrap().unwrap().success);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    let ready = store.get(&key).await.unwrap().unwrap();
    assert_ne!(ready.instance_id, failed.instance_id);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn publication_unavailable_after_request_timeout_retries_only_original_publication() {
    let store = Arc::new(StartupStore::default());
    store.deny_publication.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("publication-unavailable"));
    let request = start_request(&coordinator, "publication-unavailable");
    entered(&executor).await;
    let admitted = store.get(&key).await.unwrap().unwrap();
    executor.release.notify_one();
    assert!(
        request
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("budget exceeded")
    );
    assert_eq!(
        store.get(&key).await.unwrap().unwrap().state,
        PreviewInstanceState::Starting
    );
    store.deny_publication.store(false, Ordering::SeqCst);
    let ready = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    assert_eq!(ready.operation_id, admitted.operation_id);
    assert_eq!(ready.instance_id, admitted.instance_id);
    let calls = store.publications.lock().unwrap().clone();
    assert!(calls.len() > 1);
    assert!(
        calls.iter().all(
            |(operation, revision, pid, port)| operation == &admitted.operation_id
                && *revision == admitted.revision
                && *pid == 42
                && Some(*port) == admitted.port
        )
    );
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn unknown_preparation_failure_reconciles_current_cleanup_then_original_entry_retries() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.failure.store(2, Ordering::SeqCst);
    executor.deny_stop.store(true, Ordering::SeqCst);
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("unknown-preparation"));
    let request = start_request(&coordinator, "unknown-preparation");
    entered(&executor).await;
    executor.release.notify_one();
    assert!(
        request
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("cleanup result unknown")
    );
    let unknown = store.get(&key).await.unwrap().unwrap();
    assert_eq!(
        unknown.state,
        PreviewInstanceState::Starting,
        "unknown cleanup cannot fabricate a failed terminal or overwrite another operation"
    );
    assert!(
        coordinator
            .start_dev(PreviewStartRequest {
                identity: identity("unknown-preparation"),
                base_path: None
            })
            .await
            .is_err()
    );
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    executor.deny_stop.store(false, Ordering::SeqCst);
    let retired = wait_state(store.as_ref(), &key, PreviewInstanceState::Failed).await;
    assert_eq!(retired.instance_id, unknown.instance_id);
    assert_eq!(retired.operation_id, unknown.operation_id);
    let retry = start_request(&coordinator, "unknown-preparation");
    entered(&executor).await;
    executor.release.notify_one();
    assert!(retry.await.unwrap().unwrap().success);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    let ready = store.get(&key).await.unwrap().unwrap();
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn stop_during_preparation_waits_for_the_original_future_and_cannot_revive_late_start() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("stop-preparation"));
    let request = start_request(&coordinator, "stop-preparation");
    entered(&executor).await;
    let admitted = store.get(&key).await.unwrap().unwrap();
    let stop = tokio::spawn({
        let coordinator = coordinator.clone();
        async move { coordinator.coordinated_stop(&admitted).await }
    });
    wait_state(store.as_ref(), &key, PreviewInstanceState::Stopping).await;
    assert!(
        !stop.is_finished(),
        "Stop must not acknowledge while preparation can still spawn"
    );
    assert!(executor.registered.lock().await.is_none());
    assert_eq!(executor.stops.load(Ordering::SeqCst), 0);
    executor.release.notify_one();
    stop.await.unwrap().unwrap();
    assert!(request.await.unwrap().is_err());
    let stopped = wait_state(store.as_ref(), &key, PreviewInstanceState::Stopped).await;
    coordinator.run_heartbeat_sweep().await;
    assert_eq!(store.get(&key).await.unwrap().unwrap(), stopped);
    assert!(executor.registered.lock().await.is_none());
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    assert_eq!(executor.stops.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stop_timeout_during_preparation_is_not_success_and_owner_finishes_exact_cleanup() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("stop-preparation-timeout"));
    let request = start_request(&coordinator, "stop-preparation-timeout");
    entered(&executor).await;
    let admitted = store.get(&key).await.unwrap().unwrap();
    let error = tokio::time::timeout(
        Duration::from_secs(3),
        coordinator.coordinated_stop(&admitted),
    )
    .await
    .unwrap()
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("waiting for the original startup")
    );
    assert_ne!(
        store.get(&key).await.unwrap().unwrap().state,
        PreviewInstanceState::Stopped
    );
    assert!(executor.registered.lock().await.is_none());
    executor.release.notify_one();
    let stopped = wait_state(store.as_ref(), &key, PreviewInstanceState::Stopped).await;
    assert!(request.await.unwrap().is_err());
    assert!(executor.registered.lock().await.is_none());
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    assert_eq!(executor.stops.load(Ordering::SeqCst), 1);
    assert_eq!(stopped.instance_id, admitted.instance_id);
}

#[tokio::test]
async fn stop_during_admission_reply_wait_prevents_native_start_after_acceptance() {
    let store = Arc::new(StartupStore::default());
    store.hold_admission_reply.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("stop-admission"));
    let request = start_request(&coordinator, "stop-admission");
    tokio::time::timeout(Duration::from_secs(3), store.admission_entered.notified())
        .await
        .unwrap();
    let stop = tokio::spawn({
        let coordinator = coordinator.clone();
        async move {
            coordinator
                .stop_dev(&PreviewStopRequest {
                    project_id: "stop-admission".into(),
                    pid: None,
                })
                .await
        }
    });
    let admitted = store.get(&key).await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if coordinator
                .startup_owners
                .get(&key, &admitted.instance_id)
                .unwrap()
                .is_some_and(|owner| owner.cancelled())
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        !stop.is_finished(),
        "pending admission must retain its owner until it can no longer spawn"
    );
    store.admission_release.notify_one();
    assert!(stop.await.unwrap().unwrap().success);
    assert!(request.await.unwrap().is_err());
    assert_eq!(executor.starts.load(Ordering::SeqCst), 0);
    let row = store.get(&key).await.unwrap().unwrap();
    assert!(!row.state.is_active());
    assert!(executor.registered.lock().await.is_none());
}

#[tokio::test]
async fn preparation_rejection_after_caller_timeout_retires_original_operation_and_allows_retry() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.failure.store(1, Ordering::SeqCst);
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("late-preparation-rejection"));
    let request = start_request(&coordinator, "late-preparation-rejection");
    entered(&executor).await;
    let admitted = store.get(&key).await.unwrap().unwrap();
    assert!(
        request
            .await
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("budget exceeded")
    );
    assert!(executor.registered.lock().await.is_none());
    executor.release.notify_one();
    let failed = wait_state(store.as_ref(), &key, PreviewInstanceState::Failed).await;
    assert_eq!(failed.operation_id, admitted.operation_id);
    assert_eq!(failed.instance_id, admitted.instance_id);
    let retry = start_request(&coordinator, "late-preparation-rejection");
    entered(&executor).await;
    executor.release.notify_one();
    assert!(retry.await.unwrap().unwrap().success);
    let ready = store.get(&key).await.unwrap().unwrap();
    assert_ne!(ready.instance_id, failed.instance_id);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn confirmed_rejection_with_retired_authority_row_returns_original_error_and_retires_owner() {
    let store = Arc::new(StartupStore::default());
    store.retire_on_failure.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.failure.store(1, Ordering::SeqCst);
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let key = compute_key(&identity("retired-failure-authority"));
    let request = start_request(&coordinator, "retired-failure-authority");
    entered(&executor).await;
    let admitted = store.get(&key).await.unwrap().unwrap();
    executor.release.notify_one();
    let error = tokio::time::timeout(Duration::from_secs(3), request)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, PreviewCoordinationError::Business { code, .. } if code == "TYPECHECK_FAILED"),
        "retired history must not turn confirmed rejection into a request timeout"
    );
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if coordinator
                .startup_owners
                .get(&key, &admitted.instance_id)
                .unwrap()
                .is_none()
            {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("confirmed no-execution owner must retire when the authority row is gone");
    assert!(store.get(&key).await.unwrap().is_none());
    assert!(executor.registered.lock().await.is_none());
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn unadmitted_existing_ready_or_blocked_attempt_cannot_stop_a_phantom_instance() {
    for ready in [false, true] {
        let project = if ready {
            "unadmitted-ready"
        } else {
            "unadmitted-blocked"
        };
        let store = Arc::new(StartupStore::default());
        store.hold_before_admission.store(true, Ordering::SeqCst);
        let executor = Arc::new(PreparationBarrierExecutor::default());
        let coordinator = owned_coordinator(store.clone(), executor.clone());
        let key = compute_key(&identity(project));
        let pending = start_request(&coordinator, project);
        tokio::time::timeout(Duration::from_secs(3), store.admission_entered.notified())
            .await
            .unwrap();
        let phantom = store.admissions.lock().unwrap()[0].1.clone();
        let AcceptStartOutcome::Admitted(actual) = store
            .inner
            .accept_start(AcceptStartInput {
                preview_key: key.clone(),
                project_id: project.into(),
                project_path: identity(project).resolved_path,
                host: coordinator.host().clone(),
                operation_id: format!("{project}-real-operation"),
                instance_id: format!("{project}-real-instance"),
                requested_port: None,
                base_path: None,
                recover_unknown_evidence: None,
            })
            .await
            .unwrap()
        else {
            panic!("real predecessor must be admitted");
        };
        if ready {
            store
                .inner
                .publish_running(
                    &key,
                    &actual.operation_id,
                    actual.revision,
                    42,
                    actual.port.unwrap(),
                    None,
                )
                .await
                .unwrap();
        }
        *executor.registered.lock().await = Some(ExecutorStartTicket {
            preview_key: key.clone(),
            log_key: project.into(),
            project_path: identity(project).resolved_path,
            port: actual.port.unwrap(),
            base_path: None,
            instance_id: actual.instance_id.clone(),
        });
        let stop = tokio::spawn({
            let coordinator = coordinator.clone();
            let project = project.to_owned();
            async move {
                coordinator
                    .stop_dev(&PreviewStopRequest {
                        project_id: project,
                        pid: None,
                    })
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if coordinator
                    .startup_owners
                    .get(&key, &phantom)
                    .unwrap()
                    .is_some_and(|owner| owner.cancelled())
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(!stop.is_finished());
        store.admission_release.notify_one();
        assert!(stop.await.unwrap().unwrap().success);
        let request_result = pending.await.unwrap();
        if ready {
            assert!(
                request_result.unwrap().success,
                "the losing admission must observe the existing Ready instance"
            );
        } else {
            assert!(
                matches!(request_result, Err(PreviewCoordinationError::Conflict(_))),
                "the losing admission must observe the existing Starting instance as blocked"
            );
        }
        let current = store.get(&key).await.unwrap().unwrap();
        assert_eq!(current.state, PreviewInstanceState::Stopped);
        assert_eq!(current.instance_id, actual.instance_id);
        assert!(executor.registered.lock().await.is_none());
        assert_eq!(
            executor.starts.load(Ordering::SeqCst),
            0,
            "the losing attempt must never execute native start"
        );
        assert_eq!(executor.stops.load(Ordering::SeqCst), 1);
        let calls = executor.stop_inputs.lock().unwrap();
        assert!(!calls.is_empty());
        assert!(calls.iter().all(|(called_key, instance)| called_key == &key && instance == &actual.instance_id), "pre-generated but unadmitted instance must never be a physical Stop target: {calls:?}");
    }
}

/// The first port-conflict port CAS has committed, but its reply is held.
/// A successful Stop in this interval must fence the ENTIRE port-retry chain.
#[tokio::test]
async fn stop_after_committed_port_retry_ack_cannot_launch_another_native_attempt() {
    let store = Arc::new(StartupStore::default());
    store.hold_retry_reply.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.failure.store(3, Ordering::SeqCst);
    let coordinator = Arc::new(PreviewCoordinator::new(
        store.clone(),
        executor.clone(),
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".into(),
        CoordinatorConfig {
            start_budget_secs: 10,
            remote_dispatch_timeout_secs: 1,
            ..CoordinatorConfig::default()
        },
    ));
    let project = "stop-port-retry-chain";
    let key = compute_key(&identity(project));
    let request = start_request(&coordinator, project);
    entered(&executor).await;
    executor.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), store.retry_committed.notified())
        .await
        .unwrap();
    assert!(
        !request.is_finished(),
        "the first attempt reply must still be held when Stop runs"
    );
    let retrying = store.get(&key).await.unwrap().unwrap();
    assert_eq!(
        retrying.state,
        PreviewInstanceState::Starting,
        "the complete retry sequence must stay visible to a cross-replica Stop"
    );
    let admission = store.admissions.lock().unwrap()[0].clone();
    assert_eq!(
        (retrying.operation_id.clone(), retrying.instance_id.clone()),
        admission
    );
    assert!(
        store.failures.lock().unwrap().is_empty(),
        "no intermediate Failed can hide the remaining physical retry"
    );
    let stop = coordinator
        .stop_dev(&PreviewStopRequest {
            project_id: project.into(),
            pid: None,
        })
        .await
        .unwrap();
    assert!(stop.success);
    assert_eq!(stop.message, "Stopped");
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    // Release the old acknowledgement only after Stop has returned success.
    // A permit lets an erroneous second native start complete, making the
    // before-fix failure a concrete extra start rather than a hanging fixture.
    executor.release.notify_one();
    store.retry_reply_release.notify_one();
    let result = tokio::time::timeout(Duration::from_secs(3), request)
        .await
        .unwrap()
        .unwrap();
    let native_starts = executor.starts.load(Ordering::SeqCst);
    let current = store.get(&key).await.unwrap().unwrap();
    if current.state.is_active() {
        coordinator.coordinated_stop(&current).await.unwrap();
    }
    assert_eq!(
        native_starts, 1,
        "a successful Stop must prevent a second native start after a delayed PortInUse acknowledgement"
    );
    assert!(
        result.is_err(),
        "the stopped logical start cannot report a later retry as successful"
    );
    assert!(
        !current.state.is_active(),
        "the stopped attempt must remain terminal after the delayed reply"
    );
}

/// With admission unavailable, repeated callers for the same key must wait on
/// one original owner/operation instead of accumulating independent futures.
#[tokio::test]
async fn repeated_request_timeouts_share_one_owner_and_original_admission_operation() {
    let store = Arc::new(StartupStore::default());
    store.deny_admission.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = owned_coordinator(store.clone(), executor.clone());
    let project = "admission-timeout-coalescing";
    let key = compute_key(&identity(project));
    for _ in 0..3 {
        let request = start_request(&coordinator, project);
        let error = tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(error.to_string().contains("budget exceeded"));
    }
    let operations: std::collections::BTreeSet<_> =
        store.admissions.lock().unwrap().iter().cloned().collect();
    let original = store.admissions.lock().unwrap()[0].clone();
    let observed_owners = operations
        .iter()
        .filter(|(_, instance)| {
            coordinator
                .startup_owners
                .get(&key, instance)
                .unwrap()
                .is_some()
        })
        .count();
    store.deny_admission.store(false, Ordering::SeqCst);
    executor.release.notify_one();
    let ready = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    let native_starts = executor.starts.load(Ordering::SeqCst);
    coordinator.coordinated_stop(&ready).await.unwrap();
    assert_eq!(
        observed_owners, 1,
        "timed-out callers must not accumulate independent owners for one key"
    );
    assert_eq!(
        operations.len(),
        1,
        "every unavailable admission retry must retain the original operation and instance"
    );
    assert_eq!((ready.operation_id, ready.instance_id), original);
    assert_eq!(native_starts, 1);
}

fn retry_coordinator(
    store: Arc<dyn PreviewLifecycleStore>,
    executor: Arc<PreparationBarrierExecutor>,
) -> Arc<PreviewCoordinator> {
    Arc::new(PreviewCoordinator::new(
        store,
        executor,
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".into(),
        CoordinatorConfig {
            start_budget_secs: 10,
            remote_dispatch_timeout_secs: 1,
            ..CoordinatorConfig::default()
        },
    ))
}

#[tokio::test]
async fn bounded_port_retries_keep_one_starting_operation_and_skip_occupied_candidates() {
    for conflicts in [2, 3] {
        let store = Arc::new(StartupStore::default());
        let executor = Arc::new(PreparationBarrierExecutor::default());
        executor.port_conflicts.store(conflicts, Ordering::SeqCst);
        let coordinator = retry_coordinator(store.clone(), executor.clone());
        let occupied = identity(&format!("occupied-port-{conflicts}"));
        let AcceptStartOutcome::Admitted(occupied) = store
            .inner
            .accept_start(AcceptStartInput {
                preview_key: compute_key(&occupied),
                project_id: occupied.project_id,
                project_path: occupied.resolved_path,
                host: coordinator.host.clone(),
                operation_id: new_id(),
                instance_id: new_id(),
                requested_port: Some(PREVIEW_PORT_MIN + 1),
                base_path: None,
                recover_unknown_evidence: None,
            })
            .await
            .unwrap()
        else {
            panic!("occupied candidate must be admitted");
        };
        let project = format!("bounded-port-sequence-{conflicts}");
        let key = compute_key(&identity(&project));
        let request = start_request(&coordinator, &project);
        let mut original = None;
        for attempt in 0..3 {
            entered(&executor).await;
            let current = store.get(&key).await.unwrap().unwrap();
            assert_eq!(current.state, PreviewInstanceState::Starting);
            let initial = original.get_or_insert_with(|| current.clone());
            assert_eq!(current.operation_id, initial.operation_id);
            assert_eq!(current.instance_id, initial.instance_id);
            assert_eq!(current.revision, initial.revision + attempt);
            assert!(
                store.failures.lock().unwrap().is_empty(),
                "no intermediate failure may retire the logical start"
            );
            executor.release.notify_one();
        }
        let result = tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .unwrap()
            .unwrap();
        let initial = original.unwrap();
        let current = store.get(&key).await.unwrap().unwrap();
        assert_eq!(
            executor.starts.load(Ordering::SeqCst),
            3,
            "at most three physical starts"
        );
        assert_eq!(store.admissions.lock().unwrap().len(), 1);
        assert_eq!(store.retries.lock().unwrap().len(), 2);
        let tickets = executor.start_tickets.lock().unwrap().clone();
        assert!(
            tickets
                .iter()
                .all(|ticket| ticket.instance_id == initial.instance_id)
        );
        assert_eq!(
            tickets.iter().map(|ticket| ticket.port).collect::<Vec<_>>(),
            vec![PREVIEW_PORT_MIN, PREVIEW_PORT_MIN + 2, PREVIEW_PORT_MIN + 3]
        );
        assert_eq!(current.revision, initial.revision + 2);
        assert_eq!(current.operation_id, initial.operation_id);
        assert_eq!(current.instance_id, initial.instance_id);
        if conflicts == 2 {
            assert!(result.unwrap().success);
            assert_eq!(current.state, PreviewInstanceState::Ready);
            assert!(store.failures.lock().unwrap().is_empty());
            coordinator.coordinated_stop(&current).await.unwrap();
        } else {
            assert!(
                result
                    .unwrap_err()
                    .to_string()
                    .contains("exhausted port retry budget")
            );
            assert_eq!(current.state, PreviewInstanceState::Failed);
            assert_eq!(store.failures.lock().unwrap().len(), 1);
            assert!(executor.registered.lock().await.is_none());
        }
        store
            .inner
            .mark_failed(
                &occupied.preview_key,
                &occupied.instance_id,
                occupied.revision,
                "fixture has no native execution",
            )
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn lost_port_cas_ack_reconciles_original_input_without_replaying_native() {
    let store = Arc::new(StartupStore::default());
    store.lost_retry_reply.store(1, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.port_conflicts.store(1, Ordering::SeqCst);
    let coordinator = retry_coordinator(store.clone(), executor.clone());
    let project = "lost-port-cas-ack";
    let key = compute_key(&identity(project));
    let request = start_request(&coordinator, project);
    entered(&executor).await;
    let original = store.get(&key).await.unwrap().unwrap();
    executor.release.notify_one();
    entered(&executor).await;
    let retrying = store.get(&key).await.unwrap().unwrap();
    assert_eq!(retrying.revision, original.revision + 1);
    assert_eq!(retrying.state, PreviewInstanceState::Starting);
    executor.release.notify_one();
    assert!(request.await.unwrap().unwrap().success);
    let current = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    let retries = store.retries.lock().unwrap().clone();
    assert_eq!(
        retries.len(),
        2,
        "one lost acknowledgement must retry metadata only"
    );
    let identity_of = |retry: &shared_types::PreviewStartPortRetry| {
        (
            retry.preview_key.clone(),
            retry.operation_id.clone(),
            retry.instance_id.clone(),
            retry.revision,
            retry.previous_port,
            retry.next_port,
        )
    };
    assert_eq!(identity_of(&retries[0]), identity_of(&retries[1]));
    assert_eq!(current.operation_id, original.operation_id);
    assert_eq!(current.instance_id, original.instance_id);
    assert_eq!(current.revision, original.revision + 1);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    assert!(store.failures.lock().unwrap().is_empty());
    coordinator.coordinated_stop(&current).await.unwrap();
}

#[tokio::test]
async fn concurrent_callers_join_selected_base_and_different_explicit_base_conflicts() {
    let store = Arc::new(StartupStore::default());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    let coordinator = retry_coordinator(store.clone(), executor.clone());
    let project = "coalesced-base-input";
    let key = compute_key(&identity(project));
    let with_base = |base: Option<&str>| {
        let coordinator = coordinator.clone();
        let base_path = base.map(str::to_owned);
        tokio::spawn(async move {
            coordinator
                .start_dev(PreviewStartRequest {
                    identity: identity(project),
                    base_path,
                })
                .await
        })
    };
    let original = with_base(Some(" custom-root "));
    entered(&executor).await;
    let row = store.get(&key).await.unwrap().unwrap();
    let owner = coordinator
        .startup_owners
        .get(&key, &row.instance_id)
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.observer_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("leader must already be waiting on its real result receiver");
    let initial_waiters = owner.observer_count();
    let no_override = with_base(None);
    let equivalent = with_base(Some("/custom-root/"));
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.observer_count() < initial_waiters + 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("both followers must join the held startup, not take a later Ready fast path");
    let conflict = with_base(Some("different-root"));
    let error = tokio::time::timeout(Duration::from_secs(3), conflict)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(error, PreviewCoordinationError::Conflict(_)));
    assert_eq!(store.admissions.lock().unwrap().len(), 1);
    executor.release.notify_one();
    for request in [original, no_override, equivalent] {
        assert!(request.await.unwrap().unwrap().success);
    }
    let ready = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    assert_eq!(ready.base_path.as_deref(), Some("/custom-root/"));
    let error = with_base(Some("other-ready-root"))
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, PreviewCoordinationError::Conflict(_)),
        "Ready fast path must also preserve an explicit base input"
    );
    assert!(with_base(None).await.unwrap().unwrap().success);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

struct ExistingOwnerPod;
#[async_trait::async_trait]
impl HostEvidence for ExistingOwnerPod {
    async fn host_pod_exists(&self, _pod_uid: &str) -> bool {
        true
    }
}

#[tokio::test]
async fn remote_stop_after_port_cas_commit_fences_owner_before_delayed_reply() {
    let store = Arc::new(StartupStore::default());
    store.hold_retry_reply.store(true, Ordering::SeqCst);
    store.lost_retry_reply.store(1, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.port_conflicts.store(1, Ordering::SeqCst);
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let port = listener.local_addr().unwrap().port();
    let config = CoordinatorConfig {
        start_budget_secs: 10,
        remote_dispatch_timeout_secs: 1,
        peer_api_port: port,
        ..CoordinatorConfig::default()
    };
    let mut owner = PreviewCoordinator::new(
        store.clone(),
        executor.clone(),
        Arc::new(ExistingOwnerPod),
        "test-preview-token".into(),
        config.clone(),
    );
    owner.host = PreviewHostIdentity {
        host_id: "owner-pod:owner-boot".into(),
        pod_name: Some("owner-pod".into()),
        pod_ip: Some("127.0.0.1".into()),
    };
    let owner = Arc::new(owner);
    let peer_executor = Arc::new(PreparationBarrierExecutor::default());
    let mut peer = PreviewCoordinator::new(
        store.clone(),
        peer_executor.clone(),
        Arc::new(ExistingOwnerPod),
        "test-preview-token".into(),
        config,
    );
    peer.host = PreviewHostIdentity {
        host_id: "peer-pod:peer-boot".into(),
        pod_name: Some("peer-pod".into()),
        pod_ip: Some("127.0.0.2".into()),
    };
    assert_ne!(owner.host.host_id, peer.host.host_id);
    let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel();
    let app = crate::internal_http::internal_router(owner.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
            .unwrap();
    });
    let project = "remote-stop-port-retry";
    let key = compute_key(&identity(project));
    let request = start_request(&owner, project);
    entered(&executor).await;
    let original = store.get(&key).await.unwrap().unwrap();
    executor.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), store.retry_committed.notified())
        .await
        .unwrap();
    let retrying = store.get(&key).await.unwrap().unwrap();
    assert_eq!(retrying.state, PreviewInstanceState::Starting);
    assert_eq!(retrying.operation_id, original.operation_id);
    assert_eq!(retrying.instance_id, original.instance_id);
    assert_eq!(retrying.revision, original.revision + 1);
    assert_eq!(retrying.pod_ip.as_deref(), Some("127.0.0.1"));
    assert!(
        peer.host_pod_exists("owner-pod").await,
        "fixture must exercise dispatch, not missing-host recovery"
    );
    let stopped = peer.coordinated_stop(&retrying).await;
    let terminal_before_reply = store.get(&key).await.unwrap().unwrap();
    let owner_cancelled = owner
        .startup_owners
        .get(&key, &original.instance_id)
        .unwrap()
        .is_none_or(|owner| owner.cancelled());
    // Drain both a correct implementation and a concrete erroneous extra start
    // before assertions, so a RED never leaves a background fixture task alive.
    executor.release.notify_one();
    store.retry_reply_release.notify_one();
    let start_result = tokio::time::timeout(Duration::from_secs(3), request)
        .await
        .unwrap()
        .unwrap();
    let current = store.get(&key).await.unwrap().unwrap();
    if current.state.is_active() {
        owner.coordinated_stop(&current).await.unwrap();
    }
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        stopped.is_ok(),
        "real remote Stop must succeed before held port reply: {stopped:?}"
    );
    assert_eq!(terminal_before_reply.state, PreviewInstanceState::Stopped);
    assert_eq!(current.state, PreviewInstanceState::Stopped);
    assert!(
        owner_cancelled,
        "remote authenticated internal Stop must reach the original process owner"
    );
    assert!(start_result.is_err());
    assert_eq!(
        executor.starts.load(Ordering::SeqCst),
        1,
        "no next native attempt after remote Stop success"
    );
    assert_eq!(peer_executor.starts.load(Ordering::SeqCst), 0);
    assert!(
        peer_executor.stop_inputs.lock().unwrap().is_empty(),
        "peer must not dispatch to its own executor"
    );
    let stop_inputs = executor.stop_inputs.lock().unwrap();
    assert!(
        !stop_inputs.is_empty(),
        "real owner endpoint must perform exact cleanup"
    );
    assert!(
        stop_inputs
            .iter()
            .all(|(called_key, instance)| called_key == &key && instance == &original.instance_id)
    );
}

/// watch wakes its registered Waker synchronously at result publication. This
/// observes actual completion ordering, without a scheduler-dependent race.
struct RegistryAtResultWake {
    owners: startup::StartupOwners,
    key: String,
    instance: String,
    observed: AtomicBool,
    still_registered: AtomicBool,
}
impl std::task::Wake for RegistryAtResultWake {
    fn wake(self: Arc<Self>) {
        self.still_registered.store(
            self.owners
                .get(&self.key, &self.instance)
                .unwrap()
                .is_some(),
            Ordering::SeqCst,
        );
        self.observed.store(true, Ordering::SeqCst);
    }
}

#[tokio::test]
async fn definitive_business_result_retires_owner_before_notification_and_immediate_retry() {
    use std::future::Future;
    let store = Arc::new(StartupStore::default());
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.failure.store(1, Ordering::SeqCst);
    let coordinator = retry_coordinator(store.clone(), executor.clone());
    let project = "retire-before-final-result";
    let key = compute_key(&identity(project));
    let request = start_request(&coordinator, project);
    entered(&executor).await;
    let original = store.get(&key).await.unwrap().unwrap();
    let owner = coordinator
        .startup_owners
        .get(&key, &original.instance_id)
        .unwrap()
        .unwrap();
    let observation = Arc::new(RegistryAtResultWake {
        owners: coordinator.startup_owners.clone(),
        key: key.clone(),
        instance: original.instance_id.clone(),
        observed: AtomicBool::new(false),
        still_registered: AtomicBool::new(false),
    });
    let waker = std::task::Waker::from(observation.clone());
    let mut outcome = Box::pin(owner.wait_result());
    assert!(
        outcome
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending(),
        "the ordering observer must be registered before the real completion"
    );
    executor.release.notify_one();
    let error = request.await.unwrap().unwrap_err();
    assert!(matches!(error, PreviewCoordinationError::Business { .. }));
    assert!(observation.observed.load(Ordering::SeqCst));
    assert!(
        !observation.still_registered.load(Ordering::SeqCst),
        "a caller notified of a definitive result must not find the completed owner in the registry"
    );
    assert!(
        coordinator
            .startup_owners
            .get(&key, &original.instance_id)
            .unwrap()
            .is_none()
    );
    drop(outcome);
    let retry = start_request(&coordinator, project);
    entered(&executor).await;
    let next = store.get(&key).await.unwrap().unwrap();
    assert_ne!(next.instance_id, original.instance_id);
    assert_ne!(next.operation_id, original.operation_id);
    executor.release.notify_one();
    assert!(retry.await.unwrap().unwrap().success);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    let ready = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn default_base_owner_rejects_explicit_temporary_port_base_but_none_followers_join() {
    let store = Arc::new(StartupStore::default());
    store.hold_retry_reply.store(true, Ordering::SeqCst);
    let executor = Arc::new(PreparationBarrierExecutor::default());
    executor.port_conflicts.store(1, Ordering::SeqCst);
    let coordinator = retry_coordinator(store.clone(), executor.clone());
    let project = "default-base-moving-port";
    let key = compute_key(&identity(project));
    let request = start_request(&coordinator, project);
    entered(&executor).await;
    let original = store.get(&key).await.unwrap().unwrap();
    let owner = coordinator
        .startup_owners
        .get(&key, &original.instance_id)
        .unwrap()
        .unwrap();
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.observer_count() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let initial_waiters = owner.observer_count();
    let original_base = default_base(original.port.unwrap());
    let reject_explicit = |base_path: String| {
        let coordinator = coordinator.clone();
        async move {
            let error = tokio::time::timeout(
                Duration::from_secs(3),
                coordinator.start_dev(PreviewStartRequest {
                    identity: identity(project),
                    base_path: Some(base_path),
                }),
            )
            .await
            .expect("explicit base must be rejected while the original default-base owner is held")
            .unwrap_err();
            assert!(matches!(error, PreviewCoordinationError::Conflict(_)));
        }
    };
    // Before the first native attempt completes, this looks equal to the
    // temporary default. A later strict-port conflict changes that default.
    reject_explicit(original_base.clone()).await;
    let preparation_follower = start_request(&coordinator, project);
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.observer_count() < initial_waiters + 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("None preparation follower must join before the port changes");
    executor.release.notify_one();
    tokio::time::timeout(Duration::from_secs(3), store.retry_committed.notified())
        .await
        .unwrap();
    let retrying = store.get(&key).await.unwrap().unwrap();
    assert_eq!(retrying.state, PreviewInstanceState::Starting);
    assert_eq!(retrying.instance_id, original.instance_id);
    assert_eq!(retrying.operation_id, original.operation_id);
    assert_ne!(retrying.port, original.port);
    // The owner still has the previous admitted snapshot while its CAS reply
    // is held. Neither that old allocation nor the newly committed port fixes
    // the effective base before this logical startup finishes.
    reject_explicit(original_base).await;
    reject_explicit(default_base(retrying.port.unwrap())).await;
    let retry_follower = start_request(&coordinator, project);
    tokio::time::timeout(Duration::from_secs(3), async {
        while owner.observer_count() < initial_waiters + 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("None CAS follower must join the same original owner");
    assert_eq!(store.admissions.lock().unwrap().len(), 1);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 1);
    store.retry_reply_release.notify_one();
    entered(&executor).await;
    executor.release.notify_one();
    for request in [request, preparation_follower, retry_follower] {
        assert!(request.await.unwrap().unwrap().success);
    }
    let ready = wait_state(store.as_ref(), &key, PreviewInstanceState::Ready).await;
    assert_eq!(ready.base_path, Some(default_base(ready.port.unwrap())));
    assert_eq!(ready.instance_id, original.instance_id);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    coordinator.coordinated_stop(&ready).await.unwrap();
}

#[tokio::test]
async fn heartbeat_cannot_publish_registered_attempt_before_port_conflict_ack_and_owner_retries() {
    let store = Arc::new(StartupStore::default());
    let executor = Arc::new(StartBarrierExecutor::default());
    executor.port_conflicts.store(1, Ordering::SeqCst);
    let coordinator = Arc::new(PreviewCoordinator::new(
        store.clone(),
        executor.clone(),
        Arc::new(SingleInstanceEvidence),
        "test-preview-token".into(),
        CoordinatorConfig {
            start_budget_secs: 10,
            ..CoordinatorConfig::default()
        },
    ));
    let project = "heartbeat-before-port-conflict";
    let key = compute_key(&identity(project));
    let mut request = start_request(&coordinator, project);
    tokio::time::timeout(Duration::from_secs(3), executor.entered.notified())
        .await
        .unwrap();
    let original = store.get(&key).await.unwrap().unwrap();
    let report = executor
        .verify_local(&key, &original.instance_id)
        .await
        .unwrap();
    assert!(
        report.identity_match && report.alive,
        "fixture must expose a registered alive process while its start result is unresolved"
    );
    assert!(
        coordinator
            .startup_owners
            .get(&key, &original.instance_id)
            .unwrap()
            .is_some()
    );
    coordinator.run_heartbeat_sweep().await;
    let before_first_ack = store.get(&key).await.unwrap().unwrap();
    let publications_before_first_ack = store.publications.lock().unwrap().len();
    // Return a definitive, already-cleaned PortInUse only after heartbeat has
    // had the opportunity to observe the temporary live registration.
    executor.acknowledge.notify_one();
    let mut completed = None;
    let entered_retry = tokio::time::timeout(Duration::from_secs(3), async {
        tokio::select! {
            _ = executor.entered.notified() => true,
            result = &mut request => { completed = Some(result.unwrap()); false }
        }
    })
    .await
    .unwrap();
    let retrying = if entered_retry {
        let row = store.get(&key).await.unwrap().unwrap();
        coordinator.run_heartbeat_sweep().await;
        let before_retry_ack = store.get(&key).await.unwrap().unwrap();
        executor.acknowledge.notify_one();
        Some((row, before_retry_ack))
    } else {
        None
    };
    let result = match completed {
        Some(result) => result,
        None => tokio::time::timeout(Duration::from_secs(3), request)
            .await
            .unwrap()
            .unwrap(),
    };
    let current = store.get(&key).await.unwrap().unwrap();
    if current.state.is_active() {
        coordinator.coordinated_stop(&current).await.unwrap();
    }
    // Assert after draining both the successful retry and the concrete old
    // failure path, so this RED cannot leave a native fixture blocked on ACK.
    assert_eq!(
        before_first_ack.state,
        PreviewInstanceState::Starting,
        "a live StartupOwner owns the unresolved readiness result; heartbeat cannot publish Ready first"
    );
    assert_eq!(publications_before_first_ack, 0);
    assert!(
        entered_retry,
        "confirmed PortInUse must continue the original logical start"
    );
    let (retrying, before_retry_ack) = retrying.unwrap();
    assert_eq!(retrying.state, PreviewInstanceState::Starting);
    assert_eq!(before_retry_ack.state, PreviewInstanceState::Starting);
    assert_eq!(retrying.operation_id, original.operation_id);
    assert_eq!(retrying.instance_id, original.instance_id);
    assert_eq!(retrying.revision, original.revision + 1);
    assert_ne!(retrying.port, original.port);
    assert!(result.unwrap().success);
    assert_eq!(current.state, PreviewInstanceState::Ready);
    assert_eq!(current.instance_id, original.instance_id);
    assert_eq!(current.operation_id, original.operation_id);
    assert_eq!(executor.starts.load(Ordering::SeqCst), 2);
    assert_eq!(executor.port_conflict_cleanups.load(Ordering::SeqCst), 1);
    let tickets = executor.start_tickets.lock().unwrap();
    assert_eq!(tickets.len(), 2);
    assert!(
        tickets
            .iter()
            .all(|ticket| ticket.instance_id == original.instance_id)
    );
    assert!(store.failures.lock().unwrap().is_empty());
}

#[tokio::test]
async fn heartbeat_still_heals_registered_historical_starting_without_an_owner() {
    let store = Arc::new(InProcessPreviewStore::new());
    let executor = Arc::new(StartBarrierExecutor::default());
    let coordinator = barrier_coordinator(store.clone(), executor.clone());
    let project = identity("ownerless-late-start");
    let key = compute_key(&project);
    let AcceptStartOutcome::Admitted(starting) = store
        .accept_start(AcceptStartInput {
            preview_key: key.clone(),
            project_id: project.project_id.clone(),
            project_path: project.resolved_path,
            host: coordinator.host.clone(),
            operation_id: new_id(),
            instance_id: new_id(),
            requested_port: None,
            base_path: Some("/historical-base/".into()),
            recover_unknown_evidence: None,
        })
        .await
        .unwrap()
    else {
        panic!("historical start must be admitted");
    };
    *executor.ticket.lock().await = Some(ExecutorStartTicket {
        preview_key: key.clone(),
        log_key: project.project_id,
        project_path: starting.project_path.clone(),
        port: starting.port.unwrap(),
        base_path: starting.base_path.clone(),
        instance_id: starting.instance_id.clone(),
    });
    assert!(
        coordinator
            .startup_owners
            .get(&key, &starting.instance_id)
            .unwrap()
            .is_none()
    );
    coordinator.run_heartbeat_sweep().await;
    let ready = store.get(&key).await.unwrap().unwrap();
    assert_eq!(ready.state, PreviewInstanceState::Ready);
    assert_eq!(ready.operation_id, starting.operation_id);
    assert_eq!(ready.instance_id, starting.instance_id);
    assert_eq!(ready.base_path.as_deref(), Some("/historical-base/"));
    assert_eq!(
        executor.starts.load(Ordering::SeqCst),
        0,
        "self-healing never replays a historical start"
    );
    coordinator.coordinated_stop(&ready).await.unwrap();
    assert_eq!(executor.stops.load(Ordering::SeqCst), 1);
}
