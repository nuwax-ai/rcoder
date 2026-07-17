//! `/api/build` 路由 (对齐 nuwax buildRoutes; dev server 经 DevServerManager)。
//!
//! 本文件含 dev server 生命周期 + 端口池 + 日志读取 7 路由。
//! `build` / `parse-build-error` / computer 相关路由见后续 task。

use std::path::PathBuf;

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ProjectContext;

// ── 类型化响应 (取代 serde_json::json! 字面量; camelCase 由 serde 统一保证) ──────
mod response {
    use serde::Serialize;

    use crate::service::dev_server::log::LogLine;
    use crate::service::dev_server::{DevProcess, KilledPid, PortAllocation};

    /// start-dev / restart-dev 共用 (对齐 nuwax: {success, message, projectId, pid, port})
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DevStarted {
        pub success: bool,
        pub message: String,
        pub project_id: String,
        pub pid: u32,
        pub port: u16,
    }

    /// stop-dev (pid 恒 null: Option 不加 skip_serializing_if → 序列化为 null, 对齐现 json!)
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DevStopped {
        pub success: bool,
        pub message: String,
        pub project_id: String,
        pub pid: Option<u32>,
        pub killed_pids: Vec<KilledPid>,
    }

    /// list-dev
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DevList {
        pub success: bool,
        pub list: Vec<DevProcess>,
    }

    /// keep-alive (action 仅重启分支有 → None 时省略, 匹配现 json! 条件追加行为)
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct KeepAlive {
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
    pub struct PortPool {
        pub success: bool,
        pub message: String,
        pub port_range: String,
        pub total_allocated: usize,
        pub allocations: Vec<PortAllocation>,
    }

    /// get-dev-log
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct DevLog {
        pub success: bool,
        pub message: String,
        pub logs: Vec<LogLine>,
        pub total_lines: usize,
        pub start_index: usize,
        pub log_file_name: String,
        pub cache_hit: bool,
        pub file_too_large: bool,
    }

    /// build
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct BuildDone {
        pub success: bool,
        pub message: String,
        pub project_id: String,
    }

    /// parse-build-error / clear-all-log-cache 共用 {success, message}
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct Simple {
        pub success: bool,
        pub message: String,
    }

    /// get-log-cache-stats (stats 内含 SCREAMING_SNAKE 键 → 逐字段 rename)
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    pub struct LogCacheStats {
        pub success: bool,
        pub message: String,
        pub stats: LogCacheStatsData,
    }

    #[derive(Serialize)]
    pub struct LogCacheStatsData {
        pub enabled: bool,
        #[serde(rename = "cacheSize")]
        pub cache_size: u64,
        #[serde(rename = "maxCacheEntries")]
        pub max_cache_entries: u64,
        #[serde(rename = "cacheDuration")]
        pub cache_duration: u64,
        #[serde(rename = "maxFileSizeMB")]
        pub max_file_size_mb: String,
        #[serde(rename = "totalCacheSizeMB")]
        pub total_cache_size_mb: String,
        #[serde(rename = "NODE_ENV")]
        pub node_env: String,
        #[serde(rename = "LOG_CACHE_ENABLED")]
        pub log_cache_enabled: String,
    }
}

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
        .route("/get-log-cache-stats", get(get_log_cache_stats))
        .route("/clear-all-log-cache", get(clear_all_log_cache))
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
) -> Result<Json<response::DevStarted>, AppError> {
    let path = project_path(&state, &q);
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .start_dev(&q.project_id, &path, base)
        .await?;
    Ok(Json(response::DevStarted {
        success: true,
        message: "Development server started".to_string(),
        project_id: q.project_id,
        pid: started.pid,
        port: started.port,
    }))
}

/// `GET /api/build/stop-dev` (对齐 nuwax stop-dev)。
async fn stop_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<response::DevStopped>, AppError> {
    let stopped = state.dev_server.stop_dev(&q.project_id).await?;
    // message 对齐 nuwax stopDevUtils: 全杀 "Stopped" / 部分杀 "Partially stopped..." / 无候选 "No running process found"
    let all_killed = stopped.killed_pids.iter().all(|k| k.killed);
    let message = if stopped.killed_pids.is_empty() {
        "No running process found"
    } else if all_killed {
        "Stopped"
    } else {
        "Partially stopped but continue execution"
    };
    Ok(Json(response::DevStopped {
        success: true,
        message: message.to_string(),
        project_id: q.project_id,
        pid: None,
        killed_pids: stopped.killed_pids,
    }))
}

