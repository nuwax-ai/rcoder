//! 应用管理路由定义

use std::sync::Arc;

use axum::{
    routing::{get, post},
    Router,
};

use super::handlers::AppManagerState;

use super::handlers;

/// 创建应用管理路由
pub fn app_manager_routes() -> Router<Arc<AppManagerState>> {
    Router::new()
        // 应用生命周期
        .route("/api/v1/apps", post(handlers::create_app))
        .route("/api/v1/apps/query", post(handlers::query_apps))
        .route("/api/v1/apps/{app_id}", get(handlers::get_app))
        .route("/api/v1/apps/{app_id}/update", post(handlers::update_app))
        .route("/api/v1/apps/{app_id}/delete", post(handlers::delete_app))
        // 应用操作
        .route("/api/v1/apps/{app_id}/start", post(handlers::start_app))
        .route("/api/v1/apps/{app_id}/stop", post(handlers::stop_app))
        .route("/api/v1/apps/{app_id}/restart", post(handlers::restart_app))
        // 查询接口
        .route("/api/v1/apps/{app_id}/logs", get(handlers::get_app_logs))
        .route("/api/v1/apps/{app_id}/health", get(handlers::get_app_health))
        .route("/api/v1/apps/{app_id}/stats", get(handlers::get_app_stats))
        .route("/api/v1/apps/{app_id}/events", get(handlers::get_app_events))
        // 文件管理
        .route("/api/v1/apps/{app_id}/upload", post(handlers::upload_file))
        .route("/api/v1/apps/{app_id}/files", get(handlers::list_files))
        .route("/api/v1/apps/{app_id}/files/delete", post(handlers::delete_file))
}
