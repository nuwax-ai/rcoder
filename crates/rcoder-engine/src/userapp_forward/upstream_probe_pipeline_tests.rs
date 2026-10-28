//! Real Turso + the production ensure/probe/forward pipeline, with a local
//! authenticated HTTP file peer and a read-only runtime observation fixture.
//! Register this file as a child of `upstream`, so no visibility is widened.
//! Docker uses its real port 60000; deploy-host uses a physical Published entry
//! for the actual ephemeral listener. Pure Kubernetes DNS needs its own cluster.
#![cfg(all(
    feature = "userapp-turso",
    any(feature = "deploy-host", not(feature = "kubernetes"))
))]

use super::*;
use axum::{
    Json, Router,
    extract::{OriginalUri, State},
    http::{HeaderMap, StatusCode},
    routing::get,
};
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderControlTarget, ContainerBasicInfo,
    FileServerRequestCredentials, ProjectAndContainerInfo, ServiceType, UserAppExecutionContext,
    UserAppLifecycleStore,
};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

const FILES_PATH: &str = "/api/v1/userapp/get-file-list";

#[derive(Clone)]
struct CurrentPeer {
    info: ContainerBasicInfo,
    token: String,
}

#[derive(Debug, PartialEq, Eq)]
struct ObservedHttp {
    path: String,
    token: Option<String>,
}

struct PipelineRuntime {
    current: Arc<Mutex<CurrentPeer>>,
    credential_reads: AtomicUsize,
    observed_credentials: Mutex<Vec<BuilderControlTarget>>,
    inspections: AtomicUsize,
    creates: AtomicUsize,
}

impl PipelineRuntime {
    fn info(&self) -> ContainerBasicInfo {
        self.current.lock().unwrap().info.clone()
    }

    fn target(&self, context: &UserAppExecutionContext) -> BuilderControlTarget {
        let info = self.info();
        assert_eq!(context.app_id, info.project_id);
        BuilderControlTarget {
            context: context.clone(),
            workload: Some(AppResourceIdentity {
                kind: AppResourceKind::Container,
                name: info.container_name,
                uid: info.container_id,
                resource_version: None,
            }),
            pod: None,
            resource_binding: None,
            restart_image: None,
            restart_runtime_workspace: None,
            builder_startup_mode: Default::default(),
        }
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for PipelineRuntime {
    fn agent_workload_namespace(&self) -> String {
        "test-ns".into()
    }

    fn agent_workload_name(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<String> {
        let info = self.info();
        assert_eq!(identifier, info.project_id);
        assert_eq!(*service_type, ServiceType::UserappBuilder);
        Ok(info.container_name)
    }

    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.creates.fetch_add(1, Ordering::SeqCst);
        panic!("registered probe/forward access must never create compute")
    }

    async fn get_container_info(
        &self,
        identifier: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        let info = self.info();
        assert_eq!(identifier, info.project_id);
        Ok(Some(info))
    }

    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        let info = self.info();
        assert_eq!(identifier, info.project_id);
        assert_eq!(*service_type, ServiceType::UserappBuilder);
        self.inspections.fetch_add(1, Ordering::SeqCst);
        Ok(Some(RuntimeContainerInfo {
            container_id: info.container_id,
            container_name: info.container_name,
            container_ip: info.container_ip,
            status: ContainerRuntimeStatus::Running,
            created_at: info.created_at,
            env_vars: None,
            service_type: Some(ServiceType::UserappBuilder),
            project_id: Some(identifier.into()),
            user_id: None,
            pod_id: None,
            app_id: Some(identifier.into()),
            workload_uid: None,
        }))
    }

    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("probe/forward access must never stop compute")
    }

