//! Real engine/store boundary regressions; the runtime is deliberately controlled.
use super::*;
use crate::userapp_builder::creation::fence_settler_tests::runtime_state;
use container_runtime_api::{
    AgentContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult, UserAppDeploymentRuntime,
    WorkspaceRuntime,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const STOPPED_FAILURE: &str = "stopped-target-recapture-failed";
const STARTING_FAILURE: &str = "pre-dispatch-stop-observation-failed";
const UNKNOWN_START: &str = "original-start-response-lost";
const LOST_PREPARE: &str = "prepared-compute-lease-reply-lost";
const RELEASE_FAILURE: &str = "original-lease-release-failed";

#[derive(Default)]
struct Calls {
    prepares: AtomicUsize,
    stops: AtomicUsize,
    starts: AtomicUsize,
    releases: AtomicUsize,
    release_attempts: AtomicUsize,
    fail_release: AtomicBool,
    held: AtomicBool,
    receipt: Mutex<Option<UserAppOperationLeaseReceipt>>,
    released_receipts: Mutex<Vec<UserAppOperationLeaseReceipt>>,
}

#[derive(Default)]
struct Runtime {
    calls: Arc<Calls>,
    docker: Mutex<Option<Arc<docker_autoremove::Mode>>>,
    stopped: AtomicBool,
    started: AtomicBool,
    fail_stopped_capture: AtomicBool,
    fail_start_preflight: AtomicBool,
    fail_start_after_dispatch: AtomicBool,
    lose_prepare_reply: AtomicBool,
    lost_prepare_context: Mutex<Option<UserAppExecutionContext>>,
    lease_inspections: AtomicUsize,
    retry_allowed: AtomicBool,
    retry_observed: tokio::sync::Notify,
    capture_gate: Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Semaphore>)>>,
    close_store_on_capture:
        Mutex<Option<Arc<dyn rcoder_storage::userapp_lifecycle::UserAppStoreControl>>>,
}

fn target(context: &UserAppExecutionContext) -> BuilderControlTarget {
    BuilderControlTarget {
        context: context.clone(),
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "retained-builder".into(),
            uid: "retained-sts-uid".into(),
            resource_version: Some("3".into()),
        }),
        pod: None,
        resource_binding: None,
        restart_image: None,
        restart_runtime_workspace: None,
        builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
    }
}

fn volumes() -> Vec<AppResourceIdentity> {
    vec![AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: "retained-workspace".into(),
        uid: "retained-pvc-uid".into(),
        resource_version: None,
    }]
}

fn receipt(context: &UserAppExecutionContext) -> UserAppOperationLeaseReceipt {
    UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "failure-tests".into(),
        name: "original-compute-lease".into(),
        uid: "original-lease-uid".into(),
        resource_version: "1".into(),
        token: context.executor_id.clone(),
    }
}

struct Lease {
    calls: Arc<Calls>,
    receipt: UserAppOperationLeaseReceipt,
}

#[async_trait::async_trait]
impl AppOperationLease for Lease {
    fn receipt(&self) -> Option<UserAppOperationLeaseReceipt> {
        Some(self.receipt.clone())
    }

