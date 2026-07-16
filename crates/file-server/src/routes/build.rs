//! `/api/build` 路由 (对齐 nuwax buildRoutes; dev server 经 DevServerManager)。
//!
//! 本文件含 dev server 生命周期 + 端口池 + 日志读取 7 路由。
//! `build` / `parse-build-error` / computer 相关路由见后续 task。

use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::workspace::ProjectContext;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/start-dev", get(start_dev))
        .route("/stop-dev", get(stop_dev))
        .route("/restart-dev", get(restart_dev))
        .route("/list-dev", get(list_dev))
        .route("/keep-alive", get(keep_alive))
        .route("/port-pool-status", get(port_pool_status))
        .route("/get-dev-log", get(get_dev_log))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BuildQuery {
    project_id: String,
    #[serde(default)]
    base_path: Option<String>,
    // 多租户隔离参数 (透传给 ProjectContext)
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    isolation_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct KeepAliveQuery {
    project_id: String,
    #[serde(default)]
    pid: Option<u32>,
    port: u16,
    #[serde(default)]
    base_path: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    isolation_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DevLogQuery {
    project_id: String,
    #[serde(default = "default_start_index")]
    start_index: usize,
    #[serde(default = "default_log_type")]
    log_type: String,
}
fn default_start_index() -> usize {
    1
}
fn default_log_type() -> String {
    "temp".to_string()
}

fn project_path(state: &AppState, q: &BuildQuery) -> PathBuf {
    state.resolver.resolve_project(&ProjectContext {
        project_id: q.project_id.clone(),
        tenant_id: q.tenant_id.clone(),
        space_id: q.space_id.clone(),
        isolation_type: q.isolation_type.clone(),
    })
}

fn project_path_keep(state: &AppState, q: &KeepAliveQuery) -> PathBuf {
    state.resolver.resolve_project(&ProjectContext {
        project_id: q.project_id.clone(),
        tenant_id: q.tenant_id.clone(),
        space_id: q.space_id.clone(),
        isolation_type: q.isolation_type.clone(),
    })
}

/// `GET /api/build/start-dev` (对齐 nuwax start-dev)。
async fn start_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<Value>, AppError> {
    let path = project_path(&state, &q);
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .start_dev(&q.project_id, &path, base)
        .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Development server started",
        "projectId": q.project_id,
        "pid": started.pid,
        "port": started.port,
    })))
}

/// `GET /api/build/stop-dev` (对齐 nuwax stop-dev)。
async fn stop_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<Value>, AppError> {
    let stopped = state.dev_server.stop_dev(&q.project_id).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Stopped",
        "projectId": q.project_id,
        "pid": null,
        "killedPids": stopped.killed_pids,
    })))
}

/// `GET /api/build/restart-dev` (对齐 nuwax restart-dev)。
async fn restart_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<Value>, AppError> {
    let path = project_path(&state, &q);
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .restart_dev(&q.project_id, &path, base)
        .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Development server restart successfully",
        "projectId": q.project_id,
        "pid": started.pid,
        "port": started.port,
    })))
}

/// `GET /api/build/list-dev` (对齐 nuwax list-dev)。
async fn list_dev(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let list = state.dev_server.list_dev()?;
    Ok(Json(json!({ "success": true, "list": list })))
}

/// `GET /api/build/keep-alive` (对齐 nuwax keep-alive)。
async fn keep_alive(
    State(state): State<AppState>,
    Query(q): Query<KeepAliveQuery>,
) -> Result<Json<Value>, AppError> {
    let path = project_path_keep(&state, &q);
    let base = q.base_path.as_deref();
    let pid = q.pid.unwrap_or(0);
    let result = state
        .dev_server
        .keep_alive(&q.project_id, pid, q.port, base, &path)
        .await?;
    let mut resp = json!({
        "success": true,
        "projectId": q.project_id,
        "pid": pid,
        "port": q.port,
    });
    if result.alive && result.action.is_none() {
        resp["message"] = json!("Development server is alive");
    }
    if let Some(action) = result.action {
        resp["message"] = json!("Development server restarted");
        resp["action"] = json!(action);
    }
    Ok(Json(resp))
}

/// `GET /api/build/port-pool-status` (对齐 nuwax port-pool-status)。
async fn port_pool_status(State(state): State<AppState>) -> Result<Json<Value>, AppError> {
    let status = state.dev_server.port_pool_status()?;
    Ok(Json(json!({
        "success": true,
        "message": "Port pool status",
        "portRange": status.port_range,
        "totalAllocated": status.total_allocated,
        "allocations": status.allocations,
    })))
}

/// `GET /api/build/get-dev-log` (对齐 nuwax get-dev-log)。
async fn get_dev_log(
    State(state): State<AppState>,
    Query(q): Query<DevLogQuery>,
) -> Result<Json<Value>, AppError> {
    let result = state
        .dev_server
        .read_dev_log(&q.project_id, q.start_index, &q.log_type)
        .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Dev log fetched",
        "logs": result.logs,
        "totalLines": result.total_lines,
        "startIndex": result.start_index,
        "logFileName": result.log_file_name,
    })))
}
