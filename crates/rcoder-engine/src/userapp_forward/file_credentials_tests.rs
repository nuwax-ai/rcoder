//! Real lifecycle store plus a narrow runtime peer observation. This verifies
//! credential provenance and errors, not Docker/Kubernetes exec itself.
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, BuilderPodIdentity,
    ContainerBasicInfo, ServiceType, UserAppExecutionContext, UserAppLifecycleStore,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

struct PeerRuntime {
    actual: Mutex<Option<ContainerRuntimeResult<FileServerRequestCredentials>>>,
    reads: AtomicUsize,
    pending: AtomicBool,
    management_not_running_rounds: AtomicUsize,
    observed: Mutex<Vec<BuilderControlTarget>>,
}

impl PeerRuntime {
    fn target(context: &UserAppExecutionContext) -> BuilderControlTarget {
        BuilderControlTarget {
            builder_startup_mode: Default::default(),
            context: context.clone(),
            resource_binding: None,
            workload: Some(AppResourceIdentity {
                kind: AppResourceKind::StatefulSet,
                name: "builder-fixtureapp".into(),
                uid: "existing-sts".into(),
                resource_version: Some("7".into()),
            }),
            pod: Some(BuilderPodIdentity {
                name: "builder-fixtureapp-0".into(),
                uid: "existing-pod".into(),
                resource_version: "11".into(),
            }),
            restart_image: None,
            restart_runtime_workspace: None,
        }
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for PeerRuntime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("credential reads cannot create compute")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        panic!("credential reads cannot use unbound discovery")
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        panic!("credential reads use the bound target")
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("credential reads cannot stop compute")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        panic!("business or port readiness cannot gate credentials")
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("credential reads cannot clean compute")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn inspect_builder_candidate(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        Ok(Self::target(context))
    }
    async fn capture_bound_builder_control(
        &self,
        context: &UserAppExecutionContext,
        _: Option<&shared_types::UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        Ok(Self::target(context))
    }
    async fn get_bound_builder_file_credentials(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<FileServerRequestCredentials> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        self.observed.lock().unwrap().push(target.clone());
        if self
            .management_not_running_rounds
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |rounds| {
                rounds.checked_sub(1)
            })
            .is_ok()
        {
            return Err(ContainerRuntimeError::ManagementNotRunning);
        }
        if self.pending.load(Ordering::SeqCst) {
            return std::future::pending().await;
        }
        self.actual
            .lock()
            .unwrap()
            .take()
            .expect("one actual peer observation")
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for PeerRuntime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for PeerRuntime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        // This fixture owns one builder and no production deployments.
        Ok(vec![])
    }
    async fn get_app_container_spec(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<container_runtime_api::ContainerSpecSnapshot> {
        panic!("builder credentials cannot read a production or latest template spec")
    }
}

async fn fixture(
    result: ContainerRuntimeResult<FileServerRequestCredentials>,
    pending: bool,
) -> (Arc<AppState>, Arc<PeerRuntime>, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("credentials.db"),
        )
        .await
        .unwrap(),
    );
    store.ensure_identity("fixtureapp").await.unwrap();
    let runtime = Arc::new(PeerRuntime {
        actual: Mutex::new(Some(result)),
        reads: AtomicUsize::new(0),
        pending: AtomicBool::new(pending),
        management_not_running_rounds: AtomicUsize::new(0),
        observed: Mutex::new(vec![]),
    });
    let mut state =
        crate::userapp_builder::builder_runtime_test_state(runtime.clone(), store, None).await;
    Arc::get_mut(&mut state).unwrap().config.file_server_proxy =
        Some(file_server_proxy::FileServerProxyConfig {
            auth_token: Some("rotated-platform-token".into()),
            ..Default::default()
        });
    (state, runtime, directory)
}

