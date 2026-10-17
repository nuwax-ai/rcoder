//! Trait-level read-only discovery regression with a real lifecycle store.
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderPodIdentity,
    ContainerBasicInfo, FileServerRequestCredentials, ProjectAndContainerInfo, UserAppAdmission,
    UserAppAdmissionOutcome, UserAppExecutionContext, UserAppLifecycleStore, UserAppOperationKind,
    UserAppOperationProgress, UserAppOperationState, UserAppResourceBinding,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

const APP_ID: &str = "locator-app";

#[path = "dev_locator_recreate_tests.rs"]
mod recreate_tests;

struct LocatorOperationLease;

#[async_trait::async_trait]
impl shared_types::AppOperationLease for LocatorOperationLease {
    async fn release(self: Box<Self>) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum TargetChange {
    WorkloadKind,
    WorkloadName,
    WorkloadUid,
    PodAdded,
    PodRemoved,
    PodName,
    PodUid,
    Context,
    Binding,
    StartupMode,
    RestartImage,
    RestartRuntimeWorkspace,
}

impl TargetChange {
    const ALL: [Self; 12] = [
        Self::WorkloadKind,
        Self::WorkloadName,
        Self::WorkloadUid,
        Self::PodAdded,
        Self::PodRemoved,
        Self::PodName,
        Self::PodUid,
        Self::Context,
        Self::Binding,
        Self::StartupMode,
        Self::RestartImage,
        Self::RestartRuntimeWorkspace,
    ];

    fn apply(self, target: &mut BuilderControlTarget) {
        match self {
            Self::WorkloadKind => {
                target.workload.as_mut().unwrap().kind = AppResourceKind::Container
            }
            Self::WorkloadName => {
                target.workload.as_mut().unwrap().name = "replacement-builder".into();
            }
            Self::WorkloadUid => {
                target.workload.as_mut().unwrap().uid = "replacement-sts".into();
            }
            Self::PodAdded => {
                target.pod = Some(BuilderPodIdentity {
                    name: "builder-locator-app-0".into(),
                    uid: "pod-current".into(),
                    resource_version: "1".into(),
                });
            }
            Self::PodRemoved => target.pod = None,
            Self::PodName => target.pod.as_mut().unwrap().name = "replacement-builder-0".into(),
            Self::PodUid => target.pod.as_mut().unwrap().uid = "replacement-pod".into(),
            Self::Context => target.context.operation_id = "different-read-operation".into(),
            Self::Binding => {
                target.resource_binding = Some(UserAppResourceBinding {
                    app_id: target.context.app_id.clone(),
                    lifecycle_id: target.context.lifecycle_id.clone(),
                    service_type: ServiceType::UserappBuilder,
                    physical_uid: target.workload.as_ref().unwrap().uid.clone(),
                    adopted_by_operation: "different-adoption".into(),
                });
            }
            Self::StartupMode => {
                target.builder_startup_mode = shared_types::BuilderStartupMode::ManagementOnly;
            }
            Self::RestartImage => target.restart_image = Some("public-test-image:next".into()),
            Self::RestartRuntimeWorkspace => {
                target.restart_runtime_workspace = Some(
                    shared_types::paths::userapp_dev_workspace(&target.context.app_id),
                );
            }
        }
    }

    fn initially_has_pod(self) -> bool {
        match self {
            Self::WorkloadKind | Self::PodAdded => false,
            Self::WorkloadName
            | Self::WorkloadUid
            | Self::PodRemoved
            | Self::PodName
            | Self::PodUid
            | Self::Context
            | Self::Binding
            | Self::StartupMode
            | Self::RestartImage
            | Self::RestartRuntimeWorkspace => true,
        }
    }
}

struct CaptureTransition {
    change: TargetChange,
    activated: bool,
}

struct LocatorRuntime {
    observations: Mutex<Vec<Option<RuntimeContainerInfo>>>,
    read_sequence: Mutex<Vec<&'static str>>,
    captured_targets: Mutex<Vec<BuilderControlTarget>>,
    reads: AtomicUsize,
    captures: AtomicUsize,
    readiness_reads: AtomicUsize,
    credential_reads: AtomicUsize,
    writes: AtomicUsize,
    operation_acquisitions: AtomicUsize,
    native_context: UserAppExecutionContext,
    pod_uid: String,
    workload_uid: String,
    docker: bool,
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    delete_on_capture: AtomicBool,
    capture_fails: AtomicBool,
    readiness_fails: AtomicBool,
    recreate_on_readiness: AtomicBool,
    missing_native_identity: AtomicBool,
    workload_exists: AtomicBool,
    pod_exists: AtomicBool,
    readiness: Mutex<shared_types::UserAppNoComputeState>,
    running_observation: Mutex<Option<container_runtime_api::UserAppReadinessTarget>>,
    remove_workload_on_readiness: AtomicBool,
    workload_resource_version: AtomicUsize,
    pod_resource_version: AtomicUsize,
    update_workload_version_on_readiness: AtomicBool,
    update_pod_version_on_readiness: AtomicBool,
    capture_transition: Mutex<Option<CaptureTransition>>,
}

impl LocatorRuntime {
    fn target(&self, context: &UserAppExecutionContext) -> BuilderControlTarget {
        BuilderControlTarget {
            builder_startup_mode: Default::default(),
            context: context.clone(),
            resource_binding: None,
            workload: self
                .workload_exists
                .load(Ordering::SeqCst)
                .then(|| AppResourceIdentity {
                    kind: if self.docker {
                        AppResourceKind::Container
                    } else {
                        AppResourceKind::StatefulSet
                    },
                    name: "builder-locator-app".into(),
                    uid: self.workload_uid.clone(),
                    resource_version: Some(
                        self.workload_resource_version
                            .load(Ordering::SeqCst)
                            .to_string(),
                    ),
                }),
            pod: self
                .pod_exists
                .load(Ordering::SeqCst)
                .then(|| BuilderPodIdentity {
                    name: "builder-locator-app-0".into(),
                    uid: self.pod_uid.clone(),
                    resource_version: self.pod_resource_version.load(Ordering::SeqCst).to_string(),
                }),
            restart_image: None,
            restart_runtime_workspace: None,
        }
    }

