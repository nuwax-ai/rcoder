//! `/api/git` 路由 (对齐 nuwax gitRoutes; gix 操作经 spawn_blocking 调用)。

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::AppError;
use crate::service::git;
use crate::workspace::{ComputerContext, ProjectContext};
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/branches", get(branches))
        .route("/tags", get(tags))
        .route("/log", get(log_history))
        .route("/file-content", post(file_content))
        .route("/status", get(status))
        .route("/init", post(init))
        .route("/add", post(add))
        .route("/commit", post(commit))
        .route("/unstage", post(unstage))
        .route("/discard", post(discard))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitQuery {
    workspace_type: Option<String>,
    project_id: Option<String>,
    user_id: Option<String>,
    c_id: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    isolation_type: Option<String>,
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

fn resolve(q: &GitQuery, state: &AppState) -> Result<(std::path::PathBuf, String), AppError> {
    let target = git::resolve_target(
        &*state.resolver,
        q.workspace_type.as_deref().unwrap_or(""),
        project_ctx(q).as_ref(),
        computer_ctx(q).as_ref(),
    )?;
    Ok((target.path().to_path_buf(), target.log_id()))
}

/// `GET /api/git/branches`
async fn branches(
    State(state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q, &state)?;
    let (branches, current) = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::list_branches(&repo)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    // nuwax: branches 为 {name: {name, current}} 对象
    let branches_obj: serde_json::Map<String, Value> = branches
        .iter()
        .map(|b| {
            let is_cur = Some(b.as_str()) == current.as_deref();
            (b.clone(), json!({ "name": b, "current": is_cur }))
        })
        .collect();
    Ok(Json(json!({
        "success": true,
        "logId": log_id,
        "branches": branches_obj,
        "current": current,
    })))
}

/// `GET /api/git/tags`
async fn tags(
    State(state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q, &state)?;
    let tags = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::list_tags(&repo)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    let latest = tags.last().cloned();
    Ok(Json(json!({ "success": true, "logId": log_id, "tags": tags, "latest": latest })))
}

/// `GET /api/git/log`
async fn log_history(
    State(state): State<AppState>,
    Query(q): Query<GitLogQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q.base, &state)?;
    let max_count = q.max_count.unwrap_or(50).clamp(1, 500);
    let skip = q.skip.unwrap_or(0);
    let commits = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::log_history(&repo, max_count, skip)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    let total = commits.len();
    Ok(Json(json!({ "success": true, "logId": log_id, "commits": commits, "total": total })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitLogQuery {
    #[serde(flatten)]
    base: GitQuery,
    max_count: Option<usize>,
    skip: Option<usize>,
}

/// `POST /api/git/file-content`
async fn file_content(
    State(state): State<AppState>,
    Query(q): Query<FileContentQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q.base, &state)?;
    let ref_spec = q.ref_.clone().unwrap_or_else(|| "HEAD".to_string());
    let file_path = q.file_path.clone();
    let ref_spec_c = ref_spec.clone();
    let file_path_c = file_path.clone();
    let content = tokio::task::spawn_blocking(move || -> Result<String, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        match git::file_content_at_ref(&repo, &ref_spec_c, &file_path_c)? {
            Some(c) => Ok(c),
            None => Ok(String::new()),
        }
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "logId": log_id,
        "filePath": file_path,
        "ref": ref_spec,
        "content": content,
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileContentQuery {
    #[serde(flatten)]
    base: GitQuery,
    /// nuwax 字段名 `ref` (Rust 关键字, 用 ref_ + serde rename)
    #[serde(rename = "ref")]
    ref_: Option<String>,
    file_path: String,
}

/// `GET /api/git/status` (对齐 nuwax status 5-bucket + conflicted/ahead/behind/tracking 固定值)
async fn status(
    State(state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q, &state)?;
    let result = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::get_status(&repo)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "logId": log_id,
        "current": result.current,
        "staged": result.staged,
        "modified": result.modified,
        "created": result.created,
        "deleted": result.deleted,
        "untracked": result.untracked,
        "conflicted": [],
        "ahead": 0,
        "behind": 0,
        "tracking": null,
    })))
}

// ── 写操作 ──────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitWriteBody {
    workspace_type: String,
    project_id: Option<String>,
    user_id: Option<String>,
    c_id: Option<String>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    isolation_type: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FilesBody {
    #[serde(flatten)]
    base: GitWriteBody,
    #[serde(default)]
    files: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CommitBody {
    #[serde(flatten)]
    base: GitWriteBody,
    message: String,
    #[serde(default)]
    files: Option<Vec<String>>,
    #[serde(default)]
    author_name: Option<String>,
    #[serde(default)]
    author_email: Option<String>,
}

fn resolve_body(
    state: &AppState,
    body: &GitWriteBody,
) -> Result<(std::path::PathBuf, String), AppError> {
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

/// `POST /api/git/init`
async fn init(
    State(state): State<AppState>,
    Json(body): Json<GitWriteBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body)?;
    let an = state.config.git_default_author_name.clone();
    let ae = state.config.git_default_author_email.clone();
    let already = tokio::task::spawn_blocking(move || git::init_repo(&path, &an, &ae))
        .await
        .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Git repository initialized",
        "logId": log_id,
        "alreadyExists": already,
    })))
}

/// `POST /api/git/add`
async fn add(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let files = body.files.unwrap_or_default();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::stage_files(&repo, &files)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Files staged successfully",
        "logId": log_id,
    })))
}

/// `POST /api/git/commit`
async fn commit(
    State(state): State<AppState>,
    Json(body): Json<CommitBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let message = body.message;
    let files = body.files.unwrap_or_default();
    let an = body
        .author_name
        .unwrap_or_else(|| state.config.git_default_author_name.clone());
    let ae = body
        .author_email
        .unwrap_or_else(|| state.config.git_default_author_email.clone());
    let result = tokio::task::spawn_blocking(move || -> Result<Option<String>, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::stage_files(&repo, &files)?;
        let st = git::get_status(&repo)?;
        if st.staged.is_empty() {
            return Ok(None);
        }
        let hash = git::commit_indexed(&repo, &message, &an, &ae)?;
        Ok(Some(hash))
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    match result {
        Some(hash) => Ok(Json(json!({
            "success": true,
            "message": "Commit successful",
            "logId": log_id,
            "commit": hash,
            "summary": { "changes": 1 },
        }))),
        None => Ok(Json(json!({
            "success": true,
            "message": "Nothing to commit",
            "logId": log_id,
            "nothingToCommit": true,
        }))),
    }
}

/// `POST /api/git/unstage`
async fn unstage(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let files = body.files.unwrap_or_default();
    let all = files.is_empty();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::unstage_files(&repo, &files)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Files unstaged successfully",
        "logId": log_id,
        "files": if all { "all" } else { "" },
    })))
}

/// `POST /api/git/discard`
async fn discard(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let files = body.files.unwrap_or_default();
    let count = files.len();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::discard_files(&repo, &files)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Changes discarded successfully",
        "logId": log_id,
        "discardedCount": count,
    })))
}
