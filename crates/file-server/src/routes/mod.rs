//! 路由层。

pub mod build;
pub mod computer;
pub mod git;
pub mod project;
pub mod static_files;

use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::AppState;

/// 业务路由与 OpenAPI 文档的唯一聚合入口。
pub fn api_router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(crate::handler::health::root))
        .routes(routes!(crate::handler::health::health))
        .nest("/api/project", project_api_router())
        .nest("/api/git", git_router())
        .nest("/api/build", build_router())
        .nest("/api/computer", computer_router())
        .nest("/api/page", page_router())
}

/// `/api/project` + `/api/code` 路由 (nuwax projectRoutes + codeRoutes 同挂 `/api/project`)。
pub fn project_api_router() -> OpenApiRouter<AppState> {
    project::router()
}

/// `/api/git` 路由 (对齐 nuwax gitRoutes)。
pub fn git_router() -> OpenApiRouter<AppState> {
    git::router()
}

/// `/api/build` 路由 (对齐 nuwax buildRoutes; dev server 生命周期 + 端口池 + 日志)。
pub fn build_router() -> OpenApiRouter<AppState> {
    build::router()
}

/// `/api/computer` 路由 (对齐 nuwax computerRoutes; 含 static 子路由)。
pub fn computer_router() -> OpenApiRouter<AppState> {
    computer::router().merge(static_files::computer_static_route())
}

/// `/api/page` 路由 (对齐 nuwax server.js 顶层 `/api/page/static`)。
pub fn page_router() -> OpenApiRouter<AppState> {
    static_files::page_router()
}