/// `GET /api/build/restart-dev` (对齐 nuwax restart-dev)。
async fn restart_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<response::DevStarted>, AppError> {
    let path = project_path(&state, &q);
    let base = q.base_path.as_deref();
    let started = state
        .dev_server
        .restart_dev(&q.project_id, &path, base)
        .await?;
    Ok(Json(response::DevStarted {
        success: true,
        message: "Development server restart successfully".to_string(),
        project_id: q.project_id,
        pid: started.pid,
        port: started.port,
    }))
}

/// `GET /api/build/list-dev` (对齐 nuwax list-dev)。
async fn list_dev(State(state): State<AppState>) -> Result<Json<response::DevList>, AppError> {
    let list = state.dev_server.list_dev()?;
    Ok(Json(response::DevList {
        success: true,
        list,
    }))
}

/// `GET /api/build/keep-alive` (对齐 nuwax keep-alive)。
async fn keep_alive(
    State(state): State<AppState>,
    Query(q): Query<KeepAliveQuery>,
) -> Result<Json<response::KeepAlive>, AppError> {
    // 对齐 nuwax buildRoutes: projectId/pid/port/basePath 必填校验
    let pid = q
        .pid
        .ok_or_else(|| AppError::validation("pid is required"))?;
    let base_str = q
        .base_path
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AppError::validation("basePath is required"))?;
    let path = project_path_keep(&state, &q);
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
    Ok(Json(response::KeepAlive {
        success: true,
        project_id: q.project_id,
        pid: out_pid,
        port: out_port,
        message,
        action,
    }))
}

/// `GET /api/build/port-pool-status` (对齐 nuwax port-pool-status)。
async fn port_pool_status(
    State(state): State<AppState>,
) -> Result<Json<response::PortPool>, AppError> {
    let status = state.dev_server.port_pool_status()?;
    Ok(Json(response::PortPool {
        success: true,
        message: "Get port pool status successfully".to_string(),
        port_range: status.port_range,
        total_allocated: status.total_allocated,
        allocations: status.allocations,
    }))
}

/// `GET /api/build/get-dev-log` (对齐 nuwax get-dev-log)。
async fn get_dev_log(
    State(state): State<AppState>,
    Query(q): Query<DevLogQuery>,
) -> Result<Json<response::DevLog>, AppError> {
    let result = state
        .dev_server
        .read_dev_log(&q.project_id, q.start_index, &q.log_type)
        .await?;
    Ok(Json(response::DevLog {
        success: true,
        message: "Get log successfully".to_string(),
        logs: result.logs,
        total_lines: result.total_lines,
        start_index: result.start_index,
        log_file_name: result.log_file_name,
        // Rust 无缓存层, 固定 false (对齐 nuwax getDevLog 字段)
        cache_hit: false,
        file_too_large: false,
    }))
}

/// `GET /api/build/build` (对齐 nuwax buildProject): install + build + 拷贝 dist。
async fn build_project(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<response::BuildDone>, AppError> {
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
        .ok_or_else(|| AppError::business("Project missing build script"))?;
    // basePath 规范化 (补首尾 /, 对齐 nuwax; vite --base 需尾斜杠)
    let base = normalize_build_base(q.base_path.as_deref());

    // 并发控制: 全局信号量 + 项目级互斥 (对齐 nuwax buildingProjects + MAX_BUILD_CONCURRENCY)
    let (sem, building) = build_concurrency(state.config.max_build_concurrency);
    // 全局并发上限: 立即拒绝 (对齐 nuwax "Concurrency is full, please try again later";
    // 非排队等待 — 排队会挂起到请求超时, nuwax 是立即 BusinessError)
    let _permit = sem
        .try_acquire()
        .map_err(|_| AppError::business("Concurrency is full, please try again later"))?;
    // 项目级互斥: 同一项目正在 build 则拒绝 (poison 恢复: 另一请求 panic 毒化锁时取回数据)
    {
        let mut b = building.lock().unwrap_or_else(|e| e.into_inner());
        if b.contains(&q.project_id) {
            return Err(AppError::business("This project is being built"));
        }
        b.insert(q.project_id.clone());
    }

    // install (对齐 nuwax: 失败则整体 build 失败, 透传 "Dependency installation failed")
    crate::service::dev_server::process::run_command_to_log(
        "pnpm",
        &["install", "--prefer-offline"],
        &path,
        &main_log,
        &temp_log,
        timeout,
    )
    .await
    .map_err(|e| AppError::system(format!("Dependency installation failed: {e}")))?;
    // build (vite: pnpm exec vite build --base X; 否则 pnpm run build)
    let build_args: Vec<&str> = if build_script.to_ascii_lowercase().contains("vite") {
        vec!["exec", "vite", "build", "--base", &base, "--debug"]
    } else {
        vec!["run", "build"]
    };
    let build_result = crate::service::dev_server::process::run_command_to_log(
        "pnpm",
        &build_args,
        &path,
        &main_log,
        &temp_log,
        timeout,
    )
    .await;
    // 释放项目级锁 (drop _permit + 显式 remove)
    building
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&q.project_id);
    if build_result.is_err() {
        // 失败: 读 build 日志用 build_error 解析友好消息 (对齐 nuwax BuildErrorParser)
        let log_content = tokio::fs::read_to_string(&temp_log)
            .await
            .unwrap_or_default();
        let friendly = crate::service::build_error::parse(&log_content);
        return Err(AppError::system(friendly));
    }

    // 拷贝 dist → {DIST_TARGET_DIR}/{projectId}/dist/ (Rust fs, 无 rm -rf shell;
    // 错误为类型化 io::Error, 路径经 PathBuf::join 无注入)
    let dst = state
        .config
        .dist_target_dir
        .join(&q.project_id)
        .join("dist");
    let src = path.join("dist");
    if !src.exists() {
        return Err(AppError::business("build produced no dist directory"));
    }
    let src2 = src.clone();
    let dst2 = dst.clone();
    tokio::task::spawn_blocking(move || copy_dir_all(&src2, &dst2))
        .await
        .map_err(|e| AppError::system(format!("copy dist join: {e}")))??;
    Ok(Json(response::BuildDone {
        success: true,
        message: "Build completed".to_string(),
        project_id: q.project_id.clone(),
    }))
}

