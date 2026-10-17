//! Controlled runtime and gRPC protocol fixtures; no AI execution is simulated.

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use async_trait::async_trait;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    DeploymentStatus, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use rcoder_storage::userapp_lifecycle::TursoUserAppStore;
use shared_types::{
    ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore as _, ServiceType,
    UserAppAgentBinding, UserAppLifecycleStore as _, grpc::*,
};
use tonic::{Request, Response, Status};

pub(crate) const APP_ID: &str = "routeapp";
pub(crate) const USER_ID: &str = "user-1";

#[derive(Debug, Clone)]
pub(crate) enum AgentCall {
    Chat(Box<ChatRequest>),
    Status(GetStatusRequest),
    Cancel(CancelRequest),
    Stop(StopAgentRequest),
    Permission(ResolvePermissionRequest),
    Progress(ProgressRequest),
}

type StopCallback = Box<dyn FnOnce() -> std::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send>;

#[derive(Default)]
pub(crate) struct ProtocolAgent {
    pub(crate) calls: Mutex<Vec<AgentCall>>,
    pub(crate) sessions: Mutex<BTreeMap<String, String>>,
    pub(crate) events: Mutex<BTreeMap<String, Vec<ProgressEvent>>>,
    /// Optional protocol fault boundary: the RPC is accepted, but its response
    /// has not reached the HTTP handler's completion registration yet.
    pub(crate) after_chat: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The captured execution has stopped, but its RPC response is held back.
    pub(crate) after_stop: Mutex<Option<StopCallback>>,
    sequence: AtomicUsize,
}

#[tonic::async_trait]
impl agent_service_server::AgentService for ProtocolAgent {
    async fn chat(&self, request: Request<ChatRequest>) -> Result<Response<ChatResponse>, Status> {
        let request = request.into_inner();
        self.calls
            .lock()
            .unwrap()
            .push(AgentCall::Chat(Box::new(request.clone())));
        let mut sessions = self.sessions.lock().unwrap();
        let sid = if !request.force_new_session && !request.session_id.is_empty() {
            request.session_id.clone()
        } else {
            sessions
                .get(&request.project_id)
                .cloned()
                .filter(|_| !request.force_new_session)
                .unwrap_or_else(|| {
                    format!(
                        "sid-{}-{}",
                        request.project_id,
                        self.sequence.fetch_add(1, Ordering::SeqCst)
                    )
                })
        };
        sessions.insert(request.project_id.clone(), sid.clone());
        drop(sessions);
        let after_chat = self.after_chat.lock().unwrap().take();
        if let Some(after_chat) = after_chat {
            after_chat();
        }
        Ok(Response::new(ChatResponse {
            success: true,
            project_id: request.project_id,
            session_id: sid,
            error: None,
            error_code: None,
            request_id: request.request_id,
            need_fallback: false,
            fallback_reason: None,
            reloaded: false,
            agent_version: None,
        }))
    }

    type SubscribeProgressStream =
        std::pin::Pin<Box<dyn futures::Stream<Item = Result<ProgressEvent, Status>> + Send>>;
    async fn subscribe_progress(
        &self,
        request: Request<ProgressRequest>,
    ) -> Result<Response<Self::SubscribeProgressStream>, Status> {
        let request = request.into_inner();
        self.calls
            .lock()
            .unwrap()
            .push(AgentCall::Progress(request.clone()));
        let events = self
            .events
            .lock()
            .unwrap()
            .get(&request.session_id)
            .cloned()
            .unwrap_or_default();
        Ok(Response::new(Box::pin(futures::stream::iter(
            events.into_iter().map(Ok),
        ))))
    }

