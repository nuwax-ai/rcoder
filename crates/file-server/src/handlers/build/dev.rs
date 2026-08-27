//! dev server 生命周期 handlers: start/stop/restart/list/keep-alive/port-pool-status。

use axum::extract::State;
use garde::Validate;
use serde::Serialize;

use super::project_path;
use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{BuildQuery, KeepAliveQuery, KilledPid};
use crate::service::dev_server::{DevProcess, PortAllocation};

// ── 类型化响应 (camelCase 由 serde 统一保证) ──────────────────────────────────

/// start-dev / restart-dev 共用 (对齐 nuwax: {success, message, projectId, pid, port})
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevStarted {
    pub success: bool,
    pub message: String,
    pub project_id: String,
    pub pid: u32,
    pub port: u16,
}

/// stop-dev (pid 恒 null: Option 不加 skip_serializing_if → 序列化为 null, 对齐现 json!)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevStopped {
    pub success: bool,
    pub message: String,
    pub project_id: String,
    pub pid: Option<u32>,
    pub killed_pids: Vec<KilledPid>,
}

/// list-dev
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevList {
    pub success: bool,
    pub list: Vec<DevProcess>,
}

/// keep-alive (action 仅重启分支有 → None 时省略, 匹配现 json! 条件追加行为)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeepAlive {
    pub success: bool,
    pub project_id: String,
    pub pid: u32,
    pub port: u16,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// port-pool-status
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PortPool {
    pub success: bool,
    pub message: String,
    pub port_range: String,
    pub total_allocated: usize,
    pub allocations: Vec<PortAllocation>,
}

async fn project_path_keep(state: &AppState, q: &KeepAliveQuery) -> AppResult<std::path::PathBuf> {
    if let Some(app_id) = q.app_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return crate::workspace::resolve_userapp_dev(app_id, None, &state.config);
    }
    state
        .resolver
        .resolve_project(&crate::workspace::ProjectContext {
            project_id: q.project_id.clone(),
            tenant_id: q.tenant_id.clone(),
            space_id: q.space_id.clone(),
            isolation_type: q.isolation_type.clone(),
        })
        .await
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// 启动开发服务器
///
/// 对齐 nuwax start-dev。
#[utoipa::path(
    get,
    path = "/start-dev",
    params(BuildQuery),
    description = r#"
启动项目开发服务器（按 package.json 等探测命令异步拉起，返回 pid/port）。端口经 PortPool 分配；重复调用幂等——已在跑则返回现有进程。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn start_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<DevStarted>, AppError> {
    let path = project_path(&state, &q).await?;
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .start_dev(&q.project_id, &path, base)
        .await?;
    Ok(Json(DevStarted {
        success: true,
        message: "Development server started".to_string(),
        project_id: q.project_id,
        pid: started.pid,
        port: started.port,
    }))
}

/// 停止开发服务器
///
/// 对齐 nuwax stop-dev。
#[utoipa::path(
    get,
    path = "/stop-dev",
    params(BuildQuery),
    description = r#"
停止项目的开发服务器进程组（按 projectId 定位，无需 pid）。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn stop_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<DevStopped>, AppError> {
    // pid 必填且非空 (BuildQuery 多 handler 共用, 仅 stop_dev 要求 pid; DTO 无法声明式校验)
    if q.pid
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return Err(AppError::validation("pid: is required"));
    }
    let stopped = state.dev_server.stop_dev(&q.project_id).await?;
    state.log_cache.delete(&q.project_id)?;
    // message 对齐 nuwax stopDevUtils: 全杀 "Stopped" / 部分杀 "Partially stopped..." / 无候选 "No running process found"
    let all_killed = stopped.killed_pids.iter().all(|k| k.killed);
    let message = if stopped.killed_pids.is_empty() {
        "No running process found"
    } else if all_killed {
        "Stopped"
    } else {
        "Partially stopped but continue execution"
    };
    Ok(Json(DevStopped {
        success: true,
        message: message.to_string(),
        project_id: q.project_id,
        pid: None,
        killed_pids: stopped.killed_pids,
    }))
}

/// 重启开发服务器
///
/// 对齐 nuwax restart-dev。
#[utoipa::path(
    get,
    path = "/restart-dev",
    params(BuildQuery),
    description = r#"
重启开发服务器：stop + start 组合，新 pid/port 在响应中返回。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn restart_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<DevStarted>, AppError> {
    let path = project_path(&state, &q).await?;
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .restart_dev(&q.project_id, &path, base)
        .await?;
    Ok(Json(DevStarted {
        success: true,
        message: "Development server restart successfully".to_string(),
        project_id: q.project_id,
        pid: started.pid,
        port: started.port,
    }))
}

/// 列出开发服务器
///
/// 列出在跑的开发服务器
///
#[utoipa::path(
    get,
    path = "/list-dev",
    description = r#"
列出当前在跑的开发服务器清单（projectId/pid/port/启动时间）——工作台面板与
端口占用排障的数据源。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn list_dev(State(state): State<AppState>) -> Result<Json<DevList>, AppError> {
    let list = state.dev_server.list_dev()?;
    Ok(Json(DevList {
        success: true,
        list,
    }))
}

/// 开发服务器保活
///
/// 对齐 nuwax keep-alive。
#[utoipa::path(
    get,
    path = "/keep-alive",
    params(KeepAliveQuery),
    description = r#"
开发服务器心跳保活：校验进程仍存活、目录仍在；**进程意外死亡时自动以原参数重启**（响应 action 字段区分 alive/started）。前端定时（如 30s）调用一次。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn keep_alive(
    State(state): State<AppState>,
    Query(q): Query<KeepAliveQuery>,
) -> Result<Json<KeepAlive>, AppError> {
    // 对齐 nuwax buildRoutes: projectId/pid/port/basePath 必填校验 (经 KeepAliveQuery garde)
    q.validate().map_err(crate::error::from_garde)?;
    // 校验已保证必填; 取数 (失败逻辑不可达, 防御性处理)
    let pid = q
        .pid
        .ok_or_else(|| AppError::system("pid missing after garde validation"))?;
    let base_str = q
        .base_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::system("base_path missing after garde validation"))?;
    let path = project_path_keep(&state, &q).await?;
    let result = state
        .dev_server
        .keep_alive(&q.project_id, pid, q.port, Some(base_str), &path)
        .await?;
    // pid/port: 重启分支用新值, alive 分支用查询入参 (对齐 nuwax)
    let out_pid = result.pid.unwrap_or(pid);
    let out_port = result.port.unwrap_or(q.port);
    // message/action: 重启分支 (action Some) → "started" + action; 否则 alive → "is alive"
    let (message, action) = if let Some(act) = result.action {
        ("Development server started".to_string(), Some(act))
    } else {
        ("Development server is alive".to_string(), None)
    };
    Ok(Json(KeepAlive {
        success: true,
        project_id: q.project_id,
        pid: out_pid,
        port: out_port,
        message,
        action,
    }))
}

/// 查询端口池状态
///
#[utoipa::path(
    get,
    path = "/port-pool-status",
    description = r#"
查开发服务器 PortPool 分配现状：可分配范围、已占用明细（哪个项目占了哪个
端口）——端口冲突与泄漏排查用。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn port_pool_status(
    State(state): State<AppState>,
) -> Result<Json<PortPool>, AppError> {
    let status = state.dev_server.port_pool_status()?;
    Ok(Json(PortPool {
        success: true,
        message: "Get port pool status successfully".to_string(),
        port_range: status.port_range,
        total_allocated: status.total_allocated,
        allocations: status.allocations,
    }))
}
