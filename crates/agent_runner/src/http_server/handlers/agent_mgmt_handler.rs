//! Agent Management HTTP Routes (P0-1)
//!
//! 7 个 HTTP 端点(全部 POST,与 rcoder 转发层保持一致):
//! - POST /agent-mgmt/agents/list
//! - POST /agent-mgmt/agents/get
//! - POST /agent-mgmt/agents/check
//! - POST /agent-mgmt/agents/install (binary streaming)
//! - POST /agent-mgmt/agents/install-from-url
//! - POST /agent-mgmt/agents/install-from-npm
//! - POST /agent-mgmt/agents/uninstall

use std::sync::Arc;

use axum::{
    Json,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use shared_types::HttpResult;
use shared_types::error_codes as ec;
use tracing::{instrument, warn};

use crate::agent_mgmt::conversion;
use crate::agent_mgmt::error::AgentMgmtError;
use crate::agent_mgmt::{
    AgentMgmtResult, AgentRegistry, InstallLockManager, PathManager, checker::AgentChecker,
    installer, uninstaller,
};
use shared_types::{InstallType, ListAgentsRequest};

/// 根据可选 version 解析 manifest
fn resolve_manifest(
    registry: &AgentRegistry,
    agent_id: &str,
    version: Option<&str>,
) -> Option<installer::AgentManifest> {
    match version {
        Some(v) if !v.is_empty() => registry.get_version(agent_id, v),
        _ => registry.get(agent_id),
    }
}

/// HTTP 应用状态扩展:把 agent_mgmt 所需依赖打包成一个 state
#[derive(Clone)]
pub struct AgentMgmtHttpState {
    pub registry: Arc<AgentRegistry>,
    pub path_manager: PathManager,
    pub lock_manager: Arc<InstallLockManager>,
}

impl AgentMgmtHttpState {
    pub fn new(registry: Arc<AgentRegistry>, path_manager: PathManager) -> Self {
        Self {
            registry,
            path_manager,
            lock_manager: Arc::new(InstallLockManager::new()),
        }
    }
}

/// 把 `AgentMgmtError` 转换为 HTTP 状态码 + 业务错误码
fn error_to_response(e: AgentMgmtError) -> Response {
    let code = e.error_code();
    let status = match code {
        ec::ERR_AGENT_MGMT_NOT_FOUND => StatusCode::NOT_FOUND,
        ec::ERR_AGENT_MGMT_ALREADY_INSTALLED | ec::ERR_AGENT_MGMT_INSTALL_CANCELLED => {
            StatusCode::CONFLICT
        }
        ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED => StatusCode::FORBIDDEN,
        ec::ERR_AGENT_MGMT_INVALID_MANIFEST
        | ec::ERR_AGENT_MGMT_INVALID_CHUNK
        | ec::ERR_AGENT_MGMT_PLATFORM_NOT_FOUND
        | ec::ERR_AGENT_MGMT_INVALID_VERSION => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let result: HttpResult<()> = HttpResult::error(code, &e.to_string());
    (status, Json(result)).into_response()
}

/// 1. POST /agent-mgmt/agents/list
///
/// 列出容器内所有已安装(含内置)agent 的元信息。
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/list",
    operation_id = "list_agents",
    summary = "列出已安装的 agent",
    description = "查询 agent_runner 注册表,返回所有已安装(含内置)agent 的元信息列表。",
    request_body = ListAgentsRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<shared_types::ListAgentsResponse>),
        (status = 500, description = "agent_runner 内部错误"),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn list_agents(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<ListAgentsRequest>,
) -> Response {
    let manifests = state.registry.list();
    let system_info = shared_types::SystemInfo::current();
    let agents: Vec<shared_types::AgentInfo> = manifests
        .iter()
        .map(conversion::manifest_to_shared_agent_info)
        .collect();
    let total = agents.len();
    let resp = shared_types::ListAgentsResponse {
        system_info,
        agents,
        total,
        install_dir: state
            .path_manager
            .install_dir()
            .to_string_lossy()
            .to_string(),
    };
    Json(HttpResult::success(resp)).into_response()
}

/// 2. POST /agent-mgmt/agents/install (binary streaming)
///
/// Body 形如:
/// ```text
/// --boundary
/// Content-Disposition: form-data; name="metadata"
///
/// {"agent_id":"x","command":"x","install_type":"BINARY"}
/// --boundary
/// Content-Disposition: form-data; name="data"; filename="x.bin"
///
/// <binary bytes>
/// --boundary--
/// ```
/// 这里为了简化,**只支持压缩包(tar.gz/zip)** + JSON header(`X-Agent-Metadata`)的简化协议:
/// - Header `X-Agent-Metadata`: JSON 字符串 `{agent_id, command, install_type, ...}`
/// - Body: raw binary
#[instrument(skip(state, headers, body))]
pub async fn install_agent(
    State(state): State<AgentMgmtHttpState>,
    headers: axum::http::HeaderMap,
    body: Bytes,
) -> Response {
    let metadata_str = match headers
        .get("x-agent-metadata")
        .and_then(|v| v.to_str().ok())
    {
        Some(s) => s.to_string(),
        None => {
            return error_to_response(AgentMgmtError::InvalidChunk(
                "X-Agent-Metadata header missing".into(),
            ));
        }
    };
    let meta: serde_json::Value = match serde_json::from_str(&metadata_str) {
        Ok(v) => v,
        Err(e) => {
            return error_to_response(AgentMgmtError::InvalidChunk(format!(
                "X-Agent-Metadata invalid JSON: {e}"
            )));
        }
    };
    // agent 字段从嵌套的 "agent" 子对象提取
    let agent_obj = meta.get("agent").unwrap_or(&serde_json::Value::Null);
    let agent_id = agent_obj
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let command = agent_obj
        .get("command")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let version = agent_obj
        .get("version")
        .and_then(|v| v.as_str())
        .map(String::from);
    let meta_args: Vec<String> = agent_obj
        .get("args")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    // install_type / source_url / npm_package / sha256 仍在顶层
    let install_type_str = meta
        .get("install_type")
        .and_then(|v| v.as_str())
        .unwrap_or("BINARY");
    let install_type = match install_type_str {
        "BINARY" | "binary" => InstallType::Binary,
        "URL" | "url" => InstallType::Url,
        "NPM" | "npm" => InstallType::Npm,
        // 未知值不能静默回退 Binary：调用方实际按 URL/NPM 语义发的 body 会被
        // 当压缩包解析, 报"only tar.gz and zip archives are supported"误导排障
        other => {
            return error_to_response(AgentMgmtError::InvalidChunk(format!(
                "unknown install_type: {other:?} (expected BINARY/URL/NPM)"
            )));
        }
    };
    let source_url = meta
        .get("source_url")
        .and_then(|v| v.as_str())
        .map(String::from);
    let npm_package = meta
        .get("npm_package")
        .and_then(|v| v.as_str())
        .map(String::from);
    let sha256 = meta
        .get("sha256")
        .and_then(|v| v.as_str())
        .map(String::from);

    let result: AgentMgmtResult<shared_types_grpc::InstallAgentResponse> = match install_type {
        InstallType::Url => {
            // 新模式: version + platforms → install_with_version_check
            // metadata JSON 中 platforms 是嵌套对象(不是字符串)
            let ver_opt = version.as_deref().filter(|s| !s.is_empty());
            let plat_opt = meta.get("platforms").filter(|v| !v.is_null());
            if let (Some(ver), Some(platforms_val)) = (ver_opt, plat_opt) {
                let platforms: std::collections::HashMap<String, shared_types::PlatformEntry> =
                    match serde_json::from_value(platforms_val.clone()) {
                        Ok(p) => p,
                        Err(e) => {
                            return error_to_response(AgentMgmtError::InvalidChunk(format!(
                                "invalid platforms field: {e}"
                            )));
                        }
                    };
                if !platforms.is_empty() {
                    let force = meta.get("force").and_then(|v| v.as_bool()).unwrap_or(false);
                    let params = installer::url_installer::VersionCheckInstallParams {
                        lock_manager: &state.lock_manager,
                        registry: &state.registry,
                        path_manager: &state.path_manager,
                        agent_id: &agent_id,
                        command: &command,
                        args: &meta_args,
                        version: ver,
                        platforms: &platforms,
                        force,
                    };
                    return installer::url_installer::install_with_version_check(params)
                        .await
                        .map(|r| {
                            let shared = conversion::install_response_to_shared(&r);
                            Json(HttpResult::success(shared)).into_response()
                        })
                        .unwrap_or_else(error_to_response);
                }
            }
            // 旧模式: 单个 source_url
            let url = match source_url {
                Some(u) => u,
                None => {
                    return error_to_response(AgentMgmtError::InvalidChunk(
                        "URL install requires source_url or platform_urls".into(),
                    ));
                }
            };
            installer::url_installer::install_from_url(
                &state.registry,
                &state.path_manager,
                &agent_id,
                &url,
                &command,
                &meta_args,
                sha256.as_deref(),
            )
            .await
        }
        InstallType::Npm => {
            let pkg = match npm_package {
                Some(p) => p,
                None => {
                    return error_to_response(AgentMgmtError::InvalidChunk(
                        "NPM install requires npm_package".into(),
                    ));
                }
            };
            installer::npm_installer::install_from_npm(
                &state.registry,
                &state.path_manager,
                &agent_id,
                &pkg,
                &command,
            )
            .await
        }
        _ => {
            installer::binary_installer::install_from_bytes(
                &state.registry,
                &state.path_manager,
                installer::binary_installer::InstallBytesParams {
                    agent_id: &agent_id,
                    command: &command,
                    args: &meta_args,
                    expected_sha256: sha256.as_deref(),
                    install_type: InstallType::Binary,
                    bytes: body,
                    version: None,
                    source: None,
                },
            )
            .await
        }
    };

    match result {
        Ok(r) => {
            let shared = conversion::install_response_to_shared(&r);
            Json(HttpResult::success(shared)).into_response()
        }
        Err(e) => {
            warn!("[agent_mgmt_http] install failed: {e}");
            error_to_response(e)
        }
    }
}

/// 3. POST /agent-mgmt/agents/install-from-url (多平台 + 版本管理)
///
/// 根据 `platforms` 映射自动选择匹配当前系统的下载 URL，
/// 支持版本比较实现幂等安装（已安装则跳过）。
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/install-from-url",
    operation_id = "install_from_url",
    summary = "从 URL 下载并安装 agent(多平台+版本管理)",
    description = "根据 platforms 多平台 URL + version 版本号,agent-runner 自动判断是否需要下载安装(幂等)。platforms key 格式为 {os}-{arch},如 linux-x86_64、darwin-arm64。",
    request_body = shared_types::InstallFromUrlRequest,
    responses(
        (status = 200, description = "安装/更新/跳过", body = HttpResult<shared_types::InstallAgentResponse>),
        (status = 400, description = "参数错误(agent_id/command/version/platforms 缺失或格式错误)"),
        (status = 400, description = "agent-runner 业务错误(platform 不匹配、version 无效、下载失败等)"),
        (status = 500, description = "agent_runner 内部错误"),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state, req))]
