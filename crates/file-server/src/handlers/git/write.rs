//! git 写 handlers: init / add / commit / unstage / discard / diff / reset / checkout / revert。

use axum::extract::State;
use garde::Validate;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{GitWriteBody, resolve_body};
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::service::git;

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilesBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    #[serde(default)]
    pub files: Option<Vec<String>>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommitBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub message: String,
    #[serde(default)]
    #[garde(skip)]
    pub files: Option<Vec<String>>,
    #[serde(default)]
    #[garde(skip)]
    pub author_name: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_email: Option<String>,
}

/// `POST /api/git/init`
#[utoipa::path(post, path = "/init", request_body = GitWriteBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn init(
    State(state): State<AppState>,
    Json(body): Json<GitWriteBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body).await?;
    let already = tokio::task::spawn_blocking(move || git::init_repo_only(&path))
        .await
        .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": if already { "Git repository already initialized" } else { "Git repository initialized successfully" },
        "logId": log_id,
        "alreadyExists": already,
    })))
}

/// `POST /api/git/add`
#[utoipa::path(post, path = "/add", request_body = FilesBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn add(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base).await?;
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
#[utoipa::path(post, path = "/commit", request_body = CommitBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn commit(
    State(state): State<AppState>,
    Json(body): Json<CommitBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let (path, log_id) = resolve_body(&state, &body.base).await?;
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
#[utoipa::path(post, path = "/unstage", request_body = FilesBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn unstage(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let files = body.files.unwrap_or_default();
    let all = files.is_empty();
    let files_echo = files.clone();
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
    let (message, files_val): (&str, Value) = if all {
        ("All files unstaged successfully", json!("all"))
    } else {
        ("Specified files unstaged successfully", json!(files_echo))
    };
    Ok(Json(json!({
        "success": true,
        "message": message,
        "logId": log_id,
        "files": files_val,
    })))
}

/// `POST /api/git/discard`
#[utoipa::path(post, path = "/discard", request_body = FilesBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn discard(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let files = body.files.unwrap_or_default();
    let buckets = tokio::task::spawn_blocking(move || -> Result<git::DiscardBuckets, AppError> {
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
        "discardedCount": buckets.len(),
        "trackedFiles": buckets.tracked_files,
        "newFiles": buckets.new_files,
        "untrackedFiles": buckets.untracked_files,
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DiffBody {
    #[serde(flatten)]
    pub base: GitWriteBody,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub from: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
}

/// `POST /api/git/diff` (对齐 nuwax diff; source: worktree|staged|commit, 默认 worktree)。
#[utoipa::path(post, path = "/diff", request_body = DiffBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn diff(
    State(state): State<AppState>,
    Json(body): Json<DiffBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let source = body.source.parse::<git::DiffSource>()?;
    let params = git::DiffParams {
        source,
        from: body.from.clone(),
        to: body.to.clone(),
        paths: body.paths.clone().unwrap_or_default(),
        max_file_size_bytes: state.config.git_diff_max_file_size_bytes,
        max_total_bytes: state.config.git_diff_max_total_bytes,
        max_output_bytes: state.config.git_diff_max_output_bytes,
    };
    let result = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::compute_diff(&repo, &params)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "logId": log_id,
        "source": source.to_string(),
        "diff": result.diff,
        "summary": {
            "files": result.files,
            "insertions": result.insertions,
            "deletions": result.deletions,
        },
    })))
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TargetBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub target: String,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ResetBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub target: String,
    #[serde(default)]
    #[garde(skip)]
    pub mode: String,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RevertBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub target: String,
    #[serde(default)]
    #[garde(skip)]
    pub message: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_name: Option<String>,
    #[serde(default)]
    #[garde(skip)]
    pub author_email: Option<String>,
}

