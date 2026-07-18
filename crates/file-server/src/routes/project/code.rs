//! project 文件增量/全量更新路由: specified-files-update / all-files-update。

use axum::extract::State;
use serde::Deserialize;
use serde_json::json;

use super::ctx_from;
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::service::code as code_service;

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct SpecifiedBody {
    pub project_id: String,
    pub code_version: String,
    pub files: Vec<code_service::FileOp>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// `POST /api/project/specified-files-update` (create/delete/rename/modify 增量)
#[utoipa::path(post, path = "/specified-files-update", request_body = SpecifiedBody, responses(crate::openapi::JsonApiResponses), tag = "Code")]
pub(super) async fn specified_files_update(
    State(state): State<AppState>,
    Json(mut body): Json<SpecifiedBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    // 路由层 decodeURIComponent (对齐 nuwax codeRoutes, 非空 string 才解, 失败保留原串)
    for op in body.files.iter_mut() {
        if let Some(c) = op.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let ctx = ctx_from(
        &project_id,
        body.tenant_id,
        body.space_id,
        body.isolation_type,
    );
    let result = code_service::specified_files_update(
        &*state.resolver,
        &state.config,
        &ctx,
        &body.code_version,
        &body.files,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Specified files updated successfully",
        "projectId": result.project_id,
        "filesCount": result.files_count,
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(super) struct AllFilesBody {
    pub project_id: String,
    pub code_version: String,
    pub files: Vec<code_service::FileEntry>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    pub base_path: Option<String>, // nuwax 接收但未使用
    #[allow(dead_code)]
    #[serde(default)]
    pub pid: Option<String>,
}

/// `POST /api/project/all-files-update` (全量覆盖 + 清理缺失)
#[utoipa::path(post, path = "/all-files-update", request_body = AllFilesBody, responses(crate::openapi::JsonApiResponses), tag = "Code")]
pub(super) async fn all_files_update(
    State(state): State<AppState>,
    Json(mut body): Json<AllFilesBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    // decodeURIComponent: 仅 text 内容 (binary base64 跳过, 安全加固)
    for f in body.files.iter_mut() {
        if f.binary == Some(true) {
            continue;
        }
        if let Some(c) = f.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let ctx = ctx_from(
        &project_id,
        body.tenant_id,
        body.space_id,
        body.isolation_type,
    );
    let result = code_service::all_files_update(
        &*state.resolver,
        &state.config,
        &ctx,
        &body.code_version,
        &body.files,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Files submitted successfully",
        "projectId": result.project_id,
        "restarted": false,
    })))
}
