//! gRPC AgentMgmtService 实现 (P0-1)
//!
//! 5 个 RPC:
//! - `ListAgents` — 列出已安装 agent(不含内置)
//! - `InstallAgent` — client streaming 上传二进制
//! - `UninstallAgent` — 卸载
//! - `CheckAgent` — 健康检查
//! - `GetAgent` — 查询单个 agent
//!
//! 错误:内部 `AgentMgmtError` 通过 `error_code()` 映射为业务错误码,转 `tonic::Status::internal(code, msg)`

use std::sync::Arc;

use bytes::Bytes;
use shared_types_grpc::{
    AgentDetailInfo as ProtoAgentDetailInfo, AgentInfo as ProtoAgentInfo, CheckAgentRequest,
    CheckAgentResponse, GetAgentRequest, GetAgentResponse, InstallAgentRequest,
    InstallAgentResponse, ListAgentsRequest, ListAgentsResponse, StaticCheckResult,
    UninstallAgentRequest, UninstallAgentResponse, agent_mgmt_service_server::AgentMgmtService,
};
use tonic::{Request, Response, Status};
use tracing::{error, instrument, warn};

use super::error::AgentMgmtResult;

use super::checker::AgentChecker;
use super::conversion;
use super::error::AgentMgmtError;
use super::installer::{binary_installer, npm_installer, url_installer};
use super::path_manager::PathManager;
use super::registry::AgentRegistry;
use super::uninstaller;

/// 根据可选 version 解析 manifest
///
/// - version 为 Some 且非空 → 查询指定版本
/// - version 为 None 或空 → 返回最新版本
fn resolve_manifest(
    registry: &AgentRegistry,
    agent_id: &str,
    version: Option<&str>,
) -> Option<super::installer::AgentManifest> {
    match version {
        Some(v) if !v.is_empty() => registry.get_version(agent_id, v),
        _ => registry.get(agent_id),
    }
}

/// gRPC 服务实现
pub struct AgentMgmtServiceImpl {
    pub registry: Arc<AgentRegistry>,
    pub path_manager: PathManager,
    pub lock_manager: Arc<super::InstallLockManager>,
}

impl AgentMgmtServiceImpl {
    pub fn new(registry: Arc<AgentRegistry>, path_manager: PathManager) -> Self {
        Self {
            registry,
            path_manager,
            lock_manager: Arc::new(super::InstallLockManager::new()),
        }
    }

    /// 业务错误 → gRPC Status(用 error_code 作为 message 前缀,便于客户端解析)
    fn to_status(e: AgentMgmtError) -> Status {
        let code = e.error_code();
        let msg = e.to_string();
        match code {
            shared_types::error_codes::ERR_AGENT_MGMT_NOT_FOUND => {
                Status::not_found(format!("{code}: {msg}"))
            }
            shared_types::error_codes::ERR_AGENT_MGMT_ALREADY_INSTALLED => {
                Status::already_exists(format!("{code}: {msg}"))
            }
            shared_types::error_codes::ERR_AGENT_MGMT_BUILTIN_PROTECTED => {
                Status::failed_precondition(format!("{code}: {msg}"))
            }
            shared_types::error_codes::ERR_AGENT_MGMT_INVALID_MANIFEST
            | shared_types::error_codes::ERR_AGENT_MGMT_INVALID_CHUNK
            | shared_types::error_codes::ERR_AGENT_MGMT_UNSUPPORTED_TYPE => {
                Status::invalid_argument(format!("{code}: {msg}"))
            }
            _ => Status::internal(format!("{code}: {msg}")),
        }
    }
}

#[tonic::async_trait]
impl AgentMgmtService for AgentMgmtServiceImpl {
    #[instrument(skip(self, _request))]
    async fn list_agents(
        &self,
        _request: Request<ListAgentsRequest>,
    ) -> Result<Response<ListAgentsResponse>, Status> {
        let manifests = self.registry.list();

        let system_info = conversion::system_info_to_proto(&shared_types::SystemInfo::current());
        let agents: Vec<ProtoAgentInfo> = manifests
            .iter()
            .map(conversion::manifest_to_proto_agent_info)
            .collect();
        let total = agents.len() as i32;

        Ok(Response::new(ListAgentsResponse {
            system_info: Some(system_info),
            agents,
            total,
            install_dir: self
                .path_manager
                .install_dir()
                .to_string_lossy()
                .to_string(),
        }))
    }

