use super::*;
use container_runtime_api::{
    AgentContainerRuntime, AgentResourceKind, ComputeDataProtection, ContainerCreateParams,
    ContainerRuntimeError, ContainerRuntimeResult, RemovedContainerInfo, RuntimeContainerInfo,
    UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

fn service() -> AgentStopSnapshot {
    AgentStopSnapshot {
        app_id_hint: None,
        namespace: Some("isolated".into()),
        identifier: "example".into(),
        service_type: ServiceType::ComputerAgentRunner,
        controller: None,
        pods: vec![],
        container: None,
        services: vec![AgentResourceIdentity {
            kind: AgentResourceKind::Service,
            name: "example-svc".into(),
            uid: "service-original".into(),
            resource_version: Some("1".into()),
            created_at: Some(Utc::now() - chrono::Duration::hours(2)),
            owner_uid: None,
        }],
        replicas: None,
        compute_data_protection: None,
        endpoint: None,
    }
}

#[test]
fn candidate_requires_two_separated_successful_observations_and_resets_on_identity_change() {
    let snapshot = service();
    let now = Instant::now();
    let interval = Duration::from_secs(10);
    let first = Candidate::observe(None, &snapshot, now, interval);
    assert!(!first.ready(now + Duration::from_secs(3600), Duration::from_secs(3600)));
    let repeated = Candidate::observe(Some(&first), &snapshot, now, interval);
    assert_eq!(repeated.observations, 1);
    let second = Candidate::observe(
        Some(&first),
        &snapshot,
        now + Duration::from_secs(3600),
        interval,
    );
    assert!(second.ready(now + Duration::from_secs(3600), Duration::from_secs(3600)));
    for changed in ["2", "successor"] {
        let mut replacement = snapshot.clone();
        replacement.services[0].resource_version = Some(changed.into());
        let reset = Candidate::observe(
            Some(&second),
            &replacement,
            now + Duration::from_secs(3600),
            interval,
        );
        assert_eq!(reset.observations, 1);
        assert!(!reset.ready(now + Duration::from_secs(3600), Duration::from_secs(3600)));
    }
}

#[test]
fn only_empty_zero_replica_controllers_and_services_are_eligible() {
    let base = service();
    let protection = Duration::from_secs(300);
    assert_eq!(protected_shape(&base, Utc::now(), protection), None);
    let mut controller = base.clone();
    let mut identity = base.services[0].clone();
    identity.kind = AgentResourceKind::StatefulSet;
    controller.controller = Some(identity.clone());
    controller.replicas = Some(0);
    assert_eq!(protected_shape(&controller, Utc::now(), protection), None);
    for replicas in [None, Some(1)] {
        controller.replicas = replicas;
        assert_eq!(
            protected_shape(&controller, Utc::now(), protection),
            Some(Classification::LiveOrUnknown)
        );
    }
    controller.replicas = Some(0);
    identity.kind = AgentResourceKind::Pod;
    controller.pods.push(identity);
    assert_eq!(
        protected_shape(&controller, Utc::now(), protection),
        Some(Classification::LiveOrUnknown)
    );
    let mut docker = base;
    docker.container = Some(docker.services[0].clone());
    assert_eq!(
        protected_shape(&docker, Utc::now(), protection),
        // No endpoint and no Docker evidence: idleness can never be probed.
        Some(Classification::ObservationIncomplete)
    );
    docker.endpoint = Some("http://127.0.0.1:50051".into());
    docker.compute_data_protection = Some(ComputeDataProtection {
        persistent_mounts: true,
        auto_remove: false,
    });
    assert_eq!(
        protected_shape(&docker, Utc::now(), protection),
        None,
        "live Docker compute with full evidence is shape-eligible for observation"
    );
    docker.compute_data_protection = Some(ComputeDataProtection {
        persistent_mounts: false,
        auto_remove: false,
    });
    assert_eq!(
        protected_shape(&docker, Utc::now(), protection),
        Some(Classification::DataProtectionRequired)
    );
}

#[test]
fn every_resource_needs_current_identity_and_its_own_protection_period() {
    let base = service();
    let mut missing = base.clone();
    missing.services[0].resource_version = None;
    assert_eq!(
        protected_shape(&missing, Utc::now(), Duration::ZERO),
        Some(Classification::ObservationIncomplete)
    );
    missing = base.clone();
    missing.services[0].created_at = None;
    assert_eq!(
        protected_shape(&missing, Utc::now(), Duration::ZERO),
        Some(Classification::ObservationIncomplete)
    );
    let mut young = base.clone();
    let mut newer = young.services[0].clone();
    newer.created_at = Some(Utc::now());
    young.services.push(newer);
    assert_eq!(
        protected_shape(&young, Utc::now(), Duration::from_secs(300)),
        Some(Classification::WithinProtection)
    );
    let empty = AgentStopSnapshot {
        app_id_hint: None,
        services: vec![],
        ..base
    };
    assert_eq!(
        protected_shape(&empty, Utc::now(), Duration::ZERO),
        Some(Classification::ObservationIncomplete)
    );
}

#[test]
fn legacy_builder_scope_is_observed_without_guessing_or_blocking_current_app() {
    let mut snapshot = service();
    snapshot.service_type = ServiceType::UserappBuilder;
    snapshot.identifier = "collaborator-app-with-dash".into();
    for hint in [None, Some("app-with-dash".into())] {
        snapshot.app_id_hint = hint;
        assert_eq!(
            protected_shape(&snapshot, Utc::now(), Duration::ZERO),
            Some(Classification::ObservationIncomplete)
        );
    }
    snapshot.identifier = "app-with-dash".into();
    snapshot.app_id_hint = Some("app-with-dash".into());
    assert_eq!(protected_shape(&snapshot, Utc::now(), Duration::ZERO), None);
}

#[test]
fn configuration_defaults_to_reclaim_and_rejects_unknown_modes() {
    let old: CleanupConfigSettings = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(old.orphan_reconcile_mode, OrphanReconcileMode::Reclaim);
    let observe: CleanupConfigSettings =
        serde_json::from_value(serde_json::json!({"orphan_reconcile_mode":"observe"})).unwrap();
    assert_eq!(observe.orphan_reconcile_mode, OrphanReconcileMode::Observe);
    assert!(
        serde_json::from_value::<CleanupConfigSettings>(
            serde_json::json!({"orphan_reconcile_mode":"force"})
        )
        .is_err()
    );
}

/// 2026-10-26 移除的准入键在已部署配置中可能残留；serde 必须忽略未知键
/// 而不是拒绝整份配置，否则升级后清理任务直接失效。
#[test]
fn configuration_ignores_removed_legacy_cid_admission_key() {
    let config: CleanupConfigSettings = serde_json::from_value(serde_json::json!({
        "legacy_cid_admission": ["0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"]
    }))
    .unwrap();
    assert_eq!(config.orphan_reconcile_mode, OrphanReconcileMode::Reclaim);
    assert_eq!(config.cleanup_interval_seconds, 300);
}

struct Runtime {
    snapshot: Mutex<AgentStopSnapshot>,
    extra_snapshots: Mutex<Vec<AgentStopSnapshot>>,
    failed: AtomicBool,
    deletes: AtomicUsize,
    capture_entered: tokio::sync::Notify,
    capture_release: tokio::sync::Notify,
    capture_blocked: AtomicBool,
    /// Live builder observation for the registration-recovery path. `None` =
    /// the runtime reports no builder (the historical fixture behaviour).
    builder: Mutex<Option<RuntimeContainerInfo>>,
    /// Workspace volume witness for the registration-recovery path.
    volumes: Mutex<VolumeWitness>,
    /// Slow builder observation (registration-recovery path): the budget
    /// counterexample blocks here.
    builder_blocked: AtomicBool,
    builder_entered: tokio::sync::Notify,
    builder_release: tokio::sync::Notify,
    /// Live volumes no longer match the recorded discovery witness.
    volumes_replaced: AtomicBool,
}

/// What the runtime reports for a builder's workspace volumes. `Absent` is the
/// historical fixture behaviour (no volume evidence at all).
#[derive(Clone, Default)]
enum VolumeWitness {
    #[default]
    Absent,
    Present(Vec<shared_types::AppResourceIdentity>),
    /// The runtime refuses the witness, e.g. a same-named PVC recreated under
    /// another lifecycle or a deleted volume.
    Refused(String),
}

#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    fn agent_workload_name(
        &self,
        identifier: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        Ok(identifier.into())
    }
    fn agent_workload_namespace(&self) -> String {
        "isolated".into()
    }
    async fn agent_resource_inventory(&self) -> ContainerRuntimeResult<Vec<AgentStopSnapshot>> {
        if self.failed.load(Ordering::Acquire) {
            return Err(ContainerRuntimeError::K8sError(
                "injected partial list failure".into(),
            ));
        }
        let mut all = vec![self.snapshot.lock().unwrap().clone()];
        all.extend(self.extra_snapshots.lock().unwrap().clone());
        Ok(all)
    }
    async fn capture_agent_stop(
        &self,
        identifier: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<AgentStopSnapshot> {
        if self.capture_blocked.load(Ordering::Acquire) {
            self.capture_entered.notify_one();
            self.capture_release.notified().await;
        }
        let first = self.snapshot.lock().unwrap().clone();
        if first.identifier == identifier {
            return Ok(first);
        }
        Ok(self
            .extra_snapshots
            .lock()
            .unwrap()
            .iter()
            .find(|entry| entry.identifier == identifier)
            .unwrap()
            .clone())
    }
    async fn retire_agent_remnants(
        &self,
        snapshot: &AgentStopSnapshot,
    ) -> ContainerRuntimeResult<AgentStopOutcome> {
        assert!(
            std::iter::once(self.snapshot.lock().unwrap().clone())
                .chain(self.extra_snapshots.lock().unwrap().clone())
                .any(|current| current == *snapshot),
            "deletion must use an original frozen target"
        );
        self.deletes.fetch_add(1, Ordering::AcqRel);
        Ok(AgentStopOutcome::Complete)
    }
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("scanner must not create")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        _: &str,
        family: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        if *family == ServiceType::UserappBuilder {
            if self.builder_blocked.load(Ordering::Acquire) {
                let released = self.builder_release.notified();
                tokio::pin!(released);
                released.as_mut().enable();
                self.builder_entered.notify_one();
                released.await;
            }
            return Ok(self.builder.lock().unwrap().clone());
        }
        Ok(None)
    }
    async fn inspect_builder_candidate(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.builder_target(context)
    }
    async fn capture_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.builder_target(context)
    }
    async fn capture_bound_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
        _binding: Option<&shared_types::UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        self.builder_target(context)
    }
    async fn capture_builder_compute_volumes(
        &self,
        _target: &shared_types::BuilderControlTarget,
    ) -> ContainerRuntimeResult<Vec<shared_types::AppResourceIdentity>> {
        match self.volumes.lock().unwrap().clone() {
            VolumeWitness::Absent => Ok(Vec::new()),
            VolumeWitness::Present(volumes) => Ok(volumes),
            VolumeWitness::Refused(reason) => Err(ContainerRuntimeError::Conflict(reason)),
        }
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("scanner must not use name-based stop")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("global cleanup forbidden")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        Ok((0, vec![]))
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
    /// Mirrors the K8s implementation's contract: the live PVC must carry the
    /// recorded UID and must not be deleting. `Replaced` reproduces a
    /// same-named PVC recreated with a new UID.
    async fn verify_recovered_volumes(
        &self,
        _context: &shared_types::UserAppExecutionContext,
        _scope: shared_types::UserAppOperationScope,
        volumes: &[shared_types::AppResourceIdentity],
    ) -> ContainerRuntimeResult<()> {
        if self.volumes_replaced.load(Ordering::Acquire) {
            return Err(ContainerRuntimeError::Conflict(
                "Recovered workspace volume was replaced or is deleting".into(),
            ));
        }
        for expected in volumes {
            if expected.uid.is_empty() || expected.name.is_empty() {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "Invalid recovered volume witness".into(),
                ));
            }
        }
        Ok(())
    }
}

