//! git 读路由: branches / tags / log / file-content / status。

use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{GitQuery, GitWriteBody, resolve, resolve_body};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::git;

/// `GET /api/git/branches`
#[utoipa::path(
    get,
    path = "/branches",
    params(GitQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(super) async fn branches(
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
#[utoipa::path(
    get,
    path = "/tags",
    params(GitQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(super) async fn tags(
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
    Ok(Json(
        json!({ "success": true, "logId": log_id, "tags": tags, "latest": latest }),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct GitLogQuery {
    #[serde(flatten)]
    pub base: GitQuery,
    pub max_count: Option<usize>,
    pub skip: Option<usize>,
    /// 指定分支 (对齐 nuwax git.log ref); 默认 HEAD。
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub file_path: Option<String>,
}

/// `GET /api/git/log`
#[utoipa::path(
    get,
    path = "/log",
    params(
        GitQuery,
        ("maxCount" = Option<usize>, Query, description = "Maximum commits to return"),
        ("skip" = Option<usize>, Query, description = "Commits to skip"),
        ("branch" = Option<String>, Query, description = "Branch or ref, defaults to HEAD"),
        ("filePath" = Option<String>, Query, description = "Filter history by file")
    ),
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(super) async fn log_history(
    State(state): State<AppState>,
    Query(q): Query<GitLogQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q.base, &state)?;
    let max_count = q.max_count.unwrap_or(50).clamp(1, 500);
    let skip = q.skip.unwrap_or(0);
    let branch = q.branch.clone();
    let file_path = q.file_path.clone();
    let commits = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::log_history(
            &repo,
            max_count,
            skip,
            branch.as_deref(),
            file_path.as_deref(),
        )
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    let total = commits.len();
    Ok(Json(
        json!({ "success": true, "logId": log_id, "commits": commits, "total": total }),
    ))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct FileContentBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    /// nuwax 字段名 `ref` (Rust 关键字, 用 ref_ + serde rename)
    #[serde(rename = "ref", default)]
    pub ref_: Option<String>,
    pub file_path: String,
}

/// `POST /api/git/file-content` (对齐 nuwax fileContent; 从 **body** 取 {ref, filePath})。
/// ref ∈ {worktree, staged, ""} → 直接读 workdir 文件 (不查 git); 否则读 ref 处 blob。
#[utoipa::path(post, path = "/file-content", request_body = FileContentBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(super) async fn file_content(
    State(state): State<AppState>,
    Json(body): Json<FileContentBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let ref_spec = body.ref_.clone().unwrap_or_else(|| "HEAD".to_string());
    let file_path = body.file_path.clone();
    let read_worktree = matches!(ref_spec.as_str(), "worktree" | "staged" | "");
    let ref_c = ref_spec.clone();
    let fp_c = file_path.clone();
    let content = tokio::task::spawn_blocking(move || -> Result<String, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        if read_worktree {
            let full = crate::path_safety::ensure_within(&path, &fp_c)?;
            return std::fs::read_to_string(&full)
                .map_err(|error| AppError::system(format!("read {}: {error}", full.display())));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        match git::file_content_at_ref(&repo, &ref_c, &fp_c)? {
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

/// `GET /api/git/status` (对齐 nuwax status 5-bucket + conflicted/ahead/behind/tracking 固定值)
#[utoipa::path(
    get,
    path = "/status",
    params(GitQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(super) async fn status(
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
