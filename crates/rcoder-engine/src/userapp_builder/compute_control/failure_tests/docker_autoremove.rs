//! Real Turso coordinator chains with controlled Docker AutoRemove observations.
//! These fixtures exercise archive/lease ordering, not an actual Docker daemon.
use super::*;

const OLD_UID: &str = "original-docker-container";
const NEW_UID: &str = "restored-docker-container";
const INVALID_ARCHIVE: &str = "original-docker-archive-is-invalid";

pub(super) struct Mode {
    store: Arc<dyn UserAppLifecycleStore>,
    volume: AppResourceIdentity,
    saved: Mutex<Option<BuilderRestartTemplate>>,
    restored: AtomicBool,
    omit_archive: AtomicBool,
    corrupt_archive: AtomicBool,
    restore_snapshots: Mutex<Vec<ComputeControlRecord>>,
    start_snapshots: Mutex<Vec<ComputeControlRecord>>,
}

pub(super) fn receipt(context: &UserAppExecutionContext) -> UserAppOperationLeaseReceipt {
    UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 17,
        inode: 23,
        token: context.executor_id.clone(),
    }
}

impl Mode {
    pub(super) fn target(
        &self,
        context: &UserAppExecutionContext,
        stopped: bool,
    ) -> BuilderControlTarget {
        let mut captured = target(context);
        captured.workload = if stopped && !self.restored.load(Ordering::SeqCst) {
            None
        } else {
            Some(AppResourceIdentity {
                kind: AppResourceKind::Container,
                name: "rcoder-app-builder-failureapp".into(),
                uid: if self.restored.load(Ordering::SeqCst) {
                    NEW_UID
                } else {
                    OLD_UID
                }
                .into(),
                resource_version: None,
            })
        };
        captured
    }

    pub(super) fn info(&self, context: &UserAppExecutionContext) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: NEW_UID.into(),
            container_name: "rcoder-app-builder-failureapp".into(),
            container_ip: "127.0.0.1".into(),
            internal_port: AGENT_FILE_SERVER_PORT,
            external_port: 0,
            project_id: context.app_id.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
            workload_uid: Some(NEW_UID.into()),
        }
    }

    pub(super) fn volumes(&self) -> Vec<AppResourceIdentity> {
        vec![self.volume.clone()]
    }

    async fn snapshot(&self, context: &UserAppExecutionContext) -> ComputeControlRecord {
        self.store
            .get_compute_control(&context.app_id, &context.operation_id)
            .await
            .unwrap()
            .unwrap()
    }

    pub(super) async fn archive(
        &self,
        source: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<BuilderRestartTemplate>> {
        assert_eq!(source.workload.as_ref().unwrap().uid, OLD_UID);
        let snapshot = self.snapshot(&source.context).await;
        assert_eq!(snapshot.stage, "draining_previous");
        assert_eq!(snapshot.lease, Some(receipt(&source.context)));
        if self.omit_archive.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let template = BuilderRestartTemplate {
            source: source.clone(),
            archive: AppResourceIdentity {
                kind: AppResourceKind::File,
                name: "original-builder-restart.json".into(),
                uid: "b".repeat(64),
                resource_version: None,
            },
            volumes: self.volumes(),
        };
        *self.saved.lock().unwrap() = Some(template.clone());
        Ok(Some(template))
    }

    pub(super) fn archive_valid(&self, template: &BuilderRestartTemplate) -> bool {
        !self.corrupt_archive.load(Ordering::SeqCst)
            && self.saved.lock().unwrap().as_ref() == Some(template)
    }

    pub(super) async fn restore(
        &self,
        template: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        if !self.archive_valid(template) {
            return Err(ContainerRuntimeError::Conflict(INVALID_ARCHIVE.into()));
        }
        let snapshot = self.snapshot(&template.source.context).await;
        assert_eq!(
            snapshot.stage, "stopped",
            "restore must precede Starting CAS"
        );
        assert!(matches!(
            snapshot.state,
            ComputeControlState::Running | ComputeControlState::RecoveryRequired
        ));
        assert_eq!(snapshot.lease, Some(receipt(&template.source.context)));
        assert_eq!(
            snapshot.checkpoint["builder_restart_template"],
            serde_json::to_value(template).unwrap(),
            "restore must consume the original durably captured archive"
        );
        assert_eq!(
            snapshot.checkpoint["builder_volumes"],
            serde_json::to_value(self.volumes()).unwrap()
        );
        assert_eq!(template.volumes, self.volumes());
        assert_eq!(template.source.workload.as_ref().unwrap().uid, OLD_UID);
        assert!(std::path::Path::new(&self.volume.uid).is_dir());
        self.restore_snapshots.lock().unwrap().push(snapshot);
        self.restored.store(true, Ordering::SeqCst);
        // Restoration creates only stopped compute; the separate start hook
        // below requires the real store's Starting CAS before any service starts.
        let mut restored = self.target(&template.source.context, false);
        restored.restart_image = template.source.restart_image.clone();
        restored.restart_runtime_workspace = template.source.restart_runtime_workspace.clone();
        restored.builder_startup_mode = template.source.builder_startup_mode;
        Ok(restored)
    }

    pub(super) async fn assert_start(&self, target: &BuilderControlTarget) {
        assert!(self.restored.load(Ordering::SeqCst));
        assert_eq!(target.workload.as_ref().unwrap().uid, NEW_UID);
        let snapshot = self.snapshot(&target.context).await;
        assert_eq!(snapshot.state, ComputeControlState::Running);
        assert_eq!(
            snapshot.stage, "starting",
            "start requires the real Starting CAS"
        );
        assert_eq!(snapshot.lease, Some(receipt(&target.context)));
        assert_eq!(
            snapshot.checkpoint["builder_volumes"],
            serde_json::to_value(self.volumes()).unwrap()
        );
        let captured: BuilderControlTarget =
            serde_json::from_value(snapshot.checkpoint.clone()).unwrap();
        assert_eq!(captured, *target);
        assert_eq!(
            snapshot.checkpoint["builder_restart_template"],
            serde_json::to_value(self.saved.lock().unwrap().as_ref().unwrap()).unwrap()
        );
        self.start_snapshots.lock().unwrap().push(snapshot);
    }
}