/// build basePath 规范化 (对齐 nuwax: 补首尾 `/`; vite --base 需尾斜杠)。
fn normalize_build_base(b: Option<&str>) -> String {
    let b = b.map(str::trim).unwrap_or("/").to_string();
    if b.is_empty() {
        return "/".to_string();
    }
    let mut s = if b.starts_with('/') {
        b
    } else {
        format!("/{b}")
    };
    if !s.ends_with('/') {
        s.push('/');
    }
    s
}

/// 全局 build 并发: 信号量(全局上限) + 正在构建项目集合(项目级互斥)。
/// 进程级单例 (OnceLock), 对齐 nuwax 模块级 buildingProjects/currentBuilds。
fn build_concurrency(
    max: usize,
) -> (
    &'static tokio::sync::Semaphore,
    &'static std::sync::Mutex<std::collections::HashSet<String>>,
) {
    use std::sync::OnceLock;
    static SEM: OnceLock<tokio::sync::Semaphore> = OnceLock::new();
    static BUILDING: OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        OnceLock::new();
    let sem = SEM.get_or_init(|| tokio::sync::Semaphore::new(max.max(1)));
    let building = BUILDING.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()));
    (sem, building)
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
    /// 必填校验 (对齐 nuwax buildRoutes projectId 校验; handler 不读, 仅供 serde 强制存在)
    #[allow(dead_code)]
    project_id: String,
    error_message: String,
}

/// `POST /api/build/parse-build-error` (对齐 nuwax BuildErrorParser)。
async fn parse_build_error(
    State(_state): State<AppState>,
    Json(body): Json<ParseErrorBody>,
) -> Result<Json<response::Simple>, AppError> {
    let msg = crate::service::build_error::parse(&body.error_message);
    Ok(Json(response::Simple {
        success: true,
        message: msg,
    }))
}

// ── 日志缓存接口 (对齐 nuwax logCacheManager; Rust 无缓存层, 返回固定 stats) ────

/// `GET /api/build/get-log-cache-stats` (对齐 nuwax; Rust 无缓存层 → 返回 disabled 形态)。
async fn get_log_cache_stats(State(_state): State<AppState>) -> Json<response::LogCacheStats> {
    Json(response::LogCacheStats {
        success: true,
        message: "Get log cache statistics successfully".to_string(),
        stats: response::LogCacheStatsData {
            enabled: false,
            cache_size: 0,
            max_cache_entries: 100,
            cache_duration: 300000,
            max_file_size_mb: "0.00".to_string(),
            total_cache_size_mb: "0.00".to_string(),
            node_env: std::env::var("NODE_ENV").unwrap_or_else(|_| "development".to_string()),
            log_cache_enabled: std::env::var("LOG_CACHE_ENABLED")
                .unwrap_or_else(|_| "false".to_string()),
        },
    })
}

/// `GET /api/build/clear-all-log-cache` (对齐 nuwax; Rust 无缓存层 → no-op)。
async fn clear_all_log_cache(State(_state): State<AppState>) -> Json<response::Simple> {
    Json(response::Simple {
        success: true,
        message: "All log caches have been cleared".to_string(),
    })
}