    async fn is_container_running(&self, identifier: &str) -> ContainerRuntimeResult<bool> {
        assert_eq!(identifier, self.info().project_id);
        Ok(true)
    }

    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }

    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("probe/forward access must never clean compute")
    }

    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }

    async fn inspect_builder_candidate(
        &self,
        context: &UserAppExecutionContext,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        Ok(self.target(context))
    }

    async fn capture_bound_builder_control(
        &self,
        context: &UserAppExecutionContext,
        binding: Option<&shared_types::UserAppResourceBinding>,
    ) -> ContainerRuntimeResult<BuilderControlTarget> {
        assert!(binding.is_none(), "this fixture has no legacy binding");
        Ok(self.target(context))
    }

    async fn get_bound_builder_file_credentials(
        &self,
        target: &BuilderControlTarget,
    ) -> ContainerRuntimeResult<FileServerRequestCredentials> {
        self.credential_reads.fetch_add(1, Ordering::SeqCst);
        self.observed_credentials
            .lock()
            .unwrap()
            .push(target.clone());
        let actual = self.current.lock().unwrap();
        let workload = target.workload.as_ref().unwrap();
        if workload.kind != AppResourceKind::Container
            || workload.uid != actual.info.container_id
            || workload.name != actual.info.container_name
            || target.context.app_id != actual.info.project_id
            || target.pod.is_some()
        {
            return Err(ContainerRuntimeError::Conflict(
                "file credential capture names another physical peer".into(),
            ));
        }
        Ok(FileServerRequestCredentials {
            proxy_token: Some(actual.token.clone()),
        })
    }
}

#[async_trait::async_trait]
impl WorkspaceRuntime for PipelineRuntime {}

#[async_trait::async_trait]
impl UserAppDeploymentRuntime for PipelineRuntime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(vec![])
    }

    async fn discover_application_identity(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<shared_types::UserAppDiscoveredIdentity>> {
        // The actual Turso record supplies the lifecycle. The fixture does not
        // manufacture adoption, root recreation or runtime completion receipts.
        Ok(None)
    }

    async fn get_app_container_spec(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<container_runtime_api::ContainerSpecSnapshot> {
        panic!("dev forwarding must not read production or template credentials")
    }
}

#[derive(Clone)]
struct HttpPeer {
    current: Arc<Mutex<CurrentPeer>>,
    requests: Arc<Mutex<Vec<ObservedHttp>>>,
}

async fn file_peer(
    State(peer): State<HttpPeer>,
    OriginalUri(uri): OriginalUri,
    headers: HeaderMap,
) -> (StatusCode, Json<serde_json::Value>) {
    let token = headers
        .get("x-proxy-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    peer.requests.lock().unwrap().push(ObservedHttp {
        path: uri.path().into(),
        token: token.clone(),
    });
    let actual = peer.current.lock().unwrap();
    if token.as_deref() != Some(actual.token.as_str()) || headers.contains_key("x-api-key") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({"success": false})),
        );
    }
    (
        StatusCode::OK,
        Json(serde_json::json!({"success": true, "uid": actual.info.container_id})),
    )
}

struct PeerServer(tokio::task::JoinHandle<()>);

impl Drop for PeerServer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(feature = "deploy-host")]
struct PublishedPeerRegistration {
    alias: String,
    physical_uid: String,
    host_port: u16,
}

#[cfg(feature = "deploy-host")]
impl PublishedPeerRegistration {
    fn new(info: &ContainerBasicInfo, host_port: u16) -> Self {
        let mut registration = Self {
            alias: info.container_name.clone(),
            physical_uid: info.container_id.clone(),
            host_port,
        };
        registration.replace(info);
        registration
    }

    fn replace(&mut self, info: &ContainerBasicInfo) {
        assert_eq!(self.alias, info.container_name);
        let observation = shared_types::published::begin_physical_observation(&info.container_id);
        assert!(observation.register(
            &[self.alias.as_str()],
            info.created_at,
            shared_types::published::Reach::Published {
                ports: std::collections::HashMap::from([(
                    shared_types::AGENT_FILE_SERVER_PORT,
                    self.host_port,
                )]),
            },
        ));
        self.physical_uid = info.container_id.clone();
    }
}

#[cfg(feature = "deploy-host")]
impl Drop for PublishedPeerRegistration {
    fn drop(&mut self) {
        shared_types::published::unregister_if_physical(&self.alias, &self.physical_uid);
    }
}

fn register_current(state: &AppState, info: &ContainerBasicInfo) {
    let mut project = ProjectAndContainerInfo::new(info.project_id.clone());
    project.set_service_type(Some(ServiceType::UserappBuilder));
    project.set_container(Some(info.clone()));
    state
        .insert_project(info.project_id.clone(), Arc::new(project))
        .unwrap();
}

