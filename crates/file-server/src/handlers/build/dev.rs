//! dev server 生命周期 handlers: start/stop/restart/list/keep-alive/port-pool-status。

use axum::extract::State;
use garde::Validate;

use super::project_path;
use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{
    BuildQuery, DevList, DevStarted, DevStopped, KeepAlive, KeepAliveQuery, PortPool,
};

// 响应结构 DevStarted/DevStopped/DevList/KeepAlive/PortPool 在 crate::models
// （带 ToSchema，200 body 注解引用之）。

/// 协调错误 → 既有错误信封映射（Unavailable→500 / Conflict→400 业务错 / Invalid→400 校验）。
pub(super) fn coordination_error(error: shared_types::PreviewCoordinationError) -> AppError {
    match error {
        shared_types::PreviewCoordinationError::Unavailable(message) => AppError::system(message),
        shared_types::PreviewCoordinationError::Conflict(message) => AppError::business(message),
        shared_types::PreviewCoordinationError::Invalid(message) => AppError::validation(message),
    }
}

/// BuildQuery → 协调身份（调用方已解析真实目录）。
fn identity_from_query(
    q: &BuildQuery,
    resolved_path: std::path::PathBuf,
) -> shared_types::PreviewProjectIdentity {
    shared_types::PreviewProjectIdentity {
        project_id: q.project_id.clone(),
        tenant_id: q.tenant_id.clone(),
        space_id: q.space_id.clone(),
        isolation_type: q.isolation_type.clone(),
        resolved_path: resolved_path.to_string_lossy().into_owned(),
    }
}

/// 协调域判定：userapp 开发卷（app_id）不进 Custom Page 协调。
fn coordination_takes_over(state: &AppState, q: &BuildQuery) -> bool {
    state.preview.is_some()
        && q.app_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
}

