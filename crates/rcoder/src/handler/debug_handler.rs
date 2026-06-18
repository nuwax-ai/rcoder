//! 调试 API 处理器
//!
//! 提供用于问题排查的调试接口
//!
//! ⚠️ 警告：这些接口仅用于开发和调试，生产环境应禁用或添加权限控制

use axum::{extract::State, http::HeaderMap};
use serde::Serialize;
use std::sync::Arc;
use tracing::info;
use utoipa::ToSchema;

use crate::router::AppState;
use shared_types::HttpResult;

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

/// 存储摘要响应（替代 SQL raw query）
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugDumpResponse {
    /// 存储摘要
    pub summary: String,
    /// 项目列表（简化信息）
    pub projects: Vec<DebugProjectInfo>,
    /// 容器列表（简化信息）
    pub containers: Vec<DebugContainerInfo>,
}

/// 调试用的项目简化信息
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugProjectInfo {
    /// 项目ID
    pub project_id: String,
    /// 会话ID
    pub session_id: Option<String>,
    /// 用户ID
    pub user_id: Option<String>,
    /// 服务类型
    pub service_type: Option<String>,
    /// 容器ID
    pub container_id: Option<String>,
    /// Agent 状态名称
    pub agent_status_name: Option<String>,
    /// 创建时间
    pub created_at: String,
    /// 最后活动时间
    pub last_activity: String,
}

/// 调试用的容器简化信息
#[derive(Debug, Serialize, ToSchema)]
pub struct DebugContainerInfo {
    /// 容器ID
    pub container_id: String,
    /// 容器名称
    pub container_name: String,
    /// 容器IP
    pub container_ip: String,
    /// 状态
    pub status: String,
    /// 服务URL
    pub service_url: String,
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
    summary = "获取存储统计信息（调试用）"
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
            body = HttpResult<Vec<DebugProjectInfo>>
        )
    ),
    tag = "debug",
    operation_id = "debug_list_projects",
    summary = "获取所有项目记录（调试用）"
)]
#[axum::debug_handler]
pub async fn debug_list_projects(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
) -> HttpResult<Vec<DebugProjectInfo>> {
    let projects = state.projects.iter();

    let result: Vec<DebugProjectInfo> = projects
        .into_iter()
        .map(|(pid, info)| DebugProjectInfo {
            project_id: pid.clone(),
            session_id: info.session_id().map(|s| s.to_string()),
            user_id: info.user_id().map(|s| s.to_string()),
            service_type: info.service_type().map(|st| st.to_string()),
            container_id: info.container().map(|c| c.container_id.clone()),
            agent_status_name: info.status().map(|s| format!("{:?}", s)),
            created_at: info.created_at().to_rfc3339(),
            last_activity: info.last_activity().to_rfc3339(),
        })
        .collect();

    info!("✅ [DEBUG_PROJECTS] Listed {} projects", result.len());

    HttpResult::success(result)
}

/// 快捷查询：获取所有容器
#[utoipa::path(
    get,
    path = "/debug/containers",
    responses(
        (
            status = 200,
            description = "获取容器列表成功",
            body = HttpResult<Vec<DebugContainerInfo>>
        )
    ),
    tag = "debug",
    operation_id = "debug_list_containers",
    summary = "获取所有容器记录（调试用）"
)]
#[axum::debug_handler]
pub async fn debug_list_containers(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
) -> HttpResult<Vec<DebugContainerInfo>> {
    let containers = state.projects.get_all_container_records();

    let result: Vec<DebugContainerInfo> = containers
        .iter()
        .map(|c| DebugContainerInfo {
            container_id: c.container_id.clone(),
            container_name: c.container_name.clone(),
            container_ip: c.container_ip.clone(),
            status: c.status.clone(),
            service_url: c.service_url.clone(),
        })
        .collect();

    info!("✅ [DEBUG_CONTAINERS] Listed {} containers", result.len());

    HttpResult::success(result)
}

/// 获取存储完整摘要（调试用）
#[utoipa::path(
    get,
    path = "/debug/sql",
    responses(
        (
            status = 200,
            description = "获取存储摘要成功",
            body = HttpResult<DebugDumpResponse>
        )
    ),
    tag = "debug",
    operation_id = "debug_dump_summary",
    summary = "获取存储摘要（调试用）",
    description = "返回存储中所有项目和容器的完整摘要信息，替代原有的 SQL raw query 接口。"
)]
#[axum::debug_handler]
pub async fn debug_dump_summary(
    State(state): State<Arc<AppState>>,
    _headers: HeaderMap,
) -> HttpResult<DebugDumpResponse> {
    let summary = state.projects.dump_summary();

    let projects: Vec<DebugProjectInfo> = state
        .projects
        .iter()
        .into_iter()
        .map(|(pid, info)| DebugProjectInfo {
            project_id: pid.clone(),
            session_id: info.session_id().map(|s| s.to_string()),
            user_id: info.user_id().map(|s| s.to_string()),
            service_type: info.service_type().map(|st| st.to_string()),
            container_id: info.container().map(|c| c.container_id.clone()),
            agent_status_name: info.status().map(|s| format!("{:?}", s)),
            created_at: info.created_at().to_rfc3339(),
            last_activity: info.last_activity().to_rfc3339(),
        })
        .collect();

    let containers: Vec<DebugContainerInfo> = state
        .projects
        .get_all_container_records()
        .iter()
        .map(|c| DebugContainerInfo {
            container_id: c.container_id.clone(),
            container_name: c.container_name.clone(),
            container_ip: c.container_ip.clone(),
            status: c.status.clone(),
            service_url: c.service_url.clone(),
        })
        .collect();

    HttpResult::success(DebugDumpResponse {
        summary,
        projects,
        containers,
    })
}