async fn fixture() -> (
    Arc<AppState>,
    Arc<Runtime>,
    Arc<Mode>,
    ComputeControlRecord,
    tempfile::TempDir,
) {
    let (state, runtime, pending, directory) = super::fixture().await;
    let workspace = directory.path().join("retained-workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("sentinel"), "retained-original-data").unwrap();
    let mode = Arc::new(Mode {
        store: state.userapp_store.clone(),
        volume: AppResourceIdentity {
            kind: AppResourceKind::HostPath,
            name: "/home/user/failureapp".into(),
            uid: workspace.to_str().unwrap().into(),
            resource_version: None,
        },
        saved: Default::default(),
        restored: AtomicBool::new(false),
        omit_archive: AtomicBool::new(false),
        corrupt_archive: AtomicBool::new(false),
        restore_snapshots: Default::default(),
        start_snapshots: Default::default(),
    });
    *runtime.docker.lock().unwrap() = Some(mode.clone());
    (state, runtime, mode, pending, directory)
}

fn assert_completed(
    runtime: &Runtime,
    mode: &Mode,
    pending: &ComputeControlRecord,
    completed: &ComputeControlRecord,
) {
    assert_eq!(completed.state, ComputeControlState::Succeeded);
    assert_eq!(completed.stage, "completed");
    assert_eq!(completed.operation_id, pending.operation_id);
    assert_eq!(completed.request_id, pending.request_id);
    assert_eq!(completed.generation, pending.generation);
    assert!(completed.lease.is_none());
    assert_eq!(runtime.calls.prepares.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 1);
    let restored = mode.restore_snapshots.lock().unwrap();
    let started = mode.start_snapshots.lock().unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(started.len(), 1);
    assert!(restored[0].revision < started[0].revision);
    assert_eq!(restored[0].lease, started[0].lease);
    assert_eq!(
        restored[0].lease.as_ref(),
        runtime.calls.receipt.lock().unwrap().as_ref()
    );
    assert_eq!(
        completed.checkpoint["builder_volumes"],
        serde_json::to_value(mode.volumes()).unwrap()
    );
    assert_eq!(
        std::fs::read_to_string(std::path::Path::new(&mode.volume.uid).join("sentinel")).unwrap(),
        "retained-original-data"
    );
}

#[tokio::test]
async fn docker_autoremove_restart_restores_original_archive_before_starting_commit() {
    let (state, runtime, mode, pending, _directory) = fixture().await;
    let result = execute_pending(&state, pending.clone()).await;
    assert!(
        result.is_ok(),
        "normal Docker AutoRemove restart must converge: {result:?}"
    );
    let completed = read(&state, &pending).await;
    assert_completed(&runtime, &mode, &pending, &completed);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn docker_autoremove_stopped_fault_recovers_same_operation_archive_and_lease() {
    let (state, runtime, mode, pending, _directory) = fixture().await;
    runtime.fail_stopped_capture.store(true, Ordering::SeqCst);
    let error = execute_pending(&state, pending.clone()).await.unwrap_err();
    assert!(format!("{error:#}").contains(STOPPED_FAILURE));
    let failure = read(&state, &pending).await;
    assert_failure(&failure, &pending, "stopped", STOPPED_FAILURE);
    assert_eq!(failure.lease, *runtime.calls.receipt.lock().unwrap());
    assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
    assert!(mode.restore_snapshots.lock().unwrap().is_empty());
    assert!(!mode.restored.load(Ordering::SeqCst));
    runtime.fail_stopped_capture.store(false, Ordering::SeqCst);
    // Invoke the real continuation used by public recover and the recovery
    // scanner, with the original stored snapshot and physical receipt.
    let resumed = resume_restart_start(&state, &failure).await;
    assert!(
        resumed.is_ok(),
        "same Docker operation must resume after fault removal: {resumed:?}"
    );
    let completed = read(&state, &pending).await;
    assert_completed(&runtime, &mode, &pending, &completed);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn docker_autoremove_missing_or_invalid_archive_never_creates_or_starts() {
    for missing in [true, false] {
        let (state, runtime, mode, pending, _directory) = fixture().await;
        mode.omit_archive.store(missing, Ordering::SeqCst);
        mode.corrupt_archive.store(!missing, Ordering::SeqCst);
        assert!(execute_pending(&state, pending.clone()).await.is_err());
        let failed = read(&state, &pending).await;
        assert_eq!(failed.state, ComputeControlState::RecoveryRequired);
        assert_eq!(failed.stage, "stopped");
        assert_eq!(failed.operation_id, pending.operation_id);
        assert_eq!(failed.request_id, pending.request_id);
        assert_eq!(failed.lease, *runtime.calls.receipt.lock().unwrap());
        assert!(resume_restart_start(&state, &failed).await.is_err());
        assert_eq!(read(&state, &pending).await, failed);
        assert_eq!(runtime.calls.stops.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.calls.starts.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.calls.releases.load(Ordering::SeqCst), 0);
        assert!(!mode.restored.load(Ordering::SeqCst));
        assert!(mode.restore_snapshots.lock().unwrap().is_empty());
        state.userapp_store_control.shutdown().await.unwrap();
    }
}
