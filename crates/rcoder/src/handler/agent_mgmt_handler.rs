//! Agent Management HTTP 处理器 (P0-4 + P0-5 重构)
//!
//! 提供 7 个 HTTP 端点(与 agent-runner 端契约一致),内部通过 gRPC 转发到
//! 对应项目的 agent_runner 容器:
//!
//! - `POST /agent-mgmt/agents/list?project_id=xxx`           list_agents (body JSON)
//! - `POST /agent-mgmt/agents/get?project_id=xxx`            get_agent    (body JSON)
//! - `POST /agent-mgmt/agents/check?project_id=xxx`          check_agent  (body JSON)
//! - `POST /agent-mgmt/agents/install`                       install_agent (multipart: file + metadata JSON)
//! - `POST /agent-mgmt/agents/install-from-url`              install_from_url (body JSON)
//! - `POST /agent-mgmt/agents/install-from-npm`              install_from_npm (body JSON)
//! - `POST /agent-mgmt/agents/uninstall`                     uninstall_agent (body JSON)
//!
//! # 参数传递约定
//!
//! 全部走 POST,body 解析:
//! - **简单 JSON 端点**:使用 [`I18nJsonOrQuery`] 提取器,优先 JSON body,兼容 `?project_id=xxx` query 调试
//! - **`install` 端点**:使用 `multipart/form-data`,字段:
//!   - `file`: 二进制文件(单文件 / tar.gz / zip)
//!   - `metadata`: JSON 字符串(含 `project_id` / `agent`(`agent_id` / `command` / `args` / `version`) / `install_type` / `source_url` / `npm_package` / `sha256`)
//!
//! # 错误模型
//!
//! 所有错误用 `AppError` 表达(axum 自动映射成 HTTP 状态 + 业务错误码 JSON)。
//! 18 个 agent-runner 业务码 + 2 个转发层专用码(见 `error_codes`)。

use axum::Json;
use axum::extract::State;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
#[allow(unused_imports)] // `json!` 仅在 `#[schema(example = json!(...))]` 宏内使用
use serde_json::json;
use shared_types::{
    AgentIdentity, AppError, HttpResult, InstallType, IsolationType, RoutingParams, ServiceType,
    error_codes as ec,
};
use std::str::FromStr;
use std::sync::Arc;
use tracing::{info, instrument, warn};

use super::utils::{
    AgentMgmtForwardCtx, I18nJsonOrQuery, InstallAgentParams, check_agent as fwd_check,
    get_agent as fwd_get, install_agent as fwd_install, list_agents as fwd_list,
    uninstall_agent as fwd_uninstall,
};
use crate::router::AppState;

// === HTTP 请求体(与 gRPC proto 解耦) ===

/// `install` 端点的 multipart body 整体结构(OpenAPI 描述专用)
///
/// 实际接收 `multipart/form-data`,包含两个 part:
/// - `file`: 压缩包(tar.gz / zip),`install_type=binary` 时必填
/// - `metadata`: JSON 字符串(必填),见 [`InstallMetadataBody`]
///
/// ⚠️ 这个类型仅为 OpenAPI 文档生成而存在,handler 实际用 `axum::extract::Multipart`
/// 直接消费 `multipart/form-data`,不走 JSON 反序列化。
#[derive(Debug, Serialize, utoipa::ToSchema)]
pub struct InstallMultipartBody {
    /// 代理二进制文件(tar.gz / zip,最大 1GB)
    pub file: String,
    /// 安装元数据,JSON 字符串,字段见 [`InstallMetadataBody`]
    pub metadata: String,
}

/// `install` 端点的 multipart `metadata` 字段结构(JSON 字符串,multipart 端点专用)
///
/// 安装 binary:压缩包(tar.gz / zip),客户端选择 type
/// URL 安装:必填 `source_url`
/// NPM 安装:必填 `npm_package`
#[derive(Debug, Deserialize, Default, Serialize, utoipa::ToSchema)]
pub struct InstallMetadataBody {
    #[serde(flatten)]
    pub routing: RoutingParams,
    /// Agent 身份信息(agent_id, command, args, version)
    pub agent: AgentIdentity,
    /// `binary` / `url` / `npm`,大小写不敏感,默认 `binary`
    #[serde(default = "default_install_type")]
    #[schema(example = "BINARY")]
    pub install_type: String,
    /// 下载 URL(URL 安装时必填,例如 `https://github.com/.../agent.tar.gz`)
    #[serde(default)]
    #[schema(example = "https://github.com/example/agent/releases/download/v1.0.0/agent.tar.gz")]
    pub source_url: Option<String>,
    /// npm 包名(NPM 安装时必填,例如 `@anthropic-ai/claude-code-acp`)
    #[serde(default)]
    #[schema(example = "@anthropic-ai/claude-code-acp")]
    pub npm_package: Option<String>,
    /// SHA-256 校验和(hex,可选,提供时安装后会校验)
    #[serde(default)]
    #[schema(example = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")]
    pub sha256: Option<String>,
}

fn default_install_type() -> String {
    "BINARY".to_string()
}

// === 内部工具:提取 project + 构造转发 ctx ===