    fn observed_target(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&UserAppResourceBinding>,
    ) -> BuilderControlTarget {
        let mut target = self.target(context);
        target.resource_binding = binding.cloned();
        if let Some(transition) = self.capture_transition.lock().unwrap().as_ref()
            && transition.activated
        {
            transition.change.apply(&mut target);
        }
        target
    }

    fn reject_write<T>(&self) -> ContainerRuntimeResult<T> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "read-only locator must not mutate compute".into(),
        ))
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for LocatorRuntime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.reject_write()
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("locator must use structured runtime discovery")
    }
    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        assert_eq!(identifier, APP_ID);
        assert_eq!(service_type, &ServiceType::UserappBuilder);
        self.read_sequence.lock().unwrap().push("find");
        self.reads.fetch_add(1, Ordering::SeqCst);
        let mut observations = self.observations.lock().unwrap();
        if observations.len() > 1 {
            Ok(observations.remove(0))
        } else {
            Ok(observations.first().cloned().flatten())
        }
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        self.reject_write()
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        panic!("port or running checks must not replace identity discovery")
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.reject_write()
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn get_bound_builder_file_credentials(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<FileServerRequestCredentials> {
        self.credential_reads.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ManagementNotRunning)
    }
    async fn inspect_builder_candidate(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        self.read_sequence.lock().unwrap().push("inspect");
        Ok(self.observed_target(context, None))
    }
    async fn capture_builder_control(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        self.capture_bound_builder_control(context, None).await
    }
    async fn capture_bound_builder_control(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        self.read_sequence.lock().unwrap().push("capture");
        self.captures.fetch_add(1, Ordering::SeqCst);
        if self.capture_fails.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "controlled builder capture failure".into(),
            ));
        }
        let metadata = if self.missing_native_identity.load(Ordering::SeqCst) {
            std::collections::BTreeMap::new()
        } else {
            self.native_context.resource_metadata()
        };
        if self.workload_exists.load(Ordering::SeqCst)
            && !shared_types::builder_identity_is_bound(
                context,
                &metadata,
                &self.workload_uid,
                binding,
            )
            .map_err(ContainerRuntimeError::Conflict)?
        {
            return Err(ContainerRuntimeError::Conflict(
                "builder lifecycle requires explicit adoption".into(),
            ));
        }
        if self.delete_on_capture.swap(false, Ordering::SeqCst) {
            admit(
                &*self.store,
                APP_ID,
                UserAppOperationKind::DeleteApplication,
            )
            .await;
        }
        let target = self.observed_target(context, binding);
        self.captured_targets.lock().unwrap().push(target.clone());
        Ok(target)
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for LocatorRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for LocatorRuntime {
    async fn get_deployment_status(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::DeploymentStatus>> {
        assert_eq!(app_id, APP_ID);
        Ok(None)
    }

    async fn acquire_app_operation(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<Box<dyn shared_types::AppOperationLease>>> {
        self.operation_acquisitions.fetch_add(1, Ordering::SeqCst);
        Ok(Some(Box::new(LocatorOperationLease)))
    }

    async fn observe_userapp_readiness(
        &self,
        app_id: &str,
        stage: shared_types::UserappStage,
    ) -> ContainerRuntimeResult<container_runtime_api::UserAppRuntimeReadiness> {
        assert_eq!(app_id, APP_ID);
        assert_eq!(stage, shared_types::UserappStage::Dev);
        self.read_sequence.lock().unwrap().push("readiness");
        self.readiness_reads.fetch_add(1, Ordering::SeqCst);
        if self.readiness_fails.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "controlled builder readiness failure".into(),
            ));
        }
        if self.recreate_on_readiness.swap(false, Ordering::SeqCst) {
            self.store
                .recreate(
                    APP_ID,
                    &self.native_context.lifecycle_id,
                    "concurrent-recreate",
                )
                .await
                .expect("another request recreates the deleted lifecycle during observation");
        }
        if self
            .remove_workload_on_readiness
            .swap(false, Ordering::SeqCst)
        {
            self.workload_exists.store(false, Ordering::SeqCst);
        }
        if self
            .update_workload_version_on_readiness
            .swap(false, Ordering::SeqCst)
        {
            self.workload_resource_version
                .fetch_add(1, Ordering::SeqCst);
        }
        if self
            .update_pod_version_on_readiness
            .swap(false, Ordering::SeqCst)
        {
            self.pod_resource_version.fetch_add(1, Ordering::SeqCst);
        }
        if let Some(transition) = self.capture_transition.lock().unwrap().as_mut() {
            transition.activated = true;
        }
        if let Some(target) = self.running_observation.lock().unwrap().clone() {
            return Ok(container_runtime_api::UserAppRuntimeReadiness::Running(
                Box::new(target),
            ));
        }
        Ok(container_runtime_api::UserAppRuntimeReadiness::NotRunning(
            *self.readiness.lock().unwrap(),
        ))
    }

    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(vec![])
    }
}

