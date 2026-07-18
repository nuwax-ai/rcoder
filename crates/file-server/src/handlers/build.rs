//! `/api/build` HTTP handlers (对齐 nuwax buildRoutes; dev server 经 DevServerManager)。
//!
//! 本文件含 dev server 生命周期 + 端口池 + 日志读取 7 路由。
//! `build` / `parse-build-error` / computer 相关路由见后续 task。

use std::path::PathBuf;

use axum::extract::State;
use serde::Deserialize;
use serde_json::Value;

use super::build_support::{copy_dir_all, normalize_build_base};
use crate::AppState;
use crate::error::{AppError, AppResult};
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::pnpm::{self, InstallOptions, LogFiles};
use crate::workspace::ProjectContext;

// ── 类型化响应 (取代 serde_json::json! 字面量; camelCase 由 serde 统一保证) ──────
pub(super) mod response {
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
        pub log_cache_enabled: bool,
    }
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildQuery {
    project_id: String,
    #[serde(default)]
    pid: Option<String>,
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct KeepAliveQuery {
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevLogQuery {
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

fn project_path(state: &AppState, q: &BuildQuery) -> AppResult<PathBuf> {
    state.resolver.resolve_project(&ProjectContext {
        project_id: q.project_id.clone(),
        tenant_id: q.tenant_id.clone(),
        space_id: q.space_id.clone(),
        isolation_type: q.isolation_type.clone(),
    })
}

fn project_path_keep(state: &AppState, q: &KeepAliveQuery) -> AppResult<PathBuf> {
    state.resolver.resolve_project(&ProjectContext {
        project_id: q.project_id.clone(),
        tenant_id: q.tenant_id.clone(),
        space_id: q.space_id.clone(),
        isolation_type: q.isolation_type.clone(),
    })
}

/// `GET /api/build/start-dev` (对齐 nuwax start-dev)。
#[utoipa::path(
    get,
    path = "/start-dev",
    params(BuildQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn start_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<response::DevStarted>, AppError> {
    let path = project_path(&state, &q)?;
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
#[utoipa::path(
    get,
    path = "/stop-dev",
    params(BuildQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn stop_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<response::DevStopped>, AppError> {
    q.pid
        .as_deref()
        .filter(|pid| !pid.trim().is_empty())
        .ok_or_else(|| AppError::validation("Process ID cannot be empty"))?;
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
    Ok(Json(response::DevStopped {
        success: true,
        message: message.to_string(),
        project_id: q.project_id,
        pid: None,
        killed_pids: stopped.killed_pids,
    }))
}

/// `GET /api/build/restart-dev` (对齐 nuwax restart-dev)。
#[utoipa::path(
    get,
    path = "/restart-dev",
    params(BuildQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn restart_dev(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<response::DevStarted>, AppError> {
    let path = project_path(&state, &q)?;
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
#[utoipa::path(
    get,
    path = "/list-dev",
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn list_dev(
    State(state): State<AppState>,
) -> Result<Json<response::DevList>, AppError> {
    let list = state.dev_server.list_dev()?;
    Ok(Json(response::DevList {
        success: true,
        list,
    }))
}

/// `GET /api/build/keep-alive` (对齐 nuwax keep-alive)。
#[utoipa::path(
    get,
    path = "/keep-alive",
    params(KeepAliveQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn keep_alive(
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
    let path = project_path_keep(&state, &q)?;
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
#[utoipa::path(
    get,
    path = "/port-pool-status",
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn port_pool_status(
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
#[utoipa::path(
    get,
    path = "/get-dev-log",
    params(DevLogQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn get_dev_log(
    State(state): State<AppState>,
    Query(q): Query<DevLogQuery>,
) -> Result<Json<response::DevLog>, AppError> {
    let log_dir = crate::service::dev_server::log::log_dir(&state.config, &q.project_id);
    let snapshot = crate::service::dev_server::log::snapshot_dev_log(&log_dir, &q.log_type).await?;
    let mut cache_hit = false;
    let mut file_too_large = false;
    let result = if let Some(snapshot) = snapshot {
        if let Some(cached) = state
            .log_cache
            .get(&q.project_id, &snapshot, q.start_index)?
        {
            cache_hit = true;
            cached
        } else {
            let full = state
                .dev_server
                .read_dev_log(&q.project_id, 1, &q.log_type)
                .await?;
            file_too_large = state.config.log_cache_enabled
                && snapshot.size_bytes > state.config.log_cache_max_file_size_bytes;
            state.log_cache.insert(&q.project_id, snapshot, &full)?;
            crate::service::dev_server::log::slice_log_result(&full, q.start_index)
        }
    } else {
        state
            .dev_server
            .read_dev_log(&q.project_id, q.start_index, &q.log_type)
            .await?
    };
    let message = if cache_hit {
        "Get log successfully (cache)"
    } else if file_too_large {
        "Get log successfully (file too large, not cached)"
    } else {
        "Get log successfully"
    };
    Ok(Json(response::DevLog {
        success: true,
        message: message.to_string(),
        logs: result.logs,
        total_lines: result.total_lines,
        start_index: result.start_index,
        log_file_name: result.log_file_name,
        cache_hit,
        file_too_large,
    }))
}

/// `GET /api/build/build` (对齐 nuwax buildProject): install + build + 拷贝 dist。
#[utoipa::path(
    get,
    path = "/build",
    params(BuildQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn build_project(
    State(state): State<AppState>,
    Query(q): Query<BuildQuery>,
) -> Result<Json<response::BuildDone>, AppError> {
    let path = project_path(&state, &q)?;
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
    // 立即拒绝超容量/同项目重复 build；guard 在所有退出路径自动释放。
    let _build_guard = state.build_manager.try_start(&q.project_id)?;

    // install (对齐 nuwax: 失败则整体 build 失败, 透传 "Dependency installation failed")
    let install_logs = LogFiles::new(&main_log, &temp_log);
    pnpm::install(
        &path,
        &InstallOptions::prefer_offline(),
        Some(&install_logs),
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
    if let Err(build_error) = build_result {
        // 失败: 读 build 日志用 build_error 解析友好消息 (对齐 nuwax BuildErrorParser)
        let log_content = crate::service::fs_util::read_to_string_bounded(
            &temp_log,
            state.config.log_read_max_bytes,
            "build log",
        )
        .await
        .unwrap_or_else(|_| build_error.to_string());
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
        tracing::warn!(project_id = %q.project_id, path = %src.display(), "build produced no dist directory");
        return Ok(Json(response::BuildDone {
            success: true,
            message: "Build completed".to_string(),
            project_id: q.project_id.clone(),
        }));
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

// ── 日志缓存接口 (对齐 nuwax logCacheManager; Rust 无缓存层, 返回固定 stats) ────

/// `GET /api/build/get-log-cache-stats` (对齐 nuwax logCacheManager.getStats)。
#[utoipa::path(
    get,
    path = "/get-log-cache-stats",
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn get_log_cache_stats(
    State(state): State<AppState>,
) -> Result<Json<response::LogCacheStats>, AppError> {
    let stats = state.log_cache.stats()?;
    Ok(Json(response::LogCacheStats {
        success: true,
        message: "Get log cache statistics successfully".to_string(),
        stats: response::LogCacheStatsData {
            enabled: stats.enabled,
            cache_size: stats.cache_size,
            max_cache_entries: stats.max_cache_entries,
            cache_duration: stats.cache_duration,
            max_file_size_mb: format!("{:.2}", stats.max_file_size_bytes as f64 / 1_048_576.0),
            total_cache_size_mb: format!(
                "{:.2}",
                stats.total_cache_size_bytes as f64 / 1_048_576.0
            ),
            node_env: std::env::var("NODE_ENV").unwrap_or_else(|_| "development".to_string()),
            log_cache_enabled: stats.enabled,
        },
    }))
}

/// `GET /api/build/clear-all-log-cache` (对齐 nuwax logCacheManager.clear)。
#[utoipa::path(
    get,
    path = "/clear-all-log-cache",
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn clear_all_log_cache(
    State(state): State<AppState>,
) -> Result<Json<response::Simple>, AppError> {
    state.log_cache.clear()?;
    Ok(Json(response::Simple {
        success: true,
        message: "All log caches have been cleared".to_string(),
    }))
}