fn envelope_pid(pid: i64) -> AppResult<u32> {
    u32::try_from(pid)
        .map_err(|_| AppError::system(format!("dev server pid {pid} out of u32 range")))
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
    responses((status = 200, description = "dev server 已启动（pid/port 供 keep-alive 心跳回传）", body = DevStarted), crate::openapi::ErrorApiResponses),
    tag = "Build"
)]
pub(crate) async fn start_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<DevStarted>, AppError> {
    let path = project_path(&state, &q).await?;
    if coordination_takes_over(&state, &q)
        && let Some(preview) = state.preview.clone()
    {
        let envelope = preview
            .start_dev(shared_types::PreviewStartRequest {
                identity: identity_from_query(&q, path),
                base_path: q.base_path.clone(),
            })
            .await
            .map_err(coordination_error)?;
        return Ok(Json(DevStarted {
            success: envelope.success,
            message: envelope.message,
            project_id: envelope.project_id,
            pid: envelope_pid(envelope.pid)?,
            port: envelope.port,
        }));
    }
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .start_dev(&q.project_id, &path, base, None)
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
    responses((status = 200, description = "已停止（killedPids 为被杀进程明细）", body = DevStopped), crate::openapi::ErrorApiResponses),
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
    if coordination_takes_over(&state, &q)
        && let Some(preview) = state.preview.clone()
    {
        let pid = q.pid.as_deref().and_then(|p| p.trim().parse::<i64>().ok());
        let envelope = preview
            .stop_dev(&shared_types::PreviewStopRequest {
                project_id: q.project_id.clone(),
                pid,
            })
            .await
            .map_err(coordination_error)?;
        return Ok(Json(DevStopped {
            success: envelope.success,
            message: envelope.message,
            project_id: envelope.project_id,
            pid: None,
            killed_pids: Vec::new(),
        }));
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
    responses((status = 200, description = "dev server 已启动（pid/port 供 keep-alive 心跳回传）", body = DevStarted), crate::openapi::ErrorApiResponses),
    tag = "Build"
)]
pub(crate) async fn restart_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<DevStarted>, AppError> {
    let path = project_path(&state, &q).await?;
    if coordination_takes_over(&state, &q)
        && let Some(preview) = state.preview.clone()
    {
        let envelope = preview
            .restart_dev(shared_types::PreviewRestartRequest {
                identity: identity_from_query(&q, path),
                base_path: q.base_path.clone(),
            })
            .await
            .map_err(coordination_error)?;
        return Ok(Json(DevStarted {
            success: envelope.success,
            message: envelope.message,
            project_id: envelope.project_id,
            pid: envelope_pid(envelope.pid)?,
            port: envelope.port,
        }));
    }
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .restart_dev(&q.project_id, &path, base, None)
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
    responses((status = 200, description = "在跑的 dev server 进程列表", body = DevList), crate::openapi::ErrorApiResponses),
    tag = "Build"
)]
pub(crate) async fn list_dev(State(state): State<AppState>) -> Result<Json<DevList>, AppError> {
    if let Some(preview) = state.preview.clone() {
        let entries = preview.list_dev().await.map_err(coordination_error)?;
        let list = entries
            .into_iter()
            .map(|e| crate::models::DevProcess {
                pid: e.pid.and_then(|p| u32::try_from(p).ok()).unwrap_or(0),
                port: e.port.unwrap_or(0),
                project_id: e.project_id,
                started_at: e
                    .started_at
                    .map(|t| t.timestamp_millis())
                    .unwrap_or_default(),
                instance_id: None,
                base_path: None,
                log_dir: std::path::PathBuf::new(),
                temp_log_name: String::new(),
            })
            .collect();
        return Ok(Json(DevList {
            success: true,
            list,
        }));
    }
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
    responses((status = 200, description = "心跳结果（action=restarted 表示探活失败已自动重启，存活时省略）", body = KeepAlive), crate::openapi::ErrorApiResponses),
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
    // 协调域：userapp 开发卷（app_id）不进协调。
    if state.preview.is_some()
        && q.app_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .is_none()
        && let Some(preview) = state.preview.clone()
    {
        let envelope = preview
            .keep_alive_dev(&shared_types::PreviewKeepAliveRequest {
                identity: shared_types::PreviewProjectIdentity {
                    project_id: q.project_id.clone(),
                    tenant_id: q.tenant_id.clone(),
                    space_id: q.space_id.clone(),
                    isolation_type: q.isolation_type.clone(),
                    resolved_path: path.to_string_lossy().into_owned(),
                },
                port: q.port,
                pid: Some(i64::from(pid)),
                base_path: Some(base_str.to_string()),
            })
            .await
            .map_err(coordination_error)?;
        return Ok(Json(KeepAlive {
            success: envelope.success,
            project_id: envelope.project_id,
            pid: envelope
                .pid
                .map_or(pid, |p| u32::try_from(p).unwrap_or(pid)),
            port: envelope.port.unwrap_or(q.port),
            message: envelope.message,
            action: envelope.action,
            reason: envelope.reason,
        }));
    }
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
        reason: None,
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
    responses((status = 200, description = "端口池分配快照（projectId → port）", body = PortPool), crate::openapi::ErrorApiResponses),
    tag = "Build"
)]
pub(crate) async fn port_pool_status(
    State(state): State<AppState>,
) -> Result<Json<PortPool>, AppError> {
    if let Some(preview) = state.preview.clone() {
        let status = preview
            .port_pool_status()
            .await
            .map_err(coordination_error)?;
        let allocations = status
            .allocations
            .into_iter()
            .map(|a| crate::models::PortAllocation {
                project_id: a.project_id,
                port: a.port,
            })
            .collect();
        return Ok(Json(PortPool {
            success: true,
            message: "Get port pool status successfully".to_string(),
            port_range: status.port_range,
            total_allocated: status.total_allocated,
            allocations,
        }));
    }
    let status = state.dev_server.port_pool_status()?;
    Ok(Json(PortPool {
        success: true,
        message: "Get port pool status successfully".to_string(),
        port_range: status.port_range,
        total_allocated: status.total_allocated,
        allocations: status.allocations,
    }))
}