impl Runtime {
    /// Live builder control target: the observed Pod/controller pair, with the
    /// StatefulSet workload the identity checks compare against.
    fn builder_target(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        let observed = self
            .builder
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| ContainerRuntimeError::K8sError("no builder observed".into()))?;
        Ok(shared_types::BuilderControlTarget {
            context: context.clone(),
            resource_binding: None,
            workload: Some(shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::StatefulSet,
                name: observed.container_name.clone(),
                uid: observed.workload_uid.clone().ok_or_else(|| {
                    ContainerRuntimeError::K8sError("workload uid missing".into())
                })?,
                resource_version: Some("1".into()),
            }),
            pod: Some(shared_types::BuilderPodIdentity {
                name: format!("{}-0", observed.container_name),
                uid: observed.container_id,
                resource_version: "2".into(),
            }),
            restart_image: None,
            restart_runtime_workspace: None,
            builder_startup_mode: Default::default(),
        })
    }
}

#[cfg(feature = "userapp-turso")]
async fn fixture() -> (Scanner, Arc<Runtime>, Arc<ScriptedProbe>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("orphan.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(Runtime {
        snapshot: Mutex::new(service()),
        extra_snapshots: Mutex::new(Vec::new()),
        failed: AtomicBool::new(false),
        deletes: AtomicUsize::new(0),
        capture_entered: tokio::sync::Notify::new(),
        capture_release: tokio::sync::Notify::new(),
        capture_blocked: AtomicBool::new(false),
        builder: Mutex::new(None),
        volumes: Mutex::new(VolumeWitness::default()),
        builder_blocked: AtomicBool::new(false),
        builder_entered: tokio::sync::Notify::new(),
        builder_release: tokio::sync::Notify::new(),
        volumes_replaced: AtomicBool::new(false),
    });
    let state =
        crate::userapp_builder::builder_runtime_test_state(runtime.clone(), store, None).await;
    let config = CleanupConfigSettings {
        cleanup_interval_seconds: 1,
        long_idle_timeout_seconds: 2,
        container_protection_seconds: 0,
        ..Default::default()
    };
    let probe = Arc::new(ScriptedProbe::default());
    (
        Scanner {
            state,
            config,
            activity_probe: probe.clone(),
            seen: HashMap::new(),
            next_workload: None,
        },
        runtime,
        probe,
        directory,
    )
}

