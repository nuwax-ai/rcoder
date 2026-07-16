//! 路由层。

pub mod project;

use axum::Router;

use crate::AppState;

/// `/api/project` + `/api/code` 路由 (nuwax projectRoutes + codeRoutes 同挂 `/api/project`)。
pub fn project_api_router() -> Router<AppState> {
    project::router()
}