/// 验证多租户路由参数（复用 computer_chat_handler 的验证模式）
///
/// 规则:pod_id 有值时,isolation_type / tenant_id / space_id 必须非空且有效。
fn validate_routing_params(routing: &RoutingParams) -> Result<(), AppError> {
    if let Some(ref pod_id) = routing.pod_id {
        if pod_id.trim().is_empty() {
            return Err(AppError::with_message(
                ec::ERR_VALIDATION,
                "pod_id cannot be empty".to_string(),
            ));
        }
        // pod_id 有值时,isolation_type 必填
        match routing.isolation_type.as_deref() {
            None | Some("") => {
                return Err(AppError::with_message(
                    ec::ERR_VALIDATION,
                    "isolation_type is required when pod_id is provided".to_string(),
                ));
            }
            Some(it) => {
                if IsolationType::from_str(it).is_err() {
                    return Err(AppError::with_message(
                        ec::ERR_VALIDATION,
                        format!(
                            "invalid isolation_type '{}', expected: tenant, space, project",
                            it
                        ),
                    ));
                }
            }
        }
        // pod_id 有值时,tenant_id 必填
        if routing
            .tenant_id
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(AppError::with_message(
                ec::ERR_VALIDATION,
                "tenant_id is required when pod_id is provided".to_string(),
            ));
        }
        // pod_id 有值时,space_id 必填
        if routing
            .space_id
            .as_deref()
            .is_none_or(|s| s.trim().is_empty())
        {
            return Err(AppError::with_message(
                ec::ERR_VALIDATION,
                "space_id is required when pod_id is provided".to_string(),
            ));
        }
    }
    Ok(())
}

/// 解析容器目标（支持 project_id 和 user_id/pod_id 两条查找路径）
///
/// - Path A: `project_id` 有值 → storage lookup（向后兼容）
/// - Path B: `user_id` 或 `pod_id` 有值 → 运行时容器查找（多租户模式）
/// - Path C: 都没有 → ERR_VALIDATION
async fn resolve_container_target(
    state: &Arc<AppState>,
    project_id: Option<&str>,
    routing: &RoutingParams,
) -> Result<Arc<shared_types::ProjectAndContainerInfo>, AppError> {
    // Path A: project_id 优先（向后兼容）
    if let Some(pid) = project_id.filter(|s| !s.is_empty()) {
        return state.get_project(pid).ok_or_else(|| {
            AppError::with_i18n_key(ec::ERR_PROJECT_NOT_FOUND, "error.project_not_found")
        });
    }

    // Path B: user_id 或 pod_id 路由
    let container_identifier = routing
        .pod_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .or(routing.user_id.as_deref().filter(|s| !s.is_empty()));

    if let Some(identifier) = container_identifier {
        let container_info = state
            .runtime()
            .get_container_info_by_identifier(identifier, &ServiceType::ComputerAgentRunner)
            .await
            .map_err(|e| {
                warn!(
                    "[agent_mgmt] container lookup failed: identifier={}, error={}",
                    identifier, e
                );
                AppError::with_message(
                    ec::ERR_CONTAINER_NOT_FOUND,
                    format!("container lookup failed: {}", e),
                )
            })?
            .ok_or_else(|| {
                AppError::with_message(
                    ec::ERR_CONTAINER_NOT_FOUND,
                    format!("no running container found for identifier: {}", identifier),
                )
            })?;

        let mut info = shared_types::ProjectAndContainerInfo::new(String::new());
        info.set_user_id(routing.user_id.clone());
        info.set_pod_id(routing.pod_id.clone());
        info.set_container(Some(container_info));
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        return Ok(Arc::new(info));
    }

    // Path C: 没有任何标识符
    Err(AppError::with_message(
        ec::ERR_VALIDATION,
        "project_id, user_id, or pod_id is required".to_string(),
    ))
}

fn build_ctx(state: &Arc<AppState>) -> AgentMgmtForwardCtx {
    AgentMgmtForwardCtx::from_state(
        state.grpc_pool.clone(),
        state.runtime.clone(),
        state.container_prefix_rcoder.clone(),
        state.container_prefix_computer.clone(),
        shared_types::current_request_locale(),
    )
}

// === Handlers ===

/// 列出某项目下所有已安装的 agent
///
/// 查询该项目对应 agent_runner 容器内的 agent 注册表,返回所有
/// `installed` / `builtin` 状态的 agent 元信息。
///
/// - **路径**:`POST /agent-mgmt/agents/list`
/// - **转发**:rcoder → agent_runner gRPC `ListAgents`
/// - **典型场景**:管理后台/CLI 工具展示项目可用 agent 列表
///
/// ## 请求体示例
///
/// ```json
/// { "project_id": "demo-project-001" }
/// ```
///
/// ## 响应示例(200)
///
/// ```json
/// {
///   "code": "0000",
///   "message": "success",
///   "data": { "agents": [ { "id": "codex-acp", "version": "0.1.0", ... } ] },
///   "tid": "a1b2c3d4e5f6",
///   "success": true
/// }
/// ```
///
/// ## 错误码
///
/// - `400 ERR_VALIDATION` — `project_id` 为空
/// - `404 ERR_PROJECT_NOT_FOUND` — 项目不存在
/// - `500 ERR_INTERNAL_SERVER_ERROR` — agent_runner I/O / 序列化失败
/// - `503 ERR_AGENT_RUNNER_UNAVAILABLE` — agent_runner 容器离线
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/list",
    operation_id = "list_agents",
    summary = "列出项目下已安装的 agent",
    description = "查询 agent_runner 注册表,返回该项目下所有已安装(含内置)agent 的元信息。支持 `?project_id=xxx` query 调试,JSON body 优先。",
    request_body = shared_types::ListAgentsRequest,
    responses(
        (status = 200, description = "查询成功", body = HttpResult<shared_types::ListAgentsResponse>),
        (status = 400, description = "参数错误(project_id 缺失)"),
        (status = 404, description = "项目不存在"),
        (status = 500, description = "agent_runner I/O 或内部错误"),
        (status = 503, description = "Agent Runner 不可用(容器离线/gRPC 连接失败)"),
    ),
    tag = "agent-mgmt",
)]
#[instrument(skip(state, body))]
pub async fn list_agents(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(body): I18nJsonOrQuery<shared_types::ListAgentsRequest>,
) -> Result<Json<HttpResult<shared_types::ListAgentsResponse>>, AppError> {
    validate_routing_params(&body.routing)?;

    // 优先从文件直接读取注册表（支持 rcoder 直接安装的场景）
    // 根据参数动态判断 ServiceType
    let service_type = if body.routing.user_id.is_some() || body.routing.pod_id.is_some() {
        ServiceType::ComputerAgentRunner
    } else {
        ServiceType::WebAgentRunner
    };

    let strategy = super::agent_install_strategy::create_strategy(&service_type);
    if let Some(strategy) = strategy {
        // 构造最小化的 ProjectAndContainerInfo 用于解析安装目录
        let mut project = shared_types::ProjectAndContainerInfo::new(String::new());
        project.set_user_id(body.routing.user_id.clone());
        project.set_pod_id(body.routing.pod_id.clone());
        project.set_service_type(Some(service_type));

        if let Ok(install_ctx) = strategy.resolve_install_context(&project, &body.routing) {
            let registry_path = install_ctx.install_dir.join("registry.json");
            if registry_path.exists() {
                match read_registry_from_file(&registry_path) {
                    Ok(resp) => return Ok(Json(HttpResult::success(resp))),
                    Err(e) => {
                        warn!("[agent_mgmt] Failed to read registry from file: {}", e);
                    }
                }
            }
        }
    }

    // 回退到 gRPC 调用（需要容器运行）
    let project =
        resolve_container_target(&state, body.routing.project_id.as_deref(), &body.routing).await?;
    let ctx = build_ctx(&state);
    let resp = fwd_list(&ctx, &project).await?;
    Ok(Json(HttpResult::success(resp)))
}

