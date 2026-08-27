//! 日志读取与缓存 handlers: get-dev-log / get-log-cache-stats / clear-all-log-cache。

use axum::extract::State;
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::dev_server::log::LogLine;

// ── 类型化响应 ────────────────────────────────────────────────────────────────

/// get-dev-log
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevLog {
    pub success: bool,
    pub message: String,
    pub logs: Vec<LogLine>,
    pub total_lines: usize,
    pub start_index: usize,
    pub log_file_name: String,
    pub cache_hit: bool,
    pub file_too_large: bool,
}

/// parse-build-error / clear-all-log-cache 共用 {success, message}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Simple {
    pub success: bool,
    pub message: String,
}

/// get-log-cache-stats (stats 内含 SCREAMING_SNAKE 键 → 逐字段 rename)
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogCacheStats {
    pub success: bool,
    pub message: String,
    pub stats: LogCacheStatsData,
}

#[derive(Serialize)]
pub(crate) struct LogCacheStatsData {
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

// ── Query ─────────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DevLogQuery {
    /// 项目 ID
    pub(crate) project_id: String,
    /// 日志起始行号 (1 起; 缺省 1 即从头读取)
    #[serde(default = "default_start_index")]
    start_index: usize,
    /// 日志类型: `temp`(运行日志,默认) / `app`(应用自定义日志)
    #[serde(default = "default_log_type")]
    log_type: String,
}
fn default_start_index() -> usize {
    1
}
fn default_log_type() -> String {
    "temp".to_string()
}

// ── Handlers ──────────────────────────────────────────────────────────────────

/// 读取开发日志
///
/// 对齐 nuwax get-dev-log。
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
) -> Result<Json<DevLog>, AppError> {
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
    Ok(Json(DevLog {
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

// ── 日志缓存接口 (对齐 nuwax logCacheManager; Rust 无缓存层, 返回固定 stats) ────

/// 查询日志缓存统计
///
/// 对齐 nuwax logCacheManager.getStats。
#[utoipa::path(
    get,
    path = "/get-log-cache-stats",
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn get_log_cache_stats(
    State(state): State<AppState>,
) -> Result<Json<LogCacheStats>, AppError> {
    let stats = state.log_cache.stats()?;
    Ok(Json(LogCacheStats {
        success: true,
        message: "Get log cache statistics successfully".to_string(),
        stats: LogCacheStatsData {
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

/// 清空全部日志缓存
///
/// 对齐 nuwax logCacheManager.clear。
#[utoipa::path(
    get,
    path = "/clear-all-log-cache",
    responses(crate::openapi::JsonApiResponses),
    tag = "Build"
)]
pub(crate) async fn clear_all_log_cache(
    State(state): State<AppState>,
) -> Result<Json<Simple>, AppError> {
    state.log_cache.clear()?;
    Ok(Json(Simple {
        success: true,
        message: "All log caches have been cleared".to_string(),
    }))
}
