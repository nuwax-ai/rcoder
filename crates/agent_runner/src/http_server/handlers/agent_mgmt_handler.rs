//! Agent Management HTTP Routes (P0-1)
//!
//! 8 个 HTTP 端点(全部 POST,与 rcoder 转发层保持一致):
//! - POST /agent-mgmt/agents/list
//! - POST /agent-mgmt/agents/get
//! - POST /agent-mgmt/agents/check
//! - POST /agent-mgmt/default-agents/list
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
use shared_types_grpc::InstallAgentRequest;
use tracing::{instrument, warn};

use crate::agent_mgmt::conversion;
use crate::agent_mgmt::error::AgentMgmtError;
use crate::agent_mgmt::installer::AgentManifest;
use crate::agent_mgmt::{
    AgentMgmtResult, AgentRegistry, PathManager, checker::AgentChecker, installer, uninstaller,
};
use shared_types::{InstallType, ListAgentsRequest};

/// HTTP 应用状态扩展:把 agent_mgmt 所需依赖打包成一个 state
#[derive(Clone)]
pub struct AgentMgmtHttpState {
    pub registry: Arc<AgentRegistry>,
    pub path_manager: PathManager,
}

impl AgentMgmtHttpState {
    pub fn new(registry: Arc<AgentRegistry>, path_manager: PathManager) -> Self {
        Self {
            registry,
            path_manager,
        }
    }
}