/// 从文件直接读取注册表
fn read_registry_from_file(
    registry_path: &std::path::Path,
) -> Result<shared_types::ListAgentsResponse, AppError> {
    let data = std::fs::read_to_string(registry_path).map_err(|e| {
        AppError::with_message(
            ec::ERR_INTERNAL_SERVER_ERROR,
            format!("read registry file: {}", e),
        )
    })?;

    let manifests: Vec<crate::agent_download::registry_update::AgentManifest> =
        serde_json::from_str(&data).map_err(|e| {
            AppError::with_message(
                ec::ERR_INTERNAL_SERVER_ERROR,
                format!("parse registry JSON: {}", e),
            )
        })?;

    let agents: Vec<shared_types::AgentInfo> = manifests
        .into_iter()
        .map(|m| shared_types::AgentInfo {
            agent_id: m.agent_id,
            install_type: match m.install_type.as_str() {
                "npm" => shared_types::InstallType::Npm,
                "url" => shared_types::InstallType::Url,
                _ => shared_types::InstallType::Binary,
            },
            status: shared_types::AgentInstallStatus::Available,
            version: m.version,
            binary_path: Some(m.binary_path),
            installed_at: Some(m.installed_at),
        })
        .collect();

    let total = agents.len();
    let install_dir = registry_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();

    Ok(shared_types::ListAgentsResponse {
        system_info: shared_types::SystemInfo::current(),
        agents,
        total,
        install_dir,
    })
}

/// 查询某个 agent 的详细信息(版本、安装路径、健康状态等)
///
/// 按 `agent_id` 在 agent_runner 注册表中查找,未找到时返回 `data: null`。
///
/// - **路径**:`POST /agent-mgmt/agents/get`
/// - **转发**:rcoder → agent_runner gRPC `GetAgent`
/// - **典型场景**:管理后台展示 agent 详情、调试安装路径冲突
///
/// ## 请求体示例
///
/// ```json
/// { "project_id": "demo-project-001", "agent_id": "codex-acp" }
/// ```
///
/// ## 错误码
///
/// - `400 ERR_VALIDATION` — `project_id` 或 `agent_id` 为空
/// - `404 ERR_PROJECT_NOT_FOUND` — 项目不存在(agent 不存在时业务上不算 404,见 `data`)
/// - `500 ERR_INTERNAL_SERVER_ERROR` — agent_runner I/O 失败
/// - `503 ERR_AGENT_RUNNER_UNAVAILABLE` — agent_runner 容器离线
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/get",
    operation_id = "get_agent",
    summary = "查询单个 agent 的详细信息",
    description = "按 agent_id 在 agent_runner 注册表查询详细信息,未找到时 data 字段为 null(不视为错误)。",
    request_body = shared_types::GetAgentRequest,
    responses(
        (status = 200, description = "查询成功;未找到时 data 字段为 null", body = HttpResult<Option<shared_types::AgentDetailInfo>>),
        (status = 400, description = "参数错误(project_id 或 agent_id 缺失)"),
        (status = 404, description = "项目不存在"),
        (status = 500, description = "agent_runner I/O 或内部错误"),
        (status = 503, description = "Agent Runner 不可用"),
    ),
    tag = "agent-mgmt",
)]
#[instrument(skip(state, body))]
pub async fn get_agent(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(body): I18nJsonOrQuery<shared_types::GetAgentRequest>,
) -> Result<Json<HttpResult<Option<shared_types::AgentDetailInfo>>>, AppError> {
    validate_routing_params(&body.routing)?;
    let project =
        resolve_container_target(&state, body.routing.project_id.as_deref(), &body.routing).await?;
    let ctx = build_ctx(&state);
    let resp = fwd_get(&ctx, &project, &body.agent_id, body.version.as_deref()).await?;
    Ok(Json(HttpResult::success(resp)))
}

