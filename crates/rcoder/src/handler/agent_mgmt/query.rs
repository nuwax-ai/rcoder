//! agent-mgmt 查询面（list / get / check）。

use axum::Json;
use axum::extract::State;
use shared_types::{AppError, HttpResult, InstallType, error_codes as ec};
use std::sync::Arc;
use tracing::{instrument, warn};

use super::super::utils::{
    I18nJsonOrQuery, check_agent as fwd_check, get_agent as fwd_get, list_agents as fwd_list,
};
use super::helpers::{build_ctx, resolve_container_target, validate_routing_params};
use crate::router::AppState;

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
    let service_type = super::helpers::infer_service_type(&body.routing);

    let strategy = super::super::agent_install_strategy::create_strategy(&service_type);
    if let Some(strategy) = strategy {
        // 构造最小化的 ProjectAndContainerInfo 用于解析安装目录
        let project = super::helpers::minimal_install_project(&body.routing, service_type);

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

    let manifests: Vec<agent_provisioning::AgentManifest> =
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
                "npm" => InstallType::Npm,
                "url" => InstallType::Url,
                _ => InstallType::Binary,
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
