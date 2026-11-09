//! 后台状态检查的物理身份契约回归网（specs/prod-agent-health-recovery-20261024）。
//!
//! 锁住的偏移敏感点：
//! - 查询前必须核实当前物理身份：换代/缺席/查找失败都不向注册旧地址发 RPC
//! - RPC 结果按观察代次条件写回：迟到响应不得覆盖新注册的状态与活动时间
//! - Computer 族换代经运行时 verify 证明 + durable CAS 受控重登记后才探测新实例；
//!   Web/UserappBuilder 族不借 Computer 专用动作，换代只观察留痕
//! - Userapp 族无 runtime 身份源，保持注册地址直探测，不触碰 runtime 查找
//!
//! 真实 Tonic 服务绑 127.0.0.1 动态端口；探针端口经 cfg(test) 覆盖注入，
//! 不占用固定 GRPC_DEFAULT_PORT（避免并行测试端口竞争）。

use super::checker::ContainerStatusChecker;
use super::state::ContainerStatusCheckerConfig;
use crate::app_state::AppState;
use crate::config::AppConfig;
use crate::grpc::{GrpcChannelPool, SessionStreamRegistry};
use crate::storage::{ProjectAdapter, ProjectStoreBackend};
use agent_provisioning::AgentDownloadManager;
use app_manager::config::{AppAccessMode, AppManagerConfig};
use arc_swap::ArcSwap;
use async_trait::async_trait;
use chrono::Utc;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeError, ContainerRuntimeResult,
    ContainerRuntimeStatus, RuntimeContainerInfo, UserAppDeploymentRuntime,
    VerifiedAgentReplacement, WorkspaceRuntime,
};
use dashmap::DashMap;
use shared_types::grpc::{
    self,
    agent_service_server::{AgentService, AgentServiceServer},
};
use shared_types::{
    AgentStatus, ApiKeyAuthConfig, ContainerBasicInfo, ProjectAndContainerInfo,
    ProjectExtendedFields, ProjectStore as _, ServiceType,
};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::broadcast;
use tonic::{Request, Response, Status};

/// 可编程身份解析桩：find_container 返回预设物理候选并记录查询；
/// verify_agent_replacement 可配置（Computer 恢复路径）。
struct IdentityRuntime {
    found: Option<RuntimeContainerInfo>,
    replacement: Option<VerifiedAgentReplacement>,
    /// verify 拒绝的错误工厂（ContainerRuntimeError 未实现 Clone，按次构造）。
    verify_error: Option<Arc<dyn Fn() -> ContainerRuntimeError + Send + Sync>>,
    stalled_lookup: std::sync::atomic::AtomicBool,
    queries: Mutex<Vec<(String, ServiceType)>>,
    verify_calls: Mutex<Vec<String>>,
}

impl IdentityRuntime {
    fn new(found: Option<RuntimeContainerInfo>) -> Arc<Self> {
        Arc::new(Self {
            found,
            replacement: None,
            verify_error: None,
            stalled_lookup: std::sync::atomic::AtomicBool::new(false),
            queries: Mutex::new(Vec::new()),
            verify_calls: Mutex::new(Vec::new()),
        })
    }

    fn with_replacement(self: Arc<Self>, evidence: VerifiedAgentReplacement) -> Arc<Self> {
        let mut runtime = Arc::try_unwrap(self).ok().expect("独占运行时句柄");
        runtime.replacement = Some(evidence);
        Arc::new(runtime)
    }

    fn with_verify_error<F>(self: Arc<Self>, make: F) -> Arc<Self>
    where
        F: Fn() -> ContainerRuntimeError + Send + Sync + 'static,
    {
        let mut runtime = Arc::try_unwrap(self).ok().expect("独占运行时句柄");
        runtime.verify_error = Some(Arc::new(make));
        Arc::new(runtime)
    }

    fn queried(&self) -> usize {
        self.queries.lock().unwrap().len()
    }

    fn verified(&self) -> usize {
        self.verify_calls.lock().unwrap().len()
    }
}

fn runtime_info(uid: &str, workload: Option<&str>, ip: &str, name: &str) -> RuntimeContainerInfo {
    RuntimeContainerInfo {
        container_id: uid.to_string(),
        container_name: name.to_string(),
        container_ip: ip.to_string(),
        status: ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: None,
        project_id: None,
        user_id: None,
        pod_id: None,
        app_id: None,
        workload_uid: workload.map(str::to_string),
    }
}