/// 对某个 agent 做健康检查(进程存活、版本、端口监听)
///
/// 在 agent_runner 容器内探测 `agent_id` 对应的进程:
/// - `is_alive=true` 表示进程存在并响应
/// - `version` 字段返回实际可执行文件的版本字符串
/// - 端口、命令行等附加信息在 `details` 字段
///
/// - **路径**:`POST /agent-mgmt/agents/check`
/// - **转发**:rcoder → agent_runner gRPC `CheckAgent`
/// - **典型场景**:管理后台实时刷新 agent 状态、CI 检查可用性
///
/// ## 请求体示例
///
/// ```json
/// { "project_id": "demo-project-001", "agent_id": "codex-acp" }
/// ```
///
/// ## 错误码
///
/// - `400 ERR_VALIDATION` — `project_id` 或 `agent_id` 为空
/// - `404 ERR_PROJECT_NOT_FOUND` / `ERR_AGENT_MGMT_NOT_FOUND` — 项目或 agent 不存在
/// - `500 ERR_INTERNAL_SERVER_ERROR` — agent_runner 探测失败
/// - `503 ERR_AGENT_RUNNER_UNAVAILABLE` — agent_runner 容器离线
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/check",
    operation_id = "check_agent",
    summary = "检查 agent 健康状态(进程/版本/端口)",
    description = "在 agent_runner 容器内探测 agent 进程存活状态、版本号、端口监听等健康指标。",
    request_body = shared_types::CheckAgentRequest,
    responses(
        (status = 200, description = "健康检查结果(is_alive=true 表示存活)", body = HttpResult<shared_types::CheckAgentResponse>),
        (status = 400, description = "参数错误(project_id 或 agent_id 缺失)"),
        (status = 404, description = "项目不存在 / agent 不存在"),
        (status = 500, description = "agent_runner I/O 或内部错误"),
        (status = 503, description = "Agent Runner 不可用"),
    ),
    tag = "agent-mgmt",
)]
#[instrument(skip(state, body))]
pub async fn check_agent(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(body): I18nJsonOrQuery<shared_types::CheckAgentRequest>,
) -> Result<Json<HttpResult<shared_types::CheckAgentResponse>>, AppError> {
    validate_routing_params(&body.routing)?;
    let project =
        resolve_container_target(&state, body.routing.project_id.as_deref(), &body.routing).await?;
    let ctx = build_ctx(&state);
    let resp = fwd_check(&ctx, &project, &body.agent_id, body.version.as_deref()).await?;
    Ok(Json(HttpResult::success(resp)))
}

