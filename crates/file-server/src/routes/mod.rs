//! 路由层。

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
