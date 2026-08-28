//! computer 文件**写类** handlers (JSON 类): delete-workspace / files-update。
//!
//! 拆分: [`upload`] (upload-file/upload-files multipart) /
//! [`generate`] (generate-file 文本生成) / [`import_project`] (import-project zip) /
//! 读类见 [`super::files_read`]。

pub mod generate;
pub mod import_project;
pub mod upload;

use axum::extract::State;
use serde_json::{Value, json};

use super::{resolve_computer_target, ws_path};
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::models::{DeleteWorkspaceBody, FilesUpdateBody};
use crate::ops::files::files_update_core;

// ── delete-workspace ────────────────────────────────────────────────────────────

/// 删除工作区
///
/// 对齐 nuwax deleteWorkspace; 目录不存在也返回 deleted。
#[utoipa::path(post, path = "/delete-workspace", request_body = DeleteWorkspaceBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn delete_workspace(
    State(state): State<AppState>,
    Json(body): Json<DeleteWorkspaceBody>,
) -> Result<Json<Value>, AppError> {
    let path = ws_path(&state, &body.user_id, &body.c_id).await?;
    // 不存在视为已删除 (对齐 nuwax, 只 warn)
    if path.exists() {
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|e| AppError::system(format!("delete workspace failed: {e}")))?;
    }
    Ok(Json(json!({ "success": true, "deleted": true })))
}

// ── files-update ────────────────────────────────────────────────────────────────

/// 工作区文件增量更新
///
/// 对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify。
#[utoipa::path(post, path = "/files-update", request_body = FilesUpdateBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn files_update(
    State(state): State<AppState>,
    Json(body): Json<FilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
    )
    .await?;
    let count = files_update_core(&path, body.files).await?;
    Ok(Json(json!({
        "success": true,
        "message": "User files updated successfully",
        "userId": body.user_id,
        "cId": body.c_id,
        "filesCount": count,
    })))
}