    async fn release(self: Box<Self>) -> Result<(), String> {
        assert_eq!(
            *self.calls.receipt.lock().unwrap(),
            Some(self.receipt.clone())
        );
        self.calls.release_attempts.fetch_add(1, Ordering::SeqCst);
        if self.calls.fail_release.load(Ordering::SeqCst) {
            return Err(RELEASE_FAILURE.into());
        }
        self.calls
            .released_receipts
            .lock()
            .unwrap()
            .push(self.receipt.clone());
        self.calls.releases.fetch_add(1, Ordering::SeqCst);
        self.calls.held.store(false, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl PreparedComputeLease for Lease {
    async fn activate(&mut self) -> Result<(), String> {
        self.calls.held.store(true, Ordering::SeqCst);
        Ok(())
    }
}

impl Runtime {
    async fn capture(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        if self.stopped.load(Ordering::SeqCst) && self.fail_stopped_capture.load(Ordering::SeqCst) {
            let gate = self.capture_gate.lock().unwrap().clone();
            if let Some((entered, release)) = gate {
                entered.notify_one();
                release.acquire_owned().await.unwrap().forget();
            }
            let control = self.close_store_on_capture.lock().unwrap().take();
            if let Some(control) = control {
                control.shutdown().await.unwrap();
            }
            return Err(ContainerRuntimeError::K8sError(STOPPED_FAILURE.into()));
        }
        if let Some(mode) = self.docker.lock().unwrap().clone() {
            return Ok(mode.target(context, self.stopped.load(Ordering::SeqCst)));
        }
        Ok(target(context))
    }

    fn info(&self, context: &UserAppExecutionContext) -> ContainerBasicInfo {
        if let Some(mode) = self.docker.lock().unwrap().clone() {
            return mode.info(context);
        }
        ContainerBasicInfo {
            container_id: "original-restarted-pod".into(),
            container_name: "retained-builder-0".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: AGENT_FILE_SERVER_PORT,
            external_port: 0,
            project_id: context.app_id.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
            workload_uid: Some("retained-sts-uid".into()),
        }
    }
}

#[async_trait::async_trait]
impl WorkspaceRuntime for Runtime {}

#[async_trait::async_trait]
impl UserAppDeploymentRuntime for Runtime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(vec![])
    }

    async fn get_deployment_status(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::DeploymentStatus>> {
        Ok(None)
    }

    async fn prepare_compute_operation(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
    ) -> ContainerRuntimeResult<Box<dyn PreparedComputeLease>> {
        assert_eq!(scope, UserAppOperationScope::Dev);
        self.calls.prepares.fetch_add(1, Ordering::SeqCst);
        if let Some(original) = self.lost_prepare_context.lock().unwrap().as_ref() {
            assert_eq!(context.app_id, original.app_id);
            assert_eq!(context.lifecycle_id, original.lifecycle_id);
            assert_eq!(context.operation_id, original.operation_id);
            assert_eq!(context.request_fingerprint, original.request_fingerprint);
            assert_ne!(context.executor_id, original.executor_id);
            assert_eq!(
                *self.calls.released_receipts.lock().unwrap(),
                vec![receipt(original)],
                "the discovered original prepare must be released before another attempt"
            );
        }
        let receipt = if self.docker.lock().unwrap().is_some() {
            docker_autoremove::receipt(context)
        } else {
            receipt(context)
        };
        *self.calls.receipt.lock().unwrap() = Some(receipt.clone());
        if self.lose_prepare_reply.swap(false, Ordering::SeqCst) {
            *self.lost_prepare_context.lock().unwrap() = Some(context.clone());
            return Err(ContainerRuntimeError::K8sError(LOST_PREPARE.into()));
        }
        Ok(Box::new(Lease {
            calls: self.calls.clone(),
            receipt,
        }))
    }

    async fn validate_app_operation_receipt(
        &self,
        _: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        assert_eq!(self.calls.receipt.lock().unwrap().as_ref(), Some(receipt));
        Ok(self.calls.held.load(Ordering::SeqCst))
    }

    async fn inspect_compute_drain_lease(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        registered: Option<&UserAppOperationLeaseReceipt>,
    ) -> ContainerRuntimeResult<ComputeLeaseInspection> {
        self.lease_inspections.fetch_add(1, Ordering::SeqCst);
        assert_eq!(scope, UserAppOperationScope::Dev);
        assert!(
            registered.is_none(),
            "the lost reply must not invent a local binding"
        );
        let Some(attempt) = self.lost_prepare_context.lock().unwrap().clone() else {
            assert!(!self.calls.held.load(Ordering::SeqCst));
            return Ok(ComputeLeaseInspection::Absent);
        };
        assert_eq!(*context, attempt);
        assert_eq!(
            self.calls.receipt.lock().unwrap().as_ref(),
            Some(&receipt(&attempt))
        );
        Ok(ComputeLeaseInspection::Discovered {
            receipt: receipt(&attempt),
            attempt,
        })
    }

    async fn release_app_operation_receipt(
        &self,
        _: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        assert_eq!(self.calls.receipt.lock().unwrap().as_ref(), Some(receipt));
        self.calls.release_attempts.fetch_add(1, Ordering::SeqCst);
        if self.calls.fail_release.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::K8sError(RELEASE_FAILURE.into()));
        }
        self.calls
            .released_receipts
            .lock()
            .unwrap()
            .push(receipt.clone());
        self.calls.releases.fetch_add(1, Ordering::SeqCst);
        self.calls.held.store(false, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: container_runtime_api::ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("compute recovery must not admit a second builder")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("unexpected name-based lookup")
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::RuntimeContainerInfo>> {
        panic!("unexpected discovery")
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("must stop the captured target")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        panic!("unexpected name-based probe")
    }
    async fn list_containers(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("retained data must not be cleaned")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn inspect_builder_candidate(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        self.capture(context).await
    }
    async fn capture_builder_control(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        self.capture(context).await
    }
    async fn capture_bound_builder_control(
        &self,
        context: &UserAppExecutionContext,
        _: Option<&UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        self.capture(context).await
    }
    async fn capture_builder_compute_volumes(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Vec<AppResourceIdentity>> {
        if let Some(mode) = self.docker.lock().unwrap().clone() {
            assert!(
                target.workload.is_some(),
                "Docker binds require a captured container"
            );
            return Ok(mode.volumes());
        }
        Ok(volumes())
    }
    async fn archive_builder_restart(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<BuilderRestartTemplate>> {
        let mode = self.docker.lock().unwrap().clone();
        match mode {
            Some(mode) => mode.archive(target).await,
            None => Ok(None),
        }
    }
    async fn inspect_builder_restart_archive(
        &self,
        template: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<Option<bool>> {
        let mode = self
            .docker
            .lock()
            .unwrap()
            .clone()
            .expect("Docker archive fixture");
        Ok(Some(mode.archive_valid(template)))
    }
    async fn restore_builder_restart(
        &self,
        template: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        let mode = self
            .docker
            .lock()
            .unwrap()
            .clone()
            .expect("Docker archive fixture");
        mode.restore(template).await
    }
    async fn ensure_absent_builder_control(
        &self,
        _: &BuilderControlTarget,
        _: &[AppResourceIdentity],
        _: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!(
            "Docker AutoRemove must restore its original archive, never ensure absent K8s compute"
        )
    }
    fn supports_builder_compute_fencing(&self) -> bool {
        true
    }
    async fn apply_builder_control(
        &self,
        _: &BuilderControlTarget,
        restart: bool,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        assert!(!restart);
        assert!(self.calls.held.load(Ordering::SeqCst));
        self.calls.stops.fetch_add(1, Ordering::SeqCst);
        self.stopped.store(true, Ordering::SeqCst);
        Ok(None)
    }
    async fn reconcile_builder_compute_stop(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        if self.fail_start_preflight.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::K8sError(STARTING_FAILURE.into()));
        }
        Ok(self.stopped.load(Ordering::SeqCst))
    }
    async fn prepare_builder_compute_retry(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<BuilderControlTarget>> {
        self.retry_observed.notify_one();
        Ok(self
            .retry_allowed
            .load(Ordering::SeqCst)
            .then(|| target.clone()))
    }
    async fn start_builder_control(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let mode = self.docker.lock().unwrap().clone();
        if let Some(mode) = mode {
            mode.assert_start(target).await;
        }
        assert!(self.calls.held.load(Ordering::SeqCst));
        let current_receipt = self.calls.receipt.lock().unwrap().clone().unwrap();
        assert!(
            !self
                .calls
                .released_receipts
                .lock()
                .unwrap()
                .contains(&current_receipt),
            "the current startup lease must not have been released"
        );
        self.calls.starts.fetch_add(1, Ordering::SeqCst);
        if self.fail_start_after_dispatch.load(Ordering::SeqCst) {
            self.retry_allowed.store(false, Ordering::SeqCst);
            return Err(ContainerRuntimeError::K8sError(UNKNOWN_START.into()));
        }
        self.started.store(true, Ordering::SeqCst);
        self.stopped.store(false, Ordering::SeqCst);
        Ok(Some(self.info(&target.context)))
    }
    async fn reconcile_builder_compute_start(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(self
            .started
            .load(Ordering::SeqCst)
            .then(|| self.info(&target.context)))
    }
}

mod docker_autoremove;

async fn fresh_state() -> (Arc<AppState>, Arc<Runtime>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("compute-failure.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(Runtime::default());
    runtime.retry_allowed.store(true, Ordering::SeqCst);
    let state = runtime_state(runtime.clone(), store, None).await;
    (state, runtime, directory)
}

async fn fixture() -> (
    Arc<AppState>,
    Arc<Runtime>,
    ComputeControlRecord,
    tempfile::TempDir,
) {
    let (state, runtime, directory) = fresh_state().await;
    let app = state
        .userapp_store
        .ensure_identity("failureapp")
        .await
        .unwrap();
    let pending = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id,
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Dev,
            operation_id: "original-restart".into(),
            request_id: "original-request".into(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Restart,
            restart_image_roll: false,
            builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
        })
        .await
        .unwrap();
    (state, runtime, pending, directory)
}

async fn read(state: &AppState, record: &ComputeControlRecord) -> ComputeControlRecord {
    state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap()
}

async fn seed_starting(
    state: &AppState,
    runtime: &Runtime,
    pending: &ComputeControlRecord,
) -> (ComputeControlRecord, BuilderControlTarget) {
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        scope: pending.scope,
        executor_id: "original-executor".into(),
    };
    let mut record = state
        .userapp_store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let captured = target(&record.execution_context().unwrap());
    let original_receipt = receipt(&captured.context);
    *runtime.calls.receipt.lock().unwrap() = Some(original_receipt.clone());
    runtime.calls.held.store(true, Ordering::SeqCst);
    runtime.stopped.store(true, Ordering::SeqCst);
    record = state
        .userapp_store
        .bind_compute_lease(&identity, &original_receipt)
        .await
        .unwrap();
    let mut checkpoint = serde_json::to_value(&captured).unwrap();
    checkpoint["builder_volumes"] = serde_json::to_value(volumes()).unwrap();
    checkpoint["builder_compute_single_write"] = serde_json::json!(true);
    checkpoint["builder_restart_template"] = serde_json::json!({"historical_reference": "opaque"});
    for stage in [
        ComputeControlStage::Stopping,
        ComputeControlStage::Stopped,
        ComputeControlStage::Starting,
    ] {
        progress(state, &mut record, &identity, stage, checkpoint.clone())
            .await
            .unwrap();
    }
    (record, captured)
}

async fn pause_stopped_failure(
    state: &Arc<AppState>,
    runtime: &Runtime,
    pending: &ComputeControlRecord,
) -> (
    tokio::task::JoinHandle<Result<()>>,
    Arc<tokio::sync::Semaphore>,
) {
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    *runtime.capture_gate.lock().unwrap() = Some((entered.clone(), release.clone()));
    runtime.fail_stopped_capture.store(true, Ordering::SeqCst);
    let worker_state = state.clone();
    let pending = pending.clone();
    let worker = tokio::spawn(async move { execute_pending(&worker_state, pending).await });
    tokio::time::timeout(Duration::from_secs(5), entered.notified())
        .await
        .expect("coordinator must reach the captured stopped boundary");
    (worker, release)
}

async fn recover_original(
    state: &Arc<AppState>,
    failure: &ComputeControlRecord,
) -> ComputeControlRecord {
    let accepted = recover(
        state,
        &failure.app_id,
        &failure.operation_id,
        failure.revision,
    )
    .await
    .unwrap();
    assert_eq!(accepted.operation_id, failure.operation_id);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let current = read(state, failure).await;
            if current.state == ComputeControlState::Succeeded && current.lease.is_none() {
                return current;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("original compute recovery must complete and release its receipt")
}

fn assert_failure(
    record: &ComputeControlRecord,
    original: &ComputeControlRecord,
    stage: &str,
    marker: &str,
) {
    assert_eq!(record.state, ComputeControlState::RecoveryRequired);
    assert_eq!(record.stage, stage);
    assert_eq!(record.operation_id, original.operation_id);
    assert_eq!(record.request_id, original.request_id);
    assert_eq!(record.generation, original.generation);
    assert_eq!(record.error_code.as_deref(), Some(ERR_BACKEND_ERROR));
    assert!(record.error_message.as_deref().unwrap().contains(marker));
    assert!(
        record.lease.is_some(),
        "the original physical receipt must stay protected"
    );
}

#[tokio::test]
async fn stopped_target_preparation_failure_keeps_original_diagnostic_and_can_recover() {
    let (state, runtime, pending, _directory) = fixture().await;
    runtime.fail_stopped_capture.store(true, Ordering::SeqCst);
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    let failure = read(&state, &pending).await;
    assert_failure(&failure, &pending, "stopped", STOPPED_FAILURE);
    assert!(format!("{error:#}").contains(STOPPED_FAILURE));
    assert_eq!(failure.lease, *runtime.calls.receipt.lock().unwrap());
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 1);
    let retained_volumes = failure.checkpoint["builder_volumes"].clone();

    runtime.fail_stopped_capture.store(false, Ordering::SeqCst);
    let completed = recover_original(&state, &failure).await;
    assert_eq!(completed.operation_id, failure.operation_id);
    assert_eq!(completed.request_id, failure.request_id);
    assert_eq!(completed.generation, failure.generation);
    assert_eq!(completed.checkpoint["builder_volumes"], retained_volumes);
    assert_eq!(completed.error_code, None);
    assert_eq!(completed.error_message, None);
    assert_eq!(
        runtime.calls.stops.load(Ordering::SeqCst),
        1,
        "recovery must not repeat Stop"
    );
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn starting_pre_dispatch_failure_keeps_original_diagnostic_and_can_recover() {
    let (state, runtime, pending, _directory) = fixture().await;
    let (record, captured) = seed_starting(&state, &runtime, &pending).await;
    runtime.fail_start_preflight.store(true, Ordering::SeqCst);
    let error =
        run_restart_continuation(&state, record.clone(), RestartContinuation::Dev(captured))
            .await
            .unwrap_err();
    let failure = read(&state, &record).await;
    assert_failure(&failure, &record, "starting", STARTING_FAILURE);
    assert!(format!("{error:#}").contains(STARTING_FAILURE));
    assert_eq!(failure.checkpoint, record.checkpoint);
    assert_eq!(failure.lease, record.lease);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.calls.starts.load(Ordering::SeqCst),
        0,
        "preflight must fail before runtime start"
    );
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);

    runtime.fail_start_preflight.store(false, Ordering::SeqCst);
    let completed = recover_original(&state, &failure).await;
    assert_eq!(completed.operation_id, record.operation_id);
    assert_eq!(completed.request_id, record.request_id);
    assert_eq!(completed.generation, record.generation);
    assert_eq!(completed.error_code, None);
    assert_eq!(completed.error_message, None);
    assert_eq!(
        completed.checkpoint["builder_volumes"],
        record.checkpoint["builder_volumes"]
    );
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn superseding_stop_is_not_overwritten_by_the_old_executor_failure() {
    let (state, runtime, pending, _directory) = fixture().await;
    let (worker, release) = pause_stopped_failure(&state, &runtime, &pending).await;
    let successor = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            app_id: pending.app_id.clone(),
            lifecycle_id: pending.lifecycle_id.clone(),
            scope: pending.scope,
            operation_id: "successor-stop".into(),
            request_id: "successor-stop-request".into(),
            request_fingerprint: "b".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
            builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
        })
        .await
        .unwrap();
    assert_eq!(
        read(&state, &pending).await.state,
        ComputeControlState::Superseded
    );
    release.add_permits(1);
    let error = tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains(STOPPED_FAILURE));
    assert_eq!(read(&state, &successor).await, successor);
    let status = state
        .userapp_store
        .read_compute_status(&pending.app_id, &pending.lifecycle_id, pending.scope)
        .await
        .unwrap();
    assert!(status.desired_stopped);
    assert_eq!(status.operation, Some(successor));
    let old = read(&state, &pending).await;
    assert_eq!(old.state, ComputeControlState::Failed);
    assert_eq!(old.error_code.as_deref(), Some("ERR_CANCELLED"));
    assert!(old.lease.is_none());
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn failure_report_losing_revision_cas_preserves_the_later_snapshot_and_original_error() {
    let (state, runtime, pending, _directory) = fixture().await;
    let (worker, release) = pause_stopped_failure(&state, &runtime, &pending).await;
    let mut observed = read(&state, &pending).await;
    let identity = ComputeExecutorIdentity {
        app_id: observed.app_id.clone(),
        lifecycle_id: observed.lifecycle_id.clone(),
        operation_id: observed.operation_id.clone(),
        generation: observed.generation,
        scope: observed.scope,
        executor_id: observed.executor_id.clone().unwrap(),
    };
    let checkpoint = observed.checkpoint.clone();
    progress(
        &state,
        &mut observed,
        &identity,
        ComputeControlStage::Stopped,
        checkpoint,
    )
    .await
    .unwrap();
    release.add_permits(1);
    let error = tokio::time::timeout(Duration::from_secs(5), worker)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(format!("{error:#}").contains(STOPPED_FAILURE));
    assert_eq!(
        read(&state, &pending).await,
        observed,
        "stale failure must not replace a newer checkpoint revision"
    );
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    assert!(runtime.calls.held.load(Ordering::SeqCst));

    runtime.fail_stopped_capture.store(false, Ordering::SeqCst);
    let completed = recover_original(&state, &observed).await;
    assert_eq!(completed.operation_id, pending.operation_id);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn submitted_start_failure_retains_lease_and_observes_original_result_without_replay() {
    let (state, runtime, pending, _directory) = fixture().await;
    let (record, captured) = seed_starting(&state, &runtime, &pending).await;
    runtime
        .fail_start_after_dispatch
        .store(true, Ordering::SeqCst);
    let error =
        run_restart_continuation(&state, record.clone(), RestartContinuation::Dev(captured))
            .await
            .unwrap_err();
    let uncertain = read(&state, &record).await;
    assert_failure(&uncertain, &record, "starting", UNKNOWN_START);
    assert!(format!("{error:#}").contains(UNKNOWN_START));
    assert_eq!(uncertain.checkpoint, record.checkpoint);
    assert_eq!(uncertain.lease, record.lease);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);

    let accepted = recover(
        &state,
        &uncertain.app_id,
        &uncertain.operation_id,
        uncertain.revision,
    )
    .await
    .unwrap();
    assert_eq!(accepted.operation_id, uncertain.operation_id);
    tokio::time::timeout(Duration::from_secs(5), runtime.retry_observed.notified())
        .await
        .expect("recovery must observe the original conditional-start boundary");
    assert_eq!(read(&state, &record).await, uncertain);
    assert_eq!(
        runtime.calls.starts.load(Ordering::SeqCst),
        1,
        "unknown dispatched start cannot be replayed"
    );
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    assert!(runtime.calls.held.load(Ordering::SeqCst));

    // A late positive observation belongs to the already issued first start.
    runtime.started.store(true, Ordering::SeqCst);
    runtime.stopped.store(false, Ordering::SeqCst);
    let completed = recover_original(&state, &uncertain).await;
    assert_eq!(completed.operation_id, record.operation_id);
    assert_eq!(completed.request_id, record.request_id);
    assert_eq!(completed.generation, record.generation);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn failure_reporting_store_shutdown_preserves_both_causes_without_claiming_a_commit() {
    let (state, runtime, pending, directory) = fixture().await;
    runtime.fail_stopped_capture.store(true, Ordering::SeqCst);
    *runtime.close_store_on_capture.lock().unwrap() = Some(state.userapp_store_control.clone());
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    let closed = state
        .userapp_store
        .get_compute_control(&pending.app_id, &pending.operation_id)
        .await
        .unwrap_err();
    let diagnostic = format!("{error:#}");
    assert!(
        diagnostic.contains(STOPPED_FAILURE),
        "original runtime failure disappeared: {diagnostic}"
    );
    assert!(
        diagnostic.contains(&closed.to_string()),
        "failure-report storage error disappeared: {diagnostic}; store={closed}"
    );
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    assert!(runtime.calls.held.load(Ordering::SeqCst));

    let reopened = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
        &directory.path().join("compute-failure.db"),
    )
    .await
    .unwrap();
    let persisted = reopened
        .get_compute_control(&pending.app_id, &pending.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(persisted.operation_id, pending.operation_id);
    assert_eq!(persisted.request_id, pending.request_id);
    assert_eq!(persisted.generation, pending.generation);
    assert_eq!(persisted.state, ComputeControlState::Running);
    assert_eq!(persisted.stage, "stopped");
    assert_eq!(persisted.error_code, None);
    assert_eq!(persisted.error_message, None);
    assert_eq!(persisted.lease, *runtime.calls.receipt.lock().unwrap());
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn lost_prepare_reply_recovers_the_discovered_receipt_before_reexecuting_the_same_operation()
{
    let (state, runtime, pending, _directory) = fixture().await;
    runtime.lose_prepare_reply.store(true, Ordering::SeqCst);
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    assert!(format!("{error:#}").contains(LOST_PREPARE));
    let uncertain = read(&state, &pending).await;
    assert_eq!(uncertain.state, ComputeControlState::RecoveryRequired);
    assert_eq!(uncertain.stage, "draining_previous");
    assert_eq!(uncertain.operation_id, pending.operation_id);
    assert_eq!(uncertain.request_id, pending.request_id);
    assert_eq!(uncertain.generation, pending.generation);
    assert_eq!(uncertain.error_code.as_deref(), Some(ERR_BACKEND_ERROR));
    assert!(
        uncertain
            .error_message
            .as_deref()
            .unwrap()
            .contains(LOST_PREPARE)
    );
    assert!(
        uncertain.lease.is_none(),
        "an unreturned prepare cannot be fabricated as a registered receipt"
    );
    assert!(uncertain.can_recover_compute_drain());
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    let original_attempt = runtime
        .lost_prepare_context
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let original_receipt = receipt(&original_attempt);
    assert_eq!(uncertain.execution_context().unwrap(), original_attempt);

    // recover reserves the old executor, discovers/releases its exact prepared
    // lease, and returns the same request to Pending before a fresh claim.
    let completed = recover_original(&state, &uncertain).await;
    assert_eq!(completed.operation_id, pending.operation_id);
    assert_eq!(completed.request_id, pending.request_id);
    assert_eq!(completed.generation, pending.generation);
    assert_ne!(completed.executor_id, uncertain.executor_id);
    assert_eq!(
        completed.checkpoint["builder_volumes"],
        serde_json::to_value(volumes()).unwrap()
    );
    assert_eq!(completed.error_code, None);
    assert_eq!(completed.error_message, None);
    assert_eq!(runtime.lease_inspections.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 2);
    let releases = runtime.calls.released_receipts.lock().unwrap().clone();
    assert_eq!(releases[0], original_receipt);
    assert_eq!(
        Some(&releases[1]),
        runtime.calls.receipt.lock().unwrap().as_ref()
    );
    assert_ne!(releases[0], releases[1]);
    assert!(!runtime.calls.held.load(Ordering::SeqCst));
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn predecessor_receipt_release_failure_keeps_drain_recoverable_without_a_local_lease() {
    let (state, runtime, _directory) = fresh_state().await;
    let app = state
        .userapp_store
        .ensure_identity("drainfailure")
        .await
        .unwrap();
    let admitted = state
        .userapp_store
        .admit(&UserAppAdmission {
            app_id: app.app_id.clone(),
            lifecycle_id: Some(app.lifecycle_id.clone()),
            operation_id: "previous-builder".into(),
            request_id: Some("previous-request".into()),
            request_fingerprint: "b".repeat(64),
            kind: UserAppOperationKind::EnsureBuilder,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    let old = match admitted {
        UserAppAdmissionOutcome::Accepted(record) => record,
        other @ UserAppAdmissionOutcome::Existing(_) => {
            panic!("unexpected predecessor admission: {other:?}")
        }
    };
    let running = state
        .userapp_store
        .advance(&UserAppOperationProgress {
            app_id: old.app_id.clone(),
            lifecycle_id: old.lifecycle_id.clone(),
            operation_id: old.operation_id.clone(),
            expected_revision: old.revision,
            executor_id: "previous-executor".into(),
            state: UserAppOperationState::Running,
            step: "captured_previous_builder".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    let previous_context = UserAppExecutionContext {
        app_id: running.app_id.clone(),
        lifecycle_id: running.lifecycle_id.clone(),
        operation_id: running.operation_id.clone(),
        executor_id: running.executor_id.clone().unwrap(),
        request_fingerprint: running.request_fingerprint.clone(),
    };
    let previous_receipt = receipt(&previous_context);
    state
        .userapp_store
        .bind_operation_lease(&previous_context, &previous_receipt)
        .await
        .unwrap();
    let pending = state
        .userapp_store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: "draining-restart".into(),
            request_id: "draining-request".into(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Restart,
            restart_image_roll: false,
            builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
        })
        .await
        .unwrap();
    assert!(
        pending
            .interrupted_operations
            .contains(&running.operation_id)
    );
    let terminal = state
        .userapp_store
        .advance(&UserAppOperationProgress {
            app_id: running.app_id.clone(),
            lifecycle_id: running.lifecycle_id.clone(),
            operation_id: running.operation_id.clone(),
            expected_revision: running.revision,
            executor_id: previous_context.executor_id.clone(),
            state: UserAppOperationState::Failed,
            step: "previous_builder_failed".into(),
            checkpoint: serde_json::json!({"diagnostic":"retained"}),
            error_code: Some("PREVIOUS_FAILURE".into()),
            error_message: Some("previous outcome remains unchanged".into()),
        })
        .await
        .unwrap();
    *runtime.calls.receipt.lock().unwrap() = Some(previous_receipt.clone());
    runtime.calls.held.store(true, Ordering::SeqCst);
    runtime.calls.fail_release.store(true, Ordering::SeqCst);

    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    assert!(format!("{error:#}").contains(RELEASE_FAILURE));
    let failure = read(&state, &pending).await;
    assert_eq!(failure.state, ComputeControlState::RecoveryRequired);
    assert_eq!(failure.stage, "draining_previous");
    assert_eq!(failure.operation_id, pending.operation_id);
    assert_eq!(failure.request_id, pending.request_id);
    assert_eq!(failure.generation, pending.generation);
    assert_eq!(failure.error_code.as_deref(), Some(ERR_BACKEND_ERROR));
    assert!(
        failure
            .error_message
            .as_deref()
            .unwrap()
            .contains(RELEASE_FAILURE)
    );
    assert!(
        failure.lease.is_none(),
        "the predecessor lease is not the current compute lease"
    );
    let retained = state
        .userapp_store
        .get_operation_lease(&running.app_id, &running.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retained.context, previous_context);
    assert_eq!(retained.receipt, previous_receipt);
    assert_eq!(
        state
            .userapp_store
            .get_operation(&running.app_id, &running.operation_id)
            .await
            .unwrap(),
        Some(terminal.clone())
    );
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.calls.release_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);

    runtime.calls.fail_release.store(false, Ordering::SeqCst);
    let completed = recover_original(&state, &failure).await;
    assert_eq!(completed.operation_id, pending.operation_id);
    assert_eq!(completed.request_id, pending.request_id);
    assert_eq!(completed.generation, pending.generation);
    assert_eq!(completed.error_code, None);
    assert_eq!(completed.error_message, None);
    assert!(
        state
            .userapp_store
            .get_operation_lease(&running.app_id, &running.operation_id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        state
            .userapp_store
            .get_operation(&running.app_id, &running.operation_id)
            .await
            .unwrap(),
        Some(terminal)
    );
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.release_attempts.load(Ordering::SeqCst), 3);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.calls.released_receipts.lock().unwrap()[0],
        previous_receipt
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn terminal_commit_precedes_failed_lease_release_and_cannot_be_rewritten_or_restarted() {
    let (state, runtime, pending, _directory) = fixture().await;
    runtime.calls.fail_release.store(true, Ordering::SeqCst);
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    assert!(format!("{error:#}").contains(RELEASE_FAILURE));
    let completed = read(&state, &pending).await;
    assert_eq!(completed.state, ComputeControlState::Succeeded);
    assert_eq!(completed.stage, "completed");
    assert_eq!(completed.operation_id, pending.operation_id);
    assert_eq!(completed.request_id, pending.request_id);
    assert_eq!(completed.generation, pending.generation);
    assert_eq!(completed.error_code, None);
    assert_eq!(completed.error_message, None);
    assert_eq!(completed.lease, *runtime.calls.receipt.lock().unwrap());
    assert!(
        completed.lease.is_some(),
        "terminal cleanup must retain the exact unreleased receipt"
    );
    assert_eq!(
        completed.checkpoint["builder_volumes"],
        serde_json::to_value(volumes()).unwrap()
    );
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.release_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    assert!(runtime.calls.held.load(Ordering::SeqCst));

    execute_pending(&state, pending.clone()).await.unwrap();
    assert_eq!(read(&state, &pending).await, completed);
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.release_attempts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
}