#[async_trait]
impl AgentContainerRuntime for IdentityRuntime {
    async fn create_container(
        &self,
        _params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ContainerNotFound("probe".into()))
    }
    async fn get_container_info(
        &self,
        _project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        self.queries
            .lock()
            .unwrap()
            .push((identifier.to_string(), *service_type));
        if self.stalled_lookup.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        Ok(self.found.clone())
    }
    async fn verify_agent_replacement(
        &self,
        _params: &ContainerCreateParams,
        _previous: Option<&ContainerBasicInfo>,
        _replacement: &ContainerBasicInfo,
    ) -> ContainerRuntimeResult<VerifiedAgentReplacement> {
        self.verify_calls.lock().unwrap().push("verify".into());
        if let Some(make) = &self.verify_error {
            return Err(make());
        }
        match &self.replacement {
            Some(evidence) => Ok(evidence.clone()),
            None => Err(ContainerRuntimeError::ConfigurationError(
                "verified Agent workload replacement is unsupported".into(),
            )),
        }
    }
    async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
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
impl WorkspaceRuntime for IdentityRuntime {}
#[async_trait]
impl UserAppDeploymentRuntime for IdentityRuntime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(vec![])
    }
}

/// 真实 Tonic 服务：统计请求；可在响应前执行注入动作（模拟 RPC 在途换代）。
struct ProbeAgent {
    hits: Arc<AtomicUsize>,
    is_active: bool,
    before_response: Option<Arc<dyn Fn() + Send + Sync>>,
}

#[tonic::async_trait]
impl AgentService for ProbeAgent {
    type SubscribeProgressStream =
        Pin<Box<dyn futures_util::Stream<Item = Result<grpc::ProgressEvent, Status>> + Send>>;
    async fn get_container_status(
        &self,
        _: Request<grpc::GetContainerStatusRequest>,
    ) -> Result<Response<grpc::GetContainerStatusResponse>, Status> {
        self.hits.fetch_add(1, Ordering::SeqCst);
        if let Some(before) = &self.before_response {
            before();
        }
        Ok(Response::new(grpc::GetContainerStatusResponse {
            is_active: self.is_active,
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
        Err(Status::unimplemented("unused"))
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

type ServerHandle = (
    Arc<AtomicUsize>,
    tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
);

/// 在 loopback 绑动态端口启动探针服务。
async fn spawn_probe_at(
    is_active: bool,
    before_response: Option<Arc<dyn Fn() + Send + Sync>>,
) -> (u16, ServerHandle) {
    use tokio_stream::wrappers::TcpListenerStream;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0u16))
        .await
        .unwrap_or_else(|error| panic!("绑定动态端口失败: {error}"));
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let service = ProbeAgent {
        hits: hits.clone(),
        is_active,
        before_response,
    };
    let task = tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(AgentServiceServer::new(service))
            .serve_with_incoming(TcpListenerStream::new(listener)),
    );
    (port, (hits, task))
}

/// 捕获 tracing 事件的缓冲 writer（与 background_tasks::pg_fault_tests 同一模式）。
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

async fn identity_state(runtime: Arc<IdentityRuntime>) -> (Arc<AppState>, tempfile::TempDir) {
    let adapter = ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
    let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
        300,
    )));
    let manager_config = AppManagerConfig {
        access_mode: AppAccessMode::Docker,
        ..AppManagerConfig::default()
    };
    let metadata_dir = tempfile::tempdir().expect("metadata directory");
    let metadata_store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
        &metadata_dir.path().join("userapp.turso.db"),
    )
    .await
    .expect("Turso metadata store");
    let metadata_store = Arc::new(metadata_store);
    let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
        app_manager::service::AppService::new(
            manager_config,
            runtime.clone(),
            activity.clone(),
            None,
            metadata_store.clone(),
        )
        .await
        .expect("AppService 构造失败"),
    );
    let download_dir = tempfile::tempdir().expect("tempdir");
    let agent_download_manager =
        Arc::new(AgentDownloadManager::new(download_dir.path()).expect("下载管理器构造失败"));
    let (pod_created_tx, _) = broadcast::channel(32);
    let state = Arc::new(AppState {
        userapp_store: metadata_store.clone(),
        userapp_store_control: metadata_store,
        userapp_op_flight: Arc::new(
            crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
        ),
        userapp_recovery_handle: Arc::new(Mutex::new(None)),
        config: AppConfig::default(),
        projects: Arc::new(ProjectStoreBackend::Memory(Arc::new(adapter))),
        pingora_service: None,
        userapp_error_page: None,
        grpc_pool: Arc::new(GrpcChannelPool::new()),
        session_stream_registry: Arc::new(SessionStreamRegistry::new()),
        api_key_config: Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default())),
        pod_creating: Arc::new(DashMap::new()),
        pod_created_tx: Arc::new(pod_created_tx),
        container_prefix_rcoder: "dev-rcoder".to_string(),
        container_prefix_computer: "computer-agent-runner".to_string(),
        runtime,
        agent_download_manager,
        app_service,
        activity,
        cluster_domain: "cluster.local".to_string(),
    });
    (state, metadata_dir)
}

