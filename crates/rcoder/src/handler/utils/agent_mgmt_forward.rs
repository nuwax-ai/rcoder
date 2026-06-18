//! Agent Management 转发层 (P0-4)
//!
//! 把 rcoder 收到的 `/agent-mgmt/*` HTTP 请求转发到对应项目的 agent_runner 容器内的
//! `AgentMgmtService` gRPC。容器定位复用 `chat_handler` 模式:
//!   1. `state.get_project(project_id)` 拿容器信息
//!   2. 实时 IP 解析(`get_realtime_container_ip`),失败回退到 `service_url`
//!   3. 通过 `GrpcChannelPool::get_mgmt_client` 获取/复用 gRPC Channel
//!
//! `InstallAgent` 是 client streaming:HTTP body 切成 1MB chunk + metadata 首包,
//! 再包装成 `Stream<Item = Result<InstallAgentRequest, Status>>` 调 gRPC。

use bytes::Bytes;
use container_runtime_api::ContainerRuntime;
use futures_util::stream;
use shared_types::ProjectAndContainerInfo;
use shared_types::grpc::agent_mgmt_service_client::AgentMgmtServiceClient;
use shared_types::grpc::{
    AgentDetailInfo as ProtoAgentDetailInfo, AgentInfo as ProtoAgentInfo,
    AgentInstallStatus as ProtoAgentInstallStatus, CheckAgentRequest,
    CheckAgentResponse as ProtoCheckAgentResponse, GetAgentRequest, GetAgentResponse,
    InstallAgentRequest, InstallAgentResponse as ProtoInstallAgentResponse,
    InstallType as ProtoInstallType, ListAgentsRequest as ProtoListAgentsRequest,
    ListAgentsResponse as ProtoListAgentsResponse, StaticCheckResult,
    SystemInfo as ProtoSystemInfo, UninstallAgentRequest,
    UninstallAgentResponse as ProtoUninstallResponse,
    install_agent_request::Metadata as InstallMetadata,
};
use shared_types::{
    AgentDetailInfo, AgentInfo, AgentInstallStatus, AppError, CheckAgentResponse,
    InstallAgentResponse, InstallType, ListAgentsResponse, SystemInfo, UninstallAgentResponse,
    error_codes as ec,
};
use std::sync::Arc;
use tonic::Status;
use tonic::transport::Channel;
use tracing::{debug, instrument, warn};

use crate::grpc::GrpcChannelPool;
use crate::handler::utils::grpc_addr::get_realtime_container_ip;

/// 1MB chunk size(与 `shared_types::UPLOAD_CHUNK_SIZE` 对齐)
const CHUNK_SIZE: usize = shared_types::UPLOAD_CHUNK_SIZE;

/// 转发层上下文(从 `AppState` 拆出,便于单测注入 mock)
#[derive(Clone)]
pub struct AgentMgmtForwardCtx {
    pub pool: Arc<GrpcChannelPool>,
    pub runtime: Arc<dyn ContainerRuntime>,
    pub rcoder_prefix: String,
    pub computer_prefix: String,
    /// 当前请求 locale(预留,目前未在转发层使用,保留供后续 i18n 错误消息注入)
    #[allow(dead_code)]
    pub locale: &'static str,
    /// 测试钩子:直接覆盖 gRPC endpoint。生产代码不会设置,`resolve_client`
    /// 会跳过 IP 解析直接使用此地址。仅供集成测试使用(随机端口 mock server)。
    #[doc(hidden)]
    pub endpoint_override: Option<String>,
}

/// InstallAgent 的 metadata 参数(独立参数,避免 install 三 endpoint 各自重复字段)
#[derive(Debug, Clone)]
pub struct InstallAgentParams {
    pub agent: shared_types::AgentIdentity,
    pub install_type: InstallType,
    pub source_url: Option<String>,
    pub npm_package: Option<String>,
    pub sha256: Option<String>,
    // === 多平台版本管理 ===
    pub platforms: Option<std::collections::HashMap<String, shared_types::PlatformEntry>>,
    /// 强制重新安装(取消正在进行的安装)
    pub force: bool,
}

