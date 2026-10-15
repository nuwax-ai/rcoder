//! `/api/build` HTTP handlers (对齐 nuwax buildRoutes; dev server 经 DevServerManager)。
//!
//! 拆分: [`dev`] (dev server 生命周期 6 路由) / [`logs`] (日志读取与缓存 3 路由) /
//! [`build_exec`] (build 执行)。本 mod.rs 仅做模块声明 + 共享辅助；
//! 共享 Query (BuildQuery) 定义在 [`crate::models`]。

pub(crate) mod build_exec;
pub(crate) mod dev;
pub(crate) mod logs;

use std::path::PathBuf;

use crate::AppState;
use crate::error::AppResult;
use crate::models::BuildQuery;
use crate::workspace::ProjectContext;

/// 解析项目绝对路径 (dev/build handler 共享)。
pub(crate) async fn project_path(state: &AppState, q: &BuildQuery) -> AppResult<PathBuf> {
    if let Some(app_id) = q.app_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        return crate::workspace::resolve_userapp_dev(app_id, None, &state.config);
    }
    state
        .resolver
        .resolve_project(&ProjectContext {
            project_id: q.project_id.clone(),
            tenant_id: q.tenant_id.clone(),
            space_id: q.space_id.clone(),
            isolation_type: q.isolation_type.clone(),
        })
        .await
}
