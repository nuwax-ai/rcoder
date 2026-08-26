//! computer 文件**写类** handlers (JSON 类): delete-workspace / files-update。
//!
//! 拆分: [`upload`] (upload-file/upload-files multipart) /
//! [`generate`] (generate-file 文本生成) / [`import_project`] (import-project zip) /
//! 读类见 [`super::files_read`]。

pub mod generate;
pub mod import_project;
pub mod upload;

use std::path::Path;

use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use super::{resolve_computer_target, ws_path};
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::service::code as code_service;

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteWorkspaceBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
}

// ── delete-workspace ────────────────────────────────────────────────────────────

/// `POST /api/computer/delete-workspace` (对齐 nuwax deleteWorkspace; 目录不存在也返回 deleted)。
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

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct FilesUpdateBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub(crate) user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub(crate) c_id: String,
    pub(crate) files: Vec<code_service::FileOp>,
    #[serde(default)]
    pub(crate) custom_target_dir: Option<String>,
}

/// `POST /api/computer/files-update` (对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify)。
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
    let count = files_update_impl(&path, body.files).await?;
    Ok(Json(json!({
        "success": true,
        "message": "User files updated successfully",
        "userId": body.user_id,
        "cId": body.c_id,
        "filesCount": count,
    })))
}

/// files-update 的 workspace 无关实现：返回写入的文件数（展示/回显归各域壳层）。
pub async fn files_update_impl(
    ws: &Path,
    mut files: Vec<code_service::FileOp>,
) -> Result<usize, AppError> {
    // 工作区不存在 → 创建 (对齐 nuwax computerFileUtils.updateFiles: !existsSync → mkdirSync recursive)。
    // 首次向全新 user/cId 工作区写入不应失败。
    tokio::fs::create_dir_all(ws).await?;
    // decodeURIComponent 文本内容 (对齐 nuwax safeDecodePath)
    for op in files.iter_mut() {
        if let Some(c) = op.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let count = files.len();
    // computer updateFiles: modify 用字节比较 (非 project 的行级 diff; 对齐 nuwax)
    code_service::apply_file_ops(ws, &files, code_service::ModifyStrategy::ByteCompare).await?;
    Ok(count)
}
