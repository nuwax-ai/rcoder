//! 应用管理路由定义

use std::sync::Arc;

use axum::{
    Router,
    routing::{get, post},
};

use super::handlers::AppManagerState;

use super::handlers;

/// 创建应用管理路由
pub fn app_manager_routes() -> Router<Arc<AppManagerState>> {
    Router::new()
        // 应用生命周期
        .route("/api/v1/apps", post(handlers::create_app))
        .route("/api/v1/apps/query", post(handlers::query_apps))
        .route("/api/v1/apps/runtime", get(handlers::list_app_runtimes))
        .route("/api/v1/apps/{app_id}", get(handlers::get_app))
        .route("/api/v1/apps/{app_id}/update", post(handlers::update_app))
        .route("/api/v1/apps/{app_id}/delete", post(handlers::delete_app))
        // 应用操作
        .route("/api/v1/apps/{app_id}/start", post(handlers::start_app))
        .route("/api/v1/apps/{app_id}/stop", post(handlers::stop_app))
        .route("/api/v1/apps/{app_id}/restart", post(handlers::restart_app))
        .route(
            "/api/v1/apps/{app_id}/recycle-policy",
            post(handlers::set_recycle_policy),
        )
        .route(
            "/api/v1/apps/{app_id}/releases/prepare",
            post(handlers::prepare_release),
        )
        .route(
            "/api/v1/apps/{app_id}/releases/rollback",
            post(handlers::rollback_release),
        )
        .route(
            "/api/v1/apps/{app_id}/releases/{release_id}/activate",
            post(handlers::activate_release),
        )
        .route(
            "/api/v1/apps/{app_id}/releases",
            get(handlers::list_releases),
        )
        .route(
            "/api/v1/apps/{app_id}/releases/{release_id}/delete",
            post(handlers::delete_release),
        )
        // 查询接口
        .route(
            "/api/v1/apps/{app_id}/logs/sources/query",
            post(handlers::query_app_log_sources),
        )
        .route(
            "/api/v1/apps/{app_id}/logs/query",
            post(handlers::query_app_logs),
        )
        .route(
            "/api/v1/apps/{app_id}/logs/stream",
            post(handlers::stream_app_logs_v1),
        )
        .route(
            "/api/v1/apps/{app_id}/health",
            get(handlers::get_app_health),
        )
        .route("/api/v1/apps/{app_id}/stats", get(handlers::get_app_stats))
        .route(
            "/api/v1/apps/{app_id}/events",
            get(handlers::get_app_events),
        )
        // 文件管理
        .route("/api/v1/apps/{app_id}/upload", post(handlers::upload_file))
        .route(
            "/api/v1/apps/{app_id}/upload-from-url",
            post(handlers::upload_from_url),
        )
        .route("/api/v1/apps/{app_id}/files", get(handlers::list_files))
        .route(
            "/api/v1/apps/{app_id}/files/delete",
            post(handlers::delete_file),
        )
        // 持久存储管理（v2 §5.4）
        .route(
            "/api/v1/apps/{app_id}/storage",
            get(handlers::get_app_storage),
        )
        .route(
            "/api/v1/apps/{app_id}/storage/clear",
            post(handlers::clear_app_storage),
        )
        .route(
            "/api/v1/apps/{app_id}/storage/destroy",
            post(handlers::destroy_app_storage),
        )
        .route("/api/v1/apps/storage/query", post(handlers::query_storage))
        // 数据库管理（app-runtime 镜像单容器自带 PG；rcoder exec psql 操作）
        .route(
            "/api/v1/apps/{app_id}/db/reset-password",
            post(handlers::reset_db_password),
        )
        .route(
            "/api/v1/apps/{app_id}/db/create-database",
            post(handlers::create_database),
        )
}
