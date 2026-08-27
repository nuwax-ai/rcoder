//! project 文件增量/全量更新 handlers: specified-files-update / all-files-update。

use axum::extract::State;
use garde::Validate;
use serde_json::json;

use super::ctx_from;
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::models::{AllFilesBody, SpecifiedBody};
use crate::service::code as code_service;

/// 项目文件增量更新
///
/// create/delete/rename/modify 增量。
#[utoipa::path(post, path = "/specified-files-update", request_body = SpecifiedBody, responses(crate::openapi::JsonApiResponses), tag = "Code")]
pub(crate) async fn specified_files_update(
    State(state): State<AppState>,
    Json(mut body): Json<SpecifiedBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let project_id = body.project_id.trim().to_string();
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

/// 项目文件全量更新
///
/// 全量覆盖 + 清理缺失。
#[utoipa::path(post, path = "/all-files-update", request_body = AllFilesBody, description = r#"
**全量覆盖**语义：以上传的文件集替换项目对应范围，并清理未被覆盖的缺失文件——保证远端与本地一致（区别于 specified-files-update 的定点更新）。
"#,
    responses(crate::openapi::JsonApiResponses), tag = "Code")]
pub(crate) async fn all_files_update(
    State(state): State<AppState>,
    Json(mut body): Json<AllFilesBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let project_id = body.project_id.trim().to_string();
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