/// A real deploy-host target also needs its Published entry. Do not bypass
/// production address resolution merely to make the dynamic-port tests pass.
struct ProbePublishedGuard {
    #[cfg(feature = "deploy-host")]
    names: Vec<String>,
}
impl Drop for ProbePublishedGuard {
    fn drop(&mut self) {
        #[cfg(feature = "deploy-host")]
        for name in &self.names {
            shared_types::published::unregister(name);
        }
    }
}
fn checker(state: Arc<AppState>, probe_port: u16) -> (ContainerStatusChecker, ProbePublishedGuard) {
    #[cfg(feature = "deploy-host")]
    let names: Vec<_> = state
        .projects
        .iter()
        .into_iter()
        .filter_map(|(_, info)| info.container_info().map(|basic| basic.container_name))
        .collect();
    #[cfg(feature = "deploy-host")]
    for name in &names {
        shared_types::published::register(
            name,
            std::collections::HashMap::from([(probe_port, probe_port)]),
        );
    }
    let config = ContainerStatusCheckerConfig {
        query_timeout: Duration::from_secs(2),
        ..Default::default()
    };
    (
        ContainerStatusChecker::new(config, state).with_probe_port(probe_port),
        ProbePublishedGuard {
            #[cfg(feature = "deploy-host")]
            names,
        },
    )
}

/// 测试载体：完全指定物理身份与地址的注册记录。
fn registered(
    project_id: &str,
    user_id: Option<&str>,
    service_type: ServiceType,
    uid: &str,
    workload: Option<&str>,
    ip: &str,
) -> Arc<ProjectAndContainerInfo> {
    let container_name = format!("{}-{project_id}", service_type.container_family_key());
    let container = ContainerBasicInfo {
        container_id: uid.to_string(),
        container_name,
        container_ip: ip.to_string(),
        internal_port: 8086,
        external_port: 0,
        project_id: project_id.to_string(),
        status: "running".to_string(),
        created_at: Utc::now(),
        service_url: format!("http://{ip}"),
        workload_uid: workload.map(str::to_string),
    };
    Arc::new(ProjectAndContainerInfo::from_parts(
        project_id.to_string(),
        user_id.map(str::to_string),
        None,
        None,
        Some(container),
        ProjectExtendedFields {
            service_type: Some(service_type),
            ..Default::default()
        },
    ))
}

/// D1：物理身份已换代（Web 族，无受控换代动作）时，不向注册旧地址发 RPC，
/// 必须先经 runtime 解析当前身份；换代仅观察留痕，不自动接管。
#[tokio::test]
async fn replaced_identity_is_not_probed_on_old_address() {
    let (port, (hits, server)) = spawn_probe_at(false, None).await;
    let runtime = IdentityRuntime::new(Some(runtime_info(
        "pod-new",
        Some("sts-1"),
        "127.0.0.1",
        "web-agent-runner-p1",
    )));
    let (state, _dir) = identity_state(runtime.clone()).await;
    let info = registered(
        "p1",
        None,
        ServiceType::WebAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    );
    state
        .insert_project("p1".to_string(), info.clone())
        .expect("注册旧实例");

    let (checker, _published) = checker(state.clone(), port);
    let _outcome = checker.check_single_container("p1", &info).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "换代后不得向注册旧地址发 RPC"
    );
    assert!(
        runtime.queried() > 0,
        "发 RPC 前必须先经 runtime 解析当前物理身份"
    );
    let current = state.projects.get("p1").expect("注册保留");
    assert_eq!(
        current.container_info().unwrap().container_id,
        "pod-old",
        "Web 族换代不自动接管，注册原样保留"
    );
    server.abort();
}