impl AgentMgmtForwardCtx {
    /// 从 `AppState` 构造转发上下文
    pub fn from_state(
        pool: Arc<GrpcChannelPool>,
        runtime: Arc<dyn ContainerRuntime>,
        rcoder_prefix: String,
        computer_prefix: String,
        locale: &'static str,
    ) -> Self {
        Self {
            pool,
            runtime,
            rcoder_prefix,
            computer_prefix,
            locale,
            endpoint_override: None,
        }
    }

    /// 测试钩子:覆盖 gRPC endpoint。生产代码不应调用此方法。
    #[doc(hidden)]
    #[allow(dead_code)] // 仅集成测试使用,lib 编译单元看不到调用点
    pub fn with_endpoint_override(mut self, addr: std::net::SocketAddr) -> Self {
        self.endpoint_override = Some(format!("{}:{}", addr.ip(), addr.port()));
        self
    }

    /// 解析 project_id → 容器 gRPC 客户端
    ///
    /// 失败模式:
    /// - 项目不存在 → `ERR_PROJECT_NOT_FOUND` (404)
    /// - 容器不存在 → `ERR_AGENT_RUNNER_UNAVAILABLE` (503)
    /// - 实时 IP 解析失败 → `ERR_GRPC_ADDR_ERROR`
    /// - gRPC 连接失败 → `ERR_AGENT_RUNNER_UNAVAILABLE` (503)
    #[instrument(skip(self, project))]
    pub async fn resolve_client(
        &self,
        project: &ProjectAndContainerInfo,
    ) -> Result<AgentMgmtServiceClient<Channel>, AppError> {
        // 测试快捷路径:endpoint_override 直接使用(绕过 IP 解析)
        if let Some(addr) = &self.endpoint_override {
            return self.pool.get_mgmt_client(addr).await.map_err(|e| {
                warn!("[agent_mgmt_forward] gRPC connect failed: addr={addr}, err={e}");
                AppError::with_message(ec::ERR_AGENT_RUNNER_UNAVAILABLE, e.to_string())
            });
        }

        let container = project.container().ok_or_else(|| {
            AppError::with_message(
                ec::ERR_AGENT_RUNNER_UNAVAILABLE,
                format!("project {} has no container", project.project_id()),
            )
        })?;

        let ip = get_realtime_container_ip(
            &self.runtime,
            &container.container_name,
            &container.container_ip,
            &self.rcoder_prefix,
            &self.computer_prefix,
        )
        .await
        .map_err(|e| AppError::with_message(ec::ERR_GRPC_ADDR_ERROR, e))?;

        let addr = format!("{}:{}", ip, shared_types::GRPC_DEFAULT_PORT);

        self.pool.get_mgmt_client(&addr).await.map_err(|e| {
            warn!("[agent_mgmt_forward] gRPC connect failed: addr={addr}, err={e}");
            AppError::with_message(ec::ERR_AGENT_RUNNER_UNAVAILABLE, e.to_string())
        })
    }
}

/// gRPC `Status` → `AppError`
///
/// agent-runner 在 `agent_mgmt::grpc::to_status` 把业务错误码放在 message 前缀:
/// `format!("{code}: {msg}")`。我们解析前缀还原原始业务错误码,
/// 这样前端拿到的错误码与直接调 agent-runner HTTP 一致。
pub fn status_to_app_error(s: Status) -> AppError {
    let raw_msg = s.message().to_string();
    let (prefix, rest) = raw_msg.split_once(':').unwrap_or((&raw_msg, ""));
    let prefix = prefix.trim();
    let user_msg = rest.trim();
    if is_known_agent_mgmt_code(prefix) {
        if user_msg.is_empty() {
            AppError::from_code(prefix)
        } else {
            AppError::with_message(prefix, user_msg)
        }
    } else {
        // 未知前缀(网络/超时/容器离线等)
        AppError::with_message(ec::ERR_AGENT_RUNNER_UNAVAILABLE, raw_msg)
    }
}