fn running_builder() -> RuntimeContainerInfo {
    RuntimeContainerInfo {
        container_id: "pod-current".into(),
        container_name: "builder-locator-app-0".into(),
        container_ip: "10.42.0.9".into(),
        status: ContainerRuntimeStatus::Running,
        created_at: chrono::Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some(APP_ID.into()),
        app_id: Some(APP_ID.into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    }
}

async fn fixture(
    observations: Vec<Option<RuntimeContainerInfo>>,
    old_native_lifecycle: bool,
) -> (Arc<AppState>, Arc<LocatorRuntime>, tempfile::TempDir) {
    fixture_on_runtime(observations, old_native_lifecycle, false).await
}

async fn fixture_on_runtime(
    observations: Vec<Option<RuntimeContainerInfo>>,
    old_native_lifecycle: bool,
    docker: bool,
) -> (Arc<AppState>, Arc<LocatorRuntime>, tempfile::TempDir) {
    fixture_on_application(observations, old_native_lifecycle, docker, true).await
}

async fn fixture_on_application(
    observations: Vec<Option<RuntimeContainerInfo>>,
    old_native_lifecycle: bool,
    docker: bool,
    create_application: bool,
) -> (Arc<AppState>, Arc<LocatorRuntime>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("state.db"),
        )
        .await
        .unwrap(),
    );
    let lifecycle_id = if create_application {
        store.ensure_identity(APP_ID).await.unwrap().lifecycle_id
    } else {
        "unregistered-life".into()
    };
    let workload_exists = observations.iter().any(Option::is_some);
    let runtime = Arc::new(LocatorRuntime {
        observations: Mutex::new(observations),
        read_sequence: Mutex::new(vec![]),
        captured_targets: Mutex::new(vec![]),
        reads: AtomicUsize::new(0),
        captures: AtomicUsize::new(0),
        readiness_reads: AtomicUsize::new(0),
        credential_reads: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
        operation_acquisitions: AtomicUsize::new(0),
        native_context: UserAppExecutionContext {
            app_id: APP_ID.into(),
            lifecycle_id: if old_native_lifecycle {
                "previous-life".into()
            } else {
                lifecycle_id
            },
            operation_id: "create-builder".into(),
            executor_id: "fixture".into(),
            request_fingerprint: "a".repeat(64),
        },
        pod_uid: "pod-current".into(),
        workload_uid: if docker { "pod-current" } else { "sts-current" }.into(),
        docker,
        store: store.clone(),
        delete_on_capture: AtomicBool::new(false),
        capture_fails: AtomicBool::new(false),
        readiness_fails: AtomicBool::new(false),
        recreate_on_readiness: AtomicBool::new(false),
        missing_native_identity: AtomicBool::new(false),
        workload_exists: AtomicBool::new(workload_exists),
        pod_exists: AtomicBool::new(workload_exists && !docker),
        readiness: Mutex::new(if workload_exists {
            shared_types::UserAppNoComputeState::Starting
        } else {
            shared_types::UserAppNoComputeState::Missing
        }),
        running_observation: Mutex::new(None),
        remove_workload_on_readiness: AtomicBool::new(false),
        workload_resource_version: AtomicUsize::new(1),
        pod_resource_version: AtomicUsize::new(1),
        update_workload_version_on_readiness: AtomicBool::new(false),
        update_pod_version_on_readiness: AtomicBool::new(false),
        capture_transition: Mutex::new(None),
    });
    let state =
        super::super::creation::fence_settler_tests::runtime_state(runtime.clone(), store, None)
            .await;
    (state, runtime, directory)
}

