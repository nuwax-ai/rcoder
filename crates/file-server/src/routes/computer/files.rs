//! computer 文件类路由: get-file-list / delete-workspace / files-update /
//! upload-file / upload-files / import-project。

use std::path::Path;

use axum::Json;
use axum::extract::{Multipart, Query, State};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::AppState;
use crate::error::AppError;
use crate::path_safety;
use crate::service::{code as code_service, tree};

use super::{
    UserCidQuery, bytes_field, resolve_computer_target, text_field, validate_zip_ext, ws_path,
};

// ── get-file-list ───────────────────────────────────────────────────────────────

/// `GET /api/computer/get-file-list` (对齐 nuwax getFileList):
/// 轻量元信息遍历 (不读内容) + customTargetDir 覆盖; 目录不存在返回空数组。
pub(super) async fn get_file_list(
    State(state): State<AppState>,
    Query(q): Query<UserCidQuery>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(&state, &q.user_id, &q.c_id, q.custom_target_dir.as_deref());
    // 对齐 nuwax: 目录不存在 → 返回空数组 (非报错)
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
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
pub(super) async fn delete_workspace(
    State(state): State<AppState>,
    Json(body): Json<serde_json::Map<String, Value>>,
) -> Result<Json<Value>, AppError> {
    let user_id = body
        .get("userId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = body
        .get("cId")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::validation("cId is required"))?;
    let path = ws_path(&state, user_id, cid);
    // 不存在视为已删除 (对齐 nuwax, 只 warn)
    if path.exists() {
        tokio::fs::remove_dir_all(&path)
            .await
            .map_err(|e| AppError::system(format!("delete workspace failed: {e}")))?;
    }
    Ok(Json(json!({ "success": true, "deleted": true })))
}

// ── files-update ────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct FilesUpdateBody {
    user_id: String,
    c_id: String,
    files: Vec<code_service::FileOp>,
    #[serde(default)]
    custom_target_dir: Option<String>,
}

/// `POST /api/computer/files-update` (对齐 nuwax computer updateFiles; 增量 create/delete/rename/modify)。
pub(super) async fn files_update(
    State(state): State<AppState>,
    Json(mut body): Json<FilesUpdateBody>,
) -> Result<Json<Value>, AppError> {
    let path = resolve_computer_target(
        &state,
        &body.user_id,
        &body.c_id,
        body.custom_target_dir.as_deref(),
    );
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(AppError::resource("workspace does not exist"));
    }
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
pub(super) async fn upload_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut file_path = None;
    let mut custom_target_dir = None;
    let mut data: Option<Vec<u8>> = None;
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
            "file" => data = Some(bytes_field(field).await?),
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let file_path = file_path.ok_or_else(|| AppError::validation("filePath is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref());
    let target = path_safety::ensure_within(&ws, &file_path)?;
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file_size = data.len();
    tokio::fs::write(&target, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully",
        "fileSize": file_size,
    })))
}

/// `POST /api/computer/upload-files` (对齐 nuwax computer uploadFiles; 多文件 multipart)。
/// 返回 {success, message, totalCount, successCount, failCount, results:[{success,filePath,originalname?,message?,fileSize?,error?}]}。
pub(super) async fn upload_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec: Vec<(Option<String>, Vec<u8>)> = Vec::new();
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
                files_vec.push((original, bytes_field(field).await?));
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref());
    let total = file_paths.len();
    let mut success_count = 0usize;
    let mut results: Vec<Value> = Vec::new();
    for (fp, (original, data)) in file_paths.iter().zip(files_vec) {
        // 空文件对象
        if data.is_empty() {
            results.push(json!({
                "success": false,
                "filePath": fp,
                "error": "Empty file object",
            }));
            continue;
        }
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
        let file_size = data.len();
        match write_file_create_parent(&target, data).await {
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
async fn write_file_create_parent(target: &Path, data: Vec<u8>) -> Result<(), std::io::Error> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    tokio::fs::write(target, data).await
}

// ── import-project ──────────────────────────────────────────────────────────────

/// `POST /api/computer/import-project` (对齐 nuwax computer importProject):
/// 上传 zip → 解压 + removeTopLevelDir + 白名单保留合并到工作区。
pub(super) async fn import_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut user_id = None;
    let mut cid = None;
    let mut custom_target_dir = None;
    let mut data: Option<Vec<u8>> = None;
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
                data = Some(bytes_field(field).await?);
            }
            _ => {}
        }
    }
    let user_id = user_id.ok_or_else(|| AppError::validation("userId is required"))?;
    let cid = cid.ok_or_else(|| AppError::validation("cId is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    validate_zip_ext(file_name.as_deref())?;
    let target_dir = resolve_computer_target(&state, &user_id, &cid, custom_target_dir.as_deref());
    tokio::fs::create_dir_all(&target_dir).await?;
    let res = crate::service::computer_ws::import_project(&target_dir, data).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project imported successfully",
        "userId": user_id,
        "cId": cid,
        "targetDir": res.target_dir,
    })))
}
