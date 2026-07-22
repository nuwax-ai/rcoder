//! computer 文件类 handlers: get-file-list / delete-workspace / files-update /
//! upload-file / upload-files / import-project。

use std::path::Path;

use axum::extract::State;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart, AppQuery as Query};
use crate::path_safety;
use crate::service::{code as code_service, tree};

use super::{
    UserCidQuery, file_field, resolve_computer_target, text_field, validate_zip_ext, ws_path,
};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteWorkspaceBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFileForm {
    pub user_id: String,
    pub c_id: String,
    pub file_path: String,
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesForm {
    pub user_id: String,
    pub c_id: String,
    pub custom_target_dir: Option<String>,
    pub file_paths: Vec<String>,
    pub files: Vec<crate::openapi::BinaryFile>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct ImportProjectForm {
    pub user_id: String,
    pub c_id: String,
    pub custom_target_dir: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
}

// ── get-file-list ───────────────────────────────────────────────────────────────

/// `GET /api/computer/get-file-list` (对齐 nuwax getFileList):
/// 轻量元信息遍历 (不读内容) + customTargetDir 覆盖; 目录不存在返回空数组。
#[utoipa::path(
    get,
    path = "/get-file-list",
    params(UserCidQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "Computer"
)]
pub(crate) async fn get_file_list(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref())
        .await?;
    // 对齐 nuwax: 目录不存在 → 返回空数组 (非报错)
    if !crate::service::fs_util::path_exists(&path).await? {
        return Ok(Json(json!({ "success": true, "files": [] })));
    }
    let mut files = tree::list_files_meta(&path, &state.config, q.proxy_path.as_deref()).await?;
    // fileProxyUrl 追加 ?customTargetDir (对齐 nuwax; 值需 encodeURIComponent)
    if let Some(ct) = q
        .custom_target_dir
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        let suffix = format!(
            "?customTargetDir={}",
            code_service::encode_uri_component(ct)
        );
        for f in files.iter_mut() {
            if let Some(u) = f.file_proxy_url.as_mut() {
                u.push_str(&suffix);
            }
        }
    }
    Ok(Json(json!({ "success": true, "files": files })))
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
    user_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    c_id: String,
    files: Vec<code_service::FileOp>,
    #[serde(default)]
    custom_target_dir: Option<String>,
}

/// `POST /api/computer/files-update` (对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify)。
#[utoipa::path(post, path = "/files-update", request_body = FilesUpdateBody, responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn files_update(
    State(state): State<AppState>,
    Json(mut body): Json<FilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
    )
    .await?;
    // 工作区不存在 → 创建 (对齐 nuwax computerFileUtils.updateFiles: !existsSync → mkdirSync recursive)。
    // 首次向全新 user/cId 工作区写入不应失败。
    tokio::fs::create_dir_all(&path).await?;
    // decodeURIComponent 文本内容 (对齐 nuwax safeDecodePath)
    for op in body.files.iter_mut() {
        if let Some(c) = op.contents.as_mut()
            && !c.is_empty()
        {
            *c = code_service::decode_uri_component(c);
        }
    }
    let count = body.files.len();
    // computer updateFiles: modify 用字节比较 (非 project 的行级 diff; 对齐 nuwax)
    code_service::apply_file_ops(
        &path,
        &body.files,
        code_service::ModifyStrategy::ByteCompare,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "User files updated successfully",
        "userId": body.user_id,
        "cId": body.c_id,
        "filesCount": count,
    })))
}

// ── upload-file / upload-files ──────────────────────────────────────────────────

/// `POST /api/computer/upload-file` (对齐 nuwax computer uploadFile; multipart)。
/// 返回 {success, message, fileSize} (不返回 filePath/originalname)。
#[utoipa::path(post, path = "/upload-file", request_body(content = UploadFileForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut file_path = None;
    let mut custom_target_dir = None;
    let mut data = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "filePath" => file_path = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "file" => {
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let file_path = file_path.ok_or_else(|| AppError::validation("filePath is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref()).await?;
    let target = path_safety::ensure_within(&ws, &file_path)?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file_size = data.size();
    crate::service::temp_file::copy_file(data.path(), &target).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "fileSize": file_size,
    })))
}

/// `POST /api/computer/upload-files` (对齐 nuwax computer uploadFiles; 多文件 multipart)。
/// 返回 {success, message, totalCount, successCount, failCount, results:[{success,filePath,originalname?,message?,fileSize?,error?}]}。
#[utoipa::path(post, path = "/upload-files", request_body(content = UploadFilesForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn upload_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec = Vec::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "filePaths" => file_paths.push(text_field(field).await?),
            "files" => {
                let original = field.file_name().map(|s| s.to_string());
                files_vec.push((
                    original,
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                ));
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref()).await?;
    let total = file_paths.len();
    let mut success_count = 0usize;
    let mut results: Vec<Value> = Vec::new();
    for (fp, (original, data)) in file_paths.iter().zip(&files_vec) {
        let target = match path_safety::ensure_within(&ws, fp) {
            Ok(t) => t,
            Err(_) => {
                results.push(json!({
                    "success": false,
                    "filePath": fp,
                    "originalname": original,
                    "error": "Invalid file path",
                }));
                continue;
            }
        };
        let file_size = data.size();
        match write_file_create_parent(&target, data.path()).await {
            Ok(()) => {
                success_count += 1;
                results.push(json!({
                    "success": true,
                    "filePath": fp,
                    "originalname": original,
                    "message": "File uploaded successfully",
                    "fileSize": file_size,
                }));
            }
            Err(e) => {
                results.push(json!({
                    "success": false,
                    "filePath": fp,
                    "originalname": original,
                    "error": e.to_string(),
                }));
            }
        }
    }
    let fail_count = total - success_count;
    Ok(Json(json!({
        "success": true,
        "message": "Batch upload completed",
        "totalCount": total,
        "successCount": success_count,
        "failCount": fail_count,
        "results": results,
    })))
}

/// 写文件 (父目录自动创建); 用于 upload-files 单文件隔离错误。
async fn write_file_create_parent(target: &Path, source: &Path) -> Result<(), AppError> {
    crate::service::temp_file::copy_file(source, target)
        .await
        .map(|_| ())
}

// ── import-project ──────────────────────────────────────────────────────────────

/// `POST /api/computer/import-project` (对齐 nuwax computer importProject):
/// 上传 zip → 解压 + removeTopLevelDir + 白名单保留合并到工作区。
#[utoipa::path(post, path = "/import-project", request_body(content = ImportProjectForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Computer")]
pub(crate) async fn import_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut data = None;
    let mut file_name = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "userId" => user_id = Some(text_field(field).await?),
            "cId" => cid = Some(text_field(field).await?),
            "customTargetDir" => custom_target_dir = Some(text_field(field).await?),
            "file" => {
                file_name = field.file_name().map(|s| s.to_string());
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                );
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    validate_zip_ext(file_name.as_deref())?;
    let target_dir =
        resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref()).await?;
    tokio::fs::create_dir_all(&target_dir).await?;
    let res = crate::service::computer_ws::import_project(&target_dir, data.path()).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project imported successfully",
        "userId": user_id,
        "cId": cid,
        "targetDir": res.target_dir,
    })))
}