async fn admit(
    store: &dyn UserAppLifecycleStore,
    app_id: &str,
    kind: UserAppOperationKind,
) -> shared_types::UserAppOperationRecord {
    let app = store.ensure_identity(app_id).await.unwrap();
    let outcome = store
        .admit(&UserAppAdmission {
            app_id: app_id.into(),
            lifecycle_id: Some(app.lifecycle_id),
            operation_id: uuid::Uuid::new_v4().to_string(),
            request_id: None,
            request_fingerprint: "a".repeat(64),
            kind,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();
    match outcome {
        UserAppAdmissionOutcome::Accepted(record) => record,
        UserAppAdmissionOutcome::Existing(_) => panic!("fresh fixture admission"),
    }
}

fn make_locator(state: &Arc<AppState>) -> UserappDevLocator {
    UserappDevLocator::new(Arc::downgrade(state))
}

#[derive(Clone, Copy, Debug)]
enum ReadOnlyEntry {
    Locate,
    Alive,
    Logs,
}

#[derive(Clone, Copy, Debug)]
enum MetadataOnlyScenario {
    StartingController,
    StoppedController,
    RunningPodWithoutAddress,
}

/// Exercise the public callbacks, including the two bound captures around a
/// separate runtime read. Kubernetes status updates change resourceVersion
/// without replacing the StatefulSet/Pod or changing lifecycle ownership.
async fn assert_metadata_only_update(entry: ReadOnlyEntry, scenario: MetadataOnlyScenario) {
    let (state, runtime, _directory) = fixture(vec![None], false).await;
    runtime.workload_exists.store(true, Ordering::SeqCst);
    match scenario {
        MetadataOnlyScenario::StartingController => {
            *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Starting;
            runtime
                .update_workload_version_on_readiness
                .store(true, Ordering::SeqCst);
        }
        MetadataOnlyScenario::StoppedController => {
            *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Stopped;
            runtime
                .update_workload_version_on_readiness
                .store(true, Ordering::SeqCst);
        }
        MetadataOnlyScenario::RunningPodWithoutAddress => {
            runtime.pod_exists.store(true, Ordering::SeqCst);
            *runtime.running_observation.lock().unwrap() =
                Some(container_runtime_api::UserAppReadinessTarget {
                    app_id: APP_ID.into(),
                    stage: shared_types::UserappStage::Dev,
                    instance: container_runtime_api::UserAppReadinessInstance::Kubernetes {
                        namespace: "test-ns".into(),
                        pod_name: "builder-locator-app-0".into(),
                        pod_uid: "pod-current".into(),
                        container_name: "agent".into(),
                        container_id: None,
                        owner_uid: "sts-current".into(),
                    },
                    address: None,
                    published_address: None,
                    dbx_published_address: None,
                });
            runtime
                .update_pod_version_on_readiness
                .store(true, Ordering::SeqCst);
        }
    }
    let before = state.userapp_store.get_application(APP_ID).await.unwrap();
    let locator = make_locator(&state);
    match entry {
        ReadOnlyEntry::Locate => {
            let result = shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID).await;
            match scenario {
                MetadataOnlyScenario::StoppedController => assert_eq!(result.unwrap(), None),
                MetadataOnlyScenario::StartingController
                | MetadataOnlyScenario::RunningPodWithoutAddress => match result {
                    Err(shared_types::DevEnsureError::NotReady { app_id, .. }) => {
                        assert_eq!(app_id, APP_ID);
                    }
                    Err(
                        error @ (shared_types::DevEnsureError::BuilderAbsent { .. }
                        | shared_types::DevEnsureError::OperationInFlight { .. }
                        | shared_types::DevEnsureError::ObserveFailed { .. }
                        | shared_types::DevEnsureError::EnsureFailed { .. }),
                    ) => panic!("{scenario:?}: metadata-only update was misclassified: {error:?}"),
                    Ok(instance) => panic!("{scenario:?}: no address was observed: {instance:?}"),
                },
            }
        }
        ReadOnlyEntry::Alive => assert!(
            shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
                .await
                .unwrap(),
            "{scenario:?}: retained compute is present after an RV-only update"
        ),
        ReadOnlyEntry::Logs => {
            let error =
                shared_types::UserappDevLocator::dev_logs_file_server_addr(&locator, APP_ID)
                    .await
                    .unwrap_err();
            assert_eq!(
                error,
                format!(
                    "Development container for app {APP_ID} is not running; its file logs are currently unavailable"
                ),
                "{scenario:?}: normal log unavailability must not become an identity conflict"
            );
        }
    }
    assert_eq!(
        *runtime.read_sequence.lock().unwrap(),
        [
            "find",
            "inspect",
            "capture",
            "readiness",
            "inspect",
            "capture"
        ],
        "{entry:?}/{scenario:?}: keep discovery and capture ordering without retries"
    );
    assert_eq!(runtime.reads.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.captures.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.readiness_reads.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.operation_acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        before,
        "read-only discovery must preserve the actual persisted lifecycle"
    );
    let captured = runtime.captured_targets.lock().unwrap();
    assert_eq!(captured.len(), 2);
    for target in captured.iter() {
        let workload = target.workload.as_ref().unwrap();
        assert_eq!(workload.kind, AppResourceKind::StatefulSet);
        assert_eq!(workload.uid, "sts-current");
        assert_eq!(workload.name, "builder-locator-app");
        assert_eq!(target.context, captured[0].context);
        assert_eq!(target.resource_binding, captured[0].resource_binding);
    }
    match scenario {
        MetadataOnlyScenario::StartingController | MetadataOnlyScenario::StoppedController => {
            assert_eq!(
                captured[0]
                    .workload
                    .as_ref()
                    .unwrap()
                    .resource_version
                    .as_deref(),
                Some("1")
            );
            assert_eq!(
                captured[1]
                    .workload
                    .as_ref()
                    .unwrap()
                    .resource_version
                    .as_deref(),
                Some("2")
            );
            assert!(captured[0].pod.is_none());
            assert!(captured[1].pod.is_none());
        }
        MetadataOnlyScenario::RunningPodWithoutAddress => {
            let first = captured[0].pod.as_ref().unwrap();
            let second = captured[1].pod.as_ref().unwrap();
            assert_eq!(first.uid, "pod-current");
            assert_eq!(second.uid, first.uid);
            assert_eq!(second.name, first.name);
            assert_eq!(first.resource_version, "1");
            assert_eq!(second.resource_version, "2");
            assert_eq!(captured[0].workload, captured[1].workload);
        }
    }
}

#[tokio::test]
async fn dev_locator_locate_tolerates_controller_resource_version_churn() {
    for scenario in [
        MetadataOnlyScenario::StartingController,
        MetadataOnlyScenario::StoppedController,
    ] {
        assert_metadata_only_update(ReadOnlyEntry::Locate, scenario).await;
    }
}

#[tokio::test]
async fn dev_locator_alive_tolerates_controller_resource_version_churn() {
    for scenario in [
        MetadataOnlyScenario::StartingController,
        MetadataOnlyScenario::StoppedController,
    ] {
        assert_metadata_only_update(ReadOnlyEntry::Alive, scenario).await;
    }
}

#[tokio::test]
async fn dev_locator_logs_tolerates_controller_resource_version_churn() {
    for scenario in [
        MetadataOnlyScenario::StartingController,
        MetadataOnlyScenario::StoppedController,
    ] {
        assert_metadata_only_update(ReadOnlyEntry::Logs, scenario).await;
    }
}

#[tokio::test]
async fn dev_locator_callbacks_tolerate_pod_resource_version_churn_without_address() {
    for entry in [
        ReadOnlyEntry::Locate,
        ReadOnlyEntry::Alive,
        ReadOnlyEntry::Logs,
    ] {
        assert_metadata_only_update(entry, MetadataOnlyScenario::RunningPodWithoutAddress).await;
    }
}

#[tokio::test]
async fn dev_locator_deleted_retained_builder_tolerates_resource_version_churn() {
    let (state, runtime, _directory) = fixture(vec![None], false).await;
    let (tombstone, original_deletion) = recreate_tests::seed_deleted_application(&runtime).await;
    runtime.workload_exists.store(true, Ordering::SeqCst);
    *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Stopped;
    runtime
        .update_workload_version_on_readiness
        .store(true, Ordering::SeqCst);

    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&make_locator(&state), APP_ID)
            .await
            .unwrap(),
        "a retained stopped controller still exists after an RV-only metadata update"
    );
    assert_eq!(
        *runtime.read_sequence.lock().unwrap(),
        ["inspect", "capture", "readiness", "inspect", "capture"],
        "Deleted discovery must retain the tombstone capture context without ordinary lookup"
    );
    assert_eq!(runtime.reads.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.captures.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.readiness_reads.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.operation_acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        Some(tombstone.clone()),
        "the query must not replace or rewrite the durable tombstone"
    );
    assert_eq!(
        state
            .userapp_store
            .get_operation(APP_ID, &original_deletion.operation_id)
            .await
            .unwrap(),
        Some(original_deletion),
        "the original completed deletion evidence must remain unchanged"
    );
    let captured = runtime.captured_targets.lock().unwrap();
    assert_eq!(captured.len(), 2);
    for target in captured.iter() {
        assert_eq!(target.context.lifecycle_id, tombstone.lifecycle_id);
        assert_eq!(target.context.operation_id, "read-only-recreation");
        assert_eq!(target.context, captured[0].context);
        let workload = target.workload.as_ref().unwrap();
        assert_eq!(workload.kind, AppResourceKind::StatefulSet);
        assert_eq!(workload.uid, "sts-current");
        assert_eq!(workload.name, "builder-locator-app");
        assert!(target.pod.is_none());
    }
    assert_eq!(
        captured[0]
            .workload
            .as_ref()
            .unwrap()
            .resource_version
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        captured[1]
            .workload
            .as_ref()
            .unwrap()
            .resource_version
            .as_deref(),
        Some("2")
    );
}

