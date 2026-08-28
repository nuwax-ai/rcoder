//! Agent Management 路由（从 router.rs 拆出；install 的双层 body 限制注释随迁）。

use std::sync::Arc;

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::routing::post;

use crate::handler;
use crate::router::AppState;

/// 域装配入口（install 双层 body 限制语义见内部注释块）。
pub(super) fn agent_mgmt_routes(state: Arc<AppState>) -> Router {
    // P0-5: Agent Management 路由(全部 POST + body 解析)
    // - 简单 JSON 端点使用 I18nJsonOrQuery(同时支持 JSON body 和 ?project_id=xxx query)
    // - install 端点使用 multipart/form-data(file + metadata JSON 字段)
    //
    // ⚠️ install 路由的 body 限制必须在 Router 层挂,而不是 MethodRouter 层。
    // axum 的 `Multipart` 提取器通过 `with_limited_body()` 读取
    // `DefaultBodyLimitKind` 扩展(Request 上挂的 layer 才生效),`MethodRouter::layer`
    // 出来的 MethodRouter 不携带这个扩展,无法被 multipart 识别。
    // 此外 `RequestBodyLimitLayer` 是 tower 中间件,只读取 Content-Length 头,
    // 对 streaming 的 multipart body 不直接生效,但保留作为 defense-in-depth。
    let install_route = Router::new()
        .route("/agent-mgmt/agents/install", post(handler::install_agent))
        .layer(DefaultBodyLimit::max(1024 * 1024 * 1024))
        .layer(tower_http::limit::RequestBodyLimitLayer::new(
            1024 * 1024 * 1024,
        ));

    Router::new()
        .route("/agent-mgmt/agents/list", post(handler::list_agents))
        .route("/agent-mgmt/agents/get", post(handler::get_agent))
        .route("/agent-mgmt/agents/check", post(handler::check_agent))
        .merge(install_route)
        .route(
            "/agent-mgmt/agents/install-from-url",
            post(handler::install_from_url),
        )
        .route(
            "/agent-mgmt/agents/install-from-npm",
            post(handler::install_from_npm),
        )
        .route(
            "/agent-mgmt/agents/uninstall",
            post(handler::uninstall_agent),
        )
        .with_state(state)
}