fn is_known_agent_mgmt_code(code: &str) -> bool {
    // 20 个 agent_mgmt 业务码 + `ERR_INTERNAL_SERVER_ERROR`(agent_runner 在
    // I/O/JSON/Archive 内部错误时也会以同样 `"{code}: {msg}"` 格式回传,
    // 必须识别以避免被错误地降级为 ERR_AGENT_RUNNER_UNAVAILABLE / 503)。
    matches!(
        code,
        ec::ERR_AGENT_MGMT_NOT_FOUND
            | ec::ERR_AGENT_MGMT_ALREADY_INSTALLED
            | ec::ERR_AGENT_MGMT_INVALID_MANIFEST
            | ec::ERR_AGENT_MGMT_CHECKSUM_MISMATCH
            | ec::ERR_AGENT_MGMT_ARCHIVE_BOMB
            | ec::ERR_AGENT_MGMT_PATH_TRAVERSAL
            | ec::ERR_AGENT_MGMT_COMMAND_TIMEOUT
            | ec::ERR_AGENT_MGMT_INSTALL_FAILED
            | ec::ERR_AGENT_MGMT_UNINSTALL_FAILED
            | ec::ERR_AGENT_MGMT_CHECK_FAILED
            | ec::ERR_AGENT_MGMT_BINARY_TOO_LARGE
            | ec::ERR_AGENT_MGMT_UNSUPPORTED_TYPE
            | ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED
            | ec::ERR_AGENT_MGMT_STREAM_TRUNCATED
            | ec::ERR_AGENT_MGMT_DISK_FULL
            | ec::ERR_AGENT_MGMT_PERMISSION_DENIED
            | ec::ERR_AGENT_MGMT_UNKNOWN_AGENT
            | ec::ERR_AGENT_MGMT_INVALID_CHUNK
            | ec::ERR_AGENT_MGMT_PLATFORM_NOT_FOUND
            | ec::ERR_AGENT_MGMT_INVALID_VERSION
            | ec::ERR_INTERNAL_SERVER_ERROR
    )
}

/// shared InstallType → proto i32
fn install_type_to_proto_i32(t: InstallType) -> i32 {
    match t {
        InstallType::Builtin => ProtoInstallType::Builtin as i32,
        InstallType::Binary => ProtoInstallType::Binary as i32,
        InstallType::Npm => ProtoInstallType::Npm as i32,
        InstallType::Url => ProtoInstallType::Url as i32,
        // Unknown 是 shared 端 fail-safe(不会主动发给 agent_runner)
        InstallType::Unknown => ProtoInstallType::Binary as i32,
    }
}

// === Forward functions ===

/// 列出已安装 agent(不含内置)
#[instrument(skip(ctx, project))]
pub async fn list_agents(
    ctx: &AgentMgmtForwardCtx,
    project: &ProjectAndContainerInfo,
) -> Result<ListAgentsResponse, AppError> {
    let mut client = ctx.resolve_client(project).await?;
    let req = ProtoListAgentsRequest {};
    let resp: ProtoListAgentsResponse = client
        .list_agents(req)
        .await
        .map_err(status_to_app_error)?
        .into_inner();
    Ok(list_response_to_shared(resp))
}