async fn assert_non_version_target_change_is_rejected(entry: ReadOnlyEntry, change: TargetChange) {
    // Each callback gets a fresh transition. Reusing a runtime across callbacks
    // would make later queries see an already stable replacement and would not
    // exercise the capture/readiness/capture race.
    let (state, runtime, _directory) = fixture(vec![None], false).await;
    runtime.workload_exists.store(true, Ordering::SeqCst);
    runtime
        .pod_exists
        .store(change.initially_has_pod(), Ordering::SeqCst);
    *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Starting;
    *runtime.capture_transition.lock().unwrap() = Some(CaptureTransition {
        change,
        activated: false,
    });
    let before = state.userapp_store.get_application(APP_ID).await.unwrap();
    let locator = make_locator(&state);
    match entry {
        ReadOnlyEntry::Locate => {
            match shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID).await {
                Err(shared_types::DevEnsureError::ObserveFailed { .. }) => {}
                Err(
                    error @ (shared_types::DevEnsureError::BuilderAbsent { .. }
                    | shared_types::DevEnsureError::OperationInFlight { .. }
                    | shared_types::DevEnsureError::NotReady { .. }
                    | shared_types::DevEnsureError::EnsureFailed { .. }),
                ) => panic!("{change:?}: unsafe capture transition was misclassified: {error:?}"),
                Ok(instance) => {
                    panic!("{change:?}: changed capture must not authorize discovery: {instance:?}")
                }
            }
        }
        ReadOnlyEntry::Alive => {
            shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
                .await
                .expect_err("a non-RV capture change must not become an existence answer");
        }
        ReadOnlyEntry::Logs => {
            let error =
                shared_types::UserappDevLocator::dev_logs_file_server_addr(&locator, APP_ID)
                    .await
                    .unwrap_err();
            assert!(
                error.starts_with("observe development log container:"),
                "{change:?}: a rejected capture must not become ordinary unavailable logs: {error}"
            );
        }
    }
    assert_eq!(
        *runtime.read_sequence.lock().unwrap(),
        [
            "find",
            "inspect",
            "capture",
            "readiness",
            "inspect",
            "capture"
        ],
        "{entry:?}/{change:?}: fail during the original observation without retry or mutation"
    );
    assert_eq!(runtime.reads.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.captures.load(Ordering::SeqCst), 2);
    assert_eq!(runtime.readiness_reads.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.operation_acquisitions.load(Ordering::SeqCst), 0);
    assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        before,
        "a failed read-only observation must not change the durable lifecycle"
    );
    let captured = runtime.captured_targets.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert_ne!(captured[0], captured[1]);
    let mut expected = captured[0].clone();
    change.apply(&mut expected);
    assert_eq!(
        captured[1], expected,
        "the controlled runtime must change only the requested non-RV target field"
    );
    assert_eq!(
        captured[0].workload.as_ref().unwrap().resource_version,
        captured[1].workload.as_ref().unwrap().resource_version
    );
}

#[tokio::test]
async fn dev_locator_locate_rejects_non_version_target_changes() {
    for change in TargetChange::ALL {
        assert_non_version_target_change_is_rejected(ReadOnlyEntry::Locate, change).await;
    }
}

#[tokio::test]
async fn dev_locator_alive_rejects_non_version_target_changes() {
    for change in TargetChange::ALL {
        assert_non_version_target_change_is_rejected(ReadOnlyEntry::Alive, change).await;
    }
}

#[tokio::test]
async fn dev_locator_logs_rejects_non_version_target_changes() {
    for change in TargetChange::ALL {
        assert_non_version_target_change_is_rejected(ReadOnlyEntry::Logs, change).await;
    }
}