// Real database IO must not run under Tokio auto-advancing paused time.
// Move only the deterministic observation clock; deployment E2E uses elapsed time.
fn age_candidates(scanner: &mut Scanner) {
    for candidate in scanner.seen.values_mut() {
        candidate.since -= Duration::from_secs(2);
        candidate.last -= Duration::from_secs(2);
    }
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn unreferenced_service_is_reclaimed_after_real_continuous_observation() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 1);
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn observation_error_breaks_continuity_and_original_scan_recovers() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    scanner.scan().await.unwrap();
    age_candidates(&mut scanner);
    runtime.failed.store(true, Ordering::Release);
    assert!(scanner.scan().await.is_err());
    assert!(scanner.seen.is_empty());
    runtime.failed.store(false, Ordering::Release);
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 1);
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn observe_mode_and_referenced_builder_never_retire_compute() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    scanner.config.orphan_reconcile_mode = OrphanReconcileMode::Observe;
    scanner.scan().await.unwrap();
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    scanner.config.orphan_reconcile_mode = OrphanReconcileMode::Reclaim;
    runtime.snapshot.lock().unwrap().service_type = ServiceType::UserappBuilder;
    scanner
        .state
        .userapp_store
        .ensure_identity("example")
        .await
        .unwrap();
    for _ in 0..2 {
        scanner.scan().await.unwrap();
        age_candidates(&mut scanner);
    }
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    assert!(scanner.seen.is_empty());
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn canonical_registration_arriving_after_capture_prevents_retirement() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    scanner.config.long_idle_timeout_seconds = 0;
    scanner.config.cleanup_interval_seconds = 0;
    scanner.scan().await.unwrap();
    // Advance only the candidate's observation baseline, not the live source.
    scanner
        .seen
        .values_mut()
        .for_each(|candidate| candidate.last -= Duration::from_secs(2));
    runtime.capture_blocked.store(true, Ordering::Release);
    let state = scanner.state.clone();
    let observed_runtime = runtime.clone();
    let scan = tokio::spawn(async move { scanner.scan().await });
    observed_runtime.capture_entered.notified().await;
    let mut project = ProjectAndContainerInfo::new("new-project".into());
    project.set_user_id(Some("example".into()));
    project.set_service_type(Some(ServiceType::ComputerAgentRunner));
    project.set_container(Some(ContainerBasicInfo {
        container_id: "new-cid".into(),
        container_name: "example".into(),
        container_ip: "127.0.0.1".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: "example".into(),
        status: "Running".into(),
        created_at: Utc::now(),
        service_url: "http://127.0.0.1:8086".into(),
        workload_uid: None,
    }));
    state
        .projects
        .insert("new-project".into(), Arc::new(project))
        .unwrap();
    observed_runtime.capture_release.notify_one();
    scan.await.unwrap().unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    assert!(state.projects.get("new-project").is_some());
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn scan_budget_advances_past_blocked_workload_without_resetting_other_candidates() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    scanner.config.orphan_scan_budget_seconds = 1;
    scanner.config.docker_stop_timeout_seconds = 90;
    let mut later = service();
    later.identifier = "later".into();
    later.services[0].name = "later-service".into();
    runtime.extra_snapshots.lock().unwrap().push(later);
    let blocked =
        WorkloadKey::new("isolated", ServiceType::ComputerAgentRunner, "example").unwrap();
    let guard = scanner
        .state
        .projects
        .acquire_workload_admission(
            blocked,
            WorkloadAdmissionMode::Exclusive,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    scanner.scan().await.unwrap();
    assert_eq!(scanner.next_workload.as_deref(), Some("later"));
    scanner.scan().await.unwrap();
    let later_key =
        WorkloadKey::new("isolated", ServiceType::ComputerAgentRunner, "later").unwrap();
    assert_eq!(
        scanner.seen.get(&later_key).unwrap().observations,
        1,
        "timeout of the first workload must not erase another workload's completed observation"
    );
    drop(guard);
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        1,
        "the later workload must eventually complete two canonical observations and retire"
    );
}

