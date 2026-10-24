//! Exercise complete cleanup sweeps through real memory storage and gRPC.
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, AgentResourceIdentity, AgentResourceKind, AgentStopOutcome,
    AgentStopSnapshot, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use shared_types::grpc::{
    self,
    agent_service_server::{AgentService, AgentServiceServer},
};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ProjectExtendedFields};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tonic::{Request, Response, Status};

mod absence_recovery;
mod builder_pod_recovery;
mod physical_docker;

#[derive(Clone, Default)]
struct Activity {
    active: Arc<AtomicBool>,
    unreadable: Arc<AtomicBool>,
    calls: Arc<AtomicUsize>,
    fail_at: Arc<AtomicUsize>,
    pause: Arc<AtomicBool>,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Notify>,
}

#[tonic::async_trait]
impl AgentService for Activity {
    type SubscribeProgressStream = std::pin::Pin<
        Box<dyn futures_util::Stream<Item = Result<grpc::ProgressEvent, Status>> + Send>,
    >;
    async fn get_container_status(
        &self,
        _: Request<grpc::GetContainerStatusRequest>,
    ) -> Result<Response<grpc::GetContainerStatusResponse>, Status> {
        if self.pause.load(Ordering::SeqCst) {
            let released = self.release.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            self.entered.notify_one();
            released.await;
        }
        let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        let fail_at = self.fail_at.load(Ordering::SeqCst);
        if self.unreadable.load(Ordering::SeqCst) || (fail_at != 0 && call >= fail_at) {
            return Err(Status::unavailable(
                "controlled activity observation failure",
            ));
        }
        Ok(Response::new(grpc::GetContainerStatusResponse {
            is_active: self.active.load(Ordering::SeqCst),
            ..Default::default()
        }))
    }
    async fn subscribe_progress(
        &self,
        _: Request<grpc::ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn chat(
        &self,
        _: Request<grpc::ChatRequest>,
    ) -> Result<Response<grpc::ChatResponse>, Status> {
        Err(Status::unimplemented("no AI in this protocol fixture"))
    }
    async fn cancel_session(
        &self,
        _: Request<grpc::CancelRequest>,
    ) -> Result<Response<grpc::CancelResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn resolve_permission(
        &self,
        _: Request<grpc::ResolvePermissionRequest>,
    ) -> Result<Response<grpc::ResolvePermissionResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn get_status(
        &self,
        _: Request<grpc::GetStatusRequest>,
    ) -> Result<Response<grpc::GetStatusResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn stop_agent(
        &self,
        _: Request<grpc::StopAgentRequest>,
    ) -> Result<Response<grpc::StopAgentResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
    async fn get_vnc_status(
        &self,
        _: Request<grpc::GetVncStatusRequest>,
    ) -> Result<Response<grpc::GetVncStatusResponse>, Status> {
        Err(Status::unimplemented("unused"))
    }
}

struct Runtime {
    stops: AtomicUsize,
    workload_name: std::sync::Mutex<String>,
    fail_stop: AtomicBool,
    builder: std::sync::Mutex<Option<RuntimeContainerInfo>>,
    builder_find_calls: AtomicUsize,
    pause_builder_at: AtomicUsize,
    missing: AtomicBool,
    absence_unreadable: AtomicBool,
    absence_checks: AtomicUsize,
    presence_at_check: AtomicUsize,
    pause_absence: AtomicBool,
    absence_entered: tokio::sync::Notify,
    absence_release: tokio::sync::Notify,
    physical: std::sync::Mutex<String>,
    docker: std::sync::Mutex<Option<physical_docker::Target>>,
}

impl Default for Runtime {
    fn default() -> Self {
        Self {
            stops: AtomicUsize::new(0),
            workload_name: std::sync::Mutex::new("shared-idle-fixture".into()),
            fail_stop: AtomicBool::new(false),
            builder: std::sync::Mutex::new(None),
            builder_find_calls: AtomicUsize::new(0),
            pause_builder_at: AtomicUsize::new(0),
            missing: AtomicBool::new(false),
            absence_unreadable: AtomicBool::new(false),
            absence_checks: AtomicUsize::new(0),
            presence_at_check: AtomicUsize::new(0),
            pause_absence: AtomicBool::new(false),
            absence_entered: tokio::sync::Notify::new(),
            absence_release: tokio::sync::Notify::new(),
            physical: std::sync::Mutex::new("physical-original".into()),
            docker: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    fn agent_workload_namespace(&self) -> String {
        "test-ns".into()
    }
    fn agent_workload_name(&self, _: &str, _: &ServiceType) -> ContainerRuntimeResult<String> {
        Ok(self.docker.lock().unwrap().as_ref().map_or_else(
            || self.workload_name.lock().unwrap().clone(),
            |target| target.name.clone(),
        ))
    }
    async fn capture_agent_stop(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<AgentStopSnapshot> {
        Ok(AgentStopSnapshot {
            namespace: Some("test-ns".into()),
            identifier: identifier.into(),
            service_type: *service_type,
            app_id_hint: None,
            controller: None,
            pods: vec![],
            container: Some(AgentResourceIdentity {
                kind: AgentResourceKind::Container,
                name: self.agent_workload_name(identifier, service_type)?,
                uid: self.physical.lock().unwrap().clone(),
                resource_version: None,
                created_at: None,
                owner_uid: None,
            }),
            services: vec![],
            replicas: None,
            compute_data_protection: None,
            endpoint: None,
        })
    }
    async fn agent_compute_absent(
        &self,
        _: &str,
        _: &ServiceType,
        _: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<bool> {
        if self.absence_unreadable.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::ConnectionError(
                "controlled API observation error".into(),
            ));
        }
        if self.pause_absence.load(Ordering::SeqCst) {
            let released = self.absence_release.notified();
            tokio::pin!(released);
            released.as_mut().enable();
            self.absence_entered.notify_one();
            released.await;
        }
        let count = self.absence_checks.fetch_add(1, Ordering::SeqCst) + 1;
        let present_at = self.presence_at_check.load(Ordering::SeqCst);
        Ok(self.missing.load(Ordering::SeqCst) && (present_at == 0 || count < present_at))
    }
    async fn stop_captured_agent(
        &self,
        snapshot: &AgentStopSnapshot,
    ) -> ContainerRuntimeResult<AgentStopOutcome> {
        assert_eq!(
            snapshot.container.as_ref().unwrap().uid,
            *self.physical.lock().unwrap()
        );
        self.stops.fetch_add(1, Ordering::SeqCst);
        if self.fail_stop.load(Ordering::SeqCst) {
            return Err(ContainerRuntimeError::Conflict(
                "controlled unconfirmed stop".into(),
            ));
        }
        let docker = self.docker.lock().unwrap().clone();
        if let Some(target) = docker {
            target.stop_and_confirm_gone();
        }
        Ok(AgentStopOutcome::Complete)
    }
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        panic!("cleanup must not create compute")
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
            let call = self.builder_find_calls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.pause_builder_at.load(Ordering::SeqCst) == call {
                let released = self.absence_release.notified();
                tokio::pin!(released);
                released.as_mut().enable();
                self.absence_entered.notify_one();
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
        self.capture_builder_control(context).await
    }
    async fn capture_builder_control(
        &self,
        context: &shared_types::UserAppExecutionContext,
    ) -> ContainerRuntimeResult<shared_types::BuilderControlTarget> {
        let actual = self.builder.lock().unwrap().clone().unwrap();
        Ok(shared_types::BuilderControlTarget {
            context: context.clone(),
            resource_binding: None,
            workload: Some(shared_types::AppResourceIdentity {
                kind: shared_types::AppResourceKind::StatefulSet,
                name: actual.container_name.clone(),
                uid: actual.workload_uid.unwrap(),
                resource_version: Some("1".into()),
            }),
            pod: Some(shared_types::BuilderPodIdentity {
                name: format!("{}-0", actual.container_name),
                uid: actual.container_id,
                resource_version: "2".into(),
            }),
            restart_image: None,
            restart_runtime_workspace: None,
            builder_startup_mode: Default::default(),
        })
    }
    async fn stop_idle_builder_compute(
        &self,
        _: &str,
        captured: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<()> {
        assert_eq!(
            self.builder.lock().unwrap().as_ref().unwrap().container_id,
            captured.container_id
        );
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("cleanup must use captured retirement")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(true)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("cleanup must not delete unrelated resources")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
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
}

struct Fixture {
    cleaner: AgentCleaner,
    runtime: Arc<Runtime>,
    activity: Activity,
    stop: tokio_util::sync::CancellationToken,
    server: tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    async fn new(projects: usize) -> Self {
        Self::new_with_family(projects, ServiceType::ComputerAgentRunner).await
    }

    async fn new_with_family(projects: usize, family: ServiceType) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
                &directory.path().join("state.db"),
            )
            .await
            .unwrap(),
        );
        let activity = Activity::default();
        let listener = tokio::net::TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            shared_types::GRPC_DEFAULT_PORT,
        ))
        .await
        .unwrap();
        let address = listener.local_addr().unwrap();
        let incoming = futures_util::stream::unfold(listener, |listener| async {
            Some((listener.accept().await.map(|(stream, _)| stream), listener))
        });
        let stop = tokio_util::sync::CancellationToken::new();
        let server_stop = stop.clone();
        let service = activity.clone();
        let server = tokio::spawn(async move {
            tonic::transport::Server::builder()
                .add_service(AgentServiceServer::new(service))
                .serve_with_incoming_shutdown(incoming, server_stop.cancelled())
                .await
        });
        let runtime = Arc::new(Runtime::default());
        let app_activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
            300,
        )));
        let app_service = Arc::new(
            app_manager::service::AppService::new(
                Default::default(),
                runtime.clone(),
                app_activity.clone(),
                None,
                store.clone(),
            )
            .await
            .unwrap(),
        );
        let (pod_created_tx, _) = tokio::sync::broadcast::channel(8);
        let state = Arc::new(crate::app_state::AppState {
            userapp_store: store.clone(),
            userapp_store_control: store,
            userapp_op_flight: Arc::new(
                crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
            ),
            userapp_recovery_handle: Arc::new(std::sync::Mutex::new(None)),
            config: Default::default(),
            projects: Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
                crate::storage::ProjectAdapter::new("test-ns".into(), "cluster.local".into()),
            ))),
            pingora_service: None,
            userapp_error_page: None,
            grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
            session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
            api_key_config: Arc::new(arc_swap::ArcSwap::from_pointee(Default::default())),
            pod_creating: Arc::new(dashmap::DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder: "test-web".into(),
            container_prefix_computer: "test-computer".into(),
            runtime: runtime.clone(),
            app_service,
            activity: app_activity,
            cluster_domain: "cluster.local".into(),
            agent_download_manager: Arc::new(
                agent_provisioning::AgentDownloadManager::new(directory.path().join("agents"))
                    .unwrap(),
            ),
        });
        for index in 0..projects {
            let id = if family == ServiceType::UserappBuilder {
                format!("builder-idle-{}", uuid::Uuid::new_v4().simple())
            } else {
                format!("idle-project-{index}")
            };
            let created_at = Utc::now() - chrono::Duration::hours(2);
            let container = ContainerBasicInfo {
                container_id: "physical-original".into(),
                container_name: "shared-idle-fixture".into(),
                container_ip: "127.0.0.1".into(),
                internal_port: address.port(),
                external_port: 0,
                project_id: if family == ServiceType::UserappBuilder {
                    id.clone()
                } else {
                    "idle-owner".into()
                },
                status: "running".into(),
                created_at,
                service_url: format!("http://{address}"),
                workload_uid: (family == ServiceType::UserappBuilder)
                    .then(|| "same-controller".into()),
            };
            let project = ProjectAndContainerInfo::from_parts(
                id.clone(),
                (family != ServiceType::UserappBuilder).then(|| "idle-owner".into()),
                None,
                None,
                Some(container),
                ProjectExtendedFields {
                    service_type: Some(family),
                    created_at: Some(created_at),
                    last_activity: Some(Utc::now() - chrono::Duration::minutes(30)),
                    ..Default::default()
                },
            );
            state
                .projects
                .insert_project_registration_durable(id, Arc::new(project))
                .await
                .unwrap();
        }
        let config = super::super::config::CleanupConfig {
            idle_timeout: Duration::from_secs(60),
            long_idle_timeout: Duration::from_secs(120),
            container_protection_duration: Duration::ZERO,
            log_dir: directory.path().join("logs").to_string_lossy().into_owned(),
            ..Default::default()
        };
        Self {
            cleaner: AgentCleaner::new(config, state, None),
            runtime,
            activity,
            stop,
            server,
            _directory: directory,
        }
    }
    async fn close(self) {
        self.stop.cancel();
        self.server.await.unwrap().unwrap();
        self.cleaner
            .state
            .userapp_store_control
            .shutdown()
            .await
            .unwrap();
    }

    async fn replace_physical(&self, new_id: &str) {
        *self.runtime.physical.lock().unwrap() = new_id.into();
        for (id, current) in self.cleaner.state.projects.iter() {
            let mut project = (*current).clone();
            let mut container = project.container_info().unwrap();
            container.container_id = new_id.into();
            container.container_name = self
                .runtime
                .agent_workload_name("idle-owner", &ServiceType::ComputerAgentRunner)
                .unwrap();
            project.set_container(Some(container));
            self.cleaner
                .state
                .projects
                .insert_project_registration_durable(id, Arc::new(project))
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn shared_projects_require_two_distinct_cleanup_sweeps() {
    let mut fixture = Fixture::new(2).await;
    let first = fixture.cleaner.cleanup_once().await.unwrap();
    assert_eq!(
        fixture.runtime.stops.load(Ordering::SeqCst),
        0,
        "two shared projects must not be treated as two sweeps"
    );
    assert_eq!(first.containers_destroyed, 0);
    assert_eq!(fixture.cleaner.state.projects.iter().len(), 2);
    let second = fixture.cleaner.cleanup_once().await.unwrap();
    assert_eq!(fixture.runtime.stops.load(Ordering::SeqCst), 1);
    assert_eq!(second.containers_destroyed, 1);
    assert_eq!(fixture.cleaner.state.projects.iter().len(), 0);
    fixture.close().await;
}

#[tokio::test]
async fn active_shared_workload_breaks_idle_confirmation_continuity() {
    let mut f = Fixture::new(2).await;
    f.cleaner.cleanup_once().await.unwrap();
    f.activity.active.store(true, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    f.activity.active.store(false, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    assert_eq!(f.cleaner.state.projects.iter().len(), 2);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    f.close().await;
}

#[tokio::test]
async fn unreadable_activity_resets_grace_and_later_cleanup_recovers() {
    let mut f = Fixture::new(2).await;
    f.cleaner.cleanup_once().await.unwrap();
    f.activity.unreadable.store(true, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    f.activity.unreadable.store(false, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    f.close().await;
}

#[tokio::test]
async fn same_name_replacement_does_not_inherit_old_physical_confirmation() {
    let mut f = Fixture::new(2).await;
    f.cleaner.cleanup_once().await.unwrap();
    f.replace_physical("physical-successor").await;
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    assert!(
        f.cleaner.state.projects.iter().iter().all(|(_, p)| p
            .container_info()
            .unwrap()
            .container_id
            == "physical-successor")
    );
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    f.close().await;
}

#[tokio::test]
async fn final_activity_observation_failure_does_not_reuse_old_grace() {
    let mut f = Fixture::new(2).await;
    f.cleaner.cleanup_once().await.unwrap();
    // The next sweep's two scanner RPCs succeed. Only the final fresh check
    // after acquiring canonical admission fails; no physical stop is allowed.
    f.activity.fail_at.store(
        f.activity.calls.load(Ordering::SeqCst) + 3,
        Ordering::SeqCst,
    );
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.activity.fail_at.store(0, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    f.close().await;
}

#[tokio::test]
async fn failed_stop_is_not_retried_for_shared_siblings_in_same_sweep() {
    let mut f = Fixture::new(4).await;
    f.runtime.fail_stop.store(true, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    let second = f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    assert_eq!(second.failed_cleaned, 1);
    assert_eq!(second.containers_destroyed, 0);
    assert_eq!(f.cleaner.state.projects.iter().len(), 4);
    f.runtime.fail_stop.store(false, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 2);
    assert!(f.cleaner.state.projects.iter().is_empty());
    f.close().await;
}

#[tokio::test]
async fn a_new_cleaner_leadership_generation_starts_with_fresh_confirmation() {
    let mut f = Fixture::new(1).await;
    f.cleaner.cleanup_once().await.unwrap();
    f.cleaner = AgentCleaner::new(f.cleaner.config.clone(), f.cleaner.state.clone(), None);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    f.close().await;
}

#[tokio::test]
async fn cancelled_sweep_does_not_publish_previous_idle_confirmation() {
    let mut f = Fixture::new(2).await;
    f.cleaner.cleanup_once().await.unwrap();
    f.activity.pause.store(true, Ordering::SeqCst);
    let mut pending = Box::pin(f.cleaner.cleanup_once());
    tokio::time::timeout(Duration::from_secs(5), async {
        tokio::select! {
            _ = f.activity.entered.notified() => {},
            result = &mut pending => panic!("sweep completed before controlled cancellation: {result:?}"),
        }
    }).await.unwrap();
    drop(pending);
    f.activity.pause.store(false, Ordering::SeqCst);
    f.activity.release.notify_waiters();
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    assert_eq!(f.cleaner.state.projects.iter().len(), 2);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 1);
    f.close().await;
}
