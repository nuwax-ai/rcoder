use super::*;
use container_runtime_api::{ContainerRuntimeError, ContainerRuntimeResult};
use shared_types::*;
use shared_types::{AppResourceIdentity, BuilderStartupMode, UserAppExecutionContext};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn original() -> BuilderControlTarget {
    BuilderControlTarget {
        context: UserAppExecutionContext {
            app_id: "archiveapp".into(),
            lifecycle_id: "life1".into(),
            operation_id: "restart1".into(),
            executor_id: "executor1".into(),
            request_fingerprint: "a".repeat(64),
        },
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::Container,
            name: "owned-builder".into(),
            uid: "original-container".into(),
            resource_version: None,
        }),
        resource_binding: None,
        pod: None,
        restart_image: Some("runner:captured".into()),
        restart_runtime_workspace: None,
        builder_startup_mode: BuilderStartupMode::ManagementOnly,
    }
}

#[test]
fn recapture_accepts_only_the_same_retained_physical_resource_and_frozen_intent() {
    let old = original();
    verify_retained(&old, &old, &old.context).unwrap();
    for mutation in 0..7 {
        let mut changed = old.clone();
        match mutation {
            0 => changed.workload = None,
            1 => changed.workload.as_mut().unwrap().uid = "replacement".into(),
            2 => changed.workload.as_mut().unwrap().name = "another-name".into(),
            3 => changed.context.lifecycle_id = "another-life".into(),
            4 => changed.context.operation_id = "another-operation".into(),
            5 => changed.restart_image = Some("runner:latest-recipe".into()),
            6 => changed.builder_startup_mode = BuilderStartupMode::PreserveBusinessIntent,
            _ => unreachable!(),
        }
        assert!(
            verify_retained(&old, &changed, &old.context).is_err(),
            "mutation {mutation}"
        );
    }
}

