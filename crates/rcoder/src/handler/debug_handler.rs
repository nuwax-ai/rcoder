//! 调试 API 处理器
//!
//! 提供用于问题排查的调试接口
//!
//! ⚠️ 警告：这些接口仅用于开发和调试，生产环境应禁用或添加权限控制

use axum::{Json, extract::State, http::HeaderMap};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{debug, error, warn};
use utoipa::ToSchema;

use crate::router::AppState;
use shared_types::{
    HttpResult,
    error_codes::{ERR_INTERNAL_SERVER_ERROR, ERR_INVALID_PARAMS},
    get_i18n_message,
};

use super::utils::get_locale_from_headers;

/// DuckDB 查询请求
#[derive(Debug, Deserialize, Serialize, ToSchema)]
pub struct DebugSqlQueryRequest {
    /// SQL 查询语句
    ///
    /// 支持的表：
    /// - `projects`: 项目记录表
    /// - `containers`: 容器记录表
    ///
    /// 示例查询：
    /// - `SELECT * FROM projects`
    /// - `SELECT * FROM containers`
    /// - `SELECT project_id, session_id, container_id FROM projects WHERE session_id = 'xxx'`
    #[schema(example = "SELECT * FROM projects LIMIT 10")]
    pub sql: String,
}

/// DuckDB 查询响应
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugSqlQueryResponse {
    /// 查询结果的列名
    pub columns: Vec<String>,
    /// 查询结果的行数据（每行是一个列名到值的映射）
    pub rows: Vec<serde_json::Value>,
    /// 返回的行数
    pub row_count: usize,
    /// 执行时间（毫秒）
    pub execution_time_ms: u64,
}

/// 存储统计信息响应
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugStorageStatsResponse {
    /// 项目总数
    pub total_projects: usize,
    /// 容器总数
    pub total_containers: usize,
    /// 活跃会话数
    pub active_sessions: usize,
    /// 按服务类型统计的项目数
    pub projects_by_service_type: std::collections::HashMap<String, usize>,
}

/// 执行 DuckDB SQL 查询（调试用）
///
/// ⚠️ 此接口仅用于开发和调试目的
///
/// 支持任意 SELECT 查询，用于查看内存数据库中的数据状态
#[utoipa::path(
    post,
    path = "/debug/sql",
    request_body(
        content = DebugSqlQueryRequest,
        description = "SQL 查询请求",
        content_type = "application/json"
    ),
    responses(
        (
            status = 200,
            description = "查询成功",
            body = HttpResult<DebugSqlQueryResponse>,
            example = json!({
                "success": true,
                "data": {
                    "columns": ["project_id", "session_id", "container_id"],
                    "rows": [
                        {"project_id": "user_123", "session_id": "xxx", "container_id": "yyy"}
                    ],
                    "row_count": 1,
                    "execution_time_ms": 5
                },
                "code": "0"
            })
        ),
        (
            status = 400,
            description = "SQL 语法错误或不支持的操作",
            body = HttpResult<String>
        )
    ),
    tag = "debug",
    operation_id = "debug_sql_query",
    summary = "执行 DuckDB SQL 查询（调试用）",
    description = "执行任意 SELECT 查询，用于查看内存数据库中的数据状态。\n\n⚠️ 仅用于开发和调试，生产环境应禁用此接口。"
)]
#[axum::debug_handler]
pub async fn debug_sql_query(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<DebugSqlQueryRequest>,
) -> HttpResult<DebugSqlQueryResponse> {
    let locale = get_locale_from_headers(&headers);
    let start_time = std::time::Instant::now();

    debug!("[DEBUG_SQL] executing: {}", request.sql);

    // 安全检查：Only SELECT queries are allowed
    let sql_trimmed = request.sql.trim().to_uppercase();
    if !sql_trimmed.starts_with("SELECT") {
        warn!("[DEBUG_SQL] non-SELECT query: {}", request.sql);
        return HttpResult::error_with_message(
            ERR_INVALID_PARAMS,
            locale,
            &get_i18n_message("error.select_only", locale),
        );
    }

    // 执行查询
    match execute_sql_query(&state.projects, &request.sql) {
        Ok((columns, rows)) => {
            let row_count = rows.len();
            let execution_time_ms = start_time.elapsed().as_millis() as u64;

            debug!(
                "✅ [DEBUG_SQL] 查询成功: {} 行, {} ms",
                row_count, execution_time_ms
            );

            HttpResult::success(DebugSqlQueryResponse {
                columns,
                rows,
                row_count,
                execution_time_ms,
            })
        }
        Err(e) => {
            error!("[DEBUG_SQL] Query failed: {}", e);
            HttpResult::error_with_message(
                ERR_INTERNAL_SERVER_ERROR,
                locale,
                &format!(
                    "{}: {}",
                    get_i18n_message("error.query_execution_failed", locale),
                    e
                ),
            )
        }
    }
}