/// 把 `AgentMgmtError` 转换为 HTTP 状态码 + 业务错误码
fn error_to_response(e: AgentMgmtError) -> Response {
    let code = e.error_code();
    let status = match code {
        ec::ERR_AGENT_MGMT_NOT_FOUND => StatusCode::NOT_FOUND,
        ec::ERR_AGENT_MGMT_ALREADY_INSTALLED => StatusCode::CONFLICT,
        ec::ERR_AGENT_MGMT_BUILTIN_PROTECTED => StatusCode::FORBIDDEN,
        ec::ERR_AGENT_MGMT_INVALID_MANIFEST | ec::ERR_AGENT_MGMT_INVALID_CHUNK => {
            StatusCode::BAD_REQUEST
        }
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    let result: HttpResult<()> = HttpResult::error(code, &e.to_string());
    (status, Json(result)).into_response()
}

/// 1. POST /agent-mgmt/agents/list
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/list",
    request_body = ListAgentsRequest,
    responses(
        (status = 200, description = "列出已安装 agent", body = HttpResult<shared_types::ListAgentsResponse>),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn list_agents(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<ListAgentsRequest>,
) -> Response {
    let manifests = state.registry.list(req.include_builtin);
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
        install_dir: state.path_manager.install_dir().to_string_lossy().to_string(),
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
    let metadata_str = match headers.get("x-agent-metadata").and_then(|v| v.to_str().ok()) {
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
    let agent_id = meta.get("agent_id").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let command = meta.get("command").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let install_type_str = meta
        .get("install_type")
        .and_then(|v| v.as_str())
        .unwrap_or("BINARY");
    let install_type = match install_type_str {
        "BINARY" | "binary" => InstallType::Binary,
        "URL" | "url" => InstallType::Url,
        "NPM" | "npm" => InstallType::Npm,
        _ => InstallType::Binary,
    };
    let source_url = meta.get("source_url").and_then(|v| v.as_str()).map(String::from);
    let npm_package = meta.get("npm_package").and_then(|v| v.as_str()).map(String::from);
    let sha256 = meta.get("sha256").and_then(|v| v.as_str()).map(String::from);

    let result: AgentMgmtResult<shared_types_grpc::InstallAgentResponse> = match install_type {
        InstallType::Url => {
            let url = match source_url {
                Some(u) => u,
                None => {
                    return error_to_response(AgentMgmtError::InvalidChunk(
                        "URL install requires source_url".into(),
                    ));
                }
            };
            installer::url_installer::install_from_url(
                &state.registry,
                &state.path_manager,
                &agent_id,
                &url,
                &command,
                &[],
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
                    args: &[],
                    expected_sha256: sha256.as_deref(),
                    install_type: InstallType::Binary,
                    bytes: body,
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

/// 3. POST /agent-mgmt/agents/install-from-url
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/install-from-url",
    request_body = shared_types::InstallFromUrlRequest,
    responses(
        (status = 200, description = "安装成功", body = HttpResult<shared_types::InstallAgentResponse>),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state, req))]
pub async fn install_from_url(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::InstallFromUrlRequest>,
) -> Response {
    // command 必填(URL 安装必须指定入口名)
    let command = match req.command.as_deref() {
        Some(c) if !c.is_empty() => c,
        _ => {
            return error_to_response(AgentMgmtError::InvalidManifest(
                "command is required for URL install".into(),
            ));
        }
    };
    match installer::url_installer::install_from_url(
        &state.registry,
        &state.path_manager,
        &req.agent_id,
        &req.url,
        command,
        &req.args,
        req.sha256.as_deref(),
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

/// 4. POST /agent-mgmt/agents/install-from-npm
#[instrument(skip(state, req))]
pub async fn install_from_npm(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::InstallFromPackageManagerRequest>,
) -> Response {
    // command 必填(npm 安装必须指定入口名)
    let command = match req.command.as_deref() {
        Some(c) if !c.is_empty() => c,
        _ => {
            return error_to_response(AgentMgmtError::InvalidManifest(
                "command is required for NPM install".into(),
            ));
        }
    };
    match installer::npm_installer::install_from_npm(
        &state.registry,
        &state.path_manager,
        &req.agent_id,
        &req.package,
        command,
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
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/uninstall",
    request_body = shared_types::UninstallAgentRequest,
    responses(
        (status = 200, description = "卸载成功", body = HttpResult<shared_types::UninstallAgentResponse>),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn uninstall_agent(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::UninstallAgentRequest>,
) -> Response {
    match uninstaller::uninstall(&state.registry, &req.agent_id).await {
        Ok(manifest) => {
            let resp = shared_types::UninstallAgentResponse {
                uninstalled: true,
                install_type: manifest.install_type,
                agent_id: manifest.agent_id,
            };
            Json(HttpResult::success(resp)).into_response()
        }
        Err(e) => error_to_response(e),
    }
}

/// 6. POST /agent-mgmt/agents/check
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/check",
    request_body = shared_types::CheckAgentRequest,
    responses(
        (status = 200, description = "健康检查结果", body = HttpResult<shared_types::AgentDetailInfo>),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn check_agent(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::CheckAgentRequest>,
) -> Response {
    let manifest = state.registry.get(&req.agent_id);
    let checker = AgentChecker::new(state.path_manager.clone());
    let detail = checker.detail_info(manifest.as_ref());
    Json(HttpResult::success(detail)).into_response()
}

/// 7. POST /agent-mgmt/default-agents/list
#[utoipa::path(
    post,
    path = "/agent-mgmt/default-agents/list",
    responses(
        (status = 200, description = "默认 agent 清单"),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(_state))]
pub async fn list_default_agents(State(_state): State<AgentMgmtHttpState>) -> Response {
    let defaults = installer::default_agents::list_default_agents();
    Json(HttpResult::success(defaults)).into_response()
}

/// 8. POST /agent-mgmt/agents/get
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/get",
    request_body = shared_types::CheckAgentRequest,
    responses(
        (status = 200, description = "agent 详情", body = HttpResult<Option<shared_types::AgentDetailInfo>>),
    ),
    tag = "agent-mgmt"
)]
#[instrument(skip(state))]
pub async fn get_agent(
    State(state): State<AgentMgmtHttpState>,
    Json(req): Json<shared_types::CheckAgentRequest>,
) -> Response {
    let manifest = state.registry.get(&req.agent_id);
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

// 保持 AgentManifest 在编译范围内
#[allow(dead_code)]
fn _ensure_agent_manifest(_: &AgentManifest) {}

// 保持 InstallAgentRequest 在编译范围内(供未来 streaming HTTP 支持)
#[allow(dead_code)]
fn _ensure_chunk(_: &InstallAgentRequest) {}