pub async fn install_from_url(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::InstallFromUrlRequest>,
) -> Response {
    let params = installer::url_installer::VersionCheckInstallParams {
        lock_manager: &state.lock_manager,
        registry: &state.registry,
        path_manager: &state.path_manager,
        agent_id: &req.agent.agent_id,
        command: &req.agent.command,
        args: &req.agent.args,
        version: req.agent.version.as_deref().unwrap_or(""),
        platforms: &req.platforms,
        force: req.force,
    };
    match installer::url_installer::install_with_version_check(params).await {
        Ok(r) => {
            let shared = conversion::install_response_to_shared(&r);
            Json(HttpResult::success(shared)).into_response()
        }
        Err(e) => error_to_response(e),
    }
}

/// 4. POST /agent-mgmt/agents/install-from-npm
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/install-from-npm",
    request_body = shared_types::InstallFromPackageManagerRequest,
    responses(
        (status = 200, description = "安装成功", body = HttpResult<shared_types::InstallAgentResponse>),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state, req))]
pub async fn install_from_npm(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::InstallFromPackageManagerRequest>,
) -> Response {
    if req.agent.command.is_empty() {
        return error_to_response(AgentMgmtError::InvalidManifest(
            "command is required for NPM install".into(),
        ));
    }
    match installer::npm_installer::install_from_npm(
        &state.registry,
        &state.path_manager,
        &req.agent.agent_id,
        &req.package,
        &req.agent.command,
    )
    .await
    {
        Ok(r) => {
            let shared = conversion::install_response_to_shared(&r);
            Json(HttpResult::success(shared)).into_response()
        }
        Err(e) => error_to_response(e),
    }
}

