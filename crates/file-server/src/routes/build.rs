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
        .route("/build", get(build_project))
        .route("/parse-build-error", axum::routing::post(parse_build_error))
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

/// `GET /api/build/build` (对齐 nuwax buildProject): install + build + 拷贝 dist。
async fn build_project(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<Value>, AppError> {
    let path = project_path(&state, &q);
    if !path.exists() {
        return Err(AppError::resource("project does not exist"));
    }
    let log_dir = crate::service::dev_server::log::log_dir(&state.config, &q.project_id);
    tokio::fs::create_dir_all(&log_dir)
        .await
        .map_err(|e| AppError::system(format!("create build log dir: {e}")))?;
    let now = crate::service::dev_server::now_ms();
    let main_log = log_dir.join(crate::service::dev_server::log::main_log_name());
    let temp_log = log_dir.join(crate::service::dev_server::log::temp_log_name(now));
    let timeout = state.config.dev_command_timeout_secs;

    // 读 scripts.build
    let pkg_content = tokio::fs::read_to_string(path.join("package.json"))
        .await
        .map_err(|e| AppError::business(format!("read package.json: {e}")))?;
    let pkg: Value = serde_json::from_str(&pkg_content)
        .map_err(|e| AppError::business(format!("parse package.json: {e}")))?;
    let build_script = pkg
        .get("scripts")
        .and_then(|s| s.get("build"))
        .and_then(|v| v.as_str())
        .unwrap_or("vite build");
    let base = q.base_path.as_deref().unwrap_or("/");

    // install (arg 数组, 无 shell 拼接; 失败继续, 与 nuwax 宽松一致)
    let _ = crate::service::dev_server::process::run_command_to_log(
        "pnpm",
        &["install", "--prefer-offline"],
        &path,
        &main_log,
        &temp_log,
        timeout,
    )
    .await;
    // build (vite: pnpm exec vite build --base X; 否则 pnpm run build)
    let build_args: Vec<&str> = if build_script.to_ascii_lowercase().contains("vite") {
        vec!["exec", "vite", "build", "--base", base, "--debug", "--debug"]
    } else {
        vec!["run", "build"]
    };
    crate::service::dev_server::process::run_command_to_log(
        "pnpm",
        &build_args,
        &path,
        &main_log,
        &temp_log,
        timeout,
    )
    .await?;

    // 拷贝 dist → {DIST_TARGET_DIR}/{projectId}/dist/ (Rust fs, 无 rm -rf shell;
    // 错误为类型化 io::Error, 路径经 PathBuf::join 无注入)
    let dst = state.config.dist_target_dir.join(&q.project_id).join("dist");
    let src = path.join("dist");
    if !src.exists() {
        return Err(AppError::business("build produced no dist directory"));
    }
    let src2 = src.clone();
    let dst2 = dst.clone();
    tokio::task::spawn_blocking(move || copy_dir_all(&src2, &dst2))
        .await
        .map_err(|e| AppError::system(format!("copy dist join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Build completed",
        "projectId": q.project_id,
    })))
}

/// 递归拷贝目录 (替代 `rm -rf && cp -R`); 目标存在则先清空, 保留 symlink。
fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> Result<(), AppError> {
    use std::fs;
    if dst.exists() {
        fs::remove_dir_all(dst)
            .map_err(|e| AppError::system(format!("remove old dist {}: {e}", dst.display())))?;
    }
    fs::create_dir_all(dst)
        .map_err(|e| AppError::system(format!("create dist {}: {e}", dst.display())))?;
    for entry in fs::read_dir(src)
        .map_err(|e| AppError::system(format!("read dist {}: {e}", src.display())))?
    {
        let entry = entry.map_err(|e| AppError::system(format!("read dir entry: {e}")))?;
        let ft = entry
            .file_type()
            .map_err(|e| AppError::system(format!("file type: {e}")))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if ft.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|e| AppError::system(format!("copy {}: {e}", from.display())))?;
        }
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ParseErrorBody {
    #[allow(dead_code)]
    project_id: Option<String>,
    error_message: String,
}

/// `POST /api/build/parse-build-error` (对齐 nuwax BuildErrorParser)。
async fn parse_build_error(
    State(_state): State<AppState>,
    Json(body): Json<ParseErrorBody>,
) -> Result<Json<Value>, AppError> {
    let msg = crate::service::build_error::parse(&body.error_message);
    Ok(Json(json!({ "success": true, "message": msg })))
}