#[tokio::test]
async fn builder_management_startup_window_retries_only_original_target_within_deadline() {
    let (state, runtime, _directory) = fixture(
        Ok(FileServerRequestCredentials {
            proxy_token: Some("actual-running-token".into()),
        }),
        false,
    )
    .await;
    runtime
        .management_not_running_rounds
        .store(2, Ordering::SeqCst);
    let actual = credentials(
        &state,
        UserappStage::Dev,
        "fixtureapp",
        tokio::time::Instant::now() + std::time::Duration::from_secs(2),
    )
    .await
    .unwrap();
    assert_eq!(actual.proxy_token.as_deref(), Some("actual-running-token"));
    assert_eq!(runtime.reads.load(Ordering::SeqCst), 3);
    {
        let observed = runtime.observed.lock().unwrap();
        assert_eq!(observed.len(), 3);
        assert!(observed.iter().all(|target| target == &observed[0]));
    }
    state.userapp_store_control.shutdown().await.unwrap();
    let (state, runtime, _directory) = fixture(
        Ok(FileServerRequestCredentials {
            proxy_token: Some("must-not-read-late".into()),
        }),
        false,
    )
    .await;
    runtime
        .management_not_running_rounds
        .store(usize::MAX, Ordering::SeqCst);
    let lifecycle = state
        .userapp_store
        .get_application("fixtureapp")
        .await
        .unwrap()
        .unwrap();
    let target = PeerRuntime::target(&UserAppExecutionContext {
        app_id: "fixtureapp".into(),
        lifecycle_id: lifecycle.lifecycle_id,
        operation_id: "file-credentials-observation".into(),
        executor_id: "reader".into(),
        request_fingerprint: "0".repeat(64),
    });
    let failure = development_credentials_for_target(
        runtime.as_ref(),
        &target,
        tokio::time::Instant::now() + std::time::Duration::from_millis(150),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code.as_ref(), shared_types::ERR_RUNTIME_TIMEOUT);
    assert!(
        (1..=2).contains(&runtime.reads.load(Ordering::SeqCst)),
        "the original deadline prevents a late credential read"
    );
    state.userapp_store_control.shutdown().await.unwrap();
}

#[tokio::test]
async fn builder_credentials_use_captured_effective_peer_after_template_rotation() {
    for token in [Some("old-running-secret".to_owned()), None] {
        let (state, runtime, _directory) = fixture(
            Ok(FileServerRequestCredentials {
                proxy_token: token.clone(),
            }),
            false,
        )
        .await;
        let actual = credentials(
            &state,
            UserappStage::Dev,
            "fixtureapp",
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
        assert_eq!(actual.proxy_token, token);
        assert_eq!(runtime.reads.load(Ordering::SeqCst), 1);
        let lifecycle_id = {
            let observed = runtime.observed.lock().unwrap();
            assert_eq!(observed[0].pod.as_ref().unwrap().uid, "existing-pod");
            assert_eq!(observed[0].workload.as_ref().unwrap().uid, "existing-sts");
            assert_eq!(observed[0].context.app_id, "fixtureapp");
            observed[0].context.lifecycle_id.clone()
        };
        assert_eq!(
            lifecycle_id,
            state
                .userapp_store
                .get_application("fixtureapp")
                .await
                .unwrap()
                .unwrap()
                .lifecycle_id
        );
        state.userapp_store_control.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn builder_credential_error_or_deadline_never_falls_back_to_rotated_template() {
    for (error, code) in [
        (
            ContainerRuntimeError::ContainerNotFound("captured peer gone".into()),
            shared_types::ERR_CONTAINER_NOT_FOUND,
        ),
        (
            ContainerRuntimeError::Conflict("captured Pod changed".into()),
            shared_types::ERR_CONFLICT,
        ),
        (
            ContainerRuntimeError::ConnectionError("peer query failed".into()),
            shared_types::ERR_RUNTIME_UNAVAILABLE,
        ),
    ] {
        let (state, runtime, _directory) = fixture(Err(error), false).await;
        let failure = credentials(
            &state,
            UserappStage::Dev,
            "fixtureapp",
            tokio::time::Instant::now() + std::time::Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.code.as_ref(), code);
        assert_eq!(runtime.reads.load(Ordering::SeqCst), 1);
        state.userapp_store_control.shutdown().await.unwrap();
    }
    let (state, runtime, _directory) =
        fixture(Ok(FileServerRequestCredentials::default()), true).await;
    let failure = credentials(
        &state,
        UserappStage::Dev,
        "fixtureapp",
        tokio::time::Instant::now() + std::time::Duration::from_millis(20),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.code.as_ref(), shared_types::ERR_RUNTIME_TIMEOUT);
    assert_eq!(runtime.reads.load(Ordering::SeqCst), 1);
    state.userapp_store_control.shutdown().await.unwrap();
}
