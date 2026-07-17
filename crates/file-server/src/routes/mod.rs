//! 路由层。

pub mod build;
pub mod computer;
pub mod git;
pub mod project;
pub mod static_files;

use axum::Router;

use crate::AppState;

/// `/api/project` + `/api/code` 路由 (nuwax projectRoutes + codeRoutes 同挂 `/api/project`)。
pub fn project_api_router() -> Router<AppState> {
    project::router()
}

/// `/api/git` 路由 (对齐 nuwax gitRoutes)。
pub fn git_router() -> Router<AppState> {
    git::router()
}

/// `/api/build` 路由 (对齐 nuwax buildRoutes; dev server 生命周期 + 端口池 + 日志)。
pub fn build_router() -> Router<AppState> {
    build::router()
}

/// `/api/computer` 路由 (对齐 nuwax computerRoutes; 含 static 子路由)。
pub fn computer_router() -> Router<AppState> {
    computer::router().merge(static_files::computer_static_route())
}

/// `/api/page` 路由 (对齐 nuwax server.js 顶层 `/api/page/static`)。
pub fn page_router() -> Router<AppState> {
    static_files::page_router()
}