/// 安装 agent(client streaming)
///
/// `body` 是 HTTP 收到的完整二进制 body(URL/NPM 安装时 body 为空)。
/// 拆 1MB chunk 推到 agent-runner。
#[instrument(skip(ctx, project, body, params), fields(agent_id = %params.agent.agent_id))]
pub async fn install_agent(
    ctx: &AgentMgmtForwardCtx,
    project: &ProjectAndContainerInfo,
    params: InstallAgentParams,
    body: Bytes,
) -> Result<InstallAgentResponse, AppError> {
    let mut client = ctx.resolve_client(project).await?;

    // 1. metadata-only 首包
    let first_chunk = InstallAgentRequest {
        metadata: Some(InstallMetadata {
            agent_id: Some(params.agent.agent_id.clone()),
            command: Some(params.agent.command.clone()),
            args: params.agent.args.clone(),
            sha256: params.sha256.clone(),
            install_type: Some(install_type_to_proto_i32(params.install_type)),
            source_url: params.source_url.clone(),
            npm_package: params.npm_package.clone(),
            version: params.agent.version.clone(),
            platforms: params
                .platforms
                .as_ref()
                .and_then(|p| serde_json::to_string(p).ok()),
            force: Some(params.force),
        }),
        data: vec![],
    };
    let total_bytes = body.len();
    debug!(
        "[agent_mgmt_forward] streaming install: agent_id={}, body_bytes={}",
        params.agent.agent_id, total_bytes
    );

    // 2. body 拆 chunk(URL/NPM 模式下 body 为空 → 只有一个首包)
    //    注意:必须用 `Vec<InstallAgentRequest>` 而非 `Vec<Result<_, _>>`,
    //    tonic `IntoStreamingRequest::Message` 是 `T::Item`,
    //    要求元素是 `InstallAgentRequest` 而非 `Result<...>`。
    let mut stream_items: Vec<InstallAgentRequest> =
        Vec::with_capacity(1 + (body.len() / CHUNK_SIZE) + 1);
    stream_items.push(first_chunk);
    for slice in body.chunks(CHUNK_SIZE) {
        stream_items.push(InstallAgentRequest {
            metadata: None,
            data: slice.to_vec(),
        });
    }
    let stream = stream::iter(stream_items);

    let resp: ProtoInstallAgentResponse = client
        .install_agent(stream)
        .await
        .map_err(status_to_app_error)?
        .into_inner();
    Ok(install_response_to_shared(resp))
}

/// 卸载 agent
#[instrument(skip(ctx, project))]
pub async fn uninstall_agent(
    ctx: &AgentMgmtForwardCtx,
    project: &ProjectAndContainerInfo,
    agent_id: &str,
    version: Option<&str>,
) -> Result<UninstallAgentResponse, AppError> {
    let mut client = ctx.resolve_client(project).await?;
    let req = UninstallAgentRequest {
        agent_id: agent_id.to_string(),
        version: version.map(String::from),
    };
    let resp: ProtoUninstallResponse = client
        .uninstall_agent(req)
        .await
        .map_err(status_to_app_error)?
        .into_inner();
    Ok(uninstall_response_to_shared(resp))
}

/// 健康检查指定 agent
#[instrument(skip(ctx, project))]
pub async fn check_agent(
    ctx: &AgentMgmtForwardCtx,
    project: &ProjectAndContainerInfo,
    agent_id: &str,
    version: Option<&str>,
) -> Result<CheckAgentResponse, AppError> {
    let mut client = ctx.resolve_client(project).await?;
    let req = CheckAgentRequest {
        agent_id: agent_id.to_string(),
        version: version.map(String::from),
    };
    let resp = client
        .check_agent(req)
        .await
        .map_err(status_to_app_error)?
        .into_inner();
    Ok(check_response_to_shared(resp))
}

/// 查询单个 agent 详情;未找到返回 `Ok(None)`
#[instrument(skip(ctx, project))]
pub async fn get_agent(
    ctx: &AgentMgmtForwardCtx,
    project: &ProjectAndContainerInfo,
    agent_id: &str,
    version: Option<&str>,
) -> Result<Option<AgentDetailInfo>, AppError> {
    let mut client = ctx.resolve_client(project).await?;
    let req = GetAgentRequest {
        agent_id: agent_id.to_string(),
        version: version.map(String::from),
    };
    let resp: GetAgentResponse = client
        .get_agent(req)
        .await
        .map_err(status_to_app_error)?
        .into_inner();
    if !resp.found {
        return Ok(None);
    }
    Ok(resp.agent.map(agent_detail_to_shared))
}

// === proto → shared 转换 ===

fn list_response_to_shared(p: ProtoListAgentsResponse) -> ListAgentsResponse {
    ListAgentsResponse {
        system_info: p.system_info.map(system_info_to_shared).unwrap_or_default(),
        agents: p.agents.into_iter().map(agent_info_to_shared).collect(),
        total: p.total.max(0) as usize,
        install_dir: p.install_dir,
    }
}