/// `POST /api/git/reset` (对齐 nuwax reset; mode: soft|mixed|hard, 默认 mixed)。
#[utoipa::path(post, path = "/reset", request_body = ResetBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn reset(
    State(state): State<AppState>,
    Json(body): Json<ResetBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let target = body.target.clone();
    let mode = git::ResetMode::parse(&body.mode)?;
    let mode_label = mode.to_string();
    let outcome = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::reset(&repo, &target, mode)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": format!("Reset ({}) to {} successful", mode_label, body.target),
        "logId": log_id,
        "target": body.target,
        "mode": mode_label,
        "previousHead": outcome.previous_head,
    })))
}

/// `POST /api/git/checkout` (对齐 nuwax checkout; 恢复 target 整棵 tree, 不删多余文件, 不动 HEAD)。
#[utoipa::path(post, path = "/checkout", request_body = TargetBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn checkout(
    State(state): State<AppState>,
    Json(body): Json<TargetBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let target = body.target.clone();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::checkout_tree(&repo, &target)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": format!("Checkout files from {} successful", body.target),
        "logId": log_id,
        "target": body.target,
    })))
}

/// `POST /api/git/revert` (对齐 nuwax revert; 把 tree 重置到 target 但用新 commit 保留历史)。
#[utoipa::path(post, path = "/revert", request_body = RevertBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn revert(
    State(state): State<AppState>,
    Json(body): Json<RevertBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let target = body.target.clone();
    let message = body.message.clone();
    let an = body
        .author_name
        .unwrap_or_else(|| state.config.git_default_author_name.clone());
    let ae = body
        .author_email
        .unwrap_or_else(|| state.config.git_default_author_email.clone());
    let outcome = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::revert_to_commit(&repo, &target, message.as_deref(), &an, &ae)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    match outcome.commit {
        Some(hash) => Ok(Json(json!({
            "success": true,
            "message": "Revert successful",
            "logId": log_id,
            "commit": hash,
            "target": outcome.target,
            "previousHead": outcome.previous_head,
        }))),
        None => Ok(Json(json!({
            "success": true,
            "message": "Nothing to revert, already at target state",
            "logId": log_id,
            "nothingToCommit": true,
            "target": outcome.target,
        }))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 关键回归: git 写接口 (add/commit/discard/...) 经 `#[serde(flatten)]` 复用 GitWriteBody。
    /// serde flatten 会把字段收集到 Map 再二次反序列化, 是 deserialize_with 失效的已知坑区。
    /// 此测试验证 flatten 下整数 ID (Java bigint, 如 projectId:17) 仍被正确转 String。
    #[test]
    fn flatten_git_write_body_accepts_integer_ids() {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Input {
            #[serde(flatten)]
            base: GitWriteBody,
            message: String,
        }

        // 整数 ID 经 flatten 传递
        let body: Input = serde_json::from_str(
            r#"{"workspaceType":"project","projectId":17,"userId":5,"tenantId":9,"message":"m"}"#,
        )
        .expect("flatten + integer ids must deserialize");
        assert_eq!(body.base.workspace_type, "project");
        assert_eq!(body.base.project_id.as_deref(), Some("17"));
        assert_eq!(body.base.user_id.as_deref(), Some("5"));
        assert_eq!(body.base.tenant_id.as_deref(), Some("9"));
        assert_eq!(body.message, "m");

        // 字符串 ID 不回归 (原有行为)
        let body: Input = serde_json::from_str(
            r#"{"workspaceType":"computer","userId":"u","cId":"c","message":"m"}"#,
        )
        .expect("string ids must still deserialize");
        assert_eq!(body.base.workspace_type, "computer");
        assert_eq!(body.base.user_id.as_deref(), Some("u"));
        assert_eq!(body.base.c_id.as_deref(), Some("c"));

        // ID 缺失 (flatten 下 default 仍生效 → None)
        let body: Input =
            serde_json::from_str(r#"{"workspaceType":"project","message":"m"}"#).unwrap();
        assert!(body.base.project_id.is_none());
        assert!(body.base.tenant_id.is_none());
    }
}
