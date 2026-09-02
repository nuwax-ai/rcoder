//! Pingora 代理状态 + userApp 工具族 307 文档接口（从 router.rs 拆出）。

use std::sync::Arc;

use crate::router::AppState;
use axum::Router;
use axum::routing::get;

use crate::handler;

pub(super) fn proxy_api_routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/proxy/status", get(handler::proxy_status))
        .route("/proxy/stats", get(handler::proxy_stats))
        .route("/proxy/config", get(handler::proxy_config))
        // userApp 代理族 307 文档接口（实际流量走 Pingora 8088；路径与 Pingora
        // 真实路由同形态 /api/v1/userapp/proxy/{tool}/{stage}/...——Swagger 显示
        // 即可拼接形态；stage 段 dev/prod 统一，切环境只改一段）
        // 开发域（UserappBuilder 开发容器）：ttyd/vnc/audio/ime/dbx
        .route(
            "/api/v1/userapp/proxy/ttyd/dev/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ttyd),
        )
        .route(
            "/api/v1/userapp/proxy/ttyd/dev/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_ttyd_redirect_root),
        )
        .route(
            "/api/v1/userapp/proxy/vnc/dev/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_vnc),
        )
        .route(
            "/api/v1/userapp/proxy/vnc/dev/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_vnc_redirect_root),
        )
        .route(
            "/api/v1/userapp/proxy/audio/dev/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_audio),
        )
        .route(
            "/api/v1/userapp/proxy/audio/dev/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_audio_redirect_root),
        )
        .route(
            "/api/v1/userapp/proxy/ime/dev/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ime),
        )
        .route(
            "/api/v1/userapp/proxy/ime/dev/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_ime_redirect_root),
        )
        .route(
            "/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_dev_dbx),
        )
        .route(
            "/api/v1/userapp/proxy/dbx/dev/{user_id}/{app_id}",
            get(handler::proxy_to_dev_dbx_redirect_root),
        )
        // 生产域（运行容器，部署后的生产环境）：ttyd/dbx
        .route(
            "/api/v1/userapp/proxy/ttyd/prod/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_runtime_ttyd),
        )
        .route(
            "/api/v1/userapp/proxy/ttyd/prod/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_runtime_ttyd_redirect_root),
        )
        .route(
            "/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_prod_dbx),
        )
        .route(
            "/api/v1/userapp/proxy/dbx/prod/{user_id}/{app_id}",
            get(handler::proxy_to_prod_dbx_redirect_root),
        )
        .route("/userapp/routes", get(handler::userapp_proxy_routes_doc))
        .with_state(state)
}
