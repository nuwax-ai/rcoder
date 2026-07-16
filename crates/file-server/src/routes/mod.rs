//! 路由层。

pub mod build;
pub mod computer;
pub mod git;
pub mod project;

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

/// `/api/computer` 路由 (对齐 nuwax computerRoutes)。
pub fn computer_router() -> Router<AppState> {
    computer::router()
}