/// Deterministic activity probe for discovered live compute.
struct ScriptedProbe {
    busy: AtomicBool,
    fail: AtomicBool,
    calls: AtomicUsize,
}

impl Default for ScriptedProbe {
    fn default() -> Self {
        Self {
            busy: AtomicBool::new(false),
            fail: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl ContainerActivityProbe for ScriptedProbe {
    async fn probe(&self, _snapshot: &AgentStopSnapshot) -> Result<Activity> {
        self.calls.fetch_add(1, Ordering::AcqRel);
        if self.fail.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!("probe endpoint unavailable"));
        }
        if self.busy.load(Ordering::Acquire) {
            Ok(Activity::Busy)
        } else {
            Ok(Activity::Idle)
        }
    }
}

/// A live Docker web-agent container that lost its registration (for example
/// after a memory-backend restart): full CID, StartedAt evidence, persistent
/// workspace mounts, and a probe endpoint.
#[cfg(feature = "userapp-turso")]
fn docker_web_agent() -> AgentStopSnapshot {
    AgentStopSnapshot {
        app_id_hint: None,
        namespace: Some("isolated".into()),
        identifier: "web-legacy".into(),
        service_type: ServiceType::WebAgentRunner,
        controller: None,
        pods: vec![],
        container: Some(AgentResourceIdentity {
            kind: AgentResourceKind::Container,
            name: "rcoder-web-agent-web-legacy".into(),
            uid: "cid-legacy-0001".into(),
            resource_version: Some("2026-10-24T00:00:00Z".into()),
            created_at: Some(Utc::now() - chrono::Duration::hours(48)),
            owner_uid: None,
        }),
        services: vec![],
        replicas: None,
        endpoint: Some("http://127.0.0.1:50051".into()),
        compute_data_protection: Some(ComputeDataProtection {
            persistent_mounts: true,
            auto_remove: false,
        }),
    }
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn docker_web_agent_without_registration_is_reclaimed_after_continuous_idle() {
    let (mut scanner, runtime, probe, _directory) = fixture().await;
    *runtime.snapshot.lock().unwrap() = docker_web_agent();
    scanner.scan().await.unwrap();
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        0,
        "first observation must never retire"
    );
    assert!(
        scanner
            .seen
            .values()
            .all(|candidate| candidate.observations >= 1),
        "confirmed-idle candidate must accumulate observation continuity"
    );
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        1,
        "unregistered live web agent must be reclaimed once the idle window closes"
    );
    assert!(
        probe.calls.load(Ordering::Acquire) >= 2,
        "idle must be re-confirmed under exclusive admission before retirement"
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn docker_busy_activity_resets_the_idle_window() {
    let (mut scanner, runtime, probe, _directory) = fixture().await;
    *runtime.snapshot.lock().unwrap() = docker_web_agent();
    probe.busy.store(true, Ordering::Release);
    for _ in 0..2 {
        scanner.scan().await.unwrap();
        age_candidates(&mut scanner);
    }
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    assert!(
        scanner.seen.is_empty(),
        "a busy agent must not keep idle observation continuity"
    );
    probe.busy.store(false, Ordering::Release);
    scanner.scan().await.unwrap();
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        1,
        "the idle window must restart from the first confirmed idle observation"
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn docker_probe_failure_preserves_compute_and_resets_continuity() {
    let (mut scanner, runtime, probe, _directory) = fixture().await;
    *runtime.snapshot.lock().unwrap() = docker_web_agent();
    probe.fail.store(true, Ordering::Release);
    for _ in 0..2 {
        scanner.scan().await.unwrap();
        age_candidates(&mut scanner);
    }
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    assert!(scanner.seen.is_empty());
    probe.fail.store(false, Ordering::Release);
    scanner.scan().await.unwrap();
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 1);
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn docker_same_cid_restart_resets_observation_continuity() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    *runtime.snapshot.lock().unwrap() = docker_web_agent();
    scanner.scan().await.unwrap();
    age_candidates(&mut scanner);
    runtime
        .snapshot
        .lock()
        .unwrap()
        .container
        .as_mut()
        .unwrap()
        .resource_version = Some("2026-10-24T12:00:00Z".into());
    scanner.scan().await.unwrap();
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        0,
        "a same-CID restart must invalidate pre-restart idle evidence"
    );
    let key = WorkloadKey::new("isolated", ServiceType::WebAgentRunner, "web-legacy").unwrap();
    assert_eq!(
        scanner.seen.get(&key).unwrap().observations,
        1,
        "observation must restart from scratch after the StartedAt change"
    );
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 1);
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn docker_workspace_without_persistent_mounts_is_never_retired() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    let mut snapshot = docker_web_agent();
    snapshot.compute_data_protection = Some(ComputeDataProtection {
        persistent_mounts: false,
        auto_remove: true,
    });
    *runtime.snapshot.lock().unwrap() = snapshot;
    for _ in 0..3 {
        scanner.scan().await.unwrap();
        age_candidates(&mut scanner);
    }
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        0,
        "writable-layer data must block automatic retirement"
    );
    assert!(scanner.seen.is_empty());
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn docker_userapp_builder_stays_observe_only() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    let mut snapshot = docker_web_agent();
    snapshot.service_type = ServiceType::UserappBuilder;
    snapshot.app_id_hint = Some(snapshot.identifier.clone());
    *runtime.snapshot.lock().unwrap() = snapshot;
    for _ in 0..3 {
        scanner.scan().await.unwrap();
        age_candidates(&mut scanner);
    }
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        0,
        "builder compute keeps its own lifecycle authority"
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn docker_web_agent_with_canonical_registration_stays_with_the_cleaner() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    *runtime.snapshot.lock().unwrap() = docker_web_agent();
    let mut project = ProjectAndContainerInfo::new("registered-holder".into());
    project.set_user_id(Some("web-legacy".into()));
    project.set_service_type(Some(ServiceType::WebAgentRunner));
    project.set_container(Some(ContainerBasicInfo {
        container_id: "cid-legacy-0001".into(),
        container_name: "web-legacy".into(),
        container_ip: "127.0.0.1".into(),
        internal_port: 50051,
        external_port: 0,
        project_id: "registered-holder".into(),
        status: "Running".into(),
        created_at: Utc::now(),
        service_url: "http://127.0.0.1:8086".into(),
        workload_uid: None,
    }));
    scanner
        .state
        .projects
        .insert("registered-holder".into(), Arc::new(project))
        .unwrap();
    for _ in 0..3 {
        scanner.scan().await.unwrap();
        age_candidates(&mut scanner);
    }
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        0,
        "registered compute must remain under the cleaner's authority"
    );
}

