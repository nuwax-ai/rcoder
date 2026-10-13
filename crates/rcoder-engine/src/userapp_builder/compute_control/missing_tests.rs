//! The original engine continuation against a real store, without side creation.
use super::*;
use crate::userapp_builder::creation::fence_settler_tests::runtime_state;
use container_runtime_api::{
    AgentContainerRuntime, ContainerRuntimeResult, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct Runtime {
    actual: Mutex<BuilderControlTarget>,
    volumes: Mutex<Vec<AppResourceIdentity>>,
    creates: AtomicUsize,
    releases: AtomicUsize,
    completion: AtomicBool,
    retry_partial: AtomicBool,
    held: AtomicBool,
    original_lease: UserAppOperationLeaseReceipt,
    original_identity_retained: AtomicBool,
    observation_reads: AtomicUsize,
    change_identity_during_observation: AtomicBool,
    retries: AtomicUsize,
    stopped: AtomicBool,
    call: Mutex<
        Option<(
            BuilderControlTarget,
            Vec<AppResourceIdentity>,
            UserAppOperationLeaseReceipt,
        )>,
    >,
}
impl Runtime {
    fn info(&self) -> Option<ContainerBasicInfo> {
        let target = self.actual.lock().unwrap();
        let workload = target.workload.as_ref()?;
        let pod = target.pod.as_ref()?;
        Some(ContainerBasicInfo {
            container_id: pod.uid.clone(),
            container_name: pod.name.clone(),
            container_ip: "127.0.0.1".into(),
            internal_port: AGENT_FILE_SERVER_PORT,
            external_port: 0,
            project_id: target.context.app_id.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: String::new(),
            workload_uid: Some(workload.uid.clone()),
        })
    }
    fn make_current(&self) {
        let mut target = self.actual.lock().unwrap();
        target.workload = Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "builder-current".into(),
            uid: "real-new-sts-uid".into(),
            resource_version: Some("10".into()),
        });
        target.pod = Some(BuilderPodIdentity {
            name: "builder-current-0".into(),
            uid: "real-new-pod-uid".into(),
            resource_version: "11".into(),
        });
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for Runtime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for Runtime {
    // 该 fixture 只有 dev builder 和真实生命周期 store，没有任何生产 Deployment。
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
    async fn validate_app_operation_receipt(
        &self,
        _: &UserAppExecutionContext,
        _: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self.held.load(Ordering::SeqCst))
    }
    async fn observe_app_operation_lease(
        &self,
        context: &UserAppExecutionContext,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<container_runtime_api::AppOperationLeaseObservation> {
        use container_runtime_api::AppOperationLeaseObservation;
        let read = self.observation_reads.fetch_add(1, Ordering::SeqCst);
        if !self.original_identity_retained.load(Ordering::SeqCst)
            || (read > 0
                && self
                    .change_identity_during_observation
                    .load(Ordering::SeqCst))
            || *context != self.actual.lock().unwrap().context
            || *receipt != self.original_lease
        {
            return Ok(AppOperationLeaseObservation::Unconfirmed);
        }
        Ok(if self.held.load(Ordering::SeqCst) {
            AppOperationLeaseObservation::Held
        } else {
            AppOperationLeaseObservation::ExpiredOriginalIdentity
        })
    }
    async fn release_app_operation_receipt(
        &self,
        _: &UserAppExecutionContext,
        _: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<()> {
        self.releases.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn cleanup_builder_restart_archive(
        &self,
        _: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<()> {
        panic!("opaque history must be retained")
    }
}
#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: container_runtime_api::ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("no nested EnsureBuilder")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("no unrelated lookup")
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::RuntimeContainerInfo>> {
        panic!("no unrelated discovery")
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("no side stop")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        panic!("no unrelated probe")
    }
    async fn list_containers(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("data must be retained")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn inspect_builder_candidate(
        &self,
        _: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        Ok(self.actual.lock().unwrap().clone())
    }
    async fn capture_builder_control(
        &self,
        _: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        Ok(self.actual.lock().unwrap().clone())
    }
    async fn capture_bound_builder_control(
        &self,
        _: &UserAppExecutionContext,
        _: Option<&UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        Ok(self.actual.lock().unwrap().clone())
    }
    async fn capture_builder_compute_volumes(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Vec<AppResourceIdentity>> {
        Ok(self.volumes.lock().unwrap().clone())
    }
    fn supports_builder_compute_fencing(&self) -> bool {
        true
    }
    async fn ensure_absent_builder_control(
        &self,
        target: &BuilderControlTarget,
        volumes: &[AppResourceIdentity],
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        *self.call.lock().unwrap() = Some((target.clone(), volumes.to_vec(), receipt.clone()));
        assert!(self.held.load(Ordering::SeqCst));
        assert_eq!(self.releases.load(Ordering::SeqCst), 0);
        self.make_current();
        self.completion.store(true, Ordering::SeqCst);
        Ok(self.info())
    }
    async fn reconcile_builder_compute_start(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(self
            .completion
            .load(Ordering::SeqCst)
            .then(|| self.info())
            .flatten())
    }
    async fn prepare_builder_compute_retry(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<BuilderControlTarget>> {
        self.retries.fetch_add(1, Ordering::SeqCst);
        if !self.retry_partial.load(Ordering::SeqCst) {
            return Ok(None);
        }
        let actual = self.actual.lock().unwrap();
        if actual.context != target.context || actual.workload.is_none() {
            return Err(container_runtime_api::ContainerRuntimeError::Conflict(
                "partial compute scope changed".into(),
            ));
        }
        Ok(Some(target.clone()))
    }
    async fn reconcile_builder_compute_stop(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<bool> {
        Ok(self.stopped.load(Ordering::SeqCst))
    }
    async fn builder_compute_write_acknowledged(
        &self,
        _: &BuilderControlTarget,
        restart: bool,
    ) -> ContainerRuntimeResult<bool> {
        assert!(restart);
        Ok(self.completion.load(Ordering::SeqCst))
    }
    async fn observe_completed_builder_creation(
        &self,
        target: &BuilderControlTarget,
        receipt: &UserAppOperationLeaseReceipt,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        if *receipt != self.original_lease || target.context != self.actual.lock().unwrap().context
        {
            return Err(container_runtime_api::ContainerRuntimeError::Conflict(
                "original creation receipt identity differs".into(),
            ));
        }
        self.reconcile_builder_compute_start(target).await
    }
    async fn start_builder_control(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("absent compute must use the original controlled factory")
    }
    async fn restore_builder_restart(
        &self,
        _: &BuilderRestartTemplate,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        panic!("history must not replace current compute/data")
    }
}

async fn fixture(
    previous: bool,
    starting: bool,
) -> (
    Arc<AppState>,
    Arc<Runtime>,
    ComputeControlRecord,
    tempfile::TempDir,
) {
    fixture_with_witness(previous, starting, None).await
}

async fn fixture_with_witness(
    previous: bool,
    starting: bool,
    invalid_witness: Option<usize>,
) -> (
    Arc<AppState>,
    Arc<Runtime>,
    ComputeControlRecord,
    tempfile::TempDir,
) {
    fixture_with_intent(
        previous,
        starting,
        invalid_witness,
        BuilderStartupMode::ManagementOnly,
        true,
    )
    .await
}

async fn fixture_with_intent(
    previous: bool,
    starting: bool,
    invalid_witness: Option<usize>,
    mode: BuilderStartupMode,
    roll_image: bool,
) -> (
    Arc<AppState>,
    Arc<Runtime>,
    ComputeControlRecord,
    tempfile::TempDir,
) {
    fixture_with_canonical_binding(previous, starting, invalid_witness, mode, roll_image, false)
        .await
}

async fn fixture_with_canonical_binding(
    previous: bool,
    starting: bool,
    invalid_witness: Option<usize>,
    mode: BuilderStartupMode,
    roll_image: bool,
    canonical_binding: bool,
) -> (
    Arc<AppState>,
    Arc<Runtime>,
    ComputeControlRecord,
    tempfile::TempDir,
) {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &dir.path().join("missing.db"),
        )
        .await
        .unwrap(),
    );
    let app = store.ensure_identity("missingapp").await.unwrap();
    if canonical_binding {
        let input = UserAppExecutionInput::new(
            serde_json::to_string(&AdoptBuilderRequest {
                lifecycle_id: app.lifecycle_id.clone(),
                request_id: "previous-adoption".into(),
                expected_container_id: "real-new-sts-uid".into(),
            })
            .unwrap(),
        );
        let admitted = store
            .admit_with_input(
                &UserAppAdmission {
                    runtime_policy_on_success: None,
                    command: None,
                    metadata: None,
                    app_id: app.app_id.clone(),
                    lifecycle_id: Some(app.lifecycle_id.clone()),
                    operation_id: "previous-adoption".into(),
                    request_id: Some("previous-adoption".into()),
                    request_fingerprint: input.digest(),
                    kind: UserAppOperationKind::AdoptBuilder,
                },
                Some(&input),
            )
            .await
            .unwrap();
        let accepted = match admitted {
            UserAppAdmissionOutcome::Accepted(r) | UserAppAdmissionOutcome::Existing(r) => r,
        };
        let progress = UserAppOperationProgress {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id.clone(),
            operation_id: accepted.operation_id.clone(),
            expected_revision: accepted.revision,
            executor_id: "previous-adopter".into(),
            state: UserAppOperationState::Running,
            step: "verified".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        };
        let running = store.advance(&progress).await.unwrap();
        store
            .commit_resource_binding(
                &UserAppResourceBinding {
                    app_id: app.app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    service_type: ServiceType::UserappBuilder,
                    physical_uid: "real-new-sts-uid".into(),
                    adopted_by_operation: running.operation_id.clone(),
                },
                &UserAppOperationProgress {
                    expected_revision: running.revision,
                    state: UserAppOperationState::Succeeded,
                    ..progress
                },
            )
            .await
            .unwrap();
    }
    let pending = store
        .admit_compute_control(&ComputeControlRequest {
            app_id: app.app_id.clone(),
            lifecycle_id: app.lifecycle_id,
            scope: UserAppOperationScope::Dev,
            operation_id: "original-restart".into(),
            request_id: "original-request".into(),
            request_fingerprint: "a".repeat(64),
            action: ComputeControlAction::Restart,
            restart_image_roll: roll_image,
            builder_startup_mode: mode,
        })
        .await
        .unwrap();
    let identity = ComputeExecutorIdentity {
        app_id: pending.app_id.clone(),
        lifecycle_id: pending.lifecycle_id.clone(),
        scope: pending.scope,
        operation_id: pending.operation_id.clone(),
        generation: pending.generation,
        executor_id: "original-executor".into(),
    };
    store
        .claim_compute_control(&identity, pending.revision)
        .await
        .unwrap();
    let receipt = UserAppOperationLeaseReceipt::Kubernetes {
        service_type: ServiceType::UserappBuilder,
        namespace: "test-ns".into(),
        name: "original-lease".into(),
        uid: "lease-uid".into(),
        resource_version: "2".into(),
        token: "original-executor".into(),
    };
    let mut record = store.bind_compute_lease(&identity, &receipt).await.unwrap();
    let absent = BuilderControlTarget {
        context: record.execution_context().unwrap(),
        workload: None,
        pod: None,
        resource_binding: None,
        restart_image: Some("runner:frozen".into()),
        restart_runtime_workspace: None,
        builder_startup_mode: mode,
    };
    let volumes = vec![AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: "workspace".into(),
        uid: "real-original-pvc-uid".into(),
        resource_version: None,
    }];
    let mut source = absent.clone();
    if previous {
        source.workload = Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: "previous-builder".into(),
            uid: "old-real-sts-uid".into(),
            resource_version: Some("4".into()),
        });
        source.restart_runtime_workspace =
            Some(paths::userapp_dev_workspace(&source.context.app_id));
    }
    let mut checkpoint = serde_json::to_value(&source).unwrap();
    let mut recorded_volumes = volumes.clone();
    match invalid_witness {
        Some(0) => recorded_volumes.clear(),
        Some(1) => recorded_volumes[0].kind = AppResourceKind::HostPath,
        Some(2) => recorded_volumes[0].uid.clear(),
        Some(3) => recorded_volumes.push(recorded_volumes[0].clone()),
        None => {}
        _ => unreachable!(),
    }
    checkpoint["builder_volumes"] = serde_json::to_value(&recorded_volumes).unwrap();
    checkpoint["builder_restart_template"] =
        serde_json::json!({"old_corrupt_diagnostic":"retained"});
    if previous {
        checkpoint["builder_compute_single_write"] = serde_json::json!(true);
    }
    for stage in [ComputeControlStage::Stopping, ComputeControlStage::Stopped]
        .into_iter()
        .chain(starting.then_some(ComputeControlStage::Starting))
    {
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
    record = store
        .advance_compute_control(&ComputeControlProgress {
            identity,
            expected_revision: record.revision,
            state: ComputeControlState::RecoveryRequired,
            stage: if starting {
                ComputeControlStage::Starting
            } else {
                ComputeControlStage::Stopped
            },
            checkpoint,
            error_code: Some("ORIGINAL_ERROR".into()),
            error_message: Some("original diagnostic remains until confirmed continuation".into()),
        })
        .await
        .unwrap();
    let runtime = Arc::new(Runtime {
        actual: Mutex::new(absent),
        volumes: Mutex::new(volumes),
        creates: AtomicUsize::new(0),
        releases: AtomicUsize::new(0),
        completion: AtomicBool::new(false),
        retry_partial: AtomicBool::new(false),
        held: AtomicBool::new(true),
        original_lease: receipt,
        original_identity_retained: AtomicBool::new(true),
        observation_reads: AtomicUsize::new(0),
        change_identity_during_observation: AtomicBool::new(false),
        retries: AtomicUsize::new(0),
        stopped: AtomicBool::new(false),
        call: Mutex::new(None),
    });
    let state = runtime_state(runtime.clone(), store, None).await;
    (state, runtime, record, dir)
}

#[tokio::test]
async fn missing_workload_original_stopped_restart_ensures_same_pvc_without_an_archive() {
    for previous in [false, true] {
        let (state, runtime, record, _dir) = fixture(previous, false).await;
        resume_restart_start(&state, &record).await.unwrap();
        let result = state
            .userapp_store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.state, ComputeControlState::Succeeded);
        assert_eq!(result.operation_id, record.operation_id);
        assert_eq!(result.request_id, record.request_id);
        assert_eq!(result.generation, record.generation);
        assert_eq!(
            result.checkpoint["builder_volumes"],
            record.checkpoint["builder_volumes"]
        );
        assert_eq!(result.checkpoint["workload"]["uid"], "real-new-sts-uid");
        assert!(
            result
                .checkpoint
                .get("builder_compute_single_write")
                .is_none()
        );
        assert_eq!(
            result.checkpoint["builder_restart_template_history"][0],
            record.checkpoint["builder_restart_template"]
        );
        assert!(result.checkpoint.get("builder_restart_template").is_none());
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 1);
        assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
        let call = runtime.call.lock().unwrap().clone().unwrap();
        assert_eq!(call.0.context, record.execution_context().unwrap());
        assert_eq!(Some(call.2), record.lease);
        assert!(
            call.0.workload.is_none() && call.0.pod.is_none() && call.0.resource_binding.is_none()
        );
        assert_eq!(call.0.restart_image.as_deref(), Some("runner:frozen"));
        assert_eq!(
            call.0.builder_startup_mode,
            BuilderStartupMode::ManagementOnly
        );
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn missing_workload_unknown_starting_never_replays_the_original_create() {
    let (state, runtime, record, _dir) = fixture(false, true).await;
    assert!(resume_restart_start(&state, &record).await.is_err());
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        state
            .userapp_store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap(),
        Some(record)
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_workload_completed_original_creation_is_observed_without_a_second_post() {
    let (state, runtime, record, _dir) = fixture(false, true).await;
    runtime.make_current();
    runtime.completion.store(true, Ordering::SeqCst);
    resume_restart_start(&state, &record).await.unwrap();
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    let result = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, ComputeControlState::Succeeded);
    assert_eq!(result.operation_id, record.operation_id);
    assert_eq!(
        result.checkpoint["builder_volumes"],
        record.checkpoint["builder_volumes"]
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_workload_replacement_pvc_or_foreign_current_resource_retains_original_failure() {
    for changed in 0..4 {
        let (state, runtime, record, _dir) = fixture(true, false).await;
        match changed {
            0 => runtime.volumes.lock().unwrap()[0].uid = "replacement-pvc".into(),
            1 => runtime.actual.lock().unwrap().context.lifecycle_id = "foreign-life".into(),
            2 => runtime.held.store(false, Ordering::SeqCst),
            3 => runtime.make_current(),
            _ => unreachable!(),
        }
        assert!(resume_restart_start(&state, &record).await.is_err());
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
        assert_eq!(
            state
                .userapp_store
                .get_compute_control(&record.app_id, &record.operation_id)
                .await
                .unwrap(),
            Some(record)
        );
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn missing_workload_superseded_completed_pipeline_drains_with_compute_now_absent() {
    for completed in [false, true] {
        let (state, runtime, record, _dir) = fixture(false, true).await;
        state
            .userapp_store
            .admit_compute_control(&ComputeControlRequest {
                app_id: record.app_id.clone(),
                lifecycle_id: record.lifecycle_id.clone(),
                scope: record.scope,
                operation_id: "current-stop".into(),
                request_id: "current-stop-request".into(),
                request_fingerprint: "b".repeat(64),
                action: ComputeControlAction::Stop,
                restart_image_roll: false,
                builder_startup_mode: Default::default(),
            })
            .await
            .unwrap();
        let superseded = state
            .userapp_store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap()
            .unwrap();
        runtime.completion.store(completed, Ordering::SeqCst);
        let result = reconcile_superseded(&state, &superseded).await;
        if completed {
            let drained = result.unwrap();
            assert_eq!(drained.stage, "superseded_drained");
            assert_eq!(drained.state, ComputeControlState::Failed);
            assert_eq!(drained.lease, record.lease);
        } else {
            assert!(result.is_err());
            assert_eq!(
                state
                    .userapp_store
                    .get_compute_control(&record.app_id, &record.operation_id)
                    .await
                    .unwrap(),
                Some(superseded)
            );
        }
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn missing_workload_unknown_previous_sts_starting_is_not_reclassified_as_absence() {
    let (state, runtime, record, _dir) = fixture(true, true).await;
    assert!(resume_restart_start(&state, &record).await.is_err());
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        state
            .userapp_store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap(),
        Some(record)
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_workload_invalid_retained_witness_cannot_dispatch_creation() {
    for variant in 0..4 {
        let (state, runtime, record, _dir) =
            fixture_with_witness(false, false, Some(variant)).await;
        assert!(resume_restart_start(&state, &record).await.is_err());
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
        assert_eq!(
            state
                .userapp_store
                .get_compute_control(&record.app_id, &record.operation_id)
                .await
                .unwrap(),
            Some(record)
        );
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn missing_workload_starting_owned_partial_finishes_under_the_original_cas_and_lease() {
    let (state, runtime, record, _dir) = fixture(false, true).await;
    runtime.make_current();
    runtime.actual.lock().unwrap().pod = None;
    runtime.retry_partial.store(true, Ordering::SeqCst);
    resume_restart_start(&state, &record).await.unwrap();
    let result = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, ComputeControlState::Succeeded);
    assert_eq!(result.operation_id, record.operation_id);
    assert_eq!(result.request_id, record.request_id);
    assert_eq!(result.generation, record.generation);
    assert_eq!(
        result.checkpoint["builder_volumes"],
        record.checkpoint["builder_volumes"]
    );
    assert_eq!(result.checkpoint["workload"]["uid"], "real-new-sts-uid");
    assert!(
        result
            .checkpoint
            .get("builder_compute_single_write")
            .is_none()
    );
    assert_eq!(
        runtime.creates.load(Ordering::SeqCst),
        1,
        "one factory continuation, no nested creation"
    );
    let call = runtime.call.lock().unwrap().clone().unwrap();
    assert_eq!(call.0.context, record.execution_context().unwrap());
    assert!(call.0.workload.is_none());
    assert_eq!(Some(call.2), record.lease);
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn missing_workload_preserve_no_image_roll_reuses_its_frozen_recipe_on_partial_resume() {
    let (state, runtime, record, _dir) = fixture_with_intent(
        false,
        true,
        None,
        BuilderStartupMode::PreserveBusinessIntent,
        false,
    )
    .await;
    runtime.make_current();
    runtime.actual.lock().unwrap().pod = None;
    runtime.retry_partial.store(true, Ordering::SeqCst);
    resume_restart_start(&state, &record).await.unwrap();
    let call = runtime.call.lock().unwrap().clone().unwrap();
    assert_eq!(call.0.restart_image.as_deref(), Some("runner:frozen"));
    assert_eq!(
        call.0.builder_startup_mode,
        BuilderStartupMode::PreserveBusinessIntent
    );
    let result = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, ComputeControlState::Succeeded);
    assert_eq!(
        result.checkpoint["restart_image"],
        record.checkpoint["restart_image"]
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_missing_builder_with_expired_original_lease_finishes_without_another_start() {
    let (state, runtime, record, _dir) = fixture(false, true).await;
    runtime.make_current();
    runtime.completion.store(true, Ordering::SeqCst);
    runtime.held.store(false, Ordering::SeqCst);

    let observed = resume_restart_start(&state, &record).await;
    assert!(
        observed.is_ok(),
        "the original completed creation must remain observable after lease expiry: {observed:?}"
    );
    let result = state
        .userapp_store
        .get_compute_control(&record.app_id, &record.operation_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.state, ComputeControlState::Succeeded);
    assert_eq!(result.operation_id, record.operation_id);
    assert_eq!(result.executor_id, record.executor_id);
    assert_eq!(result.generation, record.generation);
    assert_eq!(result.request_id, record.request_id);
    assert_eq!(result.request_fingerprint, record.request_fingerprint);
    assert_eq!(
        result.checkpoint["builder_volumes"],
        record.checkpoint["builder_volumes"]
    );
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert!(runtime.call.lock().unwrap().is_none());
    assert_eq!(runtime.retries.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn expired_builder_lease_does_not_complete_unknown_changed_or_partial_results() {
    for variant in 0..5 {
        let (state, runtime, record, _dir) = fixture(false, true).await;
        runtime.make_current();
        runtime.completion.store(true, Ordering::SeqCst);
        runtime.held.store(false, Ordering::SeqCst);
        match variant {
            0 => runtime
                .original_identity_retained
                .store(false, Ordering::SeqCst),
            1 => {
                runtime.completion.store(false, Ordering::SeqCst);
                runtime.retry_partial.store(true, Ordering::SeqCst);
                runtime.actual.lock().unwrap().pod = None;
            }
            2 => runtime.volumes.lock().unwrap()[0].uid = "replacement-pvc".into(),
            3 => runtime
                .change_identity_during_observation
                .store(true, Ordering::SeqCst),
            4 => runtime.actual.lock().unwrap().context.executor_id = "foreign-executor".into(),
            _ => unreachable!(),
        }
        assert!(resume_restart_start(&state, &record).await.is_err());
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.retries.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
        assert!(state.get_project(&record.app_id).is_none());
        assert_eq!(
            state
                .userapp_store
                .get_compute_control(&record.app_id, &record.operation_id)
                .await
                .unwrap(),
            Some(record)
        );
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn completed_builder_observation_losing_original_snapshot_cas_never_publishes() {
    let (state, runtime, record, _dir) = fixture(false, true).await;
    runtime.make_current();
    runtime.completion.store(true, Ordering::SeqCst);
    runtime.held.store(false, Ordering::SeqCst);
    // Use the original store claim to create a real competing revision, rather
    // than weakening the completion's exact snapshot comparison.
    let target: BuilderControlTarget = serde_json::from_value(record.checkpoint.clone()).unwrap();
    let updated = state
        .userapp_store
        .resume_builder_restart_start(&record, &target)
        .await
        .unwrap();
    assert!(resume_restart_start(&state, &record).await.is_err());
    assert!(state.get_project(&record.app_id).is_none());
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
    assert_eq!(
        state
            .userapp_store
            .get_compute_control(&record.app_id, &record.operation_id)
            .await
            .unwrap(),
        Some(updated)
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn completed_builder_observation_preserves_existing_canonical_adoption_and_rejects_other_family()
 {
    for foreign_family in [false, true] {
        let (state, runtime, record, _dir) = fixture_with_canonical_binding(
            false,
            true,
            None,
            BuilderStartupMode::ManagementOnly,
            true,
            true,
        )
        .await;
        runtime.make_current();
        runtime.completion.store(true, Ordering::SeqCst);
        runtime.held.store(false, Ordering::SeqCst);
        let binding = state
            .userapp_store
            .get_resource_binding(&ServiceType::UserappBuilder, "real-new-sts-uid")
            .await
            .unwrap()
            .unwrap();
        let mut actual_binding = binding.clone();
        if foreign_family {
            actual_binding.service_type = ServiceType::Userapp;
        }
        runtime.actual.lock().unwrap().resource_binding = Some(actual_binding);
        let result = resume_restart_start(&state, &record).await;
        if foreign_family {
            assert!(result.is_err());
            assert!(state.get_project(&record.app_id).is_none());
            assert_eq!(runtime.releases.load(Ordering::SeqCst), 0);
        } else {
            result.unwrap();
            let completed = state
                .userapp_store
                .get_compute_control(&record.app_id, &record.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(completed.state, ComputeControlState::Succeeded);
            let target: BuilderControlTarget =
                serde_json::from_value(completed.checkpoint).unwrap();
            assert_eq!(target.resource_binding, Some(binding.clone()));
            assert_eq!(binding.adopted_by_operation, "previous-adoption");
        }
        assert_eq!(
            state
                .userapp_store
                .get_resource_binding(&ServiceType::UserappBuilder, "real-new-sts-uid")
                .await
                .unwrap(),
            Some(binding)
        );
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.retries.load(Ordering::SeqCst), 0);
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn retained_builder_atomic_start_observation_preserves_its_original_checkpoint() {
    for unknown in [false, true] {
        let (state, runtime, record, _dir) = fixture(false, true).await;
        runtime.make_current();
        let identity = ComputeExecutorIdentity {
            app_id: record.app_id.clone(),
            lifecycle_id: record.lifecycle_id.clone(),
            scope: record.scope,
            operation_id: record.operation_id.clone(),
            generation: record.generation,
            executor_id: record.executor_id.clone().unwrap(),
        };
        let old: BuilderControlTarget = serde_json::from_value(record.checkpoint.clone()).unwrap();
        let running = state
            .userapp_store
            .resume_builder_restart_start(&record, &old)
            .await
            .unwrap();
        let mut bound = runtime.actual.lock().unwrap().clone();
        bound.pod = None;
        let mut checkpoint = serde_json::to_value(&bound).unwrap();
        checkpoint["builder_volumes"] = record.checkpoint["builder_volumes"].clone();
        checkpoint["builder_compute_single_write"] = true.into();
        checkpoint["builder_restart_template"] =
            record.checkpoint["builder_restart_template"].clone();
        let original = state
            .userapp_store
            .advance_compute_control(&ComputeControlProgress {
                identity,
                expected_revision: running.revision,
                state: ComputeControlState::RecoveryRequired,
                stage: ComputeControlStage::Starting,
                checkpoint,
                error_code: Some("LOST_REPLY".into()),
                error_message: Some("accepted original atomic start response was lost".into()),
            })
            .await
            .unwrap();
        runtime.held.store(false, Ordering::SeqCst);
        runtime.completion.store(!unknown, Ordering::SeqCst);
        let result = recover_confirmed(&state, &original).await;
        if unknown {
            assert!(result.is_err());
            assert_eq!(
                state
                    .userapp_store
                    .get_compute_control(&original.app_id, &original.operation_id)
                    .await
                    .unwrap(),
                Some(original)
            );
        } else {
            let completed = result.unwrap();
            assert_eq!(completed.state, ComputeControlState::Succeeded);
            assert_eq!(completed.checkpoint, original.checkpoint);
            assert_eq!(completed.executor_id, original.executor_id);
            assert_eq!(completed.generation, original.generation);
        }
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.retries.load(Ordering::SeqCst), 0);
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn expired_original_stop_observes_existing_atomic_or_absence_proof_without_new_write() {
    for absent in [false, true] {
        let (state, runtime, record, _dir) = fixture(false, false).await;
        resume_restart_start(&state, &record).await.unwrap();
        let app = state
            .userapp_store
            .get_application(&record.app_id)
            .await
            .unwrap()
            .unwrap();
        let pending = state
            .userapp_store
            .admit_compute_control(&ComputeControlRequest {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                scope: UserAppOperationScope::Dev,
                operation_id: "original-stop".into(),
                request_id: "original-stop-request".into(),
                request_fingerprint: "b".repeat(64),
                action: ComputeControlAction::Stop,
                restart_image_roll: false,
                builder_startup_mode: Default::default(),
            })
            .await
            .unwrap();
        let identity = ComputeExecutorIdentity {
            app_id: pending.app_id.clone(),
            lifecycle_id: pending.lifecycle_id.clone(),
            scope: pending.scope,
            operation_id: pending.operation_id.clone(),
            generation: pending.generation,
            executor_id: "original-executor".into(),
        };
        state
            .userapp_store
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        let bound = state
            .userapp_store
            .bind_compute_lease(&identity, &runtime.original_lease)
            .await
            .unwrap();
        let context = bound.execution_context().unwrap();
        runtime.actual.lock().unwrap().context = context.clone();
        runtime.actual.lock().unwrap().pod = None;
        let checkpoint = if absent {
            runtime.actual.lock().unwrap().workload = None;
            serde_json::to_value(ComputeAbsenceCheckpoint {
                context,
                compute_absent: true,
            })
            .unwrap()
        } else {
            serde_json::to_value(runtime.actual.lock().unwrap().clone()).unwrap()
        };
        let original = state
            .userapp_store
            .advance_compute_control(&ComputeControlProgress {
                identity,
                expected_revision: bound.revision,
                state: ComputeControlState::RecoveryRequired,
                stage: ComputeControlStage::Stopping,
                checkpoint,
                error_code: Some("LOST_REPLY".into()),
                error_message: Some("accepted original stop response was lost".into()),
            })
            .await
            .unwrap();
        runtime.held.store(false, Ordering::SeqCst);
        runtime.stopped.store(true, Ordering::SeqCst);
        runtime.creates.store(0, Ordering::SeqCst);
        runtime.releases.store(0, Ordering::SeqCst);
        let completed = recover_confirmed(&state, &original).await.unwrap();
        assert_eq!(completed.state, ComputeControlState::Succeeded);
        assert_eq!(completed.operation_id, original.operation_id);
        assert_eq!(completed.checkpoint, original.checkpoint);
        assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.retries.load(Ordering::SeqCst), 0);
        assert_eq!(runtime.releases.load(Ordering::SeqCst), 1);
        state.userapp_store_control.shutdown().await.unwrap();
    }
}