/// 上传二进制并安装 agent(multipart 端点,支持最大 1GB 压缩包)
///
/// 通过 `multipart/form-data` 上传:
/// - `file`: 二进制文件(可执行 / tar.gz / zip)
/// - `metadata`: JSON 字符串,见 [`InstallMetadataBody`]
///
/// - **路径**:`POST /agent-mgmt/agents/install`
/// - **转发**:rcoder → agent_runner gRPC `InstallAgent`(client streaming,1MB chunk)
/// - **典型场景**:本地编译产物直接部署、tar.gz 包上传
///
/// ## curl 示例
///
/// ```bash
/// curl -X POST http://host:8087/agent-mgmt/agents/install \
///   -F 'file=@./codex-acp.tar.gz' \
///   -F 'metadata={"project_id":"demo","agent_id":"codex-acp","command":"codex-acp","install_type":"BINARY"}'
/// ```
///
/// ## 错误码
///
/// - `400 ERR_VALIDATION` — `agent_id` / `command` / `file` 缺失,`install_type` 不支持,BINARY 模式 `file` 为空,URL 模式缺 `source_url`,NPM 模式缺 `npm_package`
/// - `404 ERR_PROJECT_NOT_FOUND` — 项目不存在
/// - `413` — 上传文件超过 1GB 限制
/// - `400 ERR_AGENT_MGMT_*` — agent_runner 业务错误(见下表)
/// - `500 ERR_INTERNAL_SERVER_ERROR` — agent_runner I/O 失败
/// - `503 ERR_AGENT_RUNNER_UNAVAILABLE` — agent_runner 容器离线
///
/// ## 常见 400 业务错误
///
/// | 业务码 | 含义 |
/// |--------|------|
/// | `ERR_AGENT_MGMT_ALREADY_INSTALLED` | agent_id 已存在 |
/// | `ERR_AGENT_MGMT_CHECKSUM_MISMATCH` | SHA-256 校验失败 |
/// | `ERR_AGENT_MGMT_ARCHIVE_BOMB` | 压缩炸弹(解压累计超限) |
/// | `ERR_AGENT_MGMT_PATH_TRAVERSAL` | 路径遍历攻击拦截 |
/// | `ERR_AGENT_MGMT_BINARY_TOO_LARGE` | 单文件超过 agent_runner 内部上限 |
/// | `ERR_AGENT_MGMT_DISK_FULL` | 磁盘满 |
/// | `ERR_AGENT_MGMT_STREAM_TRUNCATED` | 上传流被截断 |
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/install",
    operation_id = "install_agent",
    summary = "上传二进制并安装 agent",
    description = "通过 multipart/form-data 上传文件并安装,支持 BINARY(可执行/tar.gz/zip)。URL 和 NPM 类型请用专用端点。",
    request_body(
        content = InstallMultipartBody,
        content_type = "multipart/form-data",
        description = "multipart/form-data,字段 `file` (二进制) + `metadata` (JSON 字符串,见 InstallMetadataBody schema)"
    ),
    responses(
        (status = 200, description = "安装成功", body = HttpResult<shared_types::InstallAgentResponse>),
        (status = 400, description = "参数错误(详见端点 doc 字符串)"),
        (status = 404, description = "项目不存在"),
        (status = 413, description = "上传文件超过 1GB 限制"),
        (status = 400, description = "agent-runner 业务错误(详见 400 错误码表)"),
        (status = 500, description = "agent-runner I/O 或内部错误"),
        (status = 503, description = "Agent Runner 不可用"),
    ),
    tag = "agent-mgmt",
)]
#[instrument(skip(state, multipart))]
pub async fn install_agent(
    State(state): State<Arc<AppState>>,
    mut multipart: axum::extract::Multipart,
) -> Result<Json<HttpResult<shared_types::InstallAgentResponse>>, AppError> {
    // 1. 解析 multipart fields
    //    file 字段先写入临时文件(避免大文件全部加载到内存),然后读回 Bytes
    let mut file_bytes = Bytes::new();
    let mut meta: Option<InstallMetadataBody> = None;

    while let Some(mut field) = multipart.next_field().await.map_err(|e| {
        AppError::with_message(ec::ERR_INVALID_PARAMS, format!("invalid multipart: {e}"))
    })? {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                // 写入临时文件,避免大文件常驻内存;上传过程中边收边写,
                // 仅在转发时才整体读回 Bytes。
                // 使用 channel 将 chunk 流式传到阻塞线程,逐块落盘。
                let mut total: u64 = 0;
                let max_size = shared_types::MAX_BINARY_SIZE;
                let mut overflow_err: Option<AppError> = None;

                let tmp = tempfile::NamedTempFile::new().map_err(|e| {
                    AppError::with_message(
                        ec::ERR_INTERNAL_SERVER_ERROR,
                        format!("create temp: {e}"),
                    )
                })?;
                let tmp_path = tmp.into_temp_path();

                let (tx, mut rx) = tokio::sync::mpsc::channel::<Bytes>(4);

                // 阻塞线程:逐块写入临时文件,写完后读回并清理
                let writer = tokio::task::spawn_blocking(move || {
                    use std::io::Write;
                    let mut file = std::fs::File::create(&tmp_path)
                        .map_err(|e| std::io::Error::other(format!("create: {e}")))?;
                    while let Some(chunk) = rx.blocking_recv() {
                        file.write_all(&chunk)
                            .map_err(|e| std::io::Error::other(format!("write: {e}")))?;
                    }
                    file.flush().ok();
                    drop(file);
                    // 读回 bytes,然后 tmp_path drop 时自动清理临时文件
                    let bytes = std::fs::read(&tmp_path)?;
                    drop(tmp_path);
                    Ok::<Bytes, std::io::Error>(Bytes::from(bytes))
                });

                // 接收 multipart chunk 并转发到阻塞线程
                while let Some(chunk) = field.chunk().await.map_err(|e| {
                    AppError::with_message(ec::ERR_INVALID_PARAMS, format!("read file chunk: {e}"))
                })? {
                    total += chunk.len() as u64;
                    if total > max_size {
                        overflow_err = Some(AppError::with_message(
                            ec::ERR_VALIDATION,
                            format!("upload file exceeds {} bytes limit", max_size),
                        ));
                        break;
                    }
                    if tx.send(chunk).await.is_err() {
                        overflow_err = Some(AppError::with_message(
                            ec::ERR_INTERNAL_SERVER_ERROR,
                            "temp file writer dropped unexpectedly",
                        ));
                        break;
                    }
                }
                drop(tx); // 关闭 channel,writer 退出循环

                // 无论成功/失败,都等待 writer 完成(确保临时文件被清理)
                let writer_result = writer.await.map_err(|e| {
                    AppError::with_message(
                        ec::ERR_INTERNAL_SERVER_ERROR,
                        format!("writer panic: {e}"),
                    )
                })?;

                if let Some(err) = overflow_err {
                    return Err(err);
                }
                file_bytes = writer_result.map_err(|e| {
                    AppError::with_message(
                        ec::ERR_INTERNAL_SERVER_ERROR,
                        format!("temp file I/O: {e}"),
                    )
                })?;
            }
            "metadata" => {
                let text = field.text().await.map_err(|e| {
                    AppError::with_message(ec::ERR_INVALID_PARAMS, format!("read metadata: {e}"))
                })?;
                meta = Some(
                    serde_json::from_str::<InstallMetadataBody>(&text).map_err(|e| {
                        AppError::with_message(
                            ec::ERR_VALIDATION,
                            format!("invalid metadata JSON: {e}"),
                        )
                    })?,
                );
            }
            _ => {
                tracing::debug!("[install_agent] ignoring unknown multipart field: {name}");
            }
        }
    }

    let meta = meta
        .ok_or_else(|| AppError::with_message(ec::ERR_VALIDATION, "metadata field is required"))?;
    require_field(Some(&meta.agent.agent_id), "agent_id")?;
    require_field(Some(&meta.agent.command), "command")?;

    let install_type = parse_install_type(
        Some(&meta.install_type),
        meta.npm_package.as_ref(),
        meta.source_url.as_ref(),
    )?;

    // BINARY 模式必须提供 file 字段(仅接受 tar.gz / zip 压缩包)
    if install_type == InstallType::Binary && file_bytes.is_empty() {
        return Err(AppError::with_message(
            ec::ERR_VALIDATION,
            "BINARY install requires non-empty `file` field (tar.gz or zip archive)",
        ));
    }

    // 2. 解析 project + 构造 ctx
    validate_routing_params(&meta.routing)?;
    let project =
        resolve_container_target(&state, meta.routing.project_id.as_deref(), &meta.routing).await?;
    let ctx = build_ctx(&state);

    // 3. 构造 forward 参数
    let params = InstallAgentParams {
        agent: shared_types::AgentIdentity {
            agent_id: meta.agent.agent_id.clone(),
            command: meta.agent.command.clone(),
            args: meta.agent.args.clone(),
            version: meta.agent.version.clone(),
        },
        install_type,
        source_url: meta.source_url.clone(),
        npm_package: meta.npm_package.clone(),
        sha256: meta.sha256.clone().filter(|s| !s.is_empty()),
        platforms: None,
        force: false,
    };

    let resp = fwd_install(&ctx, &project, params, file_bytes).await?;
    Ok(Json(HttpResult::success(resp)))
}