    async fn cancel_session(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<CancelResponse>, Status> {
        let request = request.into_inner();
        self.calls
            .lock()
            .unwrap()
            .push(AgentCall::Cancel(request.clone()));
        let sessions = self.sessions.lock().unwrap();
        if !request.session_id.is_empty()
            && sessions.get(&request.project_id) != Some(&request.session_id)
        {
            return Err(Status::not_found("session does not belong to project"));
        }
        Ok(Response::new(CancelResponse {
            success: true,
            result: CancelResultType::CancelResultSuccess as i32,
            message: None,
        }))
    }

    async fn resolve_permission(
        &self,
        request: Request<ResolvePermissionRequest>,
    ) -> Result<Response<ResolvePermissionResponse>, Status> {
        let request = request.into_inner();
        self.calls
            .lock()
            .unwrap()
            .push(AgentCall::Permission(request.clone()));
        if self.sessions.lock().unwrap().get(&request.project_id) != Some(&request.session_id) {
            return Err(Status::not_found("session does not belong to project"));
        }
        Ok(Response::new(ResolvePermissionResponse {
            success: true,
            session_id: request.session_id,
            tool_call_id: request.tool_call_id,
            outcome_json: None,
            rule_saved: false,
            error_code: None,
            message: None,
        }))
    }

    async fn get_status(
        &self,
        request: Request<GetStatusRequest>,
    ) -> Result<Response<GetStatusResponse>, Status> {
        let request = request.into_inner();
        self.calls
            .lock()
            .unwrap()
            .push(AgentCall::Status(request.clone()));
        let sessions = self.sessions.lock().unwrap();
        let found = sessions
            .get(&request.project_id)
            .is_some_and(|sid| request.session_id.is_empty() || sid == &request.session_id);
        Ok(Response::new(GetStatusResponse {
            status: if found { "busy" } else { "not_found" }.into(),
            is_found: found,
            stream_epoch: None,
        }))
    }

    async fn stop_agent(
        &self,
        request: Request<StopAgentRequest>,
    ) -> Result<Response<StopAgentResponse>, Status> {
        let request = request.into_inner();
        self.calls
            .lock()
            .unwrap()
            .push(AgentCall::Stop(request.clone()));
        let found = {
            let mut sessions = self.sessions.lock().unwrap();
            if request
                .expected_session_id
                .as_ref()
                .is_some_and(|sid| sessions.get(&request.project_id) != Some(sid))
            {
                return Ok(Response::new(StopAgentResponse {
                    success: false,
                    result: "session_mismatch".into(),
                    message: None,
                    project_id: request.project_id,
                }));
            }
            sessions.remove(&request.project_id).is_some()
        };
        let after_stop = self.after_stop.lock().unwrap().take();
        if let Some(after_stop) = after_stop {
            after_stop().await;
        }
        Ok(Response::new(StopAgentResponse {
            success: found,
            result: if found { "stopped" } else { "not_found" }.into(),
            message: None,
            project_id: request.project_id,
        }))
    }

    async fn get_container_status(
        &self,
        _: Request<GetContainerStatusRequest>,
    ) -> Result<Response<GetContainerStatusResponse>, Status> {
        Err(Status::unimplemented(
            "container status is not part of this protocol fixture",
        ))
    }
    async fn get_vnc_status(
        &self,
        _: Request<GetVncStatusRequest>,
    ) -> Result<Response<GetVncStatusResponse>, Status> {
        Err(Status::unimplemented(
            "VNC is not part of this protocol fixture",
        ))
    }
}

pub(crate) struct ControlledRuntime {
    pub(crate) builder: Mutex<Option<ContainerBasicInfo>>,
    pub(crate) lookups: Mutex<Vec<(String, ServiceType)>>,
    pub(crate) writes: AtomicUsize,
}

#[async_trait]
impl AgentContainerRuntime for ControlledRuntime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected create".into(),
        ))
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn get_container_info_by_identifier(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        self.lookups
            .lock()
            .unwrap()
            .push((identifier.into(), *service_type));
        Ok(self
            .builder
            .lock()
            .unwrap()
            .clone()
            .filter(|_| identifier == APP_ID && *service_type == ServiceType::UserappBuilder))
    }
    async fn find_container(
        &self,
        _: &str,
        _: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        Ok(None)
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        self.writes.fetch_add(1, Ordering::SeqCst);
        Err(ContainerRuntimeError::ConfigurationError(
            "unexpected container stop".into(),
        ))
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(self.builder.lock().unwrap().is_some())
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
}
#[async_trait]
impl WorkspaceRuntime for ControlledRuntime {}
#[async_trait]
impl UserAppDeploymentRuntime for ControlledRuntime {
    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        Ok(vec![])
    }
}

/// Own the fixed-port service and synchronously release its listener before the next fixture.
struct ProtocolServer {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
    _port_gate: tokio::sync::OwnedMutexGuard<()>,
}

impl ProtocolServer {
    async fn start(agent: Arc<ProtocolAgent>) -> (Self, std::net::SocketAddr) {
        static PORT_GATE: std::sync::OnceLock<Arc<tokio::sync::Mutex<()>>> =
            std::sync::OnceLock::new();
        let gate = PORT_GATE
            .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone();
        let port_gate = gate.lock_owned().await;
        let listener = std::net::TcpListener::bind((
            std::net::Ipv4Addr::LOCALHOST,
            shared_types::GRPC_DEFAULT_PORT,
        ))
        .unwrap();
        listener.set_nonblocking(true).unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                let serving = tonic::transport::Server::builder()
                    .add_service(agent_service_server::AgentServiceServer::from_arc(agent))
                    .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener));
                tokio::select! {
                    result = serving => result.unwrap(),
                    _ = stopped => {},
                }
            });
            // Dropping this runtime also closes any outstanding fixture RPC connections.
        });
        (
            Self {
                shutdown: Some(shutdown),
                thread: Some(thread),
                _port_gate: port_gate,
            },
            addr,
        )
    }
}