#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn deferred_live_docker_workload_cannot_credit_unobserved_idle_time() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    *runtime.snapshot.lock().unwrap() = docker_web_agent();
    scanner.scan().await.unwrap();
    age_candidates(&mut scanner);
    let old = scanner.seen.values().next().unwrap().clone();
    // A complete inventory is unchanged but admission consumed the scan budget.
    scanner.config.orphan_scan_budget_seconds = 1;
    let key = WorkloadKey::new("isolated", ServiceType::WebAgentRunner, "web-legacy").unwrap();
    let exclusive = scanner
        .state
        .projects
        .acquire_workload_admission(
            key,
            WorkloadAdmissionMode::Exclusive,
            Instant::now() + Duration::from_secs(3),
        )
        .await
        .unwrap();
    scanner.scan().await.unwrap();
    drop(exclusive);
    assert!(scanner.seen.is_empty());
    scanner.scan().await.unwrap();
    let fresh = scanner.seen.values().next().unwrap();
    assert!(fresh.since > old.since);
    assert_eq!(fresh.observations, 1);
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
}

#[cfg(feature = "userapp-turso")]
mod probe;

/// 现场 8 个 builder 的形态：UserApp 生命周期引用仍在（`ensure_identity`）、
/// 真实同 controller 的 Pod 存活、`projects` 无登记。修复前反向库存只给
/// `LifecycleReferenced`，普通 scanner 又遍历不到——永久不进回收链。
///
/// 修复后：同一当前生命周期的有效绑定即可补齐 canonical 登记（不创建/不启动/
/// 不重放迁移），实例进入普通 scanner；且首期登记获得**新的**闲置观察窗，
/// 不会因"刚发现"就被删。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn live_builder_without_registration_is_completed_from_current_lifecycle_binding() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    // 记录在案的原卷期望（与 live 卷一致）：第二层卷验证通过。
    record_discovery_witness(&scanner.state, "witness-lifecycle", "pvc-current").await;
    let app = scanner
        .state
        .userapp_store
        .get_application("example")
        .await
        .unwrap()
        .expect("witness restore creates the root");
    *runtime.snapshot.lock().unwrap() = live_builder();
    *runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "pod-live".into(),
        container_name: "example".into(),
        container_ip: "10.42.4.207".into(),
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some("example".into()),
        app_id: Some("example".into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    });
    bind_current_lifecycle(&scanner.state, &app.lifecycle_id).await;
    *runtime.volumes.lock().unwrap() =
        VolumeWitness::Present(vec![shared_types::AppResourceIdentity {
            kind: shared_types::AppResourceKind::PersistentVolumeClaim,
            name: "example-workspace".into(),
            uid: "pvc-current".into(),
            resource_version: None,
        }]);

    scanner.scan().await.unwrap();
    let registered = scanner
        .state
        .projects
        .get("example")
        .expect("missing builder registration must be completed");
    let container = registered.container_info().expect("builder container");
    assert_eq!(container.container_id, "pod-live");
    assert_eq!(container.container_ip, "10.42.4.207");
    assert_eq!(container.workload_uid.as_deref(), Some("sts-current"));
    assert_eq!(registered.service_type(), Some(ServiceType::UserappBuilder));
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        0,
        "completing a registration must never retire compute"
    );
    // 首期登记 = 新的闲置观察窗：不会因刚补齐就被回收。
    let idle = (Utc::now() - registered.last_activity()).num_seconds();
    assert!(
        idle < 5,
        "recovered registration must start its own idle window"
    );

    // 补齐后该 workload 已被登记：反向库存按 Registered 评估，不再需要恢复。
    age_candidates(&mut scanner);
    scanner.scan().await.unwrap();
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
    assert!(
        scanner
            .state
            .projects
            .get("example")
            .is_some_and(|project| project.container_info().is_some())
    );
}