/// 从 URL 下载并安装 agent(多平台 + 版本管理)
///
/// agent-runner 自动判断:版本相同则跳过下载,版本更新则自动下载安装。
/// 支持 `platforms` 多平台 URL 映射,根据容器系统架构自动选择。
///
/// - **路径**:`POST /agent-mgmt/agents/install-from-url`
/// - **转发**:rcoder → agent_runner gRPC `InstallAgent`(metadata only)
/// - **典型场景**:业务方每次使用 agent 前调用,幂等安装
///
/// ## 请求体示例
///
/// ```json
/// {
///   "user_id": "user_123",
///   "agent": {
///     "agent_id": "codex-acp",
///     "command": "codex-acp",
///     "version": "1.2.0"
///   },
///   "platforms": {
///     "linux-x86_64": { "url": "https://cdn.example.com/codex-acp-linux-amd64.tar.gz" },
///     "linux-aarch64": { "url": "https://cdn.example.com/codex-acp-linux-arm64.tar.gz" }
///   }
/// }
/// ```
///
/// ## 错误码
///
/// - `400 ERR_VALIDATION` — `agent_id` / `command` / `version` / `platforms` 为空
/// - `404 ERR_PROJECT_NOT_FOUND` — 项目不存在
/// - `400 ERR_AGENT_MGMT_*` — 业务错误
///   - `ERR_AGENT_MGMT_PLATFORM_NOT_FOUND` — platforms 中无匹配当前系统的 URL
///   - `ERR_AGENT_MGMT_INVALID_VERSION` — version 格式不合法
///   - `ERR_AGENT_MGMT_CHECKSUM_MISMATCH` — SHA-256 校验失败
///   - `ERR_AGENT_MGMT_COMMAND_TIMEOUT` — 下载超时
///   - `ERR_AGENT_MGMT_PATH_TRAVERSAL` — 归档含路径遍历
///   - `ERR_AGENT_MGMT_DISK_FULL` — 磁盘满
/// - `500 ERR_INTERNAL_SERVER_ERROR` — agent_runner I/O 失败
/// - `503 ERR_AGENT_RUNNER_UNAVAILABLE` — agent_runner 容器离线
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/install-from-url",
    operation_id = "install_from_url",
    summary = "从 URL 下载并安装 agent(多平台+版本管理)",
    description = "支持 platforms 多平台 URL + version 版本号,agent-runner 自动判断是否需要下载安装(幂等)。",
    request_body = shared_types::InstallFromUrlRequest,
    responses(
        (status = 200, description = "安装/更新/跳过", body = HttpResult<shared_types::InstallAgentResponse>),
        (status = 400, description = "参数错误(agent_id / command / version / platforms 缺失)"),
        (status = 404, description = "项目不存在"),
        (status = 400, description = "agent-runner 业务错误(platform 不匹配、version 无效、下载失败等)"),
        (status = 500, description = "agent-runner I/O 或内部错误"),
        (status = 503, description = "Agent Runner 不可用"),
    ),
    tag = "agent-mgmt",
)]
#[instrument(skip(state, body))]
pub async fn install_from_url(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(body): I18nJsonOrQuery<shared_types::InstallFromUrlRequest>,
) -> Result<Json<HttpResult<shared_types::InstallAgentResponse>>, AppError> {
    validate_routing_params(&body.routing)?;
    require_field(Some(&body.agent.agent_id), "agent_id")?;
    require_field(Some(&body.agent.command), "command")?;
    require_field(body.agent.version.as_deref(), "version")?;
    if body.platforms.is_empty() {
        return Err(AppError::with_message(
            ec::ERR_VALIDATION,
            "platforms cannot be empty",
        ));
    }

    // 根据参数动态判断 ServiceType
    let service_type = if body.routing.user_id.is_some() || body.routing.pod_id.is_some() {
        ServiceType::ComputerAgentRunner
    } else {
        ServiceType::WebAgentRunner
    };

    let strategy =
        super::agent_install_strategy::create_strategy(&service_type).ok_or_else(|| {
            AppError::with_message(
                ec::ERR_VALIDATION,
                format!("agent installation is not supported for {:?}", service_type),
            )
        })?;

    // 构造最小化的 ProjectAndContainerInfo 用于解析安装目录
    let mut project = shared_types::ProjectAndContainerInfo::new(String::new());
    project.set_user_id(body.routing.user_id.clone());
    project.set_pod_id(body.routing.pod_id.clone());
    project.set_service_type(Some(service_type.clone()));

    let install_ctx = strategy.resolve_install_context(&project, &body.routing)?;

    info!(
        "[agent_mgmt] Install context resolved: install_dir={}, service_type={:?}",
        install_ctx.install_dir.display(),
        service_type
    );

    // 调用核心安装函数（复用 ensure_agent_installed 的逻辑）
    let version = body
        .agent
        .version
        .as_deref()
        .ok_or_else(|| AppError::with_message(ec::ERR_VALIDATION, "version is required"))?;

    let (download_result, platform_key) = super::agent_install_strategy::do_install_from_url(
        &state,
        &body.agent.agent_id,
        version,
        &body.agent.command,
        &body.agent.args,
        &body.platforms,
        &install_ctx.install_dir,
    )
    .await?;

    // 返回结果
    let resp = shared_types::InstallAgentResponse {
        agent_id: body.agent.agent_id,
        status: shared_types::AgentInstallStatus::Available,
        binary_path: body.agent.command,
        file_type: "binary".to_string(),
        file_count: None,
        file_size: download_result.file_size,
        version: body.agent.version,
        source_url: body.platforms.get(&platform_key).map(|p| p.url.clone()),
        action: Some(shared_types::InstallAction::Installed),
        installed: true,
        previous_version: None,
        platform: Some(platform_key),
    };

    Ok(Json(HttpResult::success(resp)))
}