async fn assert_read_only_rejection(state: &Arc<AppState>, runtime: &LocatorRuntime) {
    let before = state.userapp_store.get_application(APP_ID).await.unwrap();
    let locator = make_locator(state);
    assert!(matches!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID).await,
        Err(shared_types::DevEnsureError::ObserveFailed { .. })
    ));
    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .is_err()
    );
    assert!(
        shared_types::UserappDevLocator::dev_logs_file_server_addr(&locator, APP_ID)
            .await
            .is_err()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn dev_locator_accepts_only_verified_current_builder() {
    let (state, runtime, _directory) = fixture(vec![Some(running_builder())], false).await;
    // The identity fixture is not a Kubernetes NodePort provider. Supply its
    // address-layer observation for the deploy-host feature combination; this
    // test still performs no transport readiness or container mutation.
    #[cfg(feature = "deploy-host")]
    let published_key = {
        let observed = running_builder();
        let key = shared_types::build_backend_addr(
            &observed.container_name,
            &observed.container_ip,
            &state.config.app_manager.namespace,
            &state.cluster_domain,
        );
        shared_types::published::register_direct(&key, "127.0.0.1".parse().unwrap());
        key
    };
    let before = state.userapp_store.get_application(APP_ID).await.unwrap();
    let locator = make_locator(&state);
    assert_eq!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID)
            .await
            .unwrap(),
        Some(shared_types::DevBuilderInstance {
            address: "10.42.0.9".into(),
            container_id: "pod-current".into(),
        })
    );
    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert!(runtime.captures.load(Ordering::SeqCst) >= 2);
    assert!(
        shared_types::UserappDevLocator::dev_logs_file_server_addr(&locator, APP_ID)
            .await
            .unwrap()
            .ends_with(":60000")
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        before
    );
    #[cfg(feature = "deploy-host")]
    shared_types::published::unregister(&published_key);
}