/// 缺证据（无当前生命周期绑定、也无成功回执）时保留资源并给出明确分类——
/// 缺证据不是接管许可，也不是删除许可。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn live_builder_without_registration_or_evidence_is_retained() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    scanner
        .state
        .userapp_store
        .ensure_identity("example")
        .await
        .unwrap();
    *runtime.snapshot.lock().unwrap() = live_builder();
    *runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "pod-live".into(),
        container_name: "example".into(),
        container_ip: "10.42.4.207".into(),
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some("example".into()),
        app_id: Some("example".into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    });
    scanner.scan().await.unwrap();
    assert!(
        scanner
            .state
            .projects
            .get("example")
            .is_none_or(|project| project.container_info().is_none()),
        "no identity evidence must not create a registration"
    );
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
}

/// 恢复的各阶段必须共享扫描绝对预算（Codex 复核 P2）：慢的运行时观察不能让
/// 扫描整体越界，把后续 workload 和关机 drain 一起拖住。写入结果未知时不得
/// 计为成功。
#[tokio::test]
async fn registration_recovery_cannot_outlive_the_scan_budget() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    scanner.config.orphan_scan_budget_seconds = 1;
    scanner
        .state
        .userapp_store
        .ensure_identity("example")
        .await
        .unwrap();
    *runtime.snapshot.lock().unwrap() = live_builder();
    *runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "pod-live".into(),
        container_name: "example".into(),
        container_ip: "10.42.4.207".into(),
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some("example".into()),
        app_id: Some("example".into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    });
    runtime.builder_blocked.store(true, Ordering::Release);
    let observer = runtime.clone();
    let notified = tokio::spawn(async move { observer.builder_entered.notified().await });
    let scan = tokio::spawn(async move { scanner.scan().await });
    tokio::time::timeout(Duration::from_secs(5), notified)
        .await
        .expect("recovery must reach the runtime observation")
        .expect("observer task");
    // 预算内收手：整轮扫描必须返回，不能挂在慢查询上。
    let finished = tokio::time::timeout(Duration::from_secs(5), scan)
        .await
        .expect("a slow registration recovery must not outlive the scan budget");
    assert!(finished.unwrap().is_ok(), "budgeted scan still completes");
    assert_eq!(
        runtime.deletes.load(Ordering::Acquire),
        0,
        "an unfinished recovery never retires compute"
    );
    runtime.builder_release.notify_waiters();
}