/// D1-缺席：runtime 无候选时不发 RPC，注册保留（缺席不等于退休）。
#[tokio::test]
async fn absent_candidate_is_not_probed_and_registration_retained() {
    let (port, (hits, server)) = spawn_probe_at(false, None).await;
    let runtime = IdentityRuntime::new(None);
    let (state, _dir) = identity_state(runtime).await;
    let info = registered(
        "p2",
        None,
        ServiceType::WebAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    );
    state
        .insert_project("p2".to_string(), info.clone())
        .expect("注册");

    let (checker, _published) = checker(state.clone(), port);
    let _outcome = checker.check_single_container("p2", &info).await;

    assert_eq!(hits.load(Ordering::SeqCst), 0, "缺席时不得探测旧地址");
    assert!(
        state.projects.get("p2").is_some(),
        "缺席是观察结论，不退休注册"
    );
    server.abort();
}

/// D1-查找失败：runtime API 错误不推断缺席，不探测旧地址。
#[tokio::test]
async fn identity_lookup_failure_keeps_registration_without_probe() {
    let (port, (hits, server)) = spawn_probe_at(false, None).await;
    let runtime = Arc::new(FailingLookupRuntime::default());
    let (state, _dir) = identity_state_arc(runtime).await;
    let info = registered(
        "p5",
        None,
        ServiceType::WebAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    );
    state
        .insert_project("p5".to_string(), info.clone())
        .expect("注册");

    let (checker, _published) = checker(state.clone(), port);
    let _outcome = checker.check_single_container("p5", &info).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "查找失败（未知）不得探测旧地址"
    );
    assert!(state.projects.get("p5").is_some(), "未知不退休注册");
    server.abort();
}