    #[instrument(skip(self, request))]
    async fn install_agent(
        &self,
        request: Request<tonic::Streaming<InstallAgentRequest>>,
    ) -> Result<Response<InstallAgentResponse>, Status> {
        // 先读首包,根据 install_type 分发到不同 installer
        let mut stream = request.into_inner();
        let first_chunk = match stream.message().await {
            Ok(Some(c)) => c,
            Ok(None) => return Err(Self::to_status(AgentMgmtError::StreamTruncated)),
            Err(e) => {
                return Err(Self::to_status(AgentMgmtError::InvalidChunk(format!(
                    "grpc stream error: {e}"
                ))));
            }
        };

        // 从首包取 metadata（clone 一次，然后用 take/destructure 避免二次 clone）
        let metadata = first_chunk.metadata.clone().ok_or_else(|| {
            Self::to_status(AgentMgmtError::InvalidChunk(
                "first chunk missing metadata".into(),
            ))
        })?;
        let shared_types_grpc::install_agent_request::Metadata {
            agent_id,
            command,
            args,
            sha256,
            install_type: install_type_opt,
            source_url,
            npm_package,
            version,
            platforms,
            force,
        } = metadata;

        let agent_id = agent_id.ok_or_else(|| {
            Self::to_status(AgentMgmtError::InvalidChunk(
                "metadata.agent_id missing".into(),
            ))
        })?;
        let command = command.ok_or_else(|| {
            Self::to_status(AgentMgmtError::InvalidChunk(
                "metadata.command missing".into(),
            ))
        })?;
        super::path_manager::validate_command(&command)
            .map_err(|e| Self::to_status(AgentMgmtError::InvalidManifest(e)))?;
        let sha256 = sha256.filter(|s| !s.is_empty());
        let install_type_i32 = install_type_opt.unwrap_or(0);
        let install_type = conversion::install_type_from_proto(install_type_i32);

        // 分发
        let result: AgentMgmtResult<InstallAgentResponse> = match install_type {
            shared_types::InstallType::Url => {
                // 新模式: version + platforms → install_with_version_check
                let ver_opt = version.as_deref().filter(|s| !s.is_empty());
                let json_opt = platforms.as_deref().filter(|s| !s.is_empty());
                if let (Some(ver), Some(json)) = (ver_opt, json_opt) {
                    let platforms: std::collections::HashMap<String, shared_types::PlatformEntry> =
                        serde_json::from_str(json).map_err(|e| {
                            Self::to_status(AgentMgmtError::InvalidChunk(format!(
                                "invalid platforms JSON: {e}"
                            )))
                        })?;
                    if !platforms.is_empty() {
                        let force = force.unwrap_or(false);
                        let params = url_installer::VersionCheckInstallParams {
                            lock_manager: &self.lock_manager,
                            registry: &self.registry,
                            path_manager: &self.path_manager,
                            agent_id: &agent_id,
                            command: &command,
                            args: &args,
                            version: ver,
                            platforms: &platforms,
                            force,
                        };
                        return url_installer::install_with_version_check(params)
                            .await
                            .map_err(Self::to_status)
                            .map(Response::new);
                    }
                }
                // 旧模式: 单个 source_url
                let url = match source_url {
                    Some(u) => u,
                    None => {
                        return Err(Self::to_status(AgentMgmtError::InvalidChunk(
                            "URL install requires source_url or platform_urls".into(),
                        )));
                    }
                };
                url_installer::install_from_url(
                    &self.registry,
                    &self.path_manager,
                    &agent_id,
                    &url,
                    &command,
                    &args,
                    sha256.as_deref(),
                )
                .await
            }
            shared_types::InstallType::Npm => {
                let pkg = match npm_package {
                    Some(p) => p,
                    None => {
                        return Err(Self::to_status(AgentMgmtError::InvalidChunk(
                            "NPM install requires npm_package".into(),
                        )));
                    }
                };
                npm_installer::install_from_npm(
                    &self.registry,
                    &self.path_manager,
                    &agent_id,
                    &pkg,
                    &command,
                )
                .await
            }
            _ => {
                // BINARY / Builtin:走 streaming 安装（metadata 已解析，直接传入）
                let first_data = Bytes::from(first_chunk.data);
                let prepared = binary_installer::StreamMetadata {
                    agent_id,
                    command,
                    args,
                    expected_sha256: sha256,
                };
                let pinned: binary_installer::IncomingStream = Box::pin(stream);
                binary_installer::install_from_prepared_stream(
                    &self.registry,
                    &self.path_manager,
                    prepared,
                    first_data,
                    pinned,
                )
                .await
            }
        };

        match result {
            Ok(resp) => Ok(Response::new(resp)),
            Err(e) => {
                warn!("[agent_mgmt] install_agent failed: {e}");
                Err(Self::to_status(e))
            }
        }
    }