/// 原卷连续性不成立时不得补齐登记（Codex 复核 P2）：运行时读到的卷见证被拒绝
/// ——现场形态是同名 PVC 在另一代次下被重建（UID 变、内容可能已丢），或卷正在
/// 删除。此时"当前生命周期绑定"不足以证明原卷连续，必须保留资源。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn live_builder_registration_recovery_refuses_unverified_workspace_volume() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    let app = scanner
        .state
        .userapp_store
        .ensure_identity("example")
        .await
        .unwrap();
    *runtime.snapshot.lock().unwrap() = live_builder();
    *runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "pod-live".into(),
        container_name: "example".into(),
        container_ip: "10.42.4.207".into(),
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some("example".into()),
        app_id: Some("example".into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    });
    bind_current_lifecycle(&scanner.state, &app.lifecycle_id).await;
    // 同名 PVC 已在该应用之外被重建：其生命周期注解不属于当前代次。
    *runtime.volumes.lock().unwrap() = VolumeWitness::Refused(
        "Application resource identity mismatch: rcoder.io/lifecycle-id".into(),
    );

    scanner.scan().await.unwrap();
    assert!(
        scanner
            .state
            .projects
            .get("example")
            .is_none_or(|project| project.container_info().is_none()),
        "an unverifiable workspace volume must not complete a registration"
    );
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
}

/// 完全没有卷证据（空见证）同样不补齐：没有卷证据就没有原卷连续性证据，
/// 空向量不构成"无卷也可登记"。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn live_builder_registration_recovery_requires_volume_evidence() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    let app = scanner
        .state
        .userapp_store
        .ensure_identity("example")
        .await
        .unwrap();
    *runtime.snapshot.lock().unwrap() = live_builder();
    *runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "pod-live".into(),
        container_name: "example".into(),
        container_ip: "10.42.4.207".into(),
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some("example".into()),
        app_id: Some("example".into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    });
    bind_current_lifecycle(&scanner.state, &app.lifecycle_id).await;
    // VolumeWitness::Absent（fixture 默认）：运行时读不到任何卷。

    scanner.scan().await.unwrap();
    assert!(
        scanner
            .state
            .projects
            .get("example")
            .is_none_or(|project| project.container_info().is_none()),
        "missing volume evidence must not complete a registration"
    );
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
}

/// 记录在案的原卷期望与 live 卷不一致时不得补齐登记（第二层卷验证）：同名 PVC
/// 被重建（UID 变）或正在删除。`verify_recovered_volumes` 按 UID 相等判定，
/// `verify_recovered_mounts` 复核挂载集——与 ensure 链复用同一验证。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn live_builder_registration_recovery_refuses_replaced_recorded_volume() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    // 记录在案的原卷 UID 与 live 不同：同名 PVC 被重建。
    record_discovery_witness(&scanner.state, "witness-lifecycle", "pvc-original").await;
    let app = scanner
        .state
        .userapp_store
        .get_application("example")
        .await
        .unwrap()
        .expect("witness restore creates the root");
    *runtime.snapshot.lock().unwrap() = live_builder();
    *runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "pod-live".into(),
        container_name: "example".into(),
        container_ip: "10.42.4.207".into(),
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some("example".into()),
        app_id: Some("example".into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    });
    bind_current_lifecycle(&scanner.state, &app.lifecycle_id).await;
    *runtime.volumes.lock().unwrap() =
        VolumeWitness::Present(vec![shared_types::AppResourceIdentity {
            kind: shared_types::AppResourceKind::PersistentVolumeClaim,
            name: "example-workspace".into(),
            uid: "pvc-recreated".into(),
            resource_version: None,
        }]);
    runtime.volumes_replaced.store(true, Ordering::Release);

    scanner.scan().await.unwrap();
    assert!(
        scanner
            .state
            .projects
            .get("example")
            .is_none_or(|project| project.container_info().is_none()),
        "a replaced recorded volume must not complete a registration"
    );
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
}

/// 在途 dev 操作围栏期间不得补齐登记：该应用的登记归属在途操作，后台对账不能
/// 抢在它前面为正在被替换/停止的 Pod 写行。围栏释放后才恢复。
#[cfg(feature = "userapp-turso")]
#[tokio::test]
async fn live_builder_registration_recovery_defers_to_inflight_operation() {
    let (mut scanner, runtime, _probe, _directory) = fixture().await;
    let app = scanner
        .state
        .userapp_store
        .ensure_identity("example")
        .await
        .unwrap();
    *runtime.snapshot.lock().unwrap() = live_builder();
    *runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "pod-live".into(),
        container_name: "example".into(),
        container_ip: "10.42.4.207".into(),
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some("example".into()),
        app_id: Some("example".into()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("sts-current".into()),
    });
    bind_current_lifecycle(&scanner.state, &app.lifecycle_id).await;
    *runtime.volumes.lock().unwrap() =
        VolumeWitness::Present(vec![shared_types::AppResourceIdentity {
            kind: shared_types::AppResourceKind::PersistentVolumeClaim,
            name: "example-workspace".into(),
            uid: "pvc-current".into(),
            resource_version: None,
        }]);
    // dev 槽在途操作（StopBuilder）即围栏。
    scanner
        .state
        .userapp_store
        .admit(&shared_types::UserAppAdmission {
            app_id: "example".into(),
            lifecycle_id: None,
            request_id: Some("fence-request".into()),
            operation_id: "fence-stop".into(),
            request_fingerprint: "a".repeat(64),
            kind: shared_types::UserAppOperationKind::StopBuilder,
            command: None,
            metadata: None,
            runtime_policy_on_success: None,
        })
        .await
        .unwrap();

    scanner.scan().await.unwrap();
    assert!(
        scanner
            .state
            .projects
            .get("example")
            .is_none_or(|project| project.container_info().is_none()),
        "an in-flight operation must own the registration decision"
    );
    assert_eq!(runtime.deletes.load(Ordering::Acquire), 0);
}