/// 获取存储统计信息（调试用）
#[utoipa::path(
    get,
    path = "/debug/storage/stats",
    responses(
        (
            status = 200,
            description = "获取统计信息成功",
            body = HttpResult<DebugStorageStatsResponse>
        )
    ),
    tag = "debug",
    operation_id = "debug_storage_stats",
    summary = "获取 DuckDB 存储统计信息（调试用）"
)]
#[axum::debug_handler]
pub async fn debug_storage_stats(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
) -> HttpResult<DebugStorageStatsResponse> {
    let stats = state.projects.get_stats();

    let mut projects_by_service_type = std::collections::HashMap::new();
    for (st, count) in stats.projects_by_service_type {
        projects_by_service_type.insert(st.to_string(), count);
    }

    HttpResult::success(DebugStorageStatsResponse {
        total_projects: stats.total_projects,
        total_containers: stats.total_containers,
        active_sessions: stats.active_sessions,
        projects_by_service_type,
    })
}

/// 快捷查询：获取所有项目
#[utoipa::path(
    get,
    path = "/debug/projects",
    responses(
        (
            status = 200,
            description = "获取项目列表成功",
            body = HttpResult<DebugSqlQueryResponse>
        )
    ),
    tag = "debug",
    operation_id = "debug_list_projects",
    summary = "获取所有项目记录（调试用）"
)]
#[axum::debug_handler]
pub async fn debug_list_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> HttpResult<DebugSqlQueryResponse> {
    let locale = get_locale_from_headers(&headers);
    let start_time = std::time::Instant::now();
    let sql = "SELECT project_id, session_id, service_type, container_id, user_id, agent_status_name, created_at, last_activity FROM projects ORDER BY last_activity DESC";

    match execute_sql_query(&state.projects, sql) {
        Ok((columns, rows)) => HttpResult::success(DebugSqlQueryResponse {
            columns,
            row_count: rows.len(),
            rows,
            execution_time_ms: start_time.elapsed().as_millis() as u64,
        }),
        Err(e) => HttpResult::error_with_message(
            ERR_INTERNAL_SERVER_ERROR,
            locale,
            &format!(
                "{}: {}",
                get_i18n_message("error.query_execution_failed", locale),
                e
            ),
        ),
    }
}

/// 快捷查询：获取所有容器
#[utoipa::path(
    get,
    path = "/debug/containers",
    responses(
        (
            status = 200,
            description = "获取容器列表成功",
            body = HttpResult<DebugSqlQueryResponse>
        )
    ),
    tag = "debug",
    operation_id = "debug_list_containers",
    summary = "获取所有容器记录（调试用）"
)]
#[axum::debug_handler]
pub async fn debug_list_containers(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> HttpResult<DebugSqlQueryResponse> {
    let locale = get_locale_from_headers(&headers);
    let start_time = std::time::Instant::now();
    let sql = "SELECT container_id, container_name, container_ip, service_type, status, created_at, last_activity FROM containers ORDER BY last_activity DESC";

    match execute_sql_query(&state.projects, sql) {
        Ok((columns, rows)) => HttpResult::success(DebugSqlQueryResponse {
            columns,
            row_count: rows.len(),
            rows,
            execution_time_ms: start_time.elapsed().as_millis() as u64,
        }),
        Err(e) => HttpResult::error_with_message(
            ERR_INTERNAL_SERVER_ERROR,
            locale,
            &format!(
                "{}: {}",
                get_i18n_message("error.query_execution_failed", locale),
                e
            ),
        ),
    }
}

/// 执行 SQL 查询的内部函数
fn execute_sql_query(
    adapter: &crate::storage::ProjectAdapter,
    sql: &str,
) -> Result<(Vec<String>, Vec<serde_json::Value>), String> {
    // 获取底层 DuckDB 存储
    adapter.execute_raw_query(sql)
}
