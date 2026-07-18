//! `/api/git` HTTP handlers (对齐 nuwax gitRoutes; gix 操作经 spawn_blocking 调用)。
//!
//! 拆分: [`read`] (branches / tags / log / file-content / status) / [`write`]
//! (init / add / commit / unstage / discard / diff / reset / checkout / revert) /
//! [`refs`] (branch-create / branch-delete / branch-switch / tag-create / tag-delete)。
//! 本 mod.rs 提供共享 base 结构 (GitQuery / GitWriteBody) + 路径解析 helper。

use std::path::PathBuf;

use crate::AppState;
use crate::error::AppError;
use crate::service::git;
use crate::workspace::{ComputerContext, ProjectContext};
use serde::Deserialize;

pub(crate) mod read;
pub(crate) mod refs;
pub(crate) mod write;

// ── 共享 base 结构 + 路径解析 (子模块经 super:: 访问) ─────────────────────────────

/// GET 路由公共查询 (workspaceType + project/computer 标识 + 多租户)。
#[derive(Deserialize, utoipa::IntoParams, utoipa::ToSchema)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitQuery {
    pub workspace_type: Option<String>,
    pub project_id: Option<String>,
    pub user_id: Option<String>,
    pub c_id: Option<String>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

fn project_ctx(q: &GitQuery) -> Option<ProjectContext> {
    Some(ProjectContext {
        project_id: q.project_id.clone()?,
        tenant_id: q.tenant_id.clone(),
        space_id: q.space_id.clone(),
        isolation_type: q.isolation_type.clone(),
    })
}

fn computer_ctx(q: &GitQuery) -> Option<ComputerContext> {
    Some(ComputerContext {
        user_id: q.user_id.clone()?,
        cid: q.c_id.clone()?,
    })
}

/// GET 路由解析: GitQuery → (workspace path, logId)。
pub(super) fn resolve(q: &GitQuery, state: &AppState) -> Result<(PathBuf, String), AppError> {
    let target = git::resolve_target(
        &*state.resolver,
        q.workspace_type.as_deref().unwrap_or(""),
        project_ctx(q).as_ref(),
        computer_ctx(q).as_ref(),
    )?;
    Ok((target.path().to_path_buf(), target.log_id()))
}

/// POST 路由公共 body (写操作基类, 被 FilesBody / CommitBody 等经 serde flatten 复用)。
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GitWriteBody {
    pub workspace_type: String,
    pub project_id: Option<String>,
    pub user_id: Option<String>,
    pub c_id: Option<String>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// POST 路由解析: GitWriteBody → (workspace path, logId)。
pub(super) fn resolve_body(
    state: &AppState,
    body: &GitWriteBody,
) -> Result<(PathBuf, String), AppError> {
    let project_ctx = body.project_id.clone().map(|id| ProjectContext {
        project_id: id,
        tenant_id: body.tenant_id.clone(),
        space_id: body.space_id.clone(),
        isolation_type: body.isolation_type.clone(),
    });
    let computer_ctx = match (&body.user_id, &body.c_id) {
        (Some(u), Some(c)) => Some(ComputerContext {
            user_id: u.clone(),
            cid: c.clone(),
        }),
        _ => None,
    };
    let target = git::resolve_target(
        &*state.resolver,
        &body.workspace_type,
        project_ctx.as_ref(),
        computer_ctx.as_ref(),
    )?;
    Ok((target.path().to_path_buf(), target.log_id()))
}