impl Drop for ProtocolServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let stopped = thread.join();
            if !std::thread::panicking() {
                stopped.expect("fixture gRPC thread completed without failure");
            }
        }
    }
}

pub(crate) struct Fixture {
    pub(crate) state: Arc<crate::app_state::AppState>,
    pub(crate) store: Arc<TursoUserAppStore>,
    pub(crate) runtime: Arc<ControlledRuntime>,
    pub(crate) agent: Arc<ProtocolAgent>,
    pub(crate) root: tempfile::TempDir,
    _server: ProtocolServer,
}
impl Fixture {
    pub(crate) async fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(
            TursoUserAppStore::open_exclusive(&root.path().join("control.db"))
                .await
                .unwrap(),
        );
        store.ensure_identity(APP_ID).await.unwrap();
        let agent = Arc::new(ProtocolAgent::default());
        let (server, addr) = ProtocolServer::start(agent.clone()).await;
        let runtime = Arc::new(ControlledRuntime {
            builder: Mutex::new(Some(ContainerBasicInfo {
                container_id: "builder-physical-uid".into(),
                container_name: "rcoder-app-builder-routeapp".into(),
                container_ip: addr.ip().to_string(),
                internal_port: 60000,
                external_port: 0,
                project_id: APP_ID.into(),
                status: "running".into(),
                created_at: chrono::Utc::now(),
                service_url: format!("http://{}:60000", addr.ip()),
                workload_uid: None,
            })),
            lookups: Mutex::new(vec![]),
            writes: AtomicUsize::new(0),
        });
        let activity = Arc::new(app_manager::AppActivityRegistry::new(
            std::time::Duration::from_secs(300),
        ));
        let app_service = Arc::new(
            app_manager::service::AppService::new(
                app_manager::config::AppManagerConfig {
                    access_mode: app_manager::config::AppAccessMode::Docker,
                    ..Default::default()
                },
                runtime.clone(),
                activity.clone(),
                None,
                store.clone(),
            )
            .await
            .unwrap(),
        );
        let (adapter, _) =
            crate::storage::ProjectAdapter::new("test-ns".into(), "cluster.local".into());
        let projects = Arc::new(crate::storage::ProjectStoreBackend::Memory(Arc::new(
            adapter,
        )));
        let mut app_project = ProjectAndContainerInfo::new(APP_ID.into());
        app_project.set_service_type(Some(ServiceType::UserappBuilder));
        projects
            .insert(APP_ID.into(), Arc::new(app_project))
            .unwrap();
        let (pod_created_tx, _) = tokio::sync::broadcast::channel(8);
        let state = Arc::new(crate::app_state::AppState {
            userapp_store: store.clone(),
            userapp_store_control: store.clone(),
            userapp_op_flight: Arc::new(Default::default()),
            userapp_recovery_handle: Arc::new(Mutex::new(None)),
            config: crate::config::AppConfig::default(),
            projects,
            pingora_service: None,
            userapp_error_page: None,
            grpc_pool: Arc::new(crate::grpc::GrpcChannelPool::new()),
            session_stream_registry: Arc::new(crate::grpc::SessionStreamRegistry::new()),
            api_key_config: Arc::new(arc_swap::ArcSwap::from_pointee(
                crate::config::ApiKeyAuthConfig::default(),
            )),
            pod_creating: Arc::new(dashmap::DashMap::new()),
            pod_created_tx: Arc::new(pod_created_tx),
            container_prefix_rcoder: "fixture-web".into(),
            container_prefix_computer: "fixture-computer".into(),
            runtime: runtime.clone(),
            cleanup_rx: Arc::new(Mutex::new(None)),
            agent_download_manager: Arc::new(
                agent_provisioning::AgentDownloadManager::new(root.path()).unwrap(),
            ),
            app_service,
            activity,
            cluster_domain: "cluster.local".into(),
        });
        Self {
            state,
            store,
            runtime,
            agent,
            root,
            _server: server,
        }
    }

    pub(crate) async fn bind(&self, project_id: &str, sid: &str) -> UserAppAgentBinding {
        let app = self.store.get_application(APP_ID).await.unwrap().unwrap();
        let binding = self
            .state
            .projects
            .accept_userapp_agent_binding(APP_ID, &app.lifecycle_id, project_id, USER_ID)
            .await
            .unwrap();
        let binding = self
            .state
            .projects
            .register_userapp_agent_session(
                &binding,
                sid,
                &format!("register-{project_id}-{sid}"),
                tokio::time::Instant::now() + std::time::Duration::from_secs(10),
            )
            .await
            .unwrap();
        self.agent
            .sessions
            .lock()
            .unwrap()
            .insert(project_id.into(), sid.into());
        binding
    }
}
