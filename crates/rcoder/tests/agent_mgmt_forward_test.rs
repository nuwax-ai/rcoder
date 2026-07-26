//! Agent Management 转发层集成测试 (P0-4d)
//!
//! 端到端验证: HTTP body → chunk 拆分 → gRPC client streaming → mock gRPC server。
//! 复用 `tokio::net::TcpListener` 监听 127.0.0.1:0 自动分配端口,完全在内存中跑通,
//! 不依赖 Docker 或任何外部进程。

use async_trait::async_trait;
use bytes::Bytes;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntime, ContainerRuntimeError,
    ContainerRuntimeResult, UserAppDeploymentRuntime, WorkspaceRuntime, RuntimeContainerInfo,
};
use futures_util::stream::StreamExt;
use shared_types::ContainerBasicInfo;
use shared_types::ProjectAndContainerInfo;
use shared_types::ServiceType;
use shared_types::grpc::agent_mgmt_service_client::AgentMgmtServiceClient;
use shared_types::grpc::agent_mgmt_service_server::{AgentMgmtService, AgentMgmtServiceServer};
use shared_types::grpc::{
    AgentInstallStatus, CheckAgentRequest, CheckAgentResponse, GetAgentRequest, GetAgentResponse,
    InstallAgentRequest, InstallAgentResponse, InstallType, ListAgentsRequest, ListAgentsResponse,
    SystemInfo, UninstallAgentRequest, UninstallAgentResponse,
};
use shared_types::{InstallType as SharedInstallType, error_codes as ec};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use rcoder::grpc::GrpcChannelPool;
use rcoder::handler::utils::status_to_app_error;
use rcoder::handler::utils::{
    AgentMgmtForwardCtx, InstallAgentParams, check_agent, get_agent, install_agent, list_agents,
    uninstall_agent,
};

// === Mock ContainerRuntime ===

/// 不会调 Docker API 的桩实现,仅满足 trait 形状。
/// `find_container` 返回 `Ok(None)`,让 `get_realtime_container_ip` 走 fallback 分支。
#[allow(dead_code)] // 测试桩预留，待 agent_mgmt 转发相关测试补齐后启用
struct StubRuntime;

#[async_trait]
impl AgentContainerRuntime for StubRuntime {
    async fn create_container(
        &self,
        _params: ContainerCreateParams,
    ) -> ContainerRuntimeResult<ContainerBasicInfo> {
        Err(ContainerRuntimeError::ContainerNotFound("stub".into()))
    }
    async fn get_container_info(
        &self,
        _project_id: &str,
    ) -> ContainerRuntimeResult<Option<ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        _project_id: &str,
        _service_type: &ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        // 关键:返回 None 让 `get_realtime_container_ip` 走 fallback(用 ProjectAndContainerInfo 的 container_ip)
        Ok(None)
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

// 空 impl 块继承默认实现 → StubRuntime impl B+C → 自动 impl ContainerRuntime (super-trait bounds)
#[async_trait]
impl WorkspaceRuntime for StubRuntime {}
#[async_trait]
impl UserAppDeploymentRuntime for StubRuntime {}

#[allow(dead_code)]
fn stub_runtime() -> Arc<dyn ContainerRuntime> {
    Arc::new(StubRuntime)
}

// === Mock gRPC server ===

/// 记录所有收到的 InstallAgentRequest 元数据 + 数据字节数,
/// 让测试可以验证 chunk 拆分、metadata 透传、错误注入。
#[derive(Default, Debug)]
struct MockAgentMgmt {
    state: Mutex<MockState>,
}

#[derive(Default, Debug)]
struct MockState {
    /// 收到的所有 InstallAgentRequest(仅保留关键字段,避免日志爆炸)
    received_chunks: Vec<MockChunk>,
    /// 收到的 list_agents / get_agent 等 unary 请求计数
    list_calls: usize,
    get_calls: usize,
    uninstall_calls: usize,
    check_calls: usize,
    /// 模拟的 install_agent 错误返回(测试用)
    install_error: Option<String>,
}

#[derive(Debug, Clone)]
struct MockChunk {
    has_metadata: bool,
    data_len: usize,
}

impl MockAgentMgmt {
    #[allow(dead_code)] // 保留字段供将来扩展断言
    fn snapshot(&self) -> MockSnapshot {
        let s = self.state.lock().unwrap();
        MockSnapshot {
            chunks: s.received_chunks.clone(),
            list_calls: s.list_calls,
            get_calls: s.get_calls,
            uninstall_calls: s.uninstall_calls,
            check_calls: s.check_calls,
        }
    }