async fn forward_files(state: &AppState, app: &str, expected_uid: &str) {
    let request = Request::builder()
        .uri(format!("{FILES_PATH}?workspace=original"))
        .header("x-app-id", app)
        .header("x-api-key", "control-plane-key-must-not-reach-file-peer")
        .header("x-proxy-token", "untrusted-inbound-token")
        .body(Body::empty())
        .unwrap();
    let response = forward_to_dev(state, app, request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 16 * 1024)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["success"], true);
    assert_eq!(body["uid"], expected_uid);
}

/// One sequential test owns the real locator port across all three phases.
/// It exercises production ensure/cross-verify, the probe and the forwarder;
/// the runtime is a controlled observation fixture, not a Docker claim.
#[tokio::test]
async fn real_turso_probe_forward_pipeline_reads_cold_twice_hot_once_and_current_uid_token() {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &directory.path().join("probe-pipeline.db"),
        )
        .await
        .unwrap(),
    );
    let app = format!(
        "probepipe{}",
        &uuid::Uuid::new_v4().simple().to_string()[..10]
    );
    let lifecycle = store.ensure_identity(&app).await.unwrap();
    // Full physical IDs are unique as well as the alias: a parallel fixture
    // cannot share this case's published-observation invalidation generation.
    let old_uid = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let new_uid = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let current = Arc::new(Mutex::new(CurrentPeer {
        info: ContainerBasicInfo {
            container_id: old_uid.clone(),
            container_name: format!("rcoder-app-builder-{app}"),
            container_ip: "127.0.0.1".into(),
            internal_port: shared_types::AGENT_FILE_SERVER_PORT,
            external_port: 0,
            project_id: app.clone(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: format!("http://127.0.0.1:{}", shared_types::AGENT_FILE_SERVER_PORT),
            workload_uid: None,
        },
        token: "actual-cold-file-token".into(),
    }));
    let runtime = Arc::new(PipelineRuntime {
        current: current.clone(),
        credential_reads: AtomicUsize::new(0),
        observed_credentials: Mutex::new(vec![]),
        inspections: AtomicUsize::new(0),
        creates: AtomicUsize::new(0),
    });
    let mut state =
        crate::userapp_builder::builder_runtime_test_state(runtime.clone(), store, None).await;
    {
        let config = &mut Arc::get_mut(&mut state).unwrap().config;
        config.file_server_proxy = Some(file_server_proxy::FileServerProxyConfig {
            auth_token: Some("rotated-template-token-must-not-be-used".into()),
            ..Default::default()
        });
        config.userapp_storage.ensure_timeout_seconds = 10;
        config.userapp_storage.interactive_ensure_wait_seconds = 10;
    }
    register_current(&state, &runtime.info());
    let requests = Arc::new(Mutex::new(vec![]));
    let peer = HttpPeer {
        current: current.clone(),
        requests: requests.clone(),
    };
    let router = Router::new()
        .route("/api/version", get(file_peer))
        .route(FILES_PATH, get(file_peer))
        .with_state(peer);
    let listener = tokio::net::TcpListener::bind((
        std::net::Ipv4Addr::LOCALHOST,
        if shared_types::is_deploy_host() {
            0
        } else {
            shared_types::AGENT_FILE_SERVER_PORT
        },
    ))
    .await
    .expect("the actual file-peer locator requires its local listener; Docker uses port 60000");
    let listening_address = listener.local_addr().unwrap();
    #[cfg(feature = "deploy-host")]
    let mut published_route =
        PublishedPeerRegistration::new(&runtime.info(), listening_address.port());
    assert_eq!(
        dev_file_server_addr(&state, &runtime.info()).unwrap(),
        format!("http://{listening_address}"),
        "production locator must resolve the actual local file peer",
    );
    let server = PeerServer(tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    }));

    // Cold probe and actual forwarding each obtain credentials for their peer.
    forward_files(&state, &app, &old_uid).await;
    assert_eq!(runtime.credential_reads.load(Ordering::SeqCst), 2);
    assert_eq!(
        *requests.lock().unwrap(),
        vec![
            ObservedHttp {
                path: "/api/version".into(),
                token: Some("actual-cold-file-token".into())
            },
            ObservedHttp {
                path: FILES_PATH.into(),
                token: Some("actual-cold-file-token".into())
            },
        ]
    );

    // Call the actual inner resolver as well as the complete forwarding path.
    // Save its count now; check the hot forward delta first so the old
    // credentials-before-cache block fails with its concrete 2-versus-1 read.
    let before_resolve = runtime.credential_reads.load(Ordering::SeqCst);
    let address = resolve_dev_addr_inner(
        &state,
        &app,
        tokio::time::Instant::now() + std::time::Duration::from_secs(10),
        crate::userapp_builder::BuilderAccessIntent::Management,
    )
    .await
    .unwrap();
    assert_eq!(
        address,
        dev_file_server_addr(&state, &runtime.info()).unwrap()
    );
    let after_resolve = runtime.credential_reads.load(Ordering::SeqCst);
    current.lock().unwrap().token = "actual-hot-current-token".into();
    forward_files(&state, &app, &old_uid).await;
    assert_eq!(
        runtime.credential_reads.load(Ordering::SeqCst) - after_resolve,
        1,
        "hot forwarding still reads current runtime credentials once, never zero or twice"
    );
    assert_eq!(
        after_resolve - before_resolve,
        0,
        "a hot positive probe does not read credentials just to skip that probe"
    );
    assert_eq!(requests.lock().unwrap().len(), 3);
    assert_eq!(
        requests.lock().unwrap()[2],
        ObservedHttp {
            path: FILES_PATH.into(),
            token: Some("actual-hot-current-token".into()),
        }
    );

    // Keep the existing application-level positive cache warm deliberately.
    // The actual registry/runtime now name another CID. No invalidation or
    // additional probe is required by the current TTL policy, but credentials
    // for forwarding must be captured from the new physical peer.
    let before_inspections = runtime.inspections.load(Ordering::SeqCst);
    let before_replacement_reads = runtime.credential_reads.load(Ordering::SeqCst);
    let before_targets = runtime.observed_credentials.lock().unwrap().len();
    {
        let mut actual = current.lock().unwrap();
        actual.info.container_id = new_uid.clone();
        actual.info.created_at = chrono::Utc::now();
        actual.token = "actual-replacement-current-token".into();
    }
    #[cfg(feature = "deploy-host")]
    published_route.replace(&runtime.info());
    register_current(&state, &runtime.info());
    forward_files(&state, &app, &new_uid).await;
    assert_eq!(
        runtime.credential_reads.load(Ordering::SeqCst) - before_replacement_reads,
        1
    );
    assert!(
        runtime.inspections.load(Ordering::SeqCst) > before_inspections,
        "warm probe caching must not bypass ensure's actual physical cross-verification"
    );
    {
        let targets = runtime.observed_credentials.lock().unwrap();
        assert_eq!(targets.len() - before_targets, 1);
        let target = &targets[before_targets];
        assert_eq!(target.context.lifecycle_id, lifecycle.lifecycle_id);
        assert_eq!(target.workload.as_ref().unwrap().uid, new_uid);
        assert!(target.pod.is_none());
    }
    {
        let observed = requests.lock().unwrap();
        assert_eq!(observed.len(), 4);
        assert_eq!(
            observed
                .iter()
                .filter(|row| row.path == "/api/version")
                .count(),
            1,
            "same-app probe TTL is preserved across the observed instance change"
        );
        assert_eq!(
            observed[3],
            ObservedHttp {
                path: FILES_PATH.into(),
                token: Some("actual-replacement-current-token".into()),
            }
        );
    }
    assert_eq!(runtime.creates.load(Ordering::SeqCst), 0);
    state.userapp_store_control.shutdown().await.unwrap();
    drop(server);
    invalidate_probe_cache(&app);
    #[cfg(feature = "deploy-host")]
    {
        let alias = runtime.info().container_name;
        drop(published_route);
        assert!(
            shared_types::published::resolve_published_addr(
                &alias,
                shared_types::AGENT_FILE_SERVER_PORT,
            )
            .is_err(),
            "only the case's current physical registration is removed"
        );
    }
}
