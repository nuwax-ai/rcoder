//! computer 文件**写类** handlers (JSON 类): delete-workspace / files-update。
//!
//! 拆分: [`upload`] (upload-file/upload-files multipart) /
//! [`generate`] (generate-file 文本生成) / [`import_project`] (import-project zip) /
//! 读类见 [`super::files_read`]。

pub mod generate;
pub mod import_project;
pub mod upload;

use axum::extract::State;

use super::{ServiceScope, resolve_computer_target, ws_path};
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::models::{
    DeleteWorkspaceBody, DeleteWorkspaceResult, FilesUpdateBody, FilesUpdateResult,
};
use crate::ops::files::files_update_core;

// ── delete-workspace ────────────────────────────────────────────────────────────

/// 删除工作区
///
/// 对齐 nuwax deleteWorkspace; 目录不存在也返回 deleted。
#[utoipa::path(post, path = "/delete-workspace", request_body = DeleteWorkspaceBody, responses((status = 200, description = "删除结果（不存在视为已删除）", body = DeleteWorkspaceResult), crate::openapi::ErrorApiResponses), tag = "Computer")]
pub(crate) async fn delete_workspace(
    State(state): State<AppState>,
    Json(body): Json<DeleteWorkspaceBody>,
) -> Result<Json<DeleteWorkspaceResult>, AppError> {
    // 绑定目录直接定位删除 (不先建后删, 对齐 TS 1.4.5 deleteWorkspace)
    let path = ws_path(
        &state,
        &body.user_id,
        &body.c_id,
        ServiceScope {
            workspace_type: body.workspace_type.as_deref(),
            service_type: body.service_type.as_deref(),
            app_id: body.app_id.as_deref(),
            workspace_path: body.workspace_path.as_deref(),
        },
    )
    .await?;
    // 不存在视为已删除 (对齐 nuwax, 只 warn)
    if tokio::fs::try_exists(&path).await.unwrap_or(false) {
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|e| AppError::system(format!("delete workspace failed: {e}")))?;
    }
    Ok(Json(DeleteWorkspaceResult {
        success: true,
        deleted: true,
    }))
}

// ── files-update ────────────────────────────────────────────────────────────────

/// 工作区文件增量更新
///
/// 对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify。
#[utoipa::path(post, path = "/files-update", request_body = FilesUpdateBody, responses((status = 200, description = "增量更新结果（回显身份与操作数）", body = FilesUpdateResult), crate::openapi::ErrorApiResponses), tag = "Computer")]
pub(crate) async fn files_update(
    State(state): State<AppState>,
    Json(body): Json<FilesUpdateBody>,
) -> Result<Json<FilesUpdateResult>, AppError> {
    let path = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
        ServiceScope {
            workspace_type: body.workspace_type.as_deref(),
            service_type: body.service_type.as_deref(),
            app_id: body.app_id.as_deref(),
            workspace_path: body.workspace_path.as_deref(),
        },
    )
    .await?;
    let count = files_update_core(&path, body.files).await?;
    Ok(Json(FilesUpdateResult {
        success: true,
        message: "User files updated successfully".to_string(),
        user_id: body.user_id,
        c_id: body.c_id,
        files_count: count,
    }))
}
