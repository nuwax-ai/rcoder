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
        // userApp 工具族 307 文档接口（实际流量走 Pingora 8088；stage 段 dev/prod 统一：
        // 此处提供 Swagger 文档 + 可直接调用的重定向语义，对齐 devapp 先例）
        // 开发域（UserappBuilder 开发容器）：ttyd/vnc/audio/ime/dbx
        .route(
            "/userapp/dev/ttyd/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ttyd),
        )
        .route(
            "/userapp/dev/ttyd/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_ttyd_redirect_root),
        )
        .route(
            "/userapp/dev/vnc/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_vnc),
        )
        .route(
            "/userapp/dev/vnc/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_vnc_redirect_root),
        )
        .route(
            "/userapp/dev/audio/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_audio),
        )
        .route(
            "/userapp/dev/audio/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_audio_redirect_root),
        )
        .route(
            "/userapp/dev/ime/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_ime),
        )
        .route(
            "/userapp/dev/ime/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_ime_redirect_root),
        )
        .route(
            "/userapp/dev/dbx/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_dev_dbx),
        )
        .route(
            "/userapp/dev/dbx/{user_id}/{app_id}",
            get(handler::proxy_to_dev_dbx_redirect_root),
        )
        // 生产域（运行容器，部署后的生产环境）：ttyd/pgweb/dbx
        .route(
            "/userapp/prod/ttyd/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_runtime_ttyd),
        )
        .route(
            "/userapp/prod/ttyd/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_runtime_ttyd_redirect_root),
        )
        .route(
            "/userapp/prod/pgweb/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_userapp_runtime_pgweb),
        )
        .route(
            "/userapp/prod/pgweb/{user_id}/{app_id}",
            get(handler::proxy_to_userapp_runtime_pgweb_redirect_root),
        )
        .route(
            "/userapp/prod/dbx/{user_id}/{app_id}/{*path}",
            get(handler::proxy_to_prod_dbx),
        )
        .route(
            "/userapp/prod/dbx/{user_id}/{app_id}",
            get(handler::proxy_to_prod_dbx_redirect_root),
        )
        .route("/userapp/routes", get(handler::userapp_proxy_routes_doc))
        .with_state(state)
}