fn install_response_to_shared(p: ProtoInstallAgentResponse) -> InstallAgentResponse {
    use std::str::FromStr;
    InstallAgentResponse {
        agent_id: p.agent_id,
        status: agent_install_status_from_proto_i32(p.status),
        binary_path: p.binary_path,
        file_type: p.file_type,
        file_count: p.file_count.filter(|n| *n > 0).map(|n| n as usize),
        file_size: p.file_size.max(0) as u64,
        version: p.version,
        source_url: p.source_url,
        action: shared_types::InstallAction::from_str(p.action.as_str()).ok(),
        installed: p.installed,
        previous_version: if p.previous_version.is_empty() {
            None
        } else {
            Some(p.previous_version)
        },
        platform: if p.platform.is_empty() {
            None
        } else {
            Some(p.platform)
        },
    }
}

fn uninstall_response_to_shared(p: ProtoUninstallResponse) -> UninstallAgentResponse {
    UninstallAgentResponse {
        uninstalled: p.uninstalled,
        install_type: install_type_from_proto_i32(p.install_type),
        agent_id: p.agent_id,
        removed_versions: p.removed_versions,
    }
}

fn check_response_to_shared(p: ProtoCheckAgentResponse) -> CheckAgentResponse {
    CheckAgentResponse {
        system_info: p.system_info.map(system_info_to_shared).unwrap_or_default(),
        agent: p.agent.map(agent_detail_to_shared).unwrap_or_default(),
    }
}

fn system_info_to_shared(p: ProtoSystemInfo) -> SystemInfo {
    SystemInfo {
        os: p.os,
        arch: p.arch,
        platform: p.platform,
    }
}

fn agent_info_to_shared(p: ProtoAgentInfo) -> AgentInfo {
    AgentInfo {
        agent_id: p.agent_id,
        install_type: install_type_from_proto_i32(p.install_type),
        status: agent_install_status_from_proto_i32(p.status),
        version: p.version,
        binary_path: p.binary_path,
        installed_at: p.installed_at,
    }
}

fn agent_detail_to_shared(p: ProtoAgentDetailInfo) -> AgentDetailInfo {
    AgentDetailInfo {
        agent_id: p.agent_id,
        install_type: install_type_from_proto_i32(p.install_type),
        installed: p.installed,
        status: agent_install_status_from_proto_i32(p.status),
        version: p.version,
        version_check_supported: p.version_check_supported,
        static_checks: p
            .static_checks
            .map(|s| shared_types::StaticCheckResult {
                file_exists: s.file_exists,
                executable: s.executable,
                in_path: s.in_path,
            })
            .unwrap_or_default(),
    }
}

fn install_type_from_proto_i32(v: i32) -> InstallType {
    match ProtoInstallType::try_from(v).ok() {
        Some(ProtoInstallType::Builtin) => InstallType::Builtin,
        Some(ProtoInstallType::Binary) => InstallType::Binary,
        Some(ProtoInstallType::Npm) => InstallType::Npm,
        Some(ProtoInstallType::Url) => InstallType::Url,
        // Unspecified(0) 或未知值 → 退回 Binary
        _ => InstallType::Binary,
    }
}

fn agent_install_status_from_proto_i32(v: i32) -> AgentInstallStatus {
    match ProtoAgentInstallStatus::try_from(v).ok() {
        Some(ProtoAgentInstallStatus::Available) => AgentInstallStatus::Available,
        Some(ProtoAgentInstallStatus::Broken) => AgentInstallStatus::Broken,
        Some(ProtoAgentInstallStatus::NotInstalled) => AgentInstallStatus::NotInstalled,
        // 未知值(包括 proto 新版本新增的)
        _ => AgentInstallStatus::NotInstalled,
    }
}