/// D2：RPC 在途换代后，旧响应不得覆盖新注册的状态与活动时间。
#[tokio::test]
async fn late_response_cannot_overwrite_replaced_registration() {
    let runtime = IdentityRuntime::new(Some(runtime_info(
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
        "web-agent-runner-p3",
    )));
    let (state, _dir) = identity_state(runtime).await;
    let old_activity = Utc::now() - chrono::Duration::hours(3);
    let info = registered(
        "p3",
        None,
        ServiceType::WebAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    );
    state
        .insert_project("p3".to_string(), info.clone())
        .expect("注册旧实例");

    // 在途回调：RPC 返回前发布同项目的新物理登记（Active、旧活动时间）。
    let before_state = state.clone();
    let before = Arc::new(move || {
        let mut fresh = (*registered(
            "p3",
            None,
            ServiceType::WebAgentRunner,
            "pod-new",
            Some("sts-1"),
            "127.0.0.1",
        ))
        .clone();
        fresh.set_status(Some(AgentStatus::Active));
        fresh.set_timestamps(old_activity, old_activity);
        before_state
            .insert_project("p3".to_string(), Arc::new(fresh))
            .expect("发布新登记");
    });
    let (port, (hits, server)) = spawn_probe_at(false, Some(before)).await;

    let (checker, _published) = checker(state.clone(), port);
    let _outcome = checker.check_single_container("p3", &info).await;

    let current = state.projects.get("p3").expect("注册仍在");
    let basic = current.container_info().expect("容器信息仍在");
    assert_eq!(basic.container_id, "pod-new");
    assert_eq!(
        current.status(),
        Some(&AgentStatus::Active),
        "旧 Idle 响应不得覆盖新登记的 Active 状态"
    );
    assert_eq!(
        current.last_activity(),
        old_activity,
        "旧响应不得刷新新代次的活动时间"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

/// D1+恢复：Computer 族同 workload 换 Pod，经运行时 verify 证明 + durable CAS
/// 受控重登记后，才探测新实例地址；旧地址零请求；新登记带新活动时间。
#[tokio::test]
async fn computer_replacement_recovers_registration_and_probes_new_address() {
    let (port, (hits, server)) = spawn_probe_at(true, None).await;
    let identifier = "u-r";
    // Computer 注册的 basic.project_id 是容器定位键（identifier），与生产
    // build_container_basic_info 的约定一致；validate_agent_replacement 按此校验。
    let mut info_inner = (*registered(
        "proj-r",
        Some(identifier),
        ServiceType::ComputerAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    ))
    .clone();
    let mut basic = info_inner.container_info().unwrap();
    basic.project_id = identifier.to_string();
    info_inner.set_container(Some(basic));
    let info = Arc::new(info_inner);
    let workload_name = info.container_info().unwrap().container_name.clone();
    let mut replacement_basic = info.container_info().unwrap();
    replacement_basic.container_id = "pod-new".to_string();
    replacement_basic.container_ip = "127.0.0.1".to_string();
    replacement_basic.project_id = identifier.to_string();
    let evidence = VerifiedAgentReplacement {
        identifier: identifier.to_string(),
        service_type: ServiceType::ComputerAgentRunner,
        previous: Some(info.container_info().unwrap()),
        replacement: replacement_basic,
        workspace: shared_types::AgentWorkspaceIdentity {
            namespace: "test-ns".into(),
            claim_name: "claim".into(),
            claim_uid: "claim-uid".into(),
            mount_path: "/workspace".into(),
            sub_path: Some("u-r".into()),
        },
    };
    let runtime = IdentityRuntime::new(Some(runtime_info(
        "pod-new",
        Some("sts-1"),
        "127.0.0.1",
        &workload_name,
    )))
    .with_replacement(evidence);
    let (state, _dir) = identity_state(runtime.clone()).await;
    state
        .insert_project("proj-r".to_string(), info.clone())
        .expect("注册旧实例");

    let (checker, _published) = checker(state.clone(), port);
    let outcome = checker.check_single_container(identifier, &info).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        1,
        "恰好一次探测：先受控重登记，再探测新实例；任何旧地址直探测都会叠加计数"
    );
    assert_eq!(runtime.verified(), 1, "必须经运行时 verify 证明");
    let current = state.projects.get("proj-r").expect("注册仍在");
    let basic = current.container_info().expect("容器信息仍在");
    assert_eq!(basic.container_id, "pod-new", "注册已换代到新 Pod");
    assert_eq!(current.status(), Some(&AgentStatus::Active));
    assert!(outcome.unwrap(), "活跃任务必须刷新活动时间");
    server.abort();
}

/// Computer 族换 workload（而非仅换 Pod）且 verify 拒绝时：保持并重试，
/// 不探测任何地址、不改注册。
#[tokio::test]
async fn rejected_computer_replacement_keeps_registration_without_probe() {
    let (port, (hits, server)) = spawn_probe_at(false, None).await;
    let runtime = IdentityRuntime::new(Some(runtime_info(
        "pod-new",
        Some("sts-2"),
        "127.0.0.1",
        "computer-agent-runner-u-r",
    )));
    let (state, _dir) = identity_state(runtime.clone()).await;
    let info = registered(
        "proj-r2",
        Some("u-r2"),
        ServiceType::ComputerAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    );
    state
        .insert_project("proj-r2".to_string(), info.clone())
        .expect("注册旧实例");

    let (checker, _published) = checker(state.clone(), port);
    let _outcome = checker.check_single_container("u-r2", &info).await;

    assert_eq!(
        hits.load(Ordering::SeqCst),
        0,
        "verify 拒绝（未配置证据）时不探测任何地址"
    );
    assert_eq!(runtime.verified(), 1, "拒绝也必须经运行时 verify 判定");
    let current = state.projects.get("proj-r2").expect("注册保留");
    assert_eq!(
        current.container_info().unwrap().container_id,
        "pod-old",
        "verify 拒绝不改注册"
    );
    server.abort();
}

/// 诊断契约（生产巡检批次二B）：verify 以 Conflict 拒绝换代时，WARN 必须携带
/// 完整脱敏 cause 链（外层 context + typed 换代冲突 + runtime 底层原因）、
/// typed 类别与新旧物理身份；注册保留与不探测语义不变，凭据不落日志。
#[tokio::test]
async fn rejected_computer_replacement_warn_carries_full_chain_category_and_identities() {
    let (port, (hits, server)) = spawn_probe_at(false, None).await;
    let runtime = IdentityRuntime::new(Some(runtime_info(
        "pod-new",
        Some("sts-2"),
        "127.0.0.1",
        "computer-agent-runner-u-diag",
    )))
    .with_verify_error(|| {
        ContainerRuntimeError::Conflict(
            "replacement evidence rejected; token=secret-diag-token".into(),
        )
    });
    let (state, _dir) = identity_state(runtime.clone()).await;
    let info = registered(
        "proj-diag",
        Some("u-diag"),
        ServiceType::ComputerAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    );
    state
        .insert_project("proj-diag".to_string(), info.clone())
        .expect("注册旧实例");

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);

    let (checker, _published) = checker(state.clone(), port);
    let outcome = checker.check_single_container("u-diag", &info).await;

    // 保护语义不变：拒绝按失败观察上报、不探测、注册原样保留。
    assert!(outcome.is_err(), "verify 拒绝必须按失败观察上报");
    assert_eq!(hits.load(Ordering::SeqCst), 0, "拒绝时不探测任何地址");
    assert_eq!(runtime.verified(), 1, "诊断增强不得绕过运行时 verify");
    let current = state.projects.get("proj-diag").expect("注册保留");
    assert_eq!(
        current.container_info().unwrap().container_id,
        "pod-old",
        "诊断增强不得改变注册保留语义"
    );

    let text = capture.text();
    assert!(
        text.contains("Computer replacement recovery rejected"),
        "保留原 WARN 事件：{text}"
    );
    assert!(
        text.contains("Observe Computer workload replacement"),
        "外层 context 仍在链上：{text}"
    );
    assert!(
        text.contains("Container workload was replaced (sts-1 -> sts-2)"),
        "typed ContainerWorkloadReplaced 上下文必须可见：{text}"
    );
    assert!(
        text.contains("Concurrent resource update: replacement evidence rejected"),
        "runtime 底层原因必须可见：{text}"
    );
    assert!(
        text.contains(shared_types::error_codes::CONTAINER_WORKLOAD_REPLACED),
        "typed 拒绝类别必须透出：{text}"
    );
    assert!(
        text.contains("pod-old") && text.contains("pod-new"),
        "新旧物理 container_id 必须出现在诊断字段：{text}"
    );
    assert!(
        text.contains("sts-1") && text.contains("sts-2"),
        "新旧 workload UID 必须出现在诊断字段：{text}"
    );
    assert!(
        !text.contains("secret-diag-token"),
        "cause 链中的凭据必须脱敏：{text}"
    );
    server.abort();
}

