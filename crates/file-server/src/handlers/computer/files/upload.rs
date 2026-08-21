//! upload-file / upload-files handlers: multipart 单文件与批量上传。

use std::path::Path;

use axum::extract::State;
use garde::Validate;
use serde_json::{Value, json};

use super::super::{file_field, resolve_computer_target, text_field};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::path_safety;
use crate::service::temp_file::TemporaryFile;

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

/// upload-file 必填字段 (multipart 提取后构造 + garde 校验; 文件字段用内置 required)。
#[derive(garde::Validate)]
struct UploadFileFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    user_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    cid: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    file_path: Option<String>,
    #[garde(required)]
    data: Option<TemporaryFile>,
}

/// [`UploadFileFields`] 校验后的全必填形态 (parse, don't validate):
/// garde 校验通过后由 [`UploadFileFields::into_validated`] 消费转换,
/// 后续代码直接用非 Option 类型, 无需反复 ok_or_else。
struct ValidatedUploadFile {
    user_id: String,
    cid: String,
    file_path: String,
    data: TemporaryFile,
}

impl UploadFileFields {
    /// garde 校验 + 消费转换: 校验通过则返回全必填 [`ValidatedUploadFile`],
    /// 失败返回 garde 校验错误。调用方拿到的值全部非 Option, 后续代码无需再处理 Option。
    fn into_validated(self) -> Result<ValidatedUploadFile, AppError> {
        self.validate().map_err(crate::error::from_garde)?;
        // garde 已校验非空, 此处 ok_or_else 是防御性兜底 (不用 unwrap: 避免 panic)
        Ok(ValidatedUploadFile {
            user_id: self
                .user_id
                .ok_or_else(|| AppError::system("user_id missing after garde validation"))?,
            cid: self
                .cid
                .ok_or_else(|| AppError::system("c_id missing after garde validation"))?,
            file_path: self
                .file_path
                .ok_or_else(|| AppError::system("file_path missing after garde validation"))?,
            data: self
                .data
                .ok_or_else(|| AppError::system("file missing after garde validation"))?,
        })
    }
}

/// upload-files 必填字段 (filePaths/files 数组允许为空, 仅 userId/cId 必填)。
#[derive(garde::Validate)]
struct UploadFilesFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    user_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    cid: Option<String>,
}

/// [`UploadFilesFields`] 校验后的全必填形态。
struct ValidatedUploadFiles {
    user_id: String,
    cid: String,
}

impl UploadFilesFields {
    fn into_validated(self) -> Result<ValidatedUploadFiles, AppError> {
        self.validate().map_err(crate::error::from_garde)?;
        Ok(ValidatedUploadFiles {
            user_id: self
                .user_id
                .ok_or_else(|| AppError::system("user_id missing after garde validation"))?,
            cid: self
                .cid
                .ok_or_else(|| AppError::system("c_id missing after garde validation"))?,
        })
    }
}

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
    let fields = UploadFileFields {
        user_id,
        cid,
        file_path,
        data,
    };
    let v = fields.into_validated()?;
    let ws =
        resolve_computer_target(&state, &v.user_id, &v.cid, custom_target_dir.as_deref()).await?;
    upload_file_impl(&ws, &v.file_path, v.data).await
}

/// upload-file 的 workspace 无关实现。
pub(crate) async fn upload_file_impl(
    ws: &Path,
    file_path: &str,
    data: TemporaryFile,
) -> Result<Json<Value>, AppError> {
    let target = path_safety::ensure_within(ws, file_path)?;
    // copy_file 内部已 create_dir_all(parent), 无需重复
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
    let fields = UploadFilesFields { user_id, cid };
    let v = fields.into_validated()?;
    // 跨字段一致性 (文件路径与文件一一对应), 非纯字段校验, 保留手写
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let ws =
        resolve_computer_target(&state, &v.user_id, &v.cid, custom_target_dir.as_deref()).await?;
    upload_files_impl(&ws, &file_paths, &files_vec).await
}

/// upload-files 的 workspace 无关实现 (单文件错误隔离: 单个失败不影响其余)。
pub(crate) async fn upload_files_impl(
    ws: &Path,
    file_paths: &[String],
    files_vec: &[(Option<String>, TemporaryFile)],
) -> Result<Json<Value>, AppError> {
    let total = file_paths.len();
    let mut success_count = 0usize;
    let mut results: Vec<Value> = Vec::new();
    for (fp, (original, data)) in file_paths.iter().zip(files_vec) {
        let target = match path_safety::ensure_within(ws, fp) {
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
