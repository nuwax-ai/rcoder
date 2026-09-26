//! git 写 handlers: init / add / commit / unstage / discard / diff / reset / checkout / revert。

use axum::extract::State;
use garde::Validate;

use super::resolve_body;
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::models::{
    CommitBody, DiffBody, DiscardResult, FilesBody, GitAddResult, GitCheckoutResult,
    GitCommitResult, GitCommitSummary, GitDiffFileStat, GitDiffResult, GitDiffSummary,
    GitInitResult, GitResetResult, GitRevertResult, GitUnstageFiles, GitUnstageResult,
    GitWriteBody, ResetBody, RevertBody, TargetBody,
};
use crate::service::git;

/// 初始化仓库
#[utoipa::path(post, path = "/init", request_body = GitWriteBody, description = r#"
初始化 git 仓库（`git init` + 写入基础 .gitignore）。幂等：已是仓库则成功返回。
"#,
    responses((status = 200, description = "初始化结果（幂等）", body = GitInitResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn init(
    State(state): State<AppState>,
    Json(body): Json<GitWriteBody>,
) -> Result<Json<GitInitResult>, AppError> {
    let (path, log_id) = resolve_body(&state, &body).await?;
    let already = tokio::task::spawn_blocking(move || git::init_repo_only(&path))
        .await
        .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(GitInitResult {
        success: true,
        message: if already {
            "Git repository already initialized"
        } else {
            "Git repository initialized successfully"
        }
        .to_string(),
        log_id,
        already_exists: already,
    }))
}

/// 暂存文件
#[utoipa::path(post, path = "/add", request_body = FilesBody, description = r#"
暂存文件变更（加入 index）。body `files` 省略时等价 `git add -A`。
"#,
    responses((status = 200, description = "暂存结果", body = GitAddResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn add(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<GitAddResult>, AppError> {
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
    Ok(Json(GitAddResult {
        success: true,
        message: "Files staged successfully".to_string(),
        log_id,
    }))
}

/// 提交
#[utoipa::path(post, path = "/commit", request_body = CommitBody, description = r#"
提交暂存区变更：`message` 必填；可选限定 `files` 集合与 `authorName/authorEmail` 覆盖。
"#,
    responses((status = 200, description = "提交结果（含 nothingToCommit 分支）", body = GitCommitResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn commit(
    State(state): State<AppState>,
    Json(body): Json<CommitBody>,
) -> Result<Json<GitCommitResult>, AppError> {
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
        Some(hash) => Ok(Json(GitCommitResult::Committed {
            success: true,
            message: "Commit successful".to_string(),
            log_id,
            commit: hash,
            summary: GitCommitSummary { changes: 1 },
        })),
        None => Ok(Json(GitCommitResult::NothingToCommit {
            success: true,
            message: "Nothing to commit".to_string(),
            log_id,
            nothing_to_commit: true,
        })),
    }
}

/// 取消暂存
#[utoipa::path(post, path = "/unstage", request_body = FilesBody, description = r#"
取消暂存（index → 工作区回退），保留文件内容改动。
"#,
    responses((status = 200, description = "取消暂存结果（files 为 all 或路径数组）", body = GitUnstageResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn unstage(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<GitUnstageResult>, AppError> {
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
    let (message, files_val) = if all {
        (
            "All files unstaged successfully",
            GitUnstageFiles::All("all".to_string()),
        )
    } else {
        (
            "Specified files unstaged successfully",
            GitUnstageFiles::Files(files_echo),
        )
    };
    Ok(Json(GitUnstageResult {
        success: true,
        message: message.to_string(),
        log_id,
        files: files_val,
    }))
}

/// 丢弃改动
#[utoipa::path(post, path = "/discard", request_body = FilesBody, description = r#"
**丢弃**指定文件的工作区改动（不可恢复——未提交内容将丢失），慎用。
"#,
    responses((status = 200, description = "丢弃结果分桶", body = DiscardResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn discard(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<DiscardResult>, AppError> {
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
    let git::DiscardBuckets {
        tracked_files,
        new_files,
        untracked_files,
    } = buckets;
    let discarded_count = tracked_files.len() + new_files.len() + untracked_files.len();
    Ok(Json(DiscardResult {
        success: true,
        message: "Files discarded successfully",
        log_id,
        discarded_count,
        tracked_files,
        new_files,
        untracked_files,
    }))
}

/// 查看差异
///
/// 对齐 nuwax diff; source: worktree|staged|commit, 默认 worktree。
#[utoipa::path(post, path = "/diff", request_body = DiffBody, responses((status = 200, description = "unified diff 与汇总", body = GitDiffResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn diff(
    State(state): State<AppState>,
    Json(body): Json<DiffBody>,
) -> Result<Json<GitDiffResult>, AppError> {
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
    Ok(Json(GitDiffResult {
        success: true,
        log_id,
        source: source.to_string(),
        diff: result.diff,
        summary: GitDiffSummary {
            files: result
                .files
                .into_iter()
                .map(|f| GitDiffFileStat {
                    file: f.file,
                    changes: f.changes,
                    insertions: f.insertions,
                    deletions: f.deletions,
                    binary: f.binary,
                })
                .collect(),
            insertions: result.insertions,
            deletions: result.deletions,
        },
    }))
}

/// 重置到目标提交
///
/// 对齐 nuwax reset; mode: soft|mixed|hard, 默认 mixed。
#[utoipa::path(post, path = "/reset", request_body = ResetBody, responses((status = 200, description = "重置结果（含 previousHead）", body = GitResetResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn reset(
    State(state): State<AppState>,
    Json(body): Json<ResetBody>,
) -> Result<Json<GitResetResult>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let (path, log_id) = resolve_body(&state, &body.base).await?;
    let target = body.target.clone();
    let mode = git::ResetMode::parse(&body.mode)?;
    let mode_label = mode.to_string();
    let author_name = state.config.git_default_author_name.clone();
    let author_email = state.config.git_default_author_email.clone();
    let outcome = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::reset(&repo, &target, mode, &author_name, &author_email)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(GitResetResult {
        success: true,
        message: format!("Reset ({mode_label}) to {} successful", body.target),
        log_id,
        target: body.target,
        mode: mode_label,
        previous_head: outcome.previous_head,
    }))
}

/// 检出目标文件树
///
/// 对齐 nuwax checkout; 恢复 target 整棵 tree, 不删多余文件, 不动 HEAD。
#[utoipa::path(post, path = "/checkout", request_body = TargetBody, responses((status = 200, description = "检出结果", body = GitCheckoutResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn checkout(
    State(state): State<AppState>,
    Json(body): Json<TargetBody>,
) -> Result<Json<GitCheckoutResult>, AppError> {
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
    Ok(Json(GitCheckoutResult {
        success: true,
        message: format!("Checkout files from {} successful", body.target),
        log_id,
        target: body.target,
    }))
}

/// 回退到目标提交
///
/// 对齐 nuwax revert; 把 tree 重置到 target 但用新 commit 保留历史。
#[utoipa::path(post, path = "/revert", request_body = RevertBody, responses((status = 200, description = "回退结果（含 nothingToCommit 分支）", body = GitRevertResult), crate::openapi::ErrorApiResponses), tag = "Git")]
pub(crate) async fn revert(
    State(state): State<AppState>,
    Json(body): Json<RevertBody>,
) -> Result<Json<GitRevertResult>, AppError> {
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
        Some(hash) => Ok(Json(GitRevertResult::Reverted {
            success: true,
            message: "Revert successful".to_string(),
            log_id,
            commit: hash,
            target: outcome.target,
            previous_head: outcome.previous_head,
        })),
        None => Ok(Json(GitRevertResult::NothingToRevert {
            success: true,
            message: "Nothing to revert, already at target state".to_string(),
            log_id,
            nothing_to_commit: true,
            target: outcome.target,
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

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
        assert_eq!(body.base.workspace_type.as_deref(), Some("project"));
        assert_eq!(body.base.project_id.as_deref(), Some("17"));
        assert_eq!(body.base.user_id.as_deref(), Some("5"));
        assert_eq!(body.base.tenant_id.as_deref(), Some("9"));
        assert_eq!(body.message, "m");

        // 字符串 ID 不回归 (原有行为)
        let body: Input = serde_json::from_str(
            r#"{"workspaceType":"computer","userId":"u","cId":"c","message":"m"}"#,
        )
        .expect("string ids must still deserialize");
        assert_eq!(body.base.workspace_type.as_deref(), Some("computer"));
        assert_eq!(body.base.user_id.as_deref(), Some("u"));
        assert_eq!(body.base.c_id.as_deref(), Some("c"));

        // ID 缺失 (flatten 下 default 仍生效 → None)
        let body: Input =
            serde_json::from_str(r#"{"workspaceType":"project","message":"m"}"#).unwrap();
        assert!(body.base.project_id.is_none());
        assert!(body.base.tenant_id.is_none());
    }
}
