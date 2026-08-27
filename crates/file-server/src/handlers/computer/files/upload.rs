//! upload-file / upload-files handlers: multipart 单文件与批量上传。

use axum::extract::State;
use garde::Validate;
use serde_json::Value;

use crate::ops::files::{upload_file_impl, upload_files_impl};
use crate::ops::multipart::{file_field, text_field};

use super::super::resolve_computer_target;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::models::{UploadFileForm, UploadFilesForm};
use crate::service::temp_file::TemporaryFile;

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

/// 上传单文件
///
/// 对齐 nuwax computer uploadFile; multipart。
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

/// 批量上传文件
///
/// 对齐 nuwax computer uploadFiles; 多文件 multipart。
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
