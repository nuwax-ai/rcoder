//! `/api/git` 路由 (对齐 nuwax gitRoutes; gix 操作经 spawn_blocking 调用)。
//!
//! 拆分: [`read`] (branches / tags / log / file-content / status) / [`write`]
//! (init / add / commit / unstage / discard / diff / reset / checkout / revert) /
//! [`refs`] (branch-create / branch-delete / branch-switch / tag-create / tag-delete)。
//! 本 mod.rs 装 router + 共享 base 结构 (GitQuery / GitWriteBody) + 路径解析 helper。

use std::path::PathBuf;

use axum::Router;
use axum::routing::{get, post};
use serde::Deserialize;

use crate::AppState;
use crate::error::AppError;
use crate::service::git;
use crate::workspace::{ComputerContext, ProjectContext};

mod read;
mod refs;
mod write;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/branches", get(read::branches))
        .route("/tags", get(read::tags))
        .route("/log", get(read::log_history))
        .route("/file-content", post(read::file_content))
        .route("/status", get(read::status))
        .route("/init", post(write::init))
        .route("/add", post(write::add))
        .route("/commit", post(write::commit))
        .route("/unstage", post(write::unstage))
        .route("/discard", post(write::discard))
        .route("/diff", post(write::diff))
        .route("/reset", post(write::reset))
        .route("/checkout", post(write::checkout))
        .route("/revert", post(write::revert))
        .route("/branch-create", post(refs::branch_create))
        .route("/branch-delete", post(refs::branch_delete))
        .route("/branch-switch", post(refs::branch_switch))
        .route("/tag-create", post(refs::tag_create))
        .route("/tag-delete", post(refs::tag_delete))
}

// ── 共享 base 结构 + 路径解析 (子模块经 super:: 访问) ─────────────────────────────

/// GET 路由公共查询 (workspaceType + project/computer 标识 + 多租户)。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GitQuery {
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
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GitWriteBody {
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