/// 从 npm 全局安装 agent
///
/// agent_runner 端调用 `npm install -g <package>` 全局安装,常用语 `@anthropic-ai/claude-code-acp` 等
/// 官方发布的 npm 包形式的 agent。
///
/// - **路径**:`POST /agent-mgmt/agents/install-from-npm`
/// - **转发**:rcoder → agent_runner gRPC `InstallFromPackageManagerRequest`
/// - **典型场景**:一行命令安装官方推荐的 agent
///
/// ## 请求体示例
///
/// ```json
/// {
///   "project_id": "demo-project-001",
///   "agent": {
///     "agent_id": "claude-code-acp",
///     "command": "claude-code-acp"
///   },
///   "package": "@anthropic-ai/claude-code-acp"
/// }
/// ```
///
/// ## 错误码
///
/// - `400 ERR_VALIDATION` — `agent_id` / `command` / `package` 为空
/// - `404 ERR_PROJECT_NOT_FOUND` — 项目不存在
/// - `400 ERR_AGENT_MGMT_*` — 业务错误
///   - `ERR_AGENT_MGMT_ALREADY_INSTALLED` — agent_id 已存在
///   - `ERR_AGENT_MGMT_INSTALL_FAILED` — npm 安装失败(包不存在、网络错误、权限等)
///   - `ERR_AGENT_MGMT_COMMAND_TIMEOUT` — 安装超时
/// - `500 ERR_INTERNAL_SERVER_ERROR` — agent_runner I/O 失败
/// - `503 ERR_AGENT_RUNNER_UNAVAILABLE` — agent_runner 容器离线
///
/// ## 注意事项
///
/// `command` 字段一般是包名去掉 scope 后的可执行文件名,例如包 `@anthropic-ai/claude-code-acp` 对应
/// `command: "claude-code-acp"`。安装后会校验可执行文件是否在 PATH 中,否则返回 `ERR_AGENT_MGMT_INSTALL_FAILED`。
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/install-from-npm",
    operation_id = "install_from_npm",
    summary = "从 npm 全局安装 agent",
    description = "agent_runner 端调用 npm install -g 安装包,适用于官方 npm 发布的 agent(如 @anthropic-ai/claude-code-acp)。",
    request_body = shared_types::InstallFromPackageManagerRequest,
    responses(
        (status = 200, description = "安装成功", body = HttpResult<shared_types::InstallAgentResponse>),
        (status = 400, description = "参数错误(agent_id / command / package 缺失)"),
        (status = 404, description = "项目不存在"),
        (status = 400, description = "agent-runner 业务错误(npm 包无效、安装失败)"),
        (status = 500, description = "agent-runner I/O 或内部错误"),
        (status = 503, description = "Agent Runner 不可用"),
    ),
    tag = "agent-mgmt",
)]
#[instrument(skip(state, body))]
pub async fn install_from_npm(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(body): I18nJsonOrQuery<shared_types::InstallFromPackageManagerRequest>,
) -> Result<Json<HttpResult<shared_types::InstallAgentResponse>>, AppError> {
    validate_routing_params(&body.routing)?;
    let project =
        resolve_container_target(&state, body.routing.project_id.as_deref(), &body.routing).await?;
    require_field(Some(&body.agent.agent_id), "agent_id")?;
    require_field(Some(&body.agent.command), "command")?;
    require_field(Some(&body.package), "package")?;
    let params = InstallAgentParams {
        agent: body.agent.clone(),
        install_type: InstallType::Npm,
        source_url: None,
        npm_package: Some(body.package.clone()),
        sha256: None,
        platforms: None,
        force: false,
    };
    let ctx = build_ctx(&state);
    let resp = fwd_install(&ctx, &project, params, Bytes::new()).await?;
    Ok(Json(HttpResult::success(resp)))
}

/// 卸载一个已安装的 agent
///
/// agent_runner 端负责删除 agent 目录、清理注册表、停止关联进程。
/// 内置 agent(`default-agents` 列表中)受保护,卸载会返回 403。
///
/// - **路径**:`POST /agent-mgmt/agents/uninstall`
/// - **转发**:rcoder → agent_runner gRPC `UninstallAgent`
/// - **典型场景**:清理不再使用的第三方 agent
///
/// ## 请求体示例
///
/// ```json
/// { "project_id": "demo-project-001", "agent_id": "codex-acp" }
/// ```
///
/// ## 错误码
///
/// - `400 ERR_VALIDATION` — `project_id` 或 `agent_id` 为空
/// - `403 ERR_AGENT_MGMT_BUILTIN_PROTECTED` — 试图卸载内置 agent
/// - `404 ERR_PROJECT_NOT_FOUND` / `ERR_AGENT_MGMT_NOT_FOUND` — 项目或 agent 不存在
/// - `400 ERR_AGENT_MGMT_UNINSTALL_FAILED` — 卸载过程失败(文件锁、进程占用等)
/// - `500 ERR_INTERNAL_SERVER_ERROR` — agent_runner I/O 失败
/// - `503 ERR_AGENT_RUNNER_UNAVAILABLE` — agent_runner 容器离线
#[utoipa::path(
    post,
    path = "/agent-mgmt/agents/uninstall",
    operation_id = "uninstall_agent",
    summary = "卸载 agent",
    description = "删除 agent 目录并清理注册表,内置 agent 受保护(返回 403 ERR_AGENT_MGMT_BUILTIN_PROTECTED)。",
    request_body = shared_types::UninstallAgentRequest,
    responses(
        (status = 200, description = "卸载成功", body = HttpResult<shared_types::UninstallAgentResponse>),
        (status = 400, description = "参数错误(agent_id 缺失)"),
        (status = 403, description = "内置 agent 受保护(ERR_AGENT_MGMT_BUILTIN_PROTECTED)"),
        (status = 404, description = "项目不存在 / Agent 不存在"),
        (status = 400, description = "agent-runner 业务错误(卸载失败)"),
        (status = 500, description = "agent-runner I/O 或内部错误"),
        (status = 503, description = "Agent Runner 不可用"),
    ),
    tag = "agent-mgmt",
)]
#[instrument(skip(state, body))]
pub async fn uninstall_agent(
    State(state): State<Arc<AppState>>,
    I18nJsonOrQuery(body): I18nJsonOrQuery<shared_types::UninstallAgentRequest>,
) -> Result<Json<HttpResult<shared_types::UninstallAgentResponse>>, AppError> {
    validate_routing_params(&body.routing)?;
    let project =
        resolve_container_target(&state, body.routing.project_id.as_deref(), &body.routing).await?;
    let ctx = build_ctx(&state);
    let resp = fwd_uninstall(&ctx, &project, &body.agent_id, body.version.as_deref()).await?;
    Ok(Json(HttpResult::success(resp)))
}

