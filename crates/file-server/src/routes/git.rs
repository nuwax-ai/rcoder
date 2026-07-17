//! `/api/git` 路由 (对齐 nuwax gitRoutes; gix 操作经 spawn_blocking 调用)。

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::service::git;
use crate::workspace::{ComputerContext, ProjectContext};

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
        .route("/diff", post(diff))
        .route("/reset", post(reset))
        .route("/checkout", post(checkout))
        .route("/revert", post(revert))
        .route("/branch-create", post(branch_create))
        .route("/branch-delete", post(branch_delete))
        .route("/branch-switch", post(branch_switch))
        .route("/tag-create", post(tag_create))
        .route("/tag-delete", post(tag_delete))
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
    Ok(Json(
        json!({ "success": true, "logId": log_id, "tags": tags, "latest": latest }),
    ))
}

/// `GET /api/git/log`
async fn log_history(
    State(state): State<AppState>,
    Query(q): Query<GitLogQuery>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve(&q.base, &state)?;
    let max_count = q.max_count.unwrap_or(50).clamp(1, 500);
    let skip = q.skip.unwrap_or(0);
    let branch = q.branch.clone();
    let commits = tokio::task::spawn_blocking(move || -> Result<_, AppError> {
        if !path.exists() {
            return Err(AppError::resource("workspace does not exist"));
        }
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::log_history(&repo, max_count, skip, branch.as_deref())
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    let total = commits.len();
    Ok(Json(
        json!({ "success": true, "logId": log_id, "commits": commits, "total": total }),
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GitLogQuery {
    #[serde(flatten)]
    base: GitQuery,
    max_count: Option<usize>,
    skip: Option<usize>,
    /// 指定分支 (对齐 nuwax git.log ref); 默认 HEAD。
    #[serde(default)]
    branch: Option<String>,
}

/// `POST /api/git/file-content` (对齐 nuwax fileContent; 从 **body** 取 {ref, filePath})。
/// ref ∈ {worktree, staged, ""} → 直接读 workdir 文件 (不查 git); 否则读 ref 处 blob。
async fn file_content(
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
            return Ok(std::fs::read_to_string(&full).unwrap_or_default());
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileContentBody {
    #[serde(flatten)]
    base: GitWriteBody,
    /// nuwax 字段名 `ref` (Rust 关键字, 用 ref_ + serde rename)
    #[serde(rename = "ref", default)]
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
        "message": if already { "Git repository already initialized" } else { "Git repository initialized successfully" },
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
async fn discard(
    State(state): State<AppState>,
    Json(body): Json<FilesBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
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

// ── diff ───────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiffBody {
    #[serde(flatten)]
    base: GitWriteBody,
    #[serde(default)]
    source: String,
    #[serde(default)]
    from: Option<String>,
    #[serde(default)]
    to: Option<String>,
    #[serde(default)]
    paths: Option<Vec<String>>,
}

/// `POST /api/git/diff` (对齐 nuwax diff; source: worktree|staged|commit, 默认 worktree)。
async fn diff(
    State(state): State<AppState>,
    Json(body): Json<DiffBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let source = git::DiffSource::parse(&body.source)?;
    let params = git::DiffParams {
        source,
        from: body.from.clone(),
        to: body.to.clone(),
        paths: body.paths.clone().unwrap_or_default(),
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
        "source": body.source,
        "diff": result.diff,
        "summary": {
            "files": result.files,
            "insertions": result.insertions,
            "deletions": result.deletions,
        },
    })))
}

// ── reset / checkout / revert ──────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TargetBody {
    #[serde(flatten)]
    base: GitWriteBody,
    target: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResetBody {
    #[serde(flatten)]
    base: GitWriteBody,
    target: String,
    #[serde(default)]
    mode: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RevertBody {
    #[serde(flatten)]
    base: GitWriteBody,
    target: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    author_name: Option<String>,
    #[serde(default)]
    author_email: Option<String>,
}

/// `POST /api/git/reset` (对齐 nuwax reset; mode: soft|mixed|hard, 默认 mixed)。
async fn reset(
    State(state): State<AppState>,
    Json(body): Json<ResetBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
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
async fn checkout(
    State(state): State<AppState>,
    Json(body): Json<TargetBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
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
async fn revert(
    State(state): State<AppState>,
    Json(body): Json<RevertBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
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

// ── branch / tag CRUD ───────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchCreateBody {
    #[serde(flatten)]
    base: GitWriteBody,
    branch_name: String,
    #[serde(default)]
    start_point: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct BranchNameBody {
    #[serde(flatten)]
    base: GitWriteBody,
    branch_name: String,
    /// branch-delete 强制删除未合并分支 (对齐 nuwax deleteBranch force)。
    #[serde(default)]
    force: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TagCreateBody {
    #[serde(flatten)]
    base: GitWriteBody,
    tag_name: String,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TagNameBody {
    #[serde(flatten)]
    base: GitWriteBody,
    tag_name: String,
}

/// `POST /api/git/branch-create`
async fn branch_create(
    State(state): State<AppState>,
    Json(body): Json<BranchCreateBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let name = body.branch_name.clone();
    let sp = body.start_point.clone();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        // switch=true: 创建后立即 checkout (对齐 nuwax git.branch checkout:true)
        git::create_branch(&repo, &name, sp.as_deref(), true)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Branch created and switched to",
        "logId": log_id,
        "branchName": body.branch_name,
    })))
}

/// `POST /api/git/branch-delete`
async fn branch_delete(
    State(state): State<AppState>,
    Json(body): Json<BranchNameBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let name = body.branch_name.clone();
    let force = body.force.unwrap_or(false);
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let repo = git::ensure_repo(&path)?;
        if git::is_current_branch(&repo, &name)? {
            return Err(AppError::business("cannot delete the current branch"));
        }
        git::delete_branch(&repo, &name, force)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Branch deleted successfully",
        "logId": log_id,
        "branchName": body.branch_name,
    })))
}

/// `POST /api/git/tag-create`
async fn tag_create(
    State(state): State<AppState>,
    Json(body): Json<TagCreateBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let name = body.tag_name.clone();
    let msg = body.message.clone();
    let an = state.config.git_default_author_name.clone();
    let ae = state.config.git_default_author_email.clone();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let repo = git::ensure_repo(&path)?;
        // annotated tag 的 tagger 用 config author (对齐 nuwax getDefaultAuthor)
        git::create_tag(&repo, &name, msg.as_deref(), &an, &ae)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Tag created successfully",
        "logId": log_id,
        "tagName": body.tag_name,
    })))
}

/// `POST /api/git/tag-delete`
async fn tag_delete(
    State(state): State<AppState>,
    Json(body): Json<TagNameBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let name = body.tag_name.clone();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let repo = git::ensure_repo(&path)?;
        git::delete_tag(&repo, &name)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Tag deleted successfully",
        "logId": log_id,
        "tagName": body.tag_name,
    })))
}

/// `POST /api/git/branch-switch` (对齐 nuwax switchBranch; 切到已存在分支)。
async fn branch_switch(
    State(state): State<AppState>,
    Json(body): Json<BranchNameBody>,
) -> Result<Json<Value>, AppError> {
    let (path, log_id) = resolve_body(&state, &body.base)?;
    let name = body.branch_name.clone();
    tokio::task::spawn_blocking(move || -> Result<(), AppError> {
        let repo = git::ensure_repo(&path)?;
        git::ensure_gitignore(&path)?;
        git::switch_branch(&repo, &name)
    })
    .await
    .map_err(|e| AppError::system(format!("git join: {e}")))??;
    Ok(Json(json!({
        "success": true,
        "message": "Branch switched successfully",
        "logId": log_id,
        "branchName": body.branch_name,
    })))
}