/// 诊断契约：verify 以 runtime 传输错误拒绝（无 typed 冲突上下文）时，
/// 传播链返回的错误与 WARN 类别必须携带 runtime 原因与变体分类，
/// record_failure 的失败原因不再只剩最外层 context。
#[tokio::test]
async fn rejected_computer_replacement_reason_carries_runtime_cause_and_category() {
    let (port, (_hits, server)) = spawn_probe_at(false, None).await;
    let runtime = IdentityRuntime::new(Some(runtime_info(
        "pod-new",
        Some("sts-4"),
        "127.0.0.1",
        "computer-agent-runner-u-diag2",
    )))
    .with_verify_error(|| {
        ContainerRuntimeError::ConnectionError("management plane unreachable".into())
    });
    let (state, _dir) = identity_state(runtime).await;
    let info = registered(
        "proj-diag2",
        Some("u-diag2"),
        ServiceType::ComputerAgentRunner,
        "pod-old-2",
        Some("sts-3"),
        "127.0.0.1",
    );
    state
        .insert_project("proj-diag2".to_string(), info.clone())
        .expect("注册旧实例");

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);

    let (checker, _published) = checker(state, port);
    let error = checker
        .check_single_container("u-diag2", &info)
        .await
        .expect_err("verify 拒绝必须按失败观察上报");

    // 真实传播链（checker→recover→verify→replacement_verification_error）返回的
    // 错误文本：不得只剩 "Observe Computer workload replacement"。
    let text = error.to_string();
    assert!(
        text.contains("Observe Computer workload replacement"),
        "外层 context 仍在：{text}"
    );
    assert!(
        text.contains("Connection error: management plane unreachable"),
        "返回错误必须携带 runtime 底层原因：{text}"
    );
    let logs = capture.text();
    assert!(
        logs.contains("Connection error: management plane unreachable"),
        "WARN 必须携带 runtime 底层原因：{logs}"
    );
    assert!(
        logs.contains(shared_types::ERR_RUNTIME_UNAVAILABLE),
        "runtime 变体类别必须透出：{logs}"
    );
    server.abort();
}

