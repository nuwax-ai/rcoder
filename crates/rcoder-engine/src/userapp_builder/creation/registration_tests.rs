//! Registration repair exercises the real lifecycle store and project adapter.
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, ContainerRuntimeError, ContainerRuntimeResult, ContainerRuntimeStatus,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderCreationEvidence,
    BuilderCreationPredecessor, BuilderPodIdentity, ServiceType, UserAppExecutionContext,
    UserAppLifecycleStore,
};
use std::{
    collections::BTreeMap,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

struct ConfirmationRuntime {
    receipts: Mutex<BTreeMap<String, BuilderCreationEvidence>>,
    current: BuilderControlTarget,
    info: ContainerBasicInfo,
    volumes: Vec<AppResourceIdentity>,
    writes: AtomicUsize,
    recreated: std::sync::atomic::AtomicBool,
    allow_confirmation: std::sync::atomic::AtomicBool,
    confirmation_requests: AtomicUsize,
    probe_ip: Mutex<Option<String>>,
}

impl ConfirmationRuntime {
    fn observed_info(&self) -> ContainerBasicInfo {
        let mut info = self.info.clone();
        if self.recreated.load(Ordering::SeqCst) {
            info.container_id = format!("{}-recreated", self.info.container_id);
            info.created_at += chrono::Duration::seconds(1);
        }
        if let Some(ip) = self.probe_ip.lock().unwrap().as_ref() {
            info.container_ip = ip.clone();
            info.service_url = format!("http://{ip}:60000");
        }
        info
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for ConfirmationRuntime {
    async fn create_container(
        &self,
        params: container_runtime_api::ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        if !self.allow_confirmation.load(Ordering::SeqCst) {
            self.writes.fetch_add(1, Ordering::SeqCst);
            return Err(ContainerRuntimeError::ConfigurationError(
                "repair must not create".into(),
            ));
        }
        // Models the runtime's already-running ensure branch: record only a
        // private completion, without creating, deleting or restarting compute.
        self.confirmation_requests.fetch_add(1, Ordering::SeqCst);
        let context = params
            .execution_context
            .expect("admitted operation context");
        let mut target = self.capture_builder_control(&context).await?;
        target.resource_binding = params.resource_binding;
        let evidence = BuilderCreationEvidence {
            creation_lease_released: true,
            target,
            container: self.observed_info(),
            registration_predecessor: None,
        };
        self.receipts
            .lock()
            .unwrap()
            .insert(context.operation_id, evidence.clone());
        Ok(evidence.container)
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(Some(self.observed_info()))
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        let observed = self.observed_info();
        Ok(Some(RuntimeContainerInfo {
            container_id: observed.container_id.clone(),
            container_name: observed.container_name.clone(),
            container_ip: observed.container_ip.clone(),
            status: ContainerRuntimeStatus::Running,
            created_at: observed.created_at,
            env_vars: None,
            service_type: Some(ServiceType::UserappBuilder),
            project_id: Some(observed.project_id.clone()),
            app_id: Some(observed.project_id.clone()),
            user_id: None,
            pod_id: None,
            workload_uid: observed.workload_uid.clone(),
        }))
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "repair must not stop".into(),
        ))
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(true)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "repair must not clean".into(),
        ))
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn inspect_builder_candidate(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        self.capture_builder_control(context).await
    }
    async fn capture_builder_control(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        let mut current = self.current.clone();
        current.context = context.clone();
        if self.recreated.load(Ordering::SeqCst) {
            current.pod.as_mut().unwrap().uid = self.observed_info().container_id;
        }
        Ok(current)
    }
    async fn capture_bound_builder_control(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        let mut target = self.capture_builder_control(context).await?;
        if let Some(binding) = binding {
            binding
                .validate(context, &target.workload.as_ref().unwrap().uid)
                .map_err(ContainerRuntimeError::Conflict)?;
        }
        target.resource_binding = binding.cloned();
        Ok(target)
    }
    async fn recover_builder_creation(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<Option<BuilderCreationEvidence>> {
        Ok(self
            .receipts
            .lock()
            .unwrap()
            .get(&context.operation_id)
            .cloned())
    }
    async fn capture_builder_compute_volumes(
        &self,
        _: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<Vec<AppResourceIdentity>> {
        Ok(self.volumes.clone())
    }
    async fn refresh_container_reach(&self, _: &ContainerBasicInfo) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for ConfirmationRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for ConfirmationRuntime {
    async fn discover_application_identity(
        &self,
        app_id: &str,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppDiscoveredIdentity>> {
        if app_id != self.current.context.app_id {
            return Ok(None);
        }
        let found = shared_types::UserAppDiscoveredIdentity {
            app_id: app_id.into(),
            lifecycle_id: self.current.context.lifecycle_id.clone(),
            dev_uid: self
                .current
                .workload
                .as_ref()
                .map(|workload| workload.uid.clone()),
            prod_uid: None,
            dev_stopped: false,
            prod_stopped: false,
            dev_volumes: self.volumes.clone(),
            prod_volumes: vec![],
        };
        found.validate().map_err(ContainerRuntimeError::Conflict)?;
        Ok(Some(found))
    }
    async fn verify_recovered_volumes(
        &self,
        context: &UserAppExecutionContext,
        scope: shared_types::UserAppOperationScope,
        volumes: &[AppResourceIdentity],
    ) -> ContainerRuntimeResult<()> {
        context
            .validate_identity(&self.current.context.app_id)
            .map_err(ContainerRuntimeError::Conflict)?;
        if context.lifecycle_id != self.current.context.lifecycle_id {
            return Err(ContainerRuntimeError::Conflict(
                "fixture recovered lifecycle differs".into(),
            ));
        }
        let expected = match scope {
            shared_types::UserAppOperationScope::Dev => self.volumes.as_slice(),
            shared_types::UserAppOperationScope::Prod => &[],
            shared_types::UserAppOperationScope::Application => {
                return Err(ContainerRuntimeError::ConfigurationError(
                    "volume verification requires an explicit scope".into(),
                ));
            }
        };
        if volumes.len() != expected.len()
            || volumes
                .iter()
                .any(|volume| volume.kind != AppResourceKind::PersistentVolumeClaim)
            || volume_identities(expected) != volume_identities(volumes)
        {
            return Err(ContainerRuntimeError::Conflict(
                "fixture recovered volume identity differs".into(),
            ));
        }
        Ok(())
    }
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        // This fixture owns one builder and no production Deployments.
        Ok(vec![])
    }
    async fn get_deployment_status(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<container_runtime_api::DeploymentStatus>> {
        Ok(None)
    }
}

async fn fixture() -> (Arc<AppState>, Arc<ConfirmationRuntime>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("state.db"),
        )
        .await
        .unwrap(),
    );
    fixture_on_store(store, directory, "receipt-app").await
}

async fn fixture_on_store(
    store: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    directory: tempfile::TempDir,
    app_id: &str,
) -> (Arc<AppState>, Arc<ConfirmationRuntime>, tempfile::TempDir) {
    let app = store.ensure_identity(app_id).await.unwrap();
    let context = UserAppExecutionContext {
        app_id: app_id.into(),
        lifecycle_id: app.lifecycle_id,
        operation_id: "initial".into(),
        executor_id: "worker".into(),
        request_fingerprint: "a".repeat(64),
    };
    let physical = |kind: &str| {
        if app_id == "receipt-app" {
            format!("{kind}-current")
        } else {
            format!("{app_id}-{kind}")
        }
    };
    let name = if app_id == "receipt-app" {
        "builder".to_owned()
    } else {
        format!("builder-{app_id}")
    };
    let current = BuilderControlTarget {
        resource_binding: None,
        context,
        workload: Some(AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: name.clone(),
            uid: physical("sts"),
            resource_version: Some("1".into()),
        }),
        pod: Some(BuilderPodIdentity {
            name: format!("{name}-0"),
            uid: physical("pod"),
            resource_version: "1".into(),
        }),
        restart_image: None,
        restart_runtime_workspace: None,
    };
    let info = ContainerBasicInfo {
        container_id: physical("pod"),
        container_name: name,
        container_ip: "10.42.0.9".into(),
        internal_port: 60000,
        external_port: 0,
        project_id: app_id.into(),
        status: "running".into(),
        created_at: chrono::Utc::now(),
        service_url: "http://10.42.0.9:60000".into(),
        workload_uid: Some(physical("sts")),
    };
    let volumes = vec![AppResourceIdentity {
        kind: AppResourceKind::PersistentVolumeClaim,
        name: "workspace".into(),
        uid: "pvc-retained".into(),
        resource_version: None,
    }];
    let runtime = Arc::new(ConfirmationRuntime {
        receipts: Mutex::new(BTreeMap::new()),
        current,
        info,
        volumes,
        writes: AtomicUsize::new(0),
        recreated: std::sync::atomic::AtomicBool::new(false),
        allow_confirmation: std::sync::atomic::AtomicBool::new(false),
        confirmation_requests: AtomicUsize::new(0),
        probe_ip: Mutex::new(None),
    });
    let state = fence_settler_tests::runtime_state(runtime.clone(), store, None).await;
    (state, runtime, directory)
}

async fn completed(
    state: &AppState,
    runtime: &ConfirmationRuntime,
    id: &str,
    recorded: bool,
    predecessor: bool,
) -> (UserAppOperationRecord, BuilderCreationEvidence) {
    let pending = match state
        .userapp_store
        .admit(&UserAppAdmission {
            app_id: runtime.info.project_id.clone(),
            lifecycle_id: None,
            operation_id: id.into(),
            request_id: None,
            request_fingerprint: "a".repeat(64),
            kind: UserAppOperationKind::EnsureBuilder,
            runtime_policy_on_success: None,
            command: None,
            metadata: None,
        })
        .await
        .unwrap()
    {
        UserAppAdmissionOutcome::Accepted(record) => record,
        other => panic!("unexpected admission {other:?}"),
    };
    let running = progress(
        &state.userapp_store,
        &pending,
        "worker",
        UserAppOperationState::Running,
        "claimed",
        serde_json::Value::Null,
        None,
    )
    .await
    .unwrap();
    let mut target = runtime.current.clone();
    target.context.operation_id = id.into();
    target.context.lifecycle_id = running.lifecycle_id.clone();
    let source = predecessor.then(|| {
        let mut target = target.clone();
        target.workload.as_mut().unwrap().uid = "sts-old".into();
        target.pod.as_mut().unwrap().uid = "pod-old".into();
        BuilderCreationPredecessor {
            target,
            volumes: runtime.volumes.clone(),
        }
    });
    let evidence = BuilderCreationEvidence {
        creation_lease_released: true,
        target,
        container: runtime.info.clone(),
        registration_predecessor: source,
    };
    let mut checkpoint = serde_json::to_value(&evidence.container).unwrap();
    if recorded {
        checkpoint.as_object_mut().unwrap().insert(
            "builder_creation_evidence".into(),
            serde_json::to_value(&evidence).unwrap(),
        );
    }
    let operation = progress(
        &state.userapp_store,
        &running,
        "worker",
        UserAppOperationState::Succeeded,
        "builder_ready_confirmed",
        checkpoint,
        None,
    )
    .await
    .unwrap();
    runtime
        .receipts
        .lock()
        .unwrap()
        .insert(id.into(), evidence.clone());
    (operation, evidence)
}

fn seed_stale(state: &AppState, runtime: &ConfirmationRuntime) {
    let mut old = runtime.info.clone();
    old.container_id = "pod-old".into();
    old.workload_uid = Some("sts-old".into());
    crate::userapp_builder::register_builder(state, &runtime.info.project_id, &old).unwrap();
}

#[tokio::test]
async fn null_binding_confirmation_repairs_stale_registry_without_runtime_writes() {
    let (state, runtime, _directory) = fixture().await;
    let (operation, _) = completed(&state, &runtime, "confirmation", false, false).await;
    seed_stale(&state, &runtime);
    register_completion(&state, &operation, "receipt-app", &runtime.info)
        .await
        .unwrap();
    assert_eq!(
        crate::userapp_builder::registered_builder(&state, "receipt-app")
            .unwrap()
            .workload_uid,
        Some("sts-current".into())
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn compatible_completion_archives_and_overwritten_service_confirmation_are_accepted() {
    let (state, runtime, _directory) = fixture().await;
    let (old, _) = completed(&state, &runtime, "op-a", false, false).await;
    let (later, _) = completed(&state, &runtime, "op-z", false, false).await;
    seed_stale(&state, &runtime);
    register_candidates(
        &state,
        "receipt-app",
        &runtime.info,
        vec![later.clone(), old.clone()],
    )
    .await
    .unwrap();
    runtime.receipts.lock().unwrap().remove("op-a");
    seed_stale(&state, &runtime);
    register_candidates(&state, "receipt-app", &runtime.info, vec![old, later])
        .await
        .unwrap();
    assert_eq!(
        crate::userapp_builder::registered_builder(&state, "receipt-app")
            .unwrap()
            .container_id,
        "pod-current"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn discovery_preserves_authoritative_workload_uid() {
    let (state, runtime, _directory) = fixture().await;
    let found = crate::userapp_builder::registered_or_discovered_builder(&state, "receipt-app")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(found.workload_uid, Some("sts-current".into()));
    assert_eq!(
        crate::userapp_builder::registered_builder(&state, "receipt-app")
            .unwrap()
            .workload_uid,
        found.workload_uid
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn cleaned_archive_cannot_discard_recorded_predecessor_or_pvc_constraints() {
    let (state, runtime, _directory) = fixture().await;
    let (mut old, mut evidence) = completed(&state, &runtime, "op-a", true, true).await;
    let (later, _) = completed(&state, &runtime, "op-z", false, false).await;
    runtime.receipts.lock().unwrap().remove("op-a");
    evidence.registration_predecessor.as_mut().unwrap().volumes[0].uid = "different-pvc".into();
    old.checkpoint["builder_creation_evidence"] = serde_json::to_value(evidence).unwrap();
    seed_stale(&state, &runtime);
    let error = register_candidates(&state, "receipt-app", &runtime.info, vec![later, old])
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("volume identity changed"),
        "{error:#}"
    );
    assert_eq!(
        crate::userapp_builder::registered_builder(&state, "receipt-app")
            .unwrap()
            .container_id,
        "pod-old"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn conflicting_recorded_predecessors_and_private_context_fail_closed() {
    let (state, runtime, _directory) = fixture().await;
    let (first, _) = completed(&state, &runtime, "op-a", true, true).await;
    let (mut second, mut second_evidence) = completed(&state, &runtime, "op-z", true, true).await;
    runtime.receipts.lock().unwrap().remove("op-z");
    second_evidence
        .registration_predecessor
        .as_mut()
        .unwrap()
        .target
        .workload
        .as_mut()
        .unwrap()
        .uid = "other-old-sts".into();
    second.checkpoint["builder_creation_evidence"] = serde_json::to_value(second_evidence).unwrap();
    seed_stale(&state, &runtime);
    assert!(
        register_candidates(
            &state,
            "receipt-app",
            &runtime.info,
            vec![first.clone(), second]
        )
        .await
        .is_err()
    );
    runtime
        .receipts
        .lock()
        .unwrap()
        .get_mut("op-a")
        .unwrap()
        .target
        .context
        .executor_id = "foreign-worker".into();
    assert!(
        register_completion(&state, &first, "receipt-app", &runtime.info)
            .await
            .is_err()
    );
    assert_eq!(
        crate::userapp_builder::registered_builder(&state, "receipt-app")
            .unwrap()
            .container_id,
        "pod-old"
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}

#[cfg(all(feature = "rcoder-pg", feature = "deploy-host"))]
#[tokio::test]
#[ignore = "requires an isolated RCODER_PG_TEST_DSN; executed explicitly by integration validation"]
async fn pg_same_statefulset_new_pod_without_completion_returns_to_admitted_confirmation() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("isolated PostgreSQL DSN is required");
    let config = rcoder_storage::config::PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(&config)
            .await
            .unwrap(),
    );
    let app_id = format!("reg-{}", &uuid::Uuid::new_v4().simple().to_string()[..16]);
    assert!(app_id.len() <= shared_types::USERAPP_APP_ID_MAX_LEN);
    let (mut state, runtime, _directory) =
        fixture_on_store(store, tempfile::tempdir().unwrap(), &app_id).await;
    let (projects, _cleanup) = rcoder_storage::pg::PgStore::connect(
        &config,
        "registration-test".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    Arc::get_mut(&mut state).unwrap().projects = Arc::new(
        crate::storage::ProjectStoreBackend::Postgres(Arc::new(projects)),
    );
    let original_operation = format!("{app_id}-old");
    let (old, _) = completed(&state, &runtime, &original_operation, false, false).await;
    // First publish only the original succeeded result and its receipt.
    register_completion(&state, &old, &app_id, &runtime.info)
        .await
        .unwrap();
    let before = crate::userapp_builder::registered_builder(&state, &app_id).unwrap();
    // Native StatefulSet self-healing produces a new Pod, without an ensure
    // operation or a corresponding new Service / archive confirmation.
    runtime.recreated.store(true, Ordering::SeqCst);
    let candidate = crate::userapp_builder::registered_or_discovered_builder(&state, &app_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(candidate.container_id, before.container_id);
    assert!(
        crate::userapp_builder::cross_verify_registration(&state, &app_id, &app_id, &candidate)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        crate::userapp_builder::registered_builder(&state, &app_id)
            .unwrap()
            .container_id,
        before.container_id
    );
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    // Run the actual admitted creation worker, readiness HTTP probe, terminal
    // wait and PG registration, rather than stopping at admission acceptance.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    shared_types::published::register(&runtime.info.container_name, HashMap::from([(60000, port)]));
    struct MappingGuard(String);
    impl Drop for MappingGuard {
        fn drop(&mut self) {
            shared_types::published::unregister(&self.0);
        }
    }
    let _mapping = MappingGuard(runtime.info.container_name.clone());
    let probes = Arc::new(AtomicUsize::new(0));
    let counted = probes.clone();
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            axum::Router::new().route(
                "/api/version",
                axum::routing::get(move || {
                    let counted = counted.clone();
                    async move {
                        counted.fetch_add(1, Ordering::SeqCst);
                        axum::http::StatusCode::OK
                    }
                }),
            ),
        )
        .await
        .unwrap();
    });
    *runtime.probe_ip.lock().unwrap() = Some("10.42.0.10".into());
    runtime.allow_confirmation.store(true, Ordering::SeqCst);
    let confirmed = crate::userapp_builder::ensure_userapp_builder(&state, &app_id)
        .await
        .unwrap();
    assert_eq!(confirmed.container_id, runtime.observed_info().container_id);
    assert_eq!(confirmed.workload_uid, before.workload_uid);
    assert!(probes.load(Ordering::SeqCst) > 0);
    assert_eq!(runtime.confirmation_requests.load(Ordering::SeqCst), 1);
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
    let current = crate::userapp_builder::registered_or_discovered_builder(&state, &app_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(current.container_id, confirmed.container_id);
    assert!(
        crate::userapp_builder::cross_verify_registration(&state, &app_id, &app_id, &current)
            .await
            .unwrap()
            .is_some()
    );
    let lifecycle = state
        .userapp_store
        .get_application(&app_id)
        .await
        .unwrap()
        .unwrap();
    assert!(lifecycle.active_operations.dev.is_none());
    let binding = state
        .userapp_store
        .get_resource_binding(
            &ServiceType::UserappBuilder,
            confirmed.workload_uid.as_deref().unwrap(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(binding.adopted_by_operation, original_operation);
    let (peer, _cleanup) = rcoder_storage::pg::PgStore::connect(
        &config,
        "registration-peer".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    let peer_container = shared_types::ProjectStore::get(&peer, &app_id)
        .unwrap()
        .container_info()
        .unwrap();
    assert_eq!(peer_container.container_id, confirmed.container_id);
    assert_eq!(peer_container.workload_uid, confirmed.workload_uid);
    assert_eq!(peer_container.container_ip, confirmed.container_ip);
    server.abort();
    drop(server.await);
}

#[tokio::test]
async fn checkpoint_and_private_receipt_identity_conflicts_never_publish_an_address() {
    let (state, runtime, _directory) = fixture().await;
    let (operation, _) = completed(&state, &runtime, "confirmation", false, false).await;
    seed_stale(&state, &runtime);
    for field in [
        "container_id",
        "workload_uid",
        "container_name",
        "project_id",
        "created_at",
    ] {
        let mut wrong = operation.clone();
        wrong.checkpoint[field] = if field == "created_at" {
            serde_json::to_value(runtime.info.created_at + chrono::Duration::seconds(1)).unwrap()
        } else {
            serde_json::json!("foreign")
        };
        assert!(
            register_candidates(&state, "receipt-app", &runtime.info, vec![wrong])
                .await
                .is_err(),
            "{field}"
        );
        assert_eq!(
            crate::userapp_builder::registered_builder(&state, "receipt-app")
                .unwrap()
                .container_id,
            "pod-old"
        );
    }
    let original_receipt = runtime
        .receipts
        .lock()
        .unwrap()
        .get("confirmation")
        .unwrap()
        .clone();
    for field in ["pod", "workload", "application", "name", "lifecycle"] {
        let mut receipt = original_receipt.clone();
        match field {
            "pod" => receipt.target.pod.as_mut().unwrap().uid = "foreign-pod".into(),
            "workload" => receipt.target.workload.as_mut().unwrap().uid = "foreign-sts".into(),
            "application" => receipt.container.project_id = "foreign-app".into(),
            "name" => receipt.target.workload.as_mut().unwrap().name = "foreign-name".into(),
            _ => receipt.target.context.lifecycle_id = "foreign-life".into(),
        }
        runtime
            .receipts
            .lock()
            .unwrap()
            .insert("confirmation".into(), receipt);
        assert!(
            register_candidates(
                &state,
                "receipt-app",
                &runtime.info,
                vec![operation.clone()]
            )
            .await
            .is_err(),
            "{field}"
        );
        assert_eq!(
            crate::userapp_builder::registered_builder(&state, "receipt-app")
                .unwrap()
                .container_id,
            "pod-old"
        );
    }
    assert_eq!(runtime.writes.load(Ordering::SeqCst), 0);
}