#[tokio::test]
async fn dev_locator_rejects_foreign_missing_or_wrong_logical_identity() {
    for case in 0..5 {
        let mut observed = running_builder();
        match case {
            0 => observed.service_type = Some(ServiceType::ComputerAgentRunner),
            1 => observed.service_type = None,
            2 => observed.project_id = Some("another-app".into()),
            3 => {
                observed.project_id = None;
                observed.app_id = None;
            }
            4 => observed.app_id = Some("another-app".into()),
            _ => unreachable!(),
        }
        let (state, runtime, _directory) = fixture(vec![Some(observed)], false).await;
        assert_read_only_rejection(&state, &runtime).await;
        assert_eq!(runtime.captures.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn dev_locator_accepts_verified_docker_without_separate_workload_uid() {
    let mut observed = running_builder();
    observed.workload_uid = None;
    observed.project_id = None;
    let (state, runtime, _directory) = fixture_on_runtime(vec![Some(observed)], false, true).await;
    let locator = make_locator(&state);
    let actual = shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(actual.container_id, "pod-current");
    assert_eq!(actual.address, "10.42.0.9");
    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dev_locator_rejects_stale_native_lifecycle_and_physical_identity() {
    let (state, runtime, _directory) = fixture(vec![Some(running_builder())], true).await;
    assert_read_only_rejection(&state, &runtime).await;
    assert!(runtime.captures.load(Ordering::SeqCst) >= 2);
    for (pod_id, workload_uid) in [
        ("pod-replaced", Some("sts-current")),
        ("pod-current", Some("sts-replaced")),
        ("pod-current", None),
    ] {
        let mut observed = running_builder();
        observed.container_id = pod_id.into();
        observed.workload_uid = workload_uid.map(str::to_owned);
        let (state, runtime, _directory) = fixture(vec![Some(observed)], false).await;
        assert_read_only_rejection(&state, &runtime).await;
    }
}

#[tokio::test]
async fn dev_locator_rejects_retiring_lifecycle_without_waking_compute() {
    let (state, runtime, _directory) = fixture(vec![Some(running_builder())], false).await;
    admit(
        &*runtime.store,
        APP_ID,
        UserAppOperationKind::DeleteApplication,
    )
    .await;
    assert_read_only_rejection(&state, &runtime).await;
    assert_eq!(runtime.captures.load(Ordering::SeqCst), 0);
}

async fn adopt_binding(runtime: &LocatorRuntime, app_id: &str) {
    let app = runtime.store.ensure_identity(app_id).await.unwrap();
    let request = shared_types::AdoptBuilderRequest {
        lifecycle_id: app.lifecycle_id.clone(),
        request_id: uuid::Uuid::new_v4().to_string(),
        expected_container_id: runtime.pod_uid.clone(),
    };
    let input = shared_types::UserAppExecutionInput::new(serde_json::to_string(&request).unwrap());
    let admitted = runtime
        .store
        .admit_with_input(
            &UserAppAdmission {
                app_id: app_id.into(),
                lifecycle_id: Some(app.lifecycle_id),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: Some(request.request_id),
                request_fingerprint: input.digest(),
                kind: UserAppOperationKind::AdoptBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await
        .unwrap();
    let accepted = match admitted {
        UserAppAdmissionOutcome::Accepted(record) => record,
        UserAppAdmissionOutcome::Existing(_) => panic!("fresh fixture adoption"),
    };
    let running = runtime
        .store
        .advance(&UserAppOperationProgress {
            app_id: accepted.app_id.clone(),
            lifecycle_id: accepted.lifecycle_id.clone(),
            operation_id: accepted.operation_id.clone(),
            expected_revision: accepted.revision,
            executor_id: "fixture".into(),
            state: UserAppOperationState::Running,
            step: "claimed".into(),
            checkpoint: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        })
        .await
        .unwrap();
    runtime
        .store
        .commit_resource_binding(
            &UserAppResourceBinding {
                app_id: running.app_id.clone(),
                lifecycle_id: running.lifecycle_id.clone(),
                service_type: ServiceType::UserappBuilder,
                physical_uid: "sts-current".into(),
                adopted_by_operation: running.operation_id.clone(),
            },
            &UserAppOperationProgress {
                app_id: running.app_id.clone(),
                lifecycle_id: running.lifecycle_id.clone(),
                operation_id: running.operation_id.clone(),
                expected_revision: running.revision,
                executor_id: "fixture".into(),
                state: UserAppOperationState::Succeeded,
                step: "adopted".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            },
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn dev_locator_rejects_stale_physical_binding_even_with_native_labels() {
    let (state, runtime, _directory) = fixture(vec![Some(running_builder())], false).await;
    adopt_binding(&runtime, "different-app").await;
    assert_read_only_rejection(&state, &runtime).await;
}

#[tokio::test]
async fn dev_locator_accepts_explicit_legacy_binding_without_native_lifecycle() {
    let (state, runtime, _directory) = fixture(vec![Some(running_builder())], false).await;
    runtime
        .missing_native_identity
        .store(true, Ordering::SeqCst);
    adopt_binding(&runtime, APP_ID).await;
    let before = state.userapp_store.get_application(APP_ID).await.unwrap();
    let result = shared_types::UserappDevEnsure::locate_dev_builder(&make_locator(&state), APP_ID)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.container_id, "pod-current");
    assert_eq!(result.address, "10.42.0.9");
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn dev_locator_rejects_address_or_instance_change_before_returning() {
    for change in ["pod", "workload", "address", "absent"] {
        let original = running_builder();
        let mut changed = original.clone();
        match change {
            "pod" => changed.container_id = "replacement-pod".into(),
            "workload" => changed.workload_uid = Some("replacement-sts".into()),
            "address" => changed.container_ip = "10.42.0.10".into(),
            "absent" => {}
            _ => unreachable!(),
        }
        let last = (change != "absent").then_some(changed);
        let (state, runtime, _directory) = fixture(vec![Some(original), last], false).await;
        let result =
            shared_types::UserappDevEnsure::locate_dev_builder(&make_locator(&state), APP_ID).await;
        assert!(
            matches!(
                result,
                Err(shared_types::DevEnsureError::ObserveFailed { .. })
            ),
            "{change}: {result:?}"
        );
        assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn dev_locator_rechecks_lifecycle_after_runtime_capture() {
    let (state, runtime, _directory) = fixture(vec![Some(running_builder())], false).await;
    runtime.delete_on_capture.store(true, Ordering::SeqCst);
    let result =
        shared_types::UserappDevEnsure::locate_dev_builder(&make_locator(&state), APP_ID).await;
    assert!(matches!(
        result,
        Err(shared_types::DevEnsureError::ObserveFailed { .. })
    ));
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        state
            .userapp_store
            .get_application(APP_ID)
            .await
            .unwrap()
            .unwrap()
            .state,
        shared_types::UserAppLifecycleState::Deleting
    );
}

#[tokio::test]
async fn dev_locator_distinguishes_verified_pending_from_absence() {
    let mut pending = running_builder();
    pending.status = ContainerRuntimeStatus::Pending;
    pending.container_ip.clear();
    let (state, runtime, _directory) = fixture(vec![Some(pending)], false).await;
    let locator = make_locator(&state);
    assert!(matches!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID).await,
        Err(shared_types::DevEnsureError::NotReady { .. })
    ));
    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);

    let (state, runtime, _directory) = fixture(vec![None], false).await;
    let locator = make_locator(&state);
    assert!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dev_locator_missing_pod_keeps_owned_starting_controller_not_ready() {
    let (state, runtime, _directory) = fixture(vec![None], false).await;
    runtime.workload_exists.store(true, Ordering::SeqCst);
    *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Starting;
    let before = state.userapp_store.get_application(APP_ID).await.unwrap();
    let locator = make_locator(&state);
    assert!(matches!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID).await,
        Err(shared_types::DevEnsureError::NotReady { .. })
    ));
    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert!(
        shared_types::UserappDevLocator::dev_logs_file_server_addr(&locator, APP_ID)
            .await
            .is_err()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn dev_locator_owned_stopped_controller_allows_wake_but_protects_storage() {
    let (state, runtime, _directory) = fixture(vec![None], false).await;
    runtime.workload_exists.store(true, Ordering::SeqCst);
    *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Stopped;
    let before = state.userapp_store.get_application(APP_ID).await.unwrap();
    let locator = make_locator(&state);
    assert!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert!(
        shared_types::UserappDevLocator::dev_logs_file_server_addr(&locator, APP_ID)
            .await
            .is_err()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        before
    );
}

#[tokio::test]
async fn dev_locator_verified_stopped_docker_container_allows_wake() {
    let mut observed = running_builder();
    observed.status = ContainerRuntimeStatus::Succeeded;
    observed.workload_uid = None;
    observed.project_id = None;
    let (state, runtime, _directory) = fixture_on_runtime(vec![Some(observed)], false, true).await;
    *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Stopped;
    let locator = make_locator(&state);
    assert!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn dev_locator_missing_pod_does_not_bypass_unknown_or_foreign_workload() {
    for old_lifecycle in [false, true] {
        let (state, runtime, _directory) = fixture(vec![None], old_lifecycle).await;
        runtime.workload_exists.store(true, Ordering::SeqCst);
        *runtime.readiness.lock().unwrap() = if old_lifecycle {
            shared_types::UserAppNoComputeState::Starting
        } else {
            shared_types::UserAppNoComputeState::Unknown
        };
        assert_read_only_rejection(&state, &runtime).await;
    }
}

#[tokio::test]
async fn dev_locator_missing_address_binds_separate_readiness_instance() {
    for wrong_identity in [false, true] {
        let (state, runtime, _directory) = fixture(vec![None], false).await;
        runtime.workload_exists.store(true, Ordering::SeqCst);
        runtime.pod_exists.store(true, Ordering::SeqCst);
        *runtime.running_observation.lock().unwrap() =
            Some(container_runtime_api::UserAppReadinessTarget {
                app_id: APP_ID.into(),
                stage: shared_types::UserappStage::Dev,
                instance: container_runtime_api::UserAppReadinessInstance::Kubernetes {
                    namespace: "test-ns".into(),
                    pod_name: "builder-locator-app-0".into(),
                    pod_uid: "pod-current".into(),
                    container_name: "agent".into(),
                    container_id: None,
                    owner_uid: if wrong_identity {
                        "foreign-sts".into()
                    } else {
                        "sts-current".into()
                    },
                },
                address: None,
                published_address: None,
                dbx_published_address: None,
            });
        if wrong_identity {
            assert_read_only_rejection(&state, &runtime).await;
        } else {
            let result =
                shared_types::UserappDevEnsure::locate_dev_builder(&make_locator(&state), APP_ID)
                    .await;
            assert!(matches!(
                result,
                Err(shared_types::DevEnsureError::NotReady { .. })
            ));
            assert!(shared_types::UserappDevLocator::dev_container_alive(
                &make_locator(&state),
                APP_ID,
            )
            .await
            .unwrap());
            assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
        }
    }
}

#[tokio::test]
async fn dev_locator_missing_root_requires_runtime_absence_before_ensure() {
    let (state, runtime, _directory) =
        fixture_on_application(vec![None], false, false, false).await;
    let locator = make_locator(&state);
    assert!(
        shared_types::UserappDevEnsure::locate_dev_builder(&locator, APP_ID)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        !shared_types::UserappDevLocator::dev_container_alive(&locator, APP_ID)
            .await
            .unwrap()
    );
    assert!(
        state
            .userapp_store
            .get_application(APP_ID)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);

    runtime.workload_exists.store(true, Ordering::SeqCst);
    *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Starting;
    assert_read_only_rejection(&state, &runtime).await;
}

#[tokio::test]
async fn dev_locator_missing_pod_replacement_cannot_become_absent() {
    let (state, runtime, _directory) = fixture(vec![None], false).await;
    runtime.workload_exists.store(true, Ordering::SeqCst);
    runtime
        .remove_workload_on_readiness
        .store(true, Ordering::SeqCst);
    *runtime.readiness.lock().unwrap() = shared_types::UserAppNoComputeState::Missing;
    let result =
        shared_types::UserappDevEnsure::locate_dev_builder(&make_locator(&state), APP_ID).await;
    assert!(matches!(
        result,
        Err(shared_types::DevEnsureError::ObserveFailed { .. })
    ));
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

/// A parseable old address is not a live file peer. The original ensure must
/// observe absence before trying to read that vanished peer's credentials.
#[tokio::test]
async fn registered_builder_with_valid_old_address_yields_missing_before_file_credentials() {
    let (state, runtime, _directory) = fixture_on_runtime(vec![None], false, true).await;
    let registered = ContainerBasicInfo {
        container_id: "removed-builder-id".into(),
        container_name: format!("rcoder-app-builder-{APP_ID}"),
        container_ip: "192.0.2.9".into(),
        internal_port: shared_types::AGENT_FILE_SERVER_PORT,
        external_port: 0,
        project_id: APP_ID.into(),
        status: "Running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://192.0.2.9:60000".into(),
        workload_uid: None,
    };
    #[cfg(feature = "deploy-host")]
    let published_key = {
        use std::collections::HashMap;
        let key = shared_types::build_backend_addr(
            &registered.container_name,
            &registered.container_ip,
            &state.config.app_manager.namespace,
            &state.cluster_domain,
        );
        // Published mode needs the old peer's actual address-layer mapping.
        // It only makes the cached address parseable; Runtime still proves the
        // physical container absent and the test must not read credentials.
        assert!(
            shared_types::published::begin_physical_observation(&registered.container_id).register(
                &[key.as_str()],
                registered.created_at,
                shared_types::published::Reach::Published {
                    ports: HashMap::from([(shared_types::AGENT_FILE_SERVER_PORT, 16000)]),
                }
            )
        );
        key
    };
    assert!(dev_file_server_addr(&state, &registered).is_ok());
    let mut project = ProjectAndContainerInfo::new(APP_ID.into());
    project.set_service_type(Some(ServiceType::UserappBuilder));
    project.set_container(Some(registered.clone()));
    state
        .insert_project(APP_ID.into(), Arc::new(project))
        .unwrap();
    let captured_project = state.get_project(APP_ID).unwrap();
    let captured_app = state.userapp_store.get_application(APP_ID).await.unwrap();

    let actual = super::super::registered_or_discovered_builder(&state, APP_ID)
        .await
        .unwrap();
    assert!(
        actual.is_none(),
        "the vanished peer must reach ordinary creation: {actual:?}"
    );
    assert_eq!(
        runtime.reads.load(Ordering::SeqCst),
        1,
        "real runtime observation is required"
    );
    assert_eq!(
        runtime.credential_reads.load(Ordering::SeqCst),
        0,
        "no missing peer credential wait"
    );
    assert_eq!(
        runtime.writes.load(Ordering::SeqCst),
        0,
        "discovery must not mutate compute or storage"
    );
    let current_project = state.get_project(APP_ID).unwrap();
    assert!(
        Arc::ptr_eq(&captured_project, &current_project),
        "read-side absence cannot retire a replacement registration"
    );
    assert_eq!(current_project.container_info(), Some(registered));
    assert_eq!(
        state.userapp_store.get_application(APP_ID).await.unwrap(),
        captured_app
    );
    #[cfg(feature = "deploy-host")]
    assert!(shared_types::published::unregister_if_physical(
        &published_key,
        "removed-builder-id"
    ));
}