// === 工具函数 ===

fn require_field(value: Option<&str>, name: &str) -> Result<(), AppError> {
    if value.filter(|s| !s.is_empty()).is_none() {
        return Err(AppError::with_message(
            ec::ERR_VALIDATION,
            format!("{name} is required"),
        ));
    }
    Ok(())
}

fn parse_install_type(
    s: Option<&str>,
    npm_pkg: Option<&String>,
    src_url: Option<&String>,
) -> Result<InstallType, AppError> {
    // 把 `Some("")` 当作缺失(空串和 None 都回落到默认 BINARY),
    // 避免客户端传 `"install_type": ""` 拿到意外的 validation error
    let normalized = s.map(str::trim).filter(|s| !s.is_empty());
    let t = match normalized.unwrap_or("BINARY").to_ascii_uppercase().as_str() {
        "BINARY" => InstallType::Binary,
        "URL" => InstallType::Url,
        "NPM" => InstallType::Npm,
        other => {
            return Err(AppError::with_message(
                ec::ERR_VALIDATION,
                format!("unsupported install_type: {other}"),
            ));
        }
    };
    match t {
        InstallType::Url if src_url.map(|s| s.is_empty()).unwrap_or(true) => Err(
            AppError::with_message(ec::ERR_VALIDATION, "URL install requires source_url"),
        ),
        InstallType::Npm if npm_pkg.map(|s| s.is_empty()).unwrap_or(true) => Err(
            AppError::with_message(ec::ERR_VALIDATION, "NPM install requires npm_package"),
        ),
        _ => Ok(t),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_install_type;
    use shared_types::InstallType;

    #[test]
    fn parse_install_type_none_defaults_to_binary() {
        let r = parse_install_type(None, None, None).unwrap();
        assert_eq!(r, InstallType::Binary);
    }

    #[test]
    fn parse_install_type_empty_string_defaults_to_binary() {
        // 修复:Some("") 当作缺失处理
        let r = parse_install_type(Some(""), None, None).unwrap();
        assert_eq!(r, InstallType::Binary);
    }

    #[test]
    fn parse_install_type_whitespace_only_defaults_to_binary() {
        // 修复:trim 后视为空
        let r = parse_install_type(Some("   "), None, None).unwrap();
        assert_eq!(r, InstallType::Binary);
    }

    #[test]
    fn parse_install_type_uppercase_works() {
        assert_eq!(
            parse_install_type(Some("BINARY"), None, None).unwrap(),
            InstallType::Binary
        );
        assert_eq!(
            parse_install_type(Some("URL"), None, Some(&"https://x".to_string())).unwrap(),
            InstallType::Url
        );
        assert_eq!(
            parse_install_type(Some("NPM"), Some(&"@scope/p".to_string()), None).unwrap(),
            InstallType::Npm
        );
    }

    #[test]
    fn parse_install_type_lowercase_works() {
        assert_eq!(
            parse_install_type(Some("binary"), None, None).unwrap(),
            InstallType::Binary
        );
        assert_eq!(
            parse_install_type(Some("url"), None, Some(&"https://x".to_string())).unwrap(),
            InstallType::Url
        );
        assert_eq!(
            parse_install_type(Some("npm"), Some(&"@scope/p".to_string()), None).unwrap(),
            InstallType::Npm
        );
    }

    #[test]
    fn parse_install_type_mixed_case_works() {
        assert_eq!(
            parse_install_type(Some("Url"), None, Some(&"https://x".to_string())).unwrap(),
            InstallType::Url
        );
        assert_eq!(
            parse_install_type(Some("nPm"), Some(&"@scope/p".to_string()), None).unwrap(),
            InstallType::Npm
        );
    }

    #[test]
    fn parse_install_type_unknown_returns_validation_error() {
        let err = parse_install_type(Some("archive"), None, None).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("unsupported install_type"), "got: {msg}");
        // 错误消息中会是 uppercase 后的值(函数内部 to_ascii_uppercase)
        assert!(msg.contains("ARCHIVE"), "got: {msg}");
    }

    #[test]
    fn parse_install_type_url_without_source_url_errors() {
        let err = parse_install_type(Some("URL"), None, None).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("source_url"), "got: {msg}");
    }

    #[test]
    fn parse_install_type_npm_without_package_errors() {
        let err = parse_install_type(Some("NPM"), None, None).unwrap_err();
        let msg = format!("{err:?}");
        assert!(msg.contains("npm_package"), "got: {msg}");
    }
}