    #[instrument(skip(self, request))]
    async fn uninstall_agent(
        &self,
        request: Request<UninstallAgentRequest>,
    ) -> Result<Response<UninstallAgentResponse>, Status> {
        let inner = request.into_inner();
        let agent_id = inner.agent_id;
        let version = inner.version;

        let removed =
            uninstaller::uninstall_with_version(&self.registry, &agent_id, version.as_deref())
                .await
                .map_err(|e| {
                    warn!("[agent_mgmt] uninstall_agent failed: {e}");
                    Self::to_status(e)
                })?;

        let first = removed.first().ok_or_else(|| {
            error!(agent_id = %agent_id, "[agent_mgmt] uninstall returned empty list");
            Status::internal("uninstall succeeded but no manifests returned")
        })?;

        let removed_versions: Vec<String> =
            removed.iter().filter_map(|m| m.version.clone()).collect();

        Ok(Response::new(UninstallAgentResponse {
            uninstalled: true,
            install_type: conversion::install_type_to_proto(first.install_type) as i32,
            agent_id,
            removed_versions,
        }))
    }

    #[instrument(skip(self, request))]
    async fn check_agent(
        &self,
        request: Request<CheckAgentRequest>,
    ) -> Result<Response<CheckAgentResponse>, Status> {
        let inner = request.into_inner();
        let manifest = resolve_manifest(&self.registry, &inner.agent_id, inner.version.as_deref());

        let checker = AgentChecker::new(self.path_manager.clone());
        let detail = checker.detail_info(manifest.as_ref());

        let system_info = conversion::system_info_to_proto(&shared_types::SystemInfo::current());
        Ok(Response::new(CheckAgentResponse {
            system_info: Some(system_info),
            agent: Some(detail_to_proto(&detail)),
        }))
    }

    #[instrument(skip(self, request))]
    async fn get_agent(
        &self,
        request: Request<GetAgentRequest>,
    ) -> Result<Response<GetAgentResponse>, Status> {
        let inner = request.into_inner();
        let manifest = resolve_manifest(&self.registry, &inner.agent_id, inner.version.as_deref());

        match manifest {
            Some(m) => {
                let checker = AgentChecker::new(self.path_manager.clone());
                let detail = checker.detail_info(Some(&m));
                Ok(Response::new(GetAgentResponse {
                    found: true,
                    agent: Some(detail_to_proto(&detail)),
                }))
            }
            None => Ok(Response::new(GetAgentResponse {
                found: false,
                agent: None,
            })),
        }
    }
}

/// shared `AgentDetailInfo` → proto
fn detail_to_proto(d: &shared_types::AgentDetailInfo) -> ProtoAgentDetailInfo {
    ProtoAgentDetailInfo {
        agent_id: d.agent_id.clone(),
        install_type: conversion::install_type_to_proto(d.install_type) as i32,
        installed: d.installed,
        status: d.status as i32,
        version: d.version.clone(),
        version_check_supported: d.version_check_supported,
        static_checks: Some(StaticCheckResult {
            file_exists: d.static_checks.file_exists,
            executable: d.static_checks.executable,
            in_path: d.static_checks.in_path,
        }),
    }
}

// 重新导出部分函数,避免上游使用 npm/url 时还要 import 多个子模块
