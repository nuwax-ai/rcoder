//! Real Turso + bounded scanner + real local FD; Docker effects are controlled.
//! No real Docker/120s claim. Parent registers this module; baseline Mode is untouched.
#![cfg(unix)]
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError as Error,
    ContainerRuntimeResult as RtResult, DeploymentStatus, RuntimeContainerInfo,
    UserAppDeploymentRuntime, WorkspaceRuntime,
};
use sha2::{Digest as _, Sha256};
use std::os::unix::fs::{FileExt as _, MetadataExt as _};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize};

#[derive(Default)]
struct Counts {
    probes: AtomicUsize,
    creates: AtomicUsize,
    starts: AtomicUsize,
    stops: AtomicUsize,
    releases: AtomicUsize,
}

struct RetainedFd {
    file: std::fs::File,
    path: PathBuf,
    receipt: UserAppOperationLeaseReceipt,
    counts: Arc<Counts>,
    released: bool,
}
impl Drop for RetainedFd {
    fn drop(&mut self) {
        if !self.released {
            drop(self.file.unlock());
        }
    }
}
#[async_trait::async_trait]
impl AppOperationLease for RetainedFd {
    fn receipt(&self) -> Option<UserAppOperationLeaseReceipt> {
        Some(self.receipt.clone())
    }
    async fn validate_retained(&self) -> Result<bool, String> {
        let UserAppOperationLeaseReceipt::Docker {
            device,
            inode,
            token,
            ..
        } = &self.receipt
        else {
            return Ok(false);
        };
        let meta = std::fs::symlink_metadata(&self.path).map_err(|e| e.to_string())?;
        let fd = self.file.metadata().map_err(|e| e.to_string())?;
        if !meta.is_file()
            || (meta.dev(), meta.ino()) != (*device, *inode)
            || (fd.dev(), fd.ino()) != (*device, *inode)
        {
            return Ok(false);
        }
        self.file.try_lock().map_err(|e| e.to_string())?;
        let mut bytes = vec![0; token.len() + 1];
        let count = self
            .file
            .read_at(&mut bytes, 0)
            .map_err(|e| e.to_string())?;
        Ok(&bytes[..count] == token.as_bytes())
    }
    async fn release(mut self: Box<Self>) -> Result<(), String> {
        let UserAppOperationLeaseReceipt::Docker { token, .. } = &self.receipt else {
            return Err("Docker FD required".into());
        };
        AppFileMutationMarker::for_operation(token)
            .map_err(|e| e.to_string())?
            .complete(&self.file)
            .map_err(|e| e.to_string())?;
        self.file.unlock().map_err(|e| e.to_string())?;
        self.released = true;
        self.counts.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct ScannerRuntime {
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    original: BuilderControlTarget,
    template: BuilderRestartTemplate,
    volume: AppResourceIdentity,
    lock_path: PathBuf,
    receipt: UserAppOperationLeaseReceipt,
    created: AtomicBool,
    running: AtomicBool,
    supersede_during_restore: AtomicBool,
    archive_unavailable: AtomicBool,
    lose_create_response: AtomicBool,
    counts: Arc<Counts>,
}
impl ScannerRuntime {
    fn current_target(&self) -> BuilderControlTarget {
        let mut target = self.original.clone();
        target.workload = if self.created.load(Ordering::SeqCst) {
            let mut resource = target.workload.unwrap();
            resource.uid = "b".repeat(64);
            Some(resource)
        } else {
            None
        };
        target
    }
    fn info(&self) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: "b".repeat(64),
            container_name: self.original.workload.as_ref().unwrap().name.clone(),
            container_ip: "127.0.0.1".into(),
            internal_port: AGENT_FILE_SERVER_PORT,
            external_port: 0,
            project_id: self.original.context.app_id.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
            workload_uid: Some("b".repeat(64)),
        }
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for ScannerRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for ScannerRuntime {
    async fn discover_application_identity(
        &self,
        _: &str,
    ) -> RtResult<Option<UserAppDiscoveredIdentity>> {
        // This controlled runtime has no external discovery inventory. The real
        // store in this fixture already owns the application's lifecycle.
        Ok(None)
    }
    async fn list_deployments(&self) -> RtResult<Vec<DeploymentStatus>> {
        Ok(vec![])
    }
    async fn get_deployment_status(&self, _: &str) -> RtResult<Option<DeploymentStatus>> {
        Ok(None)
    }
    async fn resume_original_compute_operation(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> RtResult<Box<dyn AppOperationLease>> {
        assert_eq!(*context, self.original.context);
        assert_eq!(scope, UserAppOperationScope::Dev);
        assert_eq!(*receipt, self.receipt);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&self.lock_path)
            .map_err(|e| Error::Conflict(e.to_string()))?;
        file.try_lock()
            .map_err(|e| Error::Conflict(format!("original FD remains live: {e}")))?;
        let guard = RetainedFd {
            file,
            path: self.lock_path.clone(),
            receipt: receipt.clone(),
            counts: self.counts.clone(),
            released: false,
        };
        if !guard.validate_retained().await.map_err(Error::Conflict)? {
            return Err(Error::Conflict("original FD/path/token changed".into()));
        }
        Ok(Box::new(guard))
    }
    async fn validate_app_operation_receipt(
        &self,
        _: &UserAppExecutionContext,
        _: &UserAppOperationLeaseReceipt,
    ) -> RtResult<bool> {
        panic!("held original FD path must not open a second temporary lease");
    }
    async fn prepare_compute_operation(
        &self,
        _: &UserAppExecutionContext,
        _: UserAppOperationScope,
    ) -> RtResult<Box<dyn PreparedComputeLease>> {
        // Fresh request acceptance is asserted separately. This fixture models
        // only the original recovery, not a second application's full execution.
        Err(Error::ConfigurationError(
            "scanner fixture ends after fresh admission".into(),
        ))
    }
}
#[async_trait::async_trait]
impl AgentContainerRuntime for ScannerRuntime {
    async fn create_container(&self, _: ContainerCreateParams) -> RtResult<ContainerBasicInfo> {
        panic!("ordinary ensure cannot replace recovery's original operation");
    }
    async fn get_container_info(&self, _: &str) -> RtResult<Option<ContainerBasicInfo>> {
        panic!("unexpected lookup")
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> RtResult<Option<RuntimeContainerInfo>> {
        panic!("unexpected discovery")
    }
    async fn stop_container(&self, _: &str) -> RtResult<()> {
        self.counts.stops.fetch_add(1, Ordering::SeqCst);
        panic!("original Stop must never be replayed");
    }
    async fn is_container_running(&self, _: &str) -> RtResult<bool> {
        panic!("unexpected name probe")
    }
    async fn list_containers(&self) -> RtResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> RtResult<()> {
        panic!("retained workspace cannot be cleaned")
    }
    async fn health_check(&self) -> RtResult<()> {
        Ok(())
    }
    async fn inspect_builder_candidate(
        &self,
        context: &UserAppExecutionContext,
    ) -> RtResult<BuilderControlTarget> {
        self.capture_builder_control(context).await
    }
    async fn capture_builder_control(
        &self,
        context: &UserAppExecutionContext,
    ) -> RtResult<BuilderControlTarget> {
        assert_eq!(*context, self.original.context);
        Ok(self.current_target())
    }
    async fn capture_bound_builder_control(
        &self,
        context: &UserAppExecutionContext,
        _: Option<&UserAppResourceBinding>,
    ) -> RtResult<BuilderControlTarget> {
        self.capture_builder_control(context).await
    }
    async fn capture_builder_compute_volumes(
        &self,
        _: &BuilderControlTarget,
    ) -> RtResult<Vec<AppResourceIdentity>> {
        Ok(vec![self.volume.clone()])
    }
    async fn observe_builder_restart_stop(
        &self,
        target: &BuilderControlTarget,
    ) -> RtResult<Option<BuilderRestartStopKind>> {
        assert_eq!(*target, self.original);
        self.counts.probes.fetch_add(1, Ordering::SeqCst);
        Ok(Some(BuilderRestartStopKind::CapturedContainerAbsent))
    }
    async fn inspect_builder_restart_archive(
        &self,
        template: &BuilderRestartTemplate,
    ) -> RtResult<Option<bool>> {
        Ok(Some(
            *template == self.template && !self.archive_unavailable.load(Ordering::SeqCst),
        ))
    }
    async fn restore_builder_restart(
        &self,
        template: &BuilderRestartTemplate,
    ) -> RtResult<BuilderControlTarget> {
        assert_eq!(*template, self.template);
        let record = self
            .store
            .get_compute_control(
                &template.source.context.app_id,
                &template.source.context.operation_id,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.stage, "stopped",
            "existing restore-before-Starting ordering"
        );
        assert_eq!(record.lease.as_ref(), Some(&self.receipt));
        assert_eq!(
            std::fs::read(&self.lock_path).unwrap(),
            template.source.context.executor_id.as_bytes()
        );
        if self.supersede_during_restore.load(Ordering::SeqCst) {
            self.store
                .admit_compute_control(&ComputeControlRequest {
                    app_id: record.app_id.clone(),
                    lifecycle_id: record.lifecycle_id.clone(),
                    scope: record.scope,
                    operation_id: "successorstop".into(),
                    request_id: "successorstoprequest".into(),
                    request_fingerprint: "f".repeat(64),
                    action: ComputeControlAction::Stop,
                    restart_image_roll: false,
                    builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
                })
                .await
                .unwrap();
        }
        assert!(
            !self.created.swap(true, Ordering::SeqCst),
            "one actual controlled Create"
        );
        self.counts.creates.fetch_add(1, Ordering::SeqCst);
        if self.lose_create_response.load(Ordering::SeqCst) {
            return Err(Error::DockerError(
                "controlled Create response lost after actual submission".into(),
            ));
        }
        Ok(self.current_target())
    }
    async fn start_builder_control(
        &self,
        target: &BuilderControlTarget,
    ) -> RtResult<Option<ContainerBasicInfo>> {
        assert_eq!(*target, self.current_target());
        let record = self
            .store
            .get_compute_control(&target.context.app_id, &target.context.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            record.stage, "starting",
            "existing physical target CAS before Start"
        );
        assert_eq!(record.lease.as_ref(), Some(&self.receipt));
        self.counts.starts.fetch_add(1, Ordering::SeqCst);
        self.running.store(true, Ordering::SeqCst);
        Ok(Some(self.info()))
    }
    async fn reconcile_builder_compute_start(
        &self,
        target: &BuilderControlTarget,
    ) -> RtResult<Option<ContainerBasicInfo>> {
        assert_eq!(*target, self.current_target());
        Ok(self.running.load(Ordering::SeqCst).then(|| self.info()))
    }
}

async fn fixture() -> (
    Arc<AppState>,
    Arc<ScannerRuntime>,
    ComputeControlRecord,
    tempfile::TempDir,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("scanner-stop.db"),
        )
        .await
        .unwrap(),
    );
    let app = store.ensure_identity("scannerstop").await.unwrap();
    let fingerprint: String = Sha256::digest(
        serde_json::to_vec(&(
            &app.app_id,
            &app.lifecycle_id,
            UserAppOperationScope::Dev,
            ComputeControlAction::Restart,
            true,
        ))
        .unwrap(),
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect();
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: "originalrestart".into(),
            request_id: "originalrequest".into(),
            request_fingerprint: fingerprint.clone(),
            action: ComputeControlAction::Restart,
            restart_image_roll: true,
            builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id.clone(),
        lifecycle_id: app.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "lostexecutor".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let lock_path = directory.path().join("original.lock");
    let fd = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&lock_path)
        .unwrap();
    fd.try_lock().unwrap();
    AppFileMutationMarker::for_operation(&identity.executor_id)
        .unwrap()
        .begin(&fd)
        .unwrap();
    let metadata = fd.metadata().unwrap();
    let receipt = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: metadata.dev(),
        inode: metadata.ino(),
        token: identity.executor_id.clone(),
    };
    let claimed = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    let original = BuilderControlTarget {
        context: UserAppExecutionContext {
            app_id: app.app_id,
            lifecycle_id: app.lifecycle_id,
            operation_id: pending.operation_id.clone(),
            executor_id: identity.executor_id.clone(),
            request_fingerprint: fingerprint,
        },
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: "rcoder-app-builder-scannerstop".into(),
            uid: "a".repeat(64),
            resource_version: None,
        }),
        pod: None,
        resource_binding: None,
        restart_image: None,
        restart_runtime_workspace: None,
        builder_startup_mode: BuilderStartupMode::PreserveBusinessIntent,
    };
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("sentinel"), b"original-data").unwrap();
    let volume = AppResourceIdentity {
        kind: AppResourceKind::HostPath,
        name: "/home/user/scannerstop".into(),
        uid: workspace.to_str().unwrap().into(),
        resource_version: None,
    };
    let template = BuilderRestartTemplate {
        source: original.clone(),
        archive: AppResourceIdentity {
            kind: AppResourceKind::File,
            name: "original-recipe".into(),
            uid: "c".repeat(64),
            resource_version: None,
        },
        volumes: vec![volume.clone()],
    };
    let mut checkpoint = serde_json::to_value(&original).unwrap();
    checkpoint["builder_volumes"] = serde_json::to_value(vec![volume.clone()]).unwrap();
    checkpoint["builder_restart_template"] = serde_json::to_value(&template).unwrap();
    let record = store
        .advance_compute_control(&ComputeControlProgress {
            identity,
            expected_revision: claimed.revision,
            state: ComputeControlState::Running,
            stage: ComputeControlStage::Stopping,
            checkpoint,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    drop(fd); // Original executor disappeared; the nonempty durable marker remains.
    let runtime = Arc::new(ScannerRuntime {
        store: store.clone(),
        original,
        template,
        volume,
        lock_path,
        receipt,
        created: AtomicBool::new(false),
        running: AtomicBool::new(false),
        supersede_during_restore: AtomicBool::new(false),
        archive_unavailable: AtomicBool::new(false),
        lose_create_response: AtomicBool::new(false),
        counts: Arc::default(),
    });
    let state = runtime_state(runtime.clone(), store, None).await;
    (state, runtime, record, directory)
}

async fn scan_once(state: &Arc<AppState>) -> Vec<anyhow::Result<()>> {
    let mut tasks = RecoveryTasks::default();
    discover_pending(state, &mut tasks, &mut None)
        .await
        .unwrap();
    assert_eq!(
        tasks.active.len(),
        1,
        "Running/Stopping must enter the production bounded scanner"
    );
    let mut results = Vec::new();
    while let Some((_, result)) = tasks.next().await {
        results.push(result);
    }
    assert!(tasks.active.is_empty());
    results
}
async fn current(state: &AppState, record: &ComputeControlRecord) -> ComputeControlRecord {
    state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn scanner_recovers_lost_running_stopping_executor_under_original_fd() {
    let (state, runtime, original, _directory) = fixture().await;
    for result in scan_once(&state).await {
        result.unwrap();
    }
    let done = current(&state, &original).await;
    assert_eq!(done.state, ComputeControlState::Succeeded);
    assert_eq!(done.operation_id, original.operation_id);
    assert_eq!(done.request_id, original.request_id);
    assert_eq!(done.executor_id, original.executor_id);
    assert!(done.lease.is_none());
    assert_eq!(runtime.counts.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.creates.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.counts.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.counts.releases.load(Ordering::SeqCst), 1);
    assert!(std::fs::read(&runtime.lock_path).unwrap().is_empty());
    assert_eq!(
        std::fs::read(PathBuf::from(&runtime.volume.uid).join("sentinel")).unwrap(),
        b"original-data"
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn scanner_refuses_live_original_fd_without_mutating_original_record() {
    let (state, runtime, original, _directory) = fixture().await;
    let live = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&runtime.lock_path)
        .unwrap();
    live.try_lock().unwrap();
    let results = scan_once(&state).await;
    assert!(results.into_iter().all(|result| result.is_err()));
    assert_eq!(current(&state, &original).await, original);
    assert_eq!(runtime.counts.probes.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.releases.load(Ordering::SeqCst), 0);
    live.unlock().unwrap();
    drop(live);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn same_request_submit_does_not_replace_running_operation_before_scanner_converges() {
    let (state, runtime, original, _directory) = fixture().await;
    let request = UserAppControlRequest {
        lifecycle_id: Some(original.lifecycle_id.clone()),
        request_id: Some(original.request_id.clone()),
    };
    let replay = crate::userapp_builder::compute_control::submit(
        &state,
        original.app_id.clone(),
        original.scope,
        original.action,
        request,
    )
    .await
    .unwrap();
    assert_eq!(replay.operation_id, original.operation_id);
    assert_eq!(current(&state, &original).await, original);
    assert!(
        crate::userapp_builder::compute_control::submit(
            &state,
            original.app_id.clone(),
            original.scope,
            original.action,
            UserAppControlRequest {
                lifecycle_id: Some(original.lifecycle_id.clone()),
                request_id: Some("freshrequest".into()),
            }
        )
        .await
        .is_err(),
        "a fresh request cannot replace the still-active original intent"
    );
    for result in scan_once(&state).await {
        result.unwrap();
    }
    let done = current(&state, &original).await;
    assert_eq!(done.state, ComputeControlState::Succeeded);
    assert!(done.lease.is_none());
    let fresh = crate::userapp_builder::compute_control::submit(
        &state,
        original.app_id.clone(),
        original.scope,
        original.action,
        UserAppControlRequest {
            lifecycle_id: Some(original.lifecycle_id.clone()),
            request_id: Some("freshrequest".into()),
        },
    )
    .await
    .unwrap();
    assert_ne!(
        fresh.operation_id, original.operation_id,
        "fresh intent becomes admissible after convergence"
    );
    // The controlled fixture intentionally rejects preparing that second
    // execution; await its bounded no-write failure before closing the store.
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let row = state
                .userapp_store
                .get_compute_control(&fresh.app_id, &fresh.operation_id)
                .await
                .unwrap()
                .unwrap();
            if matches!(
                row.state,
                ComputeControlState::Failed | ComputeControlState::RecoveryRequired
            ) {
                // Preparing a lease can have an unknown result. This fixture
                // does not weaken that existing classification just because
                // its controlled runtime returned before doing any mutation.
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(runtime.counts.creates.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.counts.starts.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn stop_winning_during_existing_restore_prevents_start_and_retains_original_marker() {
    let (state, runtime, original, _directory) = fixture().await;
    runtime
        .supersede_during_restore
        .store(true, Ordering::SeqCst);
    let results = scan_once(&state).await;
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_err(),
        "the unchanged Stopped CAS must lose to the successor Stop"
    );
    let old = current(&state, &original).await;
    assert_eq!(old.state, ComputeControlState::Superseded);
    assert_eq!(old.lease, original.lease);
    assert_eq!(runtime.counts.stops.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.counts.creates.load(Ordering::SeqCst),
        1,
        "existing restore returns stopped compute before CAS"
    );
    assert_eq!(runtime.counts.starts.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.counts.releases.load(Ordering::SeqCst),
        0,
        "Superseded is not a confirmed drain"
    );
    assert_eq!(
        std::fs::read(&runtime.lock_path).unwrap(),
        original.executor_id.as_ref().unwrap().as_bytes()
    );
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&runtime.lock_path)
        .unwrap();
    fd.try_lock().unwrap(); // Unknown-result Drop unlocked, but did not clean the marker.
    drop(fd);
    let status = state
        .userapp_store
        .read_compute_status(&original.app_id, &original.lifecycle_id, original.scope)
        .await
        .unwrap();
    let successor = status.operation.unwrap();
    assert_eq!(successor.operation_id, "successorstop");
    assert_eq!(successor.state, ComputeControlState::Pending);
    assert!(successor.lease.is_none());
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn scanner_closes_only_unrebuildable_old_restart_and_preserves_timeout_through_reporting() {
    let (state, runtime, original, _directory) = fixture().await;
    runtime.archive_unavailable.store(true, Ordering::SeqCst);
    let timed_out = state
        .userapp_store
        .advance_compute_control(&ComputeControlProgress {
            identity: ComputeExecutorIdentity {
                app_id: original.app_id.clone(),
                lifecycle_id: original.lifecycle_id.clone(),
                scope: original.scope,
                operation_id: original.operation_id.clone(),
                generation: original.generation,
                executor_id: original.executor_id.clone().unwrap(),
            },
            expected_revision: original.revision,
            state: ComputeControlState::RecoveryRequired,
            stage: ComputeControlStage::Stopping,
            checkpoint: original.checkpoint.clone(),
            error_code: Some("ORIGINAL_STOP_TIMEOUT".into()),
            error_message: Some("Original Stop exceeded its unchanged 120s client budget".into()),
        })
        .await
        .unwrap();
    let results = scan_once(&state).await;
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_err(),
        "old recipe failure cannot be reported as Restart success"
    );
    let closed = current(&state, &timed_out).await;
    assert_eq!(closed.state, ComputeControlState::Failed);
    assert_eq!(closed.stage, "stopped");
    assert_eq!(closed.error_code, timed_out.error_code);
    assert_eq!(closed.error_message, timed_out.error_message);
    assert_eq!(closed.operation_id, timed_out.operation_id);
    assert_eq!(closed.request_id, timed_out.request_id);
    assert_eq!(
        closed.checkpoint["builder_restart_template"],
        timed_out.checkpoint["builder_restart_template"]
    );
    let evidence: BuilderRestartStopRecoveryEvidence =
        serde_json::from_value(closed.checkpoint[BUILDER_RESTART_STOP_OBSERVATION_KEY].clone())
            .unwrap();
    assert_eq!(evidence.original_error_code, timed_out.error_code);
    assert_eq!(evidence.original_error_message, timed_out.error_message);
    assert_eq!(
        evidence.unrebuildable.unwrap().reason,
        BuilderRestartUnrebuildableReason::ArchiveUnavailable
    );
    assert!(closed.lease.is_none());
    assert!(std::fs::read(&runtime.lock_path).unwrap().is_empty());
    assert_eq!(runtime.counts.stops.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.releases.load(Ordering::SeqCst), 1);
    let fresh = state
        .userapp_store
        .admit(&UserAppAdmission {
            app_id: closed.app_id.clone(),
            lifecycle_id: Some(closed.lifecycle_id.clone()),
            operation_id: "freshsource".into(),
            request_id: Some("freshsourcerequest".into()),
            request_fingerprint: "e".repeat(64),
            kind: UserAppOperationKind::EnsureBuilder,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    assert!(
        matches!(fresh, UserAppAdmissionOutcome::Accepted(_)),
        "the failed old recipe is not a global current Source admission gate"
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn unknown_create_response_keeps_stopped_recovery_lease_marker_and_never_starts() {
    let (state, runtime, original, _directory) = fixture().await;
    runtime.lose_create_response.store(true, Ordering::SeqCst);
    let results = scan_once(&state).await;
    assert_eq!(results.len(), 1);
    assert!(
        results[0]
            .as_ref()
            .unwrap_err()
            .to_string()
            .contains("Create response lost")
    );
    let unknown = current(&state, &original).await;
    assert_eq!(unknown.state, ComputeControlState::RecoveryRequired);
    assert_eq!(
        unknown.stage, "stopped",
        "existing Create precedes Starting CAS"
    );
    assert_eq!(unknown.lease, original.lease);
    assert_eq!(unknown.operation_id, original.operation_id);
    assert_eq!(runtime.counts.stops.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime.counts.creates.load(Ordering::SeqCst),
        1,
        "the actual controlled mutation already happened"
    );
    assert_eq!(runtime.counts.starts.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.counts.releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        std::fs::read(&runtime.lock_path).unwrap(),
        original.executor_id.as_ref().unwrap().as_bytes()
    );
    assert!(
        !crate::userapp_builder::compute_control::can_recover_original_docker_stop(&unknown),
        "a later missing recipe must not reinterpret an unknown Create as a pre-restore Stop"
    );
    let fd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&runtime.lock_path)
        .unwrap();
    fd.try_lock().unwrap(); // Drop unlocks only; original unknown marker stays.
    drop(fd);
    state.userapp_store_control.shutdown().await.unwrap();
}