    fn set_install_error(&self, code: &str) {
        self.state.lock().unwrap().install_error = Some(code.to_string());
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // 部分字段在特定测试中读取
struct MockSnapshot {
    chunks: Vec<MockChunk>,
    list_calls: usize,
    get_calls: usize,
    uninstall_calls: usize,
    check_calls: usize,
}

#[async_trait]
impl AgentMgmtService for MockAgentMgmt {
    async fn list_agents(
        &self,
        _req: Request<ListAgentsRequest>,
    ) -> Result<Response<ListAgentsResponse>, Status> {
        self.state.lock().unwrap().list_calls += 1;
        Ok(Response::new(ListAgentsResponse {
            system_info: Some(SystemInfo {
                os: "linux".into(),
                arch: "amd64".into(),
                platform: "linux/amd64".into(),
            }),
            agents: vec![shared_types::grpc::AgentInfo {
                agent_id: "codex-acp".into(),
                install_type: InstallType::Npm as i32,
                status: AgentInstallStatus::Available as i32,
                version: Some("0.1.0".into()),
                binary_path: Some("/usr/local/bin/codex-acp".into()),
                installed_at: Some(0),
            }],
            total: 1,
            install_dir: "/root/.rcoder/agents".into(),
        }))
    }

    async fn install_agent(
        &self,
        request: Request<tonic::Streaming<InstallAgentRequest>>,
    ) -> Result<Response<InstallAgentResponse>, Status> {
        // 检查是否配置了错误注入
        let error_code = self.state.lock().unwrap().install_error.clone();
        if let Some(code) = error_code {
            return Err(Status::failed_precondition(format!(
                "{code}: forced failure"
            )));
        }

        let mut stream = request.into_inner();
        let mut total_data = 0usize;
        let mut agent_id = String::new();
        let mut chunks: Vec<MockChunk> = Vec::new();
        while let Some(item) = stream.next().await {
            let chunk = item?;
            let has_meta = chunk.metadata.is_some();
            total_data += chunk.data.len();
            if let Some(meta) = &chunk.metadata
                && agent_id.is_empty()
                && let Some(id) = &meta.agent_id
            {
                agent_id = id.clone();
            }
            chunks.push(MockChunk {
                has_metadata: has_meta,
                data_len: chunk.data.len(),
            });
        }
        // 记录到 state(便于后续断言)
        {
            let mut s = self.state.lock().unwrap();
            s.received_chunks.extend(chunks);
        }
        if agent_id.is_empty() {
            agent_id = "unknown".into();
        }
        Ok(Response::new(InstallAgentResponse {
            agent_id,
            status: AgentInstallStatus::Available as i32,
            binary_path: "/usr/local/bin/installed".into(),
            file_type: "executable".into(),
            file_count: Some(1),
            file_size: total_data as i64,
            version: Some("0.1.0".into()),
            source_url: None,
            action: "installed".into(),
            installed: true,
            previous_version: String::new(),
            platform: String::new(),
        }))
    }

    async fn uninstall_agent(
        &self,
        req: Request<UninstallAgentRequest>,
    ) -> Result<Response<UninstallAgentResponse>, Status> {
        self.state.lock().unwrap().uninstall_calls += 1;
        Ok(Response::new(UninstallAgentResponse {
            uninstalled: true,
            install_type: InstallType::Npm as i32,
            agent_id: req.into_inner().agent_id,
            removed_versions: vec![],
        }))
    }

    async fn check_agent(
        &self,
        _req: Request<CheckAgentRequest>,
    ) -> Result<Response<CheckAgentResponse>, Status> {
        self.state.lock().unwrap().check_calls += 1;
        Ok(Response::new(CheckAgentResponse::default()))
    }

    async fn get_agent(
        &self,
        req: Request<GetAgentRequest>,
    ) -> Result<Response<GetAgentResponse>, Status> {
        self.state.lock().unwrap().get_calls += 1;
        let id = req.into_inner().agent_id;
        if id == "missing" {
            return Ok(Response::new(GetAgentResponse {
                found: false,
                agent: None,
            }));
        }
        Ok(Response::new(GetAgentResponse {
            found: true,
            agent: Some(shared_types::grpc::AgentDetailInfo {
                agent_id: id,
                install_type: InstallType::Npm as i32,
                installed: true,
                status: AgentInstallStatus::Available as i32,
                version: Some("0.1.0".into()),
                version_check_supported: false,
                static_checks: None,
            }),
        }))
    }
}

/// 启动 mock tonic gRPC server,返回 (server_addr, mock handle, server task)
async fn start_mock_server() -> (SocketAddr, Arc<MockAgentMgmt>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind 127.0.0.1:0");
    let addr = listener.local_addr().expect("local_addr");
    let mock = Arc::new(MockAgentMgmt::default());

    let mock_for_server = mock.clone();
    let server_task = tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        let svc = AgentMgmtServiceServer::from_arc(mock_for_server);
        let result = Server::builder()
            .add_service(svc)
            .serve_with_incoming(incoming)
            .await;
        if let Err(e) = result {
            eprintln!("[mock_server] server error: {e}");
        }
    });
    (addr, mock, server_task)
}

/// 构造一个容器 IP 指向 mock server 的 `ProjectAndContainerInfo`。
///
/// 关键:container_name 不带任何业务前缀(不是 "rcoder-..." 或 "computer-..." 开头),
/// 但本测试用 `with_endpoint_override` 绕过 IP 解析,所以 container_name 实际无所谓。
fn make_project(addr: SocketAddr) -> ProjectAndContainerInfo {
    let mut p = ProjectAndContainerInfo::new("test-project".to_string());
    p.update_extended_from_request(
        Some(ContainerBasicInfo {
            container_id: "mock-id".into(),
            container_name: "mock-server".into(),
            container_ip: addr.ip().to_string(),
            internal_port: 0,
            external_port: 0,
            project_id: "test-project".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
            service_url: format!("http://{}", addr),
        }),
        None,
        None,
        None,
    );
    p
}

fn make_ctx(addr: SocketAddr) -> AgentMgmtForwardCtx {
    AgentMgmtForwardCtx::from_state(
        Arc::new(GrpcChannelPool::new()),
        "test-namespace".to_string(),
        "test.cluster.local".to_string(),
        "en-US",
    )
    .with_endpoint_override(addr)
}

// === 测试 ===

#[tokio::test]
async fn install_agent_streams_chunks_to_mock_server() {
    let (addr, mock, _server) = start_mock_server().await;
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    // 2.5 MB body → 期望: 1 metadata-only 首包 + 3 data chunks (1MB+1MB+0.5MB)
    let body = Bytes::from(vec![0xABu8; 2_500_000]);
    let expected_chunks = 1 + 2 + 1; // metadata + 1MB + 1MB + 0.5MB

    let params = InstallAgentParams {
        agent: shared_types::AgentIdentity {
            agent_id: "codex-acp".to_string(),
            command: "codex-acp".to_string(),
            args: vec!["--serve".to_string()],
            version: None,
        },
        install_type: SharedInstallType::Binary,
        source_url: None,
        npm_package: None,
        sha256: Some("deadbeef".to_string()),
        platforms: None,
        force: false,
    };

    let resp = install_agent(&ctx, &project, params, body)
        .await
        .expect("install_agent ok");

    assert_eq!(resp.agent_id, "codex-acp");
    assert_eq!(resp.file_size, 2_500_000);
    assert_eq!(resp.status, shared_types::AgentInstallStatus::Available);

    // 验证 mock server 收到正确数量的 chunk
    let snap = mock.snapshot();
    assert_eq!(
        snap.chunks.len(),
        expected_chunks,
        "expected {expected_chunks} chunks (1 metadata + 3 data), got {:?}",
        snap.chunks
    );
    // 第 1 个 chunk 必须是 metadata-only(data 为空)
    assert!(
        snap.chunks[0].has_metadata,
        "first chunk must carry metadata"
    );
    assert_eq!(snap.chunks[0].data_len, 0);
    // 后续 chunk 必须无 metadata,data 总和 = 2.5 MB
    let total_data: usize = snap.chunks.iter().map(|c| c.data_len).sum();
    assert_eq!(total_data, 2_500_000);
    for c in &snap.chunks[1..] {
        assert!(!c.has_metadata, "non-first chunks must not carry metadata");
    }
}

#[tokio::test]
async fn install_agent_url_mode_uses_single_metadata_chunk() {
    let (addr, mock, _server) = start_mock_server().await;
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    // URL 安装: body 为空,期望只发 1 个 metadata-only chunk
    let params = InstallAgentParams {
        agent: shared_types::AgentIdentity {
            agent_id: "remote-agent".to_string(),
            command: "remote-agent".to_string(),
            args: vec![],
            version: None,
        },
        install_type: SharedInstallType::Url,
        source_url: Some("https://example.com/agent.tar.gz".to_string()),
        npm_package: None,
        sha256: None,
        platforms: None,
        force: false,
    };
    let resp = install_agent(&ctx, &project, params, Bytes::new())
        .await
        .expect("install_agent url mode ok");
    assert_eq!(resp.agent_id, "remote-agent");

    let snap = mock.snapshot();
    assert_eq!(
        snap.chunks.len(),
        1,
        "URL install should send exactly 1 metadata chunk, got {:?}",
        snap.chunks
    );
    assert!(snap.chunks[0].has_metadata);
    assert_eq!(snap.chunks[0].data_len, 0);
}

#[tokio::test]
async fn list_agents_returns_parsed_response() {
    let (addr, _mock, _server) = start_mock_server().await;
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    let resp = list_agents(&ctx, &project).await.expect("list_agents ok");
    assert_eq!(resp.total, 1);
    assert_eq!(resp.agents.len(), 1);
    assert_eq!(resp.agents[0].agent_id, "codex-acp");
    assert_eq!(resp.agents[0].install_type, SharedInstallType::Npm);
    assert_eq!(
        resp.agents[0].status,
        shared_types::AgentInstallStatus::Available
    );
    assert_eq!(resp.system_info.os, "linux");
}

#[tokio::test]
async fn get_agent_returns_none_when_not_found() {
    let (addr, _mock, _server) = start_mock_server().await;
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    let resp = get_agent(&ctx, &project, "missing", None)
        .await
        .expect("get_agent ok");
    assert!(resp.is_none(), "missing agent should return None");
}

#[tokio::test]
async fn get_agent_returns_detail_when_found() {
    let (addr, _mock, _server) = start_mock_server().await;
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    let resp = get_agent(&ctx, &project, "codex-acp", None)
        .await
        .expect("get_agent ok")
        .expect("agent should be found");
    assert_eq!(resp.agent_id, "codex-acp");
    assert!(resp.installed);
}

#[tokio::test]
async fn uninstall_agent_forwards_request() {
    let (addr, mock, _server) = start_mock_server().await;
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    let resp = uninstall_agent(&ctx, &project, "codex-acp", None)
        .await
        .expect("uninstall ok");
    assert!(resp.uninstalled);
    assert_eq!(resp.agent_id, "codex-acp");
    let snap = mock.snapshot();
    assert_eq!(snap.uninstall_calls, 1);
}

#[tokio::test]
async fn check_agent_forwards_request() {
    let (addr, mock, _server) = start_mock_server().await;
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    let _ = check_agent(&ctx, &project, "codex-acp", None)
        .await
        .expect("check ok");
    let snap = mock.snapshot();
    assert_eq!(snap.check_calls, 1);
}

/// gRPC `Status` 携带业务码前缀时,`status_to_app_error` 应当还原为对应业务错误。
#[tokio::test]
async fn business_code_in_status_propagates_as_app_error() {
    // 注入 install_agent 错误(模拟 agent-runner 返回业务错误)
    let (addr, mock, _server) = start_mock_server().await;
    mock.set_install_error(ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED);
    let ctx = make_ctx(addr);
    let project = make_project(addr);

    let params = InstallAgentParams {
        agent: shared_types::AgentIdentity {
            agent_id: "codex-acp".to_string(),
            command: "codex-acp".to_string(),
            args: vec![],
            version: None,
        },
        install_type: SharedInstallType::Binary,
        source_url: None,
        npm_package: None,
        sha256: None,
        platforms: None,
        force: false,
    };
    let err = install_agent(&ctx, &project, params, Bytes::new())
        .await
        .expect_err("should fail when mock returns business error");

    if let shared_types::AppError::Structured { code, .. } = &err {
        assert_eq!(code, ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED);
    } else {
        panic!("expected Structured AppError, got {err:?}");
    }
}

/// `status_to_app_error` 单元覆盖(防止集成测试不直接走 unit 路径时漏掉)。
#[test]
fn status_to_app_error_handles_bare_code() {
    let s = Status::failed_precondition(ec::ERR_AGENT_MGMT_NOT_FOUND);
    let err = status_to_app_error(s);
    if let shared_types::AppError::Structured { code, .. } = &err {
        assert_eq!(code, ec::ERR_AGENT_MGMT_NOT_FOUND);
    } else {
        panic!("expected Structured, got {err:?}");
    }
}

/// 静默使用 AgentMgmtServiceClient 警告(测试不直接调它,但保留 import 便于扩展)
#[allow(dead_code)]
fn _silence_unused(_c: AgentMgmtServiceClient<tonic::transport::Channel>) {}
