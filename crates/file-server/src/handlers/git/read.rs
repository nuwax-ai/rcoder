//! git 读 handlers: branches / tags / log / file-content / status。

use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{GitQuery, GitWriteBody, resolve, resolve_body};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::git;

/// 列出分支
#[utoipa::path(
    get,
    path = "/branches",
    params(GitQuery),
    description = r#"
列 git 仓库全部分支及当前检出分支（`current=true` 标记）。用于分支切换器渲染与提交前校验。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(crate) async fn branches(
    State(state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q, &state).await?;
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

/// 列出标签
#[utoipa::path(
    get,
    path = "/tags",
    params(GitQuery),
    description = r#"
列仓库全部标签（轻量引用读取）。常用于版本选择下拉。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(crate) async fn tags(
    State(state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q, &state).await?;
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
pub(crate) struct GitLogQuery {
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

/// 提交历史
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
    description = r#"
查提交历史（hash、作者、时间、message 列表，倒序）。配合 diff/file-content 做追溯。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(crate) async fn log_history(
    State(state): State<AppState>,
    Query(q): Query<GitLogQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q.base, &state).await?;
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
pub(crate) struct FileContentBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    /// nuwax 字段名 `ref` (Rust 关键字, 用 ref_ + serde rename)
    #[serde(rename = "ref", default)]
    pub ref_: Option<String>,
    pub file_path: String,
}

/// 读取文件内容
///
/// 对齐 nuwax fileContent; 从 **body** 取 {ref, filePath}。
/// ref ∈ {worktree, staged, ""} → 直接读 workdir 文件 (不查 git); 否则读 ref 处 blob。
#[utoipa::path(post, path = "/file-content", request_body = FileContentBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn file_content(
    State(state): State<AppState>,
    Json(body): Json<FileContentBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let ref_spec = body.ref_.clone().unwrap_or_else(|| "HEAD".to_string());
    let file_path = body.file_path.clone();
    let read_worktree = matches!(ref_spec.as_str(), "worktree" | "staged" | "");
    let ref_c = ref_spec.clone();
    let fp_c = file_path.clone();
    let max_bytes = state.config.git_file_content_max_bytes;
    let content = tokio::task::spawn_blocking(move || -> Result<String, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        if read_worktree {
            let full = crate::path_safety::ensure_within(&path, &fp_c)?;
            let metadata = std::fs::metadata(&full).map_err(|error| {
                AppError::system(format!("read metadata {}: {error}", full.display()))
            })?;
            if metadata.len() > max_bytes {
                return Err(AppError::validation(format!(
                    "git file content exceeds limit (max {max_bytes} bytes)"
                )));
            }
            return std::fs::read_to_string(&full)
                .map_err(|error| AppError::system(format!("read {}: {error}", full.display())));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        match git::file_content_at_ref(&repo, &ref_c, &fp_c, max_bytes)? {
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

/// 查询工作区状态
///
/// 对齐 nuwax status 5-bucket + conflicted/ahead/behind/tracking 固定值。
#[utoipa::path(
    get,
    path = "/status",
    params(GitQuery),
    description = r#"
查工作区状态：已修改/已暂存/未跟踪文件清单与当前分支——编辑器脏标记与提交面板的数据源。
"#,
    responses(crate::openapi::JsonApiResponses),
    tag = "Git"
)]
pub(crate) async fn status(
    State(state): State<AppState>,
    Query(q): Query<GitQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q, &state).await?;
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