/// 5. POST /agent-mgmt/agents/uninstall
///
/// 卸载 agent 并清理注册表。内置 agent 受保护，卸载返回 403。
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/uninstall",
    operation_id = "uninstall_agent",
    summary = "卸载 agent",
    description = "删除 agent 目录并清理注册表,内置 agent 受保护(返回 403)。",
    request_body = shared_types::UninstallAgentRequest,
    responses(
        (status = 200, description = "卸载成功", body = HttpResult<shared_types::UninstallAgentResponse>),
        (status = 400, description = "参数错误(agent_id 缺失)"),
        (status = 403, description = "内置 agent 受保护"),
        (status = 404, description = "agent 不存在"),
        (status = 500, description = "agent_runner 内部错误"),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn uninstall_agent(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::UninstallAgentRequest>,
) -> Response {
    let result = async {
        let removed = uninstaller::uninstall_with_version(
            &state.registry,
            &req.agent_id,
            req.version.as_deref(),
        )
        .await?;

        let first = removed.first().ok_or_else(|| {
            tracing::error!(agent_id = %req.agent_id, "[agent_mgmt] uninstall returned empty list");
            AgentMgmtError::InstallFailed(
                "uninstall succeeded but no manifests returned".to_string(),
            )
        })?;

        let removed_versions: Vec<String> =
            removed.iter().filter_map(|m| m.version.clone()).collect();

        Ok(shared_types::UninstallAgentResponse {
            uninstalled: true,
            install_type: first.install_type,
            agent_id: req.agent_id,
            removed_versions,
        })
    }
    .await;

    match result {
        Ok(resp) => Json(HttpResult::success(resp)).into_response(),
        Err(e) => error_to_response(e),
    }
}

/// 6. POST /agent-mgmt/agents/check
///
/// 检查指定 agent 的安装状态、文件完整性和版本信息。
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/check",
    operation_id = "check_agent",
    summary = "检查 agent 健康状态",
    description = "检查指定 agent 的安装状态、文件完整性(可执行权限/PATH)和版本信息。",
    request_body = shared_types::CheckAgentRequest,
    responses(
        (status = 200, description = "健康检查结果", body = HttpResult<shared_types::AgentDetailInfo>),
        (status = 400, description = "参数错误(agent_id 缺失)"),
        (status = 500, description = "agent_runner 内部错误"),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn check_agent(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::CheckAgentRequest>,
) -> Response {
    let manifest = resolve_manifest(&state.registry, &req.agent_id, req.version.as_deref());
    let checker = AgentChecker::new(state.path_manager.clone());
    let detail = checker.detail_info(manifest.as_ref());
    Json(HttpResult::success(detail)).into_response()
}

/// 8. POST /agent-mgmt/agents/get
///
/// 按 agent_id 查询详情，未找到时返回 `data: null`（不视为错误）。
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/get",
    operation_id = "get_agent",
    summary = "查询单个 agent 详情",
    description = "按 agent_id 查询详情,未找到时 data 字段为 null(不视为错误)。",
    request_body = shared_types::GetAgentRequest,
    responses(
        (status = 200, description = "查询成功;未找到时 data 为 null", body = HttpResult<Option<shared_types::AgentDetailInfo>>),
        (status = 400, description = "参数错误(agent_id 缺失)"),
        (status = 500, description = "agent_runner 内部错误"),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn get_agent(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::GetAgentRequest>,
) -> Response {
    let manifest = resolve_manifest(&state.registry, &req.agent_id, req.version.as_deref());
    match manifest {
        Some(m) => {
            let checker = AgentChecker::new(state.path_manager.clone());
            let detail = checker.detail_info(Some(&m));
            Json(HttpResult::success(detail)).into_response()
        }
        None => {
            let result: HttpResult<Option<shared_types::AgentDetailInfo>> =
                HttpResult::success(None);
            Json(result).into_response()
        }
    }
}