#[allow(dead_code)]
fn _ensure_used(_: &StaticCheckResult, _: &ProtoInstallType, _: &ProtoAgentInstallStatus) {}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::error_codes as ec;

    /// status_to_app_error:agent-runner 在 message 前缀放了业务错误码,
    /// 我们要能正确还原。这样前端拿到的错误码与直接调 agent-runner HTTP 一致。
    #[test]
    fn status_to_app_error_unwraps_business_code_prefix() {
        let s = Status::not_found(format!(
            "{}: agent x not found",
            ec::ERR_AGENT_MGMT_NOT_FOUND
        ));
        let err = status_to_app_error(s);
        if let AppError::Structured {
            code,
            internal_message,
            ..
        } = &err
        {
            assert_eq!(code, ec::ERR_AGENT_MGMT_NOT_FOUND);
            assert_eq!(internal_message.as_deref(), Some("agent x not found"));
        } else {
            panic!("expected Structured error, got {err:?}");
        }
    }

    /// 没有业务码前缀(裸 gRPC 错误)→ 走 ERR_AGENT_RUNNER_UNAVAILABLE
    #[test]
    fn status_to_app_error_uses_unavailable_for_unknown_prefix() {
        let s = Status::unavailable("connection refused");
        let err = status_to_app_error(s);
        if let AppError::Structured {
            code,
            internal_message,
            ..
        } = &err
        {
            assert_eq!(code, ec::ERR_AGENT_RUNNER_UNAVAILABLE);
            assert!(internal_message.is_some());
        } else {
            panic!("expected Structured error, got {err:?}");
        }
    }

    /// ERR_INTERNAL_SERVER_ERROR 业务码(agent_runner I/O/JSON/Archive 错误用)
    /// 必须被识别并保留,不能降级为 ERR_AGENT_RUNNER_UNAVAILABLE。
    /// 修复:之前这个码会被错误地降级,导致 HTTP 状态从 500 变 503。
    #[test]
    fn status_to_app_error_preserves_internal_server_error_code() {
        let s = Status::internal(format!(
            "{}: io: file not found",
            ec::ERR_INTERNAL_SERVER_ERROR
        ));
        let err = status_to_app_error(s);
        if let AppError::Structured {
            code,
            internal_message,
            ..
        } = &err
        {
            assert_eq!(
                code,
                ec::ERR_INTERNAL_SERVER_ERROR,
                "ERR_INTERNAL_SERVER_ERROR must be preserved, not wrapped as ERR_AGENT_RUNNER_UNAVAILABLE"
            );
            // internal_message 应包含原始 io 错误
            let msg = internal_message.as_deref().unwrap_or("");
            assert!(
                msg.contains("io: file not found"),
                "internal_message should preserve original error, got: {msg}"
            );
        } else {
            panic!("expected Structured error, got {err:?}");
        }
    }

    /// 单条业务错误码(无 message 后缀)→ 还原为纯 code,无 user message
    #[test]
    fn status_to_app_error_with_bare_code_keeps_no_message() {
        let s = Status::failed_precondition(ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED);
        let err = status_to_app_error(s);
        if let AppError::Structured {
            code,
            internal_message,
            i18n_key,
        } = &err
        {
            assert_eq!(code, ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED);
            assert!(internal_message.is_none());
            assert!(i18n_key.is_none());
        } else {
            panic!("expected Structured error, got {err:?}");
        }
    }

    /// install_type 转换:Binary 走 i32 2(对齐 proto)
    #[test]
    fn install_type_to_proto_i32_roundtrip() {
        for t in [
            InstallType::Builtin,
            InstallType::Binary,
            InstallType::Npm,
            InstallType::Url,
        ] {
            let v = install_type_to_proto_i32(t);
            let back = install_type_from_proto_i32(v);
            assert_eq!(back, t);
        }
    }

    /// agent_install_status 转换:每个 status 都正确对应
    #[test]
    fn agent_install_status_roundtrip() {
        for s in [
            AgentInstallStatus::Available,
            AgentInstallStatus::Broken,
            AgentInstallStatus::NotInstalled,
        ] {
            let v = match s {
                AgentInstallStatus::Available => 1, // AgentInstallStatus::Available
                AgentInstallStatus::Broken => 2,
                AgentInstallStatus::NotInstalled => 0,
                _ => unreachable!(),
            };
            let back = agent_install_status_from_proto_i32(v);
            assert_eq!(back, s);
        }
    }

    /// list_response_to_shared 保留 system_info / total / install_dir,正确转换 agents
    #[test]
    fn list_response_to_shared_basic() {
        let proto_resp = ProtoListAgentsResponse {
            system_info: Some(ProtoSystemInfo {
                os: "linux".into(),
                arch: "amd64".into(),
                platform: "linux/amd64".into(),
            }),
            agents: vec![ProtoAgentInfo {
                agent_id: "codex-acp".into(),
                install_type: ProtoInstallType::Npm as i32,
                status: ProtoAgentInstallStatus::Available as i32,
                version: Some("0.1.0".into()),
                binary_path: Some("/bin/x".into()),
                installed_at: Some(12345),
            }],
            total: 1,
            install_dir: "/tmp/agents".into(),
        };
        let shared = list_response_to_shared(proto_resp);
        assert_eq!(shared.agents.len(), 1);
        assert_eq!(shared.agents[0].agent_id, "codex-acp");
        assert_eq!(shared.agents[0].install_type, InstallType::Npm);
        assert_eq!(shared.agents[0].status, AgentInstallStatus::Available);
        assert_eq!(shared.total, 1);
        assert_eq!(shared.install_dir, "/tmp/agents");
        assert_eq!(shared.system_info.os, "linux");
    }

    /// install_response_to_shared:file_count 缺失 → None,file_size 负数 → 0
    #[test]
    fn install_response_to_shared_normalises_missing_fields() {
        let proto = ProtoInstallAgentResponse {
            agent_id: "x".into(),
            status: ProtoAgentInstallStatus::Available as i32,
            binary_path: "/b".into(),
            file_type: "executable".into(),
            file_count: Some(3),
            file_size: -1,
            version: None,
            source_url: None,
            action: "installed".into(),
            installed: true,
            previous_version: String::new(),
            platform: String::new(),
        };
        let s = install_response_to_shared(proto);
        assert_eq!(s.file_size, 0);
        assert_eq!(s.file_count, Some(3));
    }

    /// 验证 18 个业务错误码前缀 + ERR_INTERNAL_SERVER_ERROR 都能被 is_known_agent_mgmt_code 识别
    #[test]
    fn all_known_business_codes_are_recognised() {
        let known = [
            ec::ERR_AGENT_MGMT_NOT_FOUND,
            ec::ERR_AGENT_MGMT_ALREADY_INSTALLED,
            ec::ERR_AGENT_MGMT_INVALID_MANIFEST,
            ec::ERR_AGENT_MGMT_CHECKSUM_MISMATCH,
            ec::ERR_AGENT_MGMT_ARCHIVE_BOMB,
            ec::ERR_AGENT_MGMT_PATH_TRAVERSAL,
            ec::ERR_AGENT_MGMT_COMMAND_TIMEOUT,
            ec::ERR_AGENT_MGMT_INSTALL_FAILED,
            ec::ERR_AGENT_MGMT_UNINSTALL_FAILED,
            ec::ERR_AGENT_MGMT_CHECK_FAILED,
            ec::ERR_AGENT_MGMT_BINARY_TOO_LARGE,
            ec::ERR_AGENT_MGMT_UNSUPPORTED_TYPE,
            ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED,
            ec::ERR_AGENT_MGMT_STREAM_TRUNCATED,
            ec::ERR_AGENT_MGMT_DISK_FULL,
            ec::ERR_AGENT_MGMT_PERMISSION_DENIED,
            ec::ERR_AGENT_MGMT_UNKNOWN_AGENT,
            ec::ERR_AGENT_MGMT_INVALID_CHUNK,
            ec::ERR_AGENT_MGMT_PLATFORM_NOT_FOUND,
            ec::ERR_AGENT_MGMT_INVALID_VERSION,
            // agent_runner 在 I/O/JSON/Archive 内部错误时也走同 `"{code}: {msg}"` 格式,
            // 必须识别以避免被错误地降级为 ERR_AGENT_RUNNER_UNAVAILABLE
            ec::ERR_INTERNAL_SERVER_ERROR,
        ];
        for c in known {
            assert!(is_known_agent_mgmt_code(c), "missing: {c}");
        }
        // ERR_PROJECT_NOT_FOUND / ERR_AGENT_RUNNER_UNAVAILABLE 不在这里
        // (它们是 rcoder 转发层专用,不是 agent-runner 业务码)
        assert!(!is_known_agent_mgmt_code(ec::ERR_PROJECT_NOT_FOUND));
        assert!(!is_known_agent_mgmt_code(ec::ERR_AGENT_RUNNER_UNAVAILABLE));
    }
}
