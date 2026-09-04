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
        .route("/api/v1/userapp/query", post(handlers::query_apps))
        .route("/api/v1/userapp/runtime", get(handlers::list_app_runtimes))
        .route("/api/v1/userapp/{app_id}", get(handlers::get_app))
        .route(
            "/api/v1/userapp/{app_id}/update",
            post(handlers::update_app),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/delete",
            post(handlers::delete_app),
        )
        .route(
            "/api/v1/userapp/{app_id}/delete/app",
            post(handlers::purge_app),
        )
        // 应用操作
        .route("/api/v1/userapp/{app_id}/start", post(handlers::start_app))
        .route("/api/v1/userapp/{app_id}/stop", post(handlers::stop_app))
        .route(
            "/api/v1/userapp/{app_id}/restart",
            post(handlers::restart_app),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/recycle-policy",
            post(handlers::set_recycle_policy),
        )
        // 查询接口
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/logs/sources/query",
            post(handlers::query_app_log_sources),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/logs/query",
            post(handlers::query_app_logs),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/logs/stream",
            post(handlers::stream_app_logs_v1),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/health",
            get(handlers::get_app_health),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/stats",
            get(handlers::get_app_stats),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/events",
            get(handlers::get_app_events),
        )
        // 文件管理（app_stage 显式分派：dev=开发容器 workspace / prod=运行容器 /app）
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/upload",
            post(handlers::upload_file),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/upload-from-url",
            post(handlers::upload_from_url),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/files",
            get(handlers::list_files),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/files/delete",
            post(handlers::delete_file),
        )
        // 持久存储管理（v2 §5.4；app_stage=dev 为开发卷/开发环境语义）
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/storage",
            get(handlers::get_app_storage),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/storage/clear",
            post(handlers::clear_app_storage),
        )
        .route(
            "/api/v1/userapp/{app_id}/{app_stage}/storage/destroy",
            post(handlers::destroy_app_storage),
        )
        .route(
            "/api/v1/userapp/storage/{app_stage}/query",
            post(handlers::query_storage),
        )
}