/// Userapp 族：无 runtime 身份源，保持注册地址直探测，不触碰 runtime 查找；
/// 状态写入仍按物理身份条件应用（同代次正常生效）。
#[tokio::test]
async fn userapp_family_probes_registered_address_without_runtime_lookup() {
    let (port, (hits, server)) = spawn_probe_at(false, None).await;
    let runtime = IdentityRuntime::new(None);
    let (state, _dir) = identity_state(runtime.clone()).await;
    let info = registered(
        "app-9",
        None,
        ServiceType::Userapp,
        "pod-app",
        None,
        "127.0.0.1",
    );
    state
        .insert_project("app-9".to_string(), info.clone())
        .expect("注册");

    let (checker, _published) = checker(state.clone(), port);
    let _outcome = checker.check_single_container("app-9", &info).await;

    assert_eq!(hits.load(Ordering::SeqCst), 1, "Userapp 保持注册地址直探测");
    assert_eq!(runtime.queried(), 0, "Userapp 不得触碰 runtime 身份查找");
    assert_eq!(
        state.projects.get("app-9").unwrap().status(),
        Some(&AgentStatus::Idle),
        "同代次观察正常应用"
    );
    server.abort();
}

/// 规格 §3/目标 5：后台受控重登记本身不刷新用户活动时间——恢复后的新实例
/// 报 Idle 时，活动时钟必须保持恢复前的值（只有真实活跃才刷新）。
#[tokio::test]
async fn background_computer_recovery_does_not_refresh_activity_by_itself() {
    let (port, (hits, server)) = spawn_probe_at(false, None).await;
    let identifier = "u-r3";
    let stale_activity = Utc::now() - chrono::Duration::hours(4);
    let mut info_inner = (*registered(
        "proj-r3",
        Some(identifier),
        ServiceType::ComputerAgentRunner,
        "pod-old",
        Some("sts-1"),
        "127.0.0.1",
    ))
    .clone();
    let mut basic = info_inner.container_info().unwrap();
    basic.project_id = identifier.to_string();
    info_inner.set_container(Some(basic));
    info_inner.set_timestamps(stale_activity, stale_activity);
    let info = Arc::new(info_inner);

    let mut replacement_basic = info.container_info().unwrap();
    replacement_basic.container_id = "pod-new".to_string();
    replacement_basic.container_ip = "127.0.0.1".to_string();
    replacement_basic.project_id = identifier.to_string();
    let evidence = VerifiedAgentReplacement {
        identifier: identifier.to_string(),
        service_type: ServiceType::ComputerAgentRunner,
        previous: Some(info.container_info().unwrap()),
        replacement: replacement_basic,
        workspace: shared_types::AgentWorkspaceIdentity {
            namespace: "test-ns".into(),
            claim_name: "claim".into(),
            claim_uid: "claim-uid".into(),
            mount_path: "/workspace".into(),
            sub_path: Some(identifier.into()),
        },
    };
    let runtime = IdentityRuntime::new(Some(runtime_info(
        "pod-new",
        Some("sts-1"),
        "127.0.0.1",
        &info.container_info().unwrap().container_name,
    )))
    .with_replacement(evidence);
    let (state, _dir) = identity_state(runtime).await;
    state
        .insert_project("proj-r3".to_string(), info.clone())
        .expect("注册旧实例");

    let (checker, _published) = checker(state.clone(), port);
    let _outcome = checker.check_single_container(identifier, &info).await;

    let current = state.projects.get("proj-r3").expect("注册仍在");
    assert_eq!(
        current.container_info().unwrap().container_id,
        "pod-new",
        "受控重登记完成换代"
    );
    assert_eq!(
        current.status(),
        Some(&AgentStatus::Idle),
        "新实例 Idle 状态正常应用"
    );
    assert_eq!(
        current.last_activity(),
        stale_activity,
        "后台恢复与 Idle 观察都不得刷新用户活动时间"
    );
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

/// D1-查找失败用的恒错桩。
struct FailingLookupRuntime {
    queries: Mutex<Vec<(String, ServiceType)>>,
}

impl Default for FailingLookupRuntime {
    fn default() -> Self {
        Self {
            queries: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl AgentContainerRuntime for FailingLookupRuntime {
    async fn create_container(
        &self,
        _params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ContainerNotFound("probe".into()))
    }
    async fn get_container_info(
        &self,
        _project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        identifier: &str,
        service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        self.queries
            .lock()
            .unwrap()
            .push((identifier.to_string(), *service_type));
        Err(ContainerRuntimeError::ConnectionError(
            "runtime lookup unreachable".into(),
        ))
    }
    async fn stop_container(&self, _project_id: &str) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn is_container_running(&self, _project_id: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
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
impl WorkspaceRuntime for FailingLookupRuntime {}
#[async_trait]
impl UserAppDeploymentRuntime for FailingLookupRuntime {
    async fn list_deployments(
        &self,
    ) -> ContainerRuntimeResult<Vec<container_runtime_api::DeploymentStatus>> {
        Ok(vec![])
    }
}

/// FailingLookupRuntime 版本的 AppState 装配。
async fn identity_state_arc(
    runtime: Arc<FailingLookupRuntime>,
) -> (Arc<AppState>, tempfile::TempDir) {
    let adapter = ProjectAdapter::new("test-ns".to_string(), "cluster.local".to_string());
    let activity = Arc::new(app_manager::AppActivityRegistry::new(Duration::from_secs(
        300,
    )));
    let manager_config = AppManagerConfig {
        access_mode: AppAccessMode::Docker,
        ..AppManagerConfig::default()
    };
    let metadata_dir = tempfile::tempdir().expect("metadata directory");
    let metadata_store = rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
        &metadata_dir.path().join("userapp.turso.db"),
    )
    .await
    .expect("Turso metadata store");
    let metadata_store = Arc::new(metadata_store);
    let app_service: Arc<dyn app_manager::AppServiceTrait> = Arc::new(
        app_manager::service::AppService::new(
            manager_config,
            runtime.clone(),
            activity.clone(),
            None,
            metadata_store.clone(),
        )
        .await
        .expect("AppService 构造失败"),
    );
    let download_dir = tempfile::tempdir().expect("tempdir");
    let agent_download_manager =
        Arc::new(AgentDownloadManager::new(download_dir.path()).expect("下载管理器构造失败"));
    let (pod_created_tx, _) = broadcast::channel(32);
    let state = Arc::new(AppState {
        userapp_store: metadata_store.clone(),
        userapp_store_control: metadata_store,
        userapp_op_flight: Arc::new(
            crate::userapp_builder::shutdown_gate::OperationFlightGate::default(),
        ),
        userapp_recovery_handle: Arc::new(Mutex::new(None)),
        config: AppConfig::default(),
        projects: Arc::new(ProjectStoreBackend::Memory(Arc::new(adapter))),
        pingora_service: None,
        userapp_error_page: None,
        grpc_pool: Arc::new(GrpcChannelPool::new()),
        session_stream_registry: Arc::new(SessionStreamRegistry::new()),
        api_key_config: Arc::new(ArcSwap::from_pointee(ApiKeyAuthConfig::default())),
        pod_creating: Arc::new(DashMap::new()),
        pod_created_tx: Arc::new(pod_created_tx),
        container_prefix_rcoder: "dev-rcoder".to_string(),
        container_prefix_computer: "computer-agent-runner".to_string(),
        runtime,
        agent_download_manager,
        app_service,
        activity,
        cluster_domain: "cluster.local".to_string(),
    });
    (state, metadata_dir)
}

#[tokio::test]
async fn stalled_identity_lookup_has_total_budget() {
    let runtime = IdentityRuntime::new(None);
    runtime.stalled_lookup.store(true, Ordering::SeqCst);
    let (state, _dir) = identity_state(runtime).await;
    let info = registered(
        "budget-project",
        Some("budget-user"),
        ServiceType::ComputerAgentRunner,
        "old-pod",
        Some("old-workload"),
        "127.0.0.1",
    );
    let checker = ContainerStatusChecker::new(
        ContainerStatusCheckerConfig {
            query_timeout: Duration::from_millis(10),
            ..Default::default()
        },
        state,
    );
    let result = tokio::time::timeout(
        Duration::from_secs(4),
        checker.check_single_container("budget-user", &info),
    )
    .await;
    assert!(
        result.is_ok(),
        "a hung runtime lookup bypassed every connection/RPC timeout"
    );
    assert!(result.unwrap().is_err());
}

#[tokio::test]
async fn shutdown_cancels_a_stalled_identity_observation() {
    let runtime = IdentityRuntime::new(None);
    runtime.stalled_lookup.store(true, Ordering::SeqCst);
    let (state, _dir) = identity_state(runtime.clone()).await;
    let info = registered(
        "shutdown-project",
        Some("shutdown-user"),
        ServiceType::ComputerAgentRunner,
        "old-pod",
        Some("old-workload"),
        "127.0.0.1",
    );
    state
        .insert_project("shutdown-project".into(), info)
        .unwrap();
    let (shutdown, _receiver) = broadcast::channel(4);
    let worker = super::checker::start_container_status_checker(
        ContainerStatusCheckerConfig::default(),
        state,
        shutdown.clone(),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while runtime.queried() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("lookup must have started");
    shutdown.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), worker)
        .await
        .expect("shutdown must not wait for the I/O budget")
        .unwrap();
}