struct Runtime {
    target: Mutex<BuilderControlTarget>,
    volumes: Vec<AppResourceIdentity>,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    recaptures: AtomicUsize,
    restores: AtomicUsize,
    probe: Option<bool>,
    probe_error: bool,
    stopped: bool,
    gate: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Semaphore>)>,
}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for Runtime {
    async fn validate_app_operation_receipt(
        &self,
        _: &UserAppExecutionContext,
        _: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        Ok(true)
    }
}
#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: container_runtime_api::ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("no creation before archive CAS")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("unexpected lookup")
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::RuntimeContainerInfo>> {
        panic!("unexpected discovery")
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("no new stop")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        panic!("unexpected probe")
    }
    async fn list_containers(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::RuntimeContainerInfo>> {
        panic!("unexpected list")
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("no unrelated cleanup")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        panic!("unexpected health")
    }
    async fn current_builder_image(&self) -> ContainerRuntimeResult<Option<String>> {
        panic!("no current recipe may replace the frozen image")
    }
    async fn capture_bound_builder_control(
        &self,
        _: &UserAppExecutionContext,
        _: Option<&UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        Ok(self.target.lock().unwrap().clone())
    }
    async fn capture_builder_compute_volumes(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Vec<AppResourceIdentity>> {
        Ok(self.volumes.clone())
    }
    async fn reconcile_builder_compute_stop(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self.stopped)
    }
    async fn inspect_builder_restart_archive(
        &self,
        _: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<Option<bool>> {
        if self.probe_error {
            return Err(ContainerRuntimeError::DockerError(
                "permission denied reading private archive".into(),
            ));
        }
        Ok(self.probe)
    }
    async fn recapture_stopped_builder_restart(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<BuilderRestartTemplate> {
        self.recaptures.fetch_add(1, Ordering::SeqCst);
        if let Some((entered, release)) = &self.gate {
            entered.notify_one();
            release.clone().acquire_owned().await.unwrap().forget();
        }
        Ok(BuilderRestartTemplate {
            source: target.clone(),
            archive: AppResourceIdentity {
                kind: AppResourceKind::File,
                name: "captured.private.new".into(),
                uid: "new-content-sha".into(),
                resource_version: None,
            },
            volumes: self.volumes.clone(),
        })
    }
    async fn restore_builder_restart(
        &self,
        template: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
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
            record.checkpoint["builder_restart_template"],
            serde_json::to_value(template).unwrap(),
            "restore must follow the durable archive CAS"
        );
        self.restores.fetch_add(1, Ordering::SeqCst);
        let mut restored = template.source.clone();
        restored.workload.as_mut().unwrap().uid = "restored-physical".into();
        restored.resource_binding = None;
        Ok(restored)
    }
}

async fn fixture() -> (
    Runtime,
    ComputeControlRecord,
    BuilderControlTarget,
    tempfile::TempDir,
) {
    fixture_with_reference(false).await
}

async fn fixture_with_reference(
    valid_reference: bool,
) -> (
    Runtime,
    ComputeControlRecord,
    BuilderControlTarget,
    tempfile::TempDir,
) {
    fixture_with_archive(valid_reference, false).await
}

async fn fixture_with_archive(
    valid_reference: bool,
    mismatched_volume: bool,
) -> (
    Runtime,
    ComputeControlRecord,
    BuilderControlTarget,
    tempfile::TempDir,
) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("archive.db"),
        )
        .await
        .unwrap(),
    );
    let app = store.ensure_identity("archiveapp").await.unwrap();
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: "restart1".into(),
            request_id: "restartrequest".into(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Restart,
            restart_image_roll: false,
            builder_startup_mode: BuilderStartupMode::ManagementOnly,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: app.app_id,
        lifecycle_id: app.lifecycle_id,
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "executor1".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let lease = UserAppOperationLeaseReceipt::Docker {
        service_type: ServiceType::UserappBuilder,
        device: 1,
        inode: 2,
        token: "lease-token".into(),
    };
    let mut record = store.bind_compute_lease(&identity, &lease).await.unwrap();
    let mut target = original();
    target.context = record.execution_context().unwrap();
    let volumes = vec![AppResourceIdentity {
        kind: AppResourceKind::HostPath,
        name: "/owned/workspace".into(),
        uid: "original-volume".into(),
        resource_version: None,
    }];
    let mut checkpoint = serde_json::to_value(&target).unwrap();
    checkpoint["builder_volumes"] = serde_json::to_value(&volumes).unwrap();
    checkpoint["builder_restart_template"] = if valid_reference {
        let mut archived_volumes = volumes.clone();
        if mismatched_volume {
            archived_volumes[0].uid = "foreign-volume".into();
        }
        serde_json::to_value(BuilderRestartTemplate {
            source: target.clone(),
            archive: AppResourceIdentity {
                kind: AppResourceKind::File,
                name: "old.private".into(),
                uid: "old-sha".into(),
                resource_version: None,
            },
            volumes: archived_volumes,
        })
        .unwrap()
    } else {
        serde_json::Value::Null
    };
    for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped] {
        record = store
            .advance_compute_control(&ComputeControlProgress {
                identity: identity.clone(),
                expected_revision: record.revision,
                state: ComputeControlState::Running,
                stage,
                checkpoint: checkpoint.clone(),
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
    }
    (
        Runtime {
            target: Mutex::new(target.clone()),
            volumes,
            store,
            recaptures: AtomicUsize::new(0),
            restores: AtomicUsize::new(0),
            probe: Some(false),
            probe_error: false,
            stopped: true,
            gate: None,
        },
        record,
        target,
        directory,
    )
}

#[tokio::test]
async fn stopped_archive_recapture_commits_reference_before_restore_and_keeps_history_through_starting()
 {
    let (runtime, record, old, _directory) = fixture().await;
    let (captured, template) = prepare(&runtime, runtime.store.as_ref(), &record, &old, &old)
        .await
        .unwrap();
    assert_eq!(captured.operation_id, record.operation_id);
    assert_eq!(captured.stage, "stopped");
    assert_eq!(captured.lease, record.lease);
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    assert_eq!(
        captured.checkpoint["builder_restart_template_history"][0]["value"],
        serde_json::Value::Null
    );
    assert_eq!(
        captured.checkpoint["builder_restart_template_history"][0]["was_present"],
        true
    );
    let mut retry_runtime = runtime;
    retry_runtime.probe = Some(true);
    let (retry, reused) = prepare(
        &retry_runtime,
        retry_runtime.store.as_ref(),
        &captured,
        &old,
        &old,
    )
    .await
    .unwrap();
    assert_eq!(retry, captured);
    assert_eq!(reused, template);
    assert_eq!(
        retry_runtime.recaptures.load(Ordering::SeqCst),
        1,
        "retry must keep its saved private content"
    );
    let restored = retry_runtime
        .restore_builder_restart(&template)
        .await
        .unwrap();
    let starting = retry_runtime
        .store
        .resume_builder_restart_start(&captured, &restored)
        .await
        .unwrap();
    assert_eq!(starting.stage, "starting");
    assert_eq!(
        starting.checkpoint["builder_restart_template_history"],
        captured.checkpoint["builder_restart_template_history"]
    );
    assert_eq!(starting.operation_id, record.operation_id);
    assert_eq!(starting.lease, record.lease);
    retry_runtime.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn late_stop_cas_rejects_archive_recapture_without_restoring_or_rewriting_old_reference() {
    let (mut runtime, record, old, _directory) = fixture().await;
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    runtime.gate = Some((entered.clone(), release.clone()));
    let runtime = Arc::new(runtime);
    let recovering = tokio::spawn({
        let runtime = runtime.clone();
        let record = record.clone();
        let old = old.clone();
        async move {
            prepare(
                runtime.as_ref(),
                runtime.store.as_ref(),
                &record,
                &old,
                &old,
            )
            .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(3), entered.notified())
        .await
        .unwrap();
    runtime
        .store
        .admit_compute_control(&ComputeControlRequest {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            scope: UserAppOperationScope::Dev,
            operation_id: "laterstop".into(),
            request_id: "laterstoprequest".into(),
            request_fingerprint: "b".repeat(64),
            action: ComputeControlAction::Stop,
            restart_image_roll: false,
            builder_startup_mode: Default::default(),
        })
        .await
        .unwrap();
    release.add_permits(1);
    let error = recovering.await.unwrap().unwrap_err();
    assert!(matches!(
        error.downcast_ref::<UserAppStoreError>(),
        Some(UserAppStoreError::VersionConflict)
    ));
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    let unchanged = runtime
        .store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(unchanged.checkpoint, record.checkpoint);
    runtime.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn archive_io_errors_and_unconfirmed_stops_never_recapture_or_commit_a_new_reference() {
    for io_error in [false, true] {
        let (mut runtime, record, old, _directory) = fixture_with_reference(io_error).await;
        if io_error {
            // A structurally valid reference allows the actual backend probe;
            // permission/transport errors cannot be recast as missing bytes.
            runtime.probe_error = true;
        } else {
            runtime.stopped = false;
        }
        assert!(
            prepare(&runtime, runtime.store.as_ref(), &record, &old, &old)
                .await
                .is_err()
        );
        assert_eq!(runtime.recaptures.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
        assert_eq!(
            runtime
                .store
                .get_compute_control(&record.app_id, &record.operation_id)
                .await
                .unwrap()
                .unwrap()
                .revision,
            record.revision
        );
        runtime.store.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn observed_bad_payload_replaces_only_its_reference_and_preserves_original_metadata() {
    let (runtime, record, old, _directory) = fixture_with_reference(true).await;
    let original_reference = record.checkpoint["builder_restart_template"].clone();
    let (captured, template) = prepare(&runtime, runtime.store.as_ref(), &record, &old, &old)
        .await
        .unwrap();
    assert_ne!(
        captured.checkpoint["builder_restart_template"],
        original_reference
    );
    assert_eq!(
        captured.checkpoint["builder_restart_template_history"][0]["value"],
        original_reference
    );
    assert_eq!(template.source, old);
    assert_eq!(template.volumes, runtime.volumes);
    assert_eq!(runtime.recaptures.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    runtime.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn starting_unknown_write_never_recaptures_a_private_archive() {
    let (runtime, record, old, _directory) = fixture().await;
    let starting = runtime
        .store
        .resume_builder_restart_start(&record, &old)
        .await
        .unwrap();
    assert_eq!(starting.stage, "starting");
    assert!(
        prepare(&runtime, runtime.store.as_ref(), &starting, &old, &old)
            .await
            .is_err()
    );
    assert_eq!(runtime.recaptures.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    assert_eq!(
        runtime
            .store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap()
            .unwrap(),
        starting
    );
    runtime.store.shutdown().await.unwrap();
}

#[tokio::test]
async fn typed_archive_with_foreign_volume_cannot_bypass_original_volume_witness() {
    let (mut runtime, record, old, _directory) = fixture_with_archive(true, true).await;
    runtime.probe = Some(true);
    let prior = record.checkpoint["builder_restart_template"].clone();
    let (captured, template) = prepare(&runtime, runtime.store.as_ref(), &record, &old, &old)
        .await
        .unwrap();
    assert_eq!(
        runtime.recaptures.load(Ordering::SeqCst),
        1,
        "typed metadata cannot validate a foreign volume reference"
    );
    assert_eq!(template.volumes, runtime.volumes);
    assert_eq!(
        captured.checkpoint["builder_restart_template_history"][0]["value"],
        prior
    );
    assert_eq!(runtime.restores.load(Ordering::SeqCst), 0);
    runtime.store.shutdown().await.unwrap();
}
