//! `/api/build` HTTP handlers (对齐 nuwax buildRoutes; dev server 经 DevServerManager)。
//!
//! 拆分: [`dev`] (dev server 生命周期 6 路由) / [`logs`] (日志读取与缓存 3 路由) /
//! [`build_exec`] (build 执行)。本 mod.rs 仅做模块声明 + 共享 Query/辅助。

pub(crate) mod build_exec;
pub(crate) mod dev;
pub(crate) mod logs;

use std::path::PathBuf;

use serde::Deserialize;

use crate::AppState;
use crate::error::AppResult;
use crate::workspace::ProjectContext;

/// 多 handler 共用的项目查询参数 (start/stop/restart/build)。
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BuildQuery {
    pub(crate) project_id: String,
    /// UserApp 开发卷定位 (可选): 传 appId 时 workspace 走 UserApp 开发卷
    /// (`{USERAPP_WORKSPACE_DIR}/{appId}`), 与 projectId 定位二选一。
    #[serde(default)]
    pub(crate) app_id: Option<String>,
    #[serde(default)]
    pub(crate) pid: Option<String>,
    #[serde(default)]
    pub(crate) base_path: Option<String>,
    // 多租户隔离参数 (透传给 ProjectContext)
    #[serde(default)]
    pub(crate) tenant_id: Option<String>,
    #[serde(default)]
    pub(crate) space_id: Option<String>,
    #[serde(default)]
    pub(crate) isolation_type: Option<String>,
}

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
