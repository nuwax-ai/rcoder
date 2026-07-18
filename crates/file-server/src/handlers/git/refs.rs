//! git 引用 (branch/tag) CRUD handlers: branch-create / branch-delete / branch-switch /
//! tag-create / tag-delete。

use axum::extract::State;
use garde::Validate;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{GitWriteBody, resolve_body};
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::service::git;

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BranchCreateBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub branch_name: String,
    #[serde(default)]
    #[garde(skip)]
    pub start_point: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BranchNameBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub branch_name: String,
    /// branch-delete 强制删除未合并分支 (对齐 nuwax deleteBranch force)。
    #[serde(default)]
    #[garde(skip)]
    pub force: Option<bool>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TagCreateBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub tag_name: String,
    #[serde(default)]
    #[garde(skip)]
    pub message: Option<String>,
}

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TagNameBody {
    #[serde(flatten)]
    #[garde(skip)]
    pub base: GitWriteBody,
    #[garde(length(min = 1))]
    pub tag_name: String,
}

/// `POST /api/git/branch-create`
#[utoipa::path(post, path = "/branch-create", request_body = BranchCreateBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn branch_create(
    State(state): State<AppState>,
    Json(body): Json<BranchCreateBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
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
#[utoipa::path(post, path = "/branch-delete", request_body = BranchNameBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn branch_delete(
    State(state): State<AppState>,
    Json(body): Json<BranchNameBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
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
#[utoipa::path(post, path = "/tag-create", request_body = TagCreateBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn tag_create(
    State(state): State<AppState>,
    Json(body): Json<TagCreateBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
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
#[utoipa::path(post, path = "/tag-delete", request_body = TagNameBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn tag_delete(
    State(state): State<AppState>,
    Json(body): Json<TagNameBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
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
#[utoipa::path(post, path = "/branch-switch", request_body = BranchNameBody, responses(crate::openapi::JsonApiResponses), tag = "Git")]
pub(crate) async fn branch_switch(
    State(state): State<AppState>,
    Json(body): Json<BranchNameBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
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