/// 记录在案的原卷期望（发现路径写入 `userapp_recovery_witnesses` 的同一数据）。
/// 必须在 `ensure_identity` 之前调用：`restore_identity` 只在根记录新建时落见证。
#[cfg(feature = "userapp-turso")]
async fn record_discovery_witness(state: &AppState, lifecycle_id: &str, volume_uid: &str) {
    state
        .userapp_store
        .restore_discovered_identity(&shared_types::UserAppDiscoveredIdentity {
            app_id: "example".into(),
            lifecycle_id: lifecycle_id.into(),
            dev_uid: Some("pod-live".into()),
            prod_uid: None,
            dev_stopped: false,
            prod_stopped: false,
            dev_volumes: vec![shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::PersistentVolumeClaim,
                name: "example-workspace".into(),
                uid: volume_uid.into(),
                resource_version: None,
            }],
            prod_volumes: Vec::new(),
        })
        .await
        .unwrap();
}

/// 存活 builder 的快照：controller + Pod + Service，年龄足够、身份完整。
#[cfg(feature = "userapp-turso")]
fn live_builder() -> AgentStopSnapshot {
    let created = Some(Utc::now() - chrono::Duration::hours(3));
    AgentStopSnapshot {
        app_id_hint: Some("example".into()),
        namespace: Some("isolated".into()),
        identifier: "example".into(),
        service_type: ServiceType::UserappBuilder,
        controller: Some(AgentResourceIdentity {
            kind: AgentResourceKind::StatefulSet,
            name: "example".into(),
            uid: "sts-current".into(),
            resource_version: Some("7".into()),
            created_at: created,
            owner_uid: None,
        }),
        pods: vec![AgentResourceIdentity {
            kind: AgentResourceKind::Pod,
            name: "example-0".into(),
            uid: "pod-live".into(),
            resource_version: Some("9".into()),
            created_at: created,
            owner_uid: Some("sts-current".into()),
        }],
        container: None,
        services: vec![AgentResourceIdentity {
            kind: AgentResourceKind::Service,
            name: "example-svc".into(),
            uid: "service-live".into(),
            resource_version: Some("3".into()),
            created_at: created,
            owner_uid: None,
        }],
        replicas: Some(1),
        compute_data_protection: None,
        endpoint: None,
    }
}

/// 当前生命周期对该 controller 的有效绑定（今天保护该 workload 的同一证据）。
#[cfg(feature = "userapp-turso")]
async fn bind_current_lifecycle(state: &AppState, lifecycle_id: &str) {
    let request = shared_types::AdoptBuilderRequest {
        lifecycle_id: lifecycle_id.into(),
        request_id: "binding-request".into(),
        expected_container_id: "pod-live".into(),
    };
    let input = shared_types::UserAppExecutionInput::new(serde_json::to_string(&request).unwrap());
    let admitted = state
        .userapp_store
        .admit_with_input(
            &shared_types::UserAppAdmission {
                app_id: "example".into(),
                lifecycle_id: Some(lifecycle_id.into()),
                request_id: Some("binding-request".into()),
                operation_id: "binding-operation".into(),
                request_fingerprint: input.digest(),
                kind: shared_types::UserAppOperationKind::AdoptBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            },
            Some(&input),
        )
        .await
        .unwrap();
    let running = match admitted {
        shared_types::UserAppAdmissionOutcome::Accepted(record) => state
            .userapp_store
            .advance(&shared_types::UserAppOperationProgress {
                app_id: record.app_id.clone(),
                lifecycle_id: record.lifecycle_id.clone(),
                operation_id: record.operation_id.clone(),
                expected_revision: record.revision,
                executor_id: "fixture".into(),
                state: shared_types::UserAppOperationState::Running,
                step: "claimed".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap(),
        other => panic!("fresh fixture admission: {other:?}"),
    };
    state
        .userapp_store
        .commit_resource_binding(
            &shared_types::UserAppResourceBinding {
                app_id: running.app_id.clone(),
                lifecycle_id: running.lifecycle_id.clone(),
                service_type: ServiceType::UserappBuilder,
                physical_uid: "sts-current".into(),
                adopted_by_operation: running.operation_id.clone(),
            },
            &shared_types::UserAppOperationProgress {
                app_id: running.app_id.clone(),
                lifecycle_id: running.lifecycle_id.clone(),
                operation_id: running.operation_id.clone(),
                expected_revision: running.revision,
                executor_id: "fixture".into(),
                state: shared_types::UserAppOperationState::Succeeded,
                step: "adopted".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            },
        )
        .await
        .unwrap();
}
