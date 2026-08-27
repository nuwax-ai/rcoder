//! project 文件上传 handlers (multipart): upload-single-file / upload-batch-files /
//! upload-attachment-file / upload-project。

use axum::extract::State;
use garde::Validate;
use serde_json::json;

use super::{ctx_from, file_field, text_field, validate_zip_ext};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart};
use crate::models::{
    UploadAttachmentForm, UploadBatchFilesForm, UploadProjectForm, UploadSingleFileForm,
};
use crate::service::temp_file::TemporaryFile;
use crate::service::{project as project_service, upload as upload_service};

// ── multipart 提取后必填字段校验 (garde 声明式) ─────────────────────────────────

#[derive(garde::Validate)]
struct UploadSingleFileFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    project_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    code_version: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    file_path: Option<String>,
    #[garde(required)]
    data: Option<TemporaryFile>,
}

#[derive(garde::Validate)]
struct UploadBatchFilesFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    project_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    code_version: Option<String>,
}

#[derive(garde::Validate)]
struct UploadAttachmentFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    project_id: Option<String>,
    #[garde(required)]
    data: Option<TemporaryFile>,
}

#[derive(garde::Validate)]
struct UploadProjectFields {
    #[garde(custom(crate::validation_rules::required_not_blank))]
    project_id: Option<String>,
    #[garde(custom(crate::validation_rules::required_not_blank))]
    code_version: Option<String>,
    #[garde(required)]
    data: Option<TemporaryFile>,
}

// ── upload-single-file (multipart) ───────────────────────────────────────────────

/// 单文件上传
#[utoipa::path(post, path = "/upload-single-file", request_body(content = UploadSingleFileForm, content_type = "multipart/form-data"), description = r#"
上传单个文件到项目内指定路径（multipart：file + 目标路径字段）。适合配置/小文件即时修改。
"#,
    responses(crate::openapi::JsonApiResponses), tag = "Code")]
pub(crate) async fn upload_single_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut code_version = None;
    let mut file_path = None;
    let mut data = None;
    let mut tenant = None;
    let mut space = None;
    let mut iso = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "projectId" => project_id = Some(text_field(field).await?),
            "codeVersion" => code_version = Some(text_field(field).await?),
            "filePath" => file_path = Some(text_field(field).await?),
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
            "tenantId" => tenant = Some(text_field(field).await?),
            "spaceId" => space = Some(text_field(field).await?),
            "isolationType" => iso = Some(text_field(field).await?),
            _ => {}
        }
    }
    let fields = UploadSingleFileFields {
        project_id,
        code_version,
        file_path,
        data,
    };
    fields.validate().map_err(crate::error::from_garde)?;
    // 校验已保证必填; 取数 (失败逻辑不可达, 防御性处理)
    let project_id = fields
        .project_id
        .ok_or_else(|| AppError::system("project_id missing after garde validation"))?;
    let code_version = fields
        .code_version
        .ok_or_else(|| AppError::system("code_version missing after garde validation"))?;
    let file_path = fields
        .file_path
        .ok_or_else(|| AppError::system("file_path missing after garde validation"))?;
    let data = fields
        .data
        .ok_or_else(|| AppError::system("file missing after garde validation"))?;

    let ctx = ctx_from(project_id.trim(), tenant, space, iso);
    let result = upload_service::upload_single_file(
        &*state.resolver,
        &state.config,
        &ctx,
        &file_path,
        data.path(),
        &code_version,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "File uploaded successfully, no need to restart development server",
        "projectId": result.project_id,
        "restarted": false,
    })))
}

// ── upload-batch-files (multipart) ───────────────────────────────────────────────

/// 批量文件上传
#[utoipa::path(post, path = "/upload-batch-files", request_body(content = UploadBatchFilesForm, content_type = "multipart/form-data"), description = r#"
批量上传多个文件（multipart 多 file 字段 + 各自相对路径），一次请求落盘多文件。
"#,
    responses(crate::openapi::JsonApiResponses), tag = "Code")]
pub(crate) async fn upload_batch_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut code_version = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec = Vec::new();
    let mut tenant = None;
    let mut space = None;
    let mut iso = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "projectId" => project_id = Some(text_field(field).await?),
            "codeVersion" => code_version = Some(text_field(field).await?),
            "filePaths" => file_paths.push(text_field(field).await?),
            "files" => files_vec.push(
                file_field(
                    field,
                    state.config.upload_max_file_size_bytes,
                    &state.config.upload_project_dir.join("temp"),
                )
                .await?,
            ),
            "tenantId" => tenant = Some(text_field(field).await?),
            "spaceId" => space = Some(text_field(field).await?),
            "isolationType" => iso = Some(text_field(field).await?),
            _ => {}
        }
    }
    let fields = UploadBatchFilesFields {
        project_id,
        code_version,
    };
    fields.validate().map_err(crate::error::from_garde)?;
    // 校验已保证必填; 取数 (失败逻辑不可达, 防御性处理)
    let project_id = fields
        .project_id
        .ok_or_else(|| AppError::system("project_id missing after garde validation"))?;
    let code_version = fields
        .code_version
        .ok_or_else(|| AppError::system("code_version missing after garde validation"))?;
    // 跨字段一致性 (文件路径与文件一一对应), 非纯字段校验, 保留手写
    if file_paths.len() != files_vec.len() {
        return Err(AppError::validation("filePaths and files count mismatch"));
    }
    let pairs: Vec<(String, std::path::PathBuf)> = file_paths
        .into_iter()
        .zip(files_vec.iter().map(|file| file.path().to_path_buf()))
        .collect();
    let ctx = ctx_from(project_id.trim(), tenant, space, iso);
    let written = upload_service::upload_batch_files(
        &*state.resolver,
        &state.config,
        &ctx,
        pairs,
        &code_version,
    )
    .await?;
    let count = written.len();
    Ok(Json(json!({
        "success": true,
        "message": format!("{count} files uploaded successfully"),
        "projectId": project_id.trim(),
        "fileCount": count,
        "files": written,
        "restarted": false,
    })))
}

// ── upload-attachment-file (multipart) ───────────────────────────────────────────

/// 附件上传
#[utoipa::path(post, path = "/upload-attachment-file", request_body(content = UploadAttachmentForm, content_type = "multipart/form-data"), description = r#"
上传附件类文件（对话附件/资源文件场景）；路径归属与校验规则见 Schema 字段说明。
"#,
    responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn upload_attachment_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut file_name = None;
    let mut data = None;
    let mut original_name = None;
    let mut tenant = None;
    let mut space = None;
    let mut iso = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "projectId" => project_id = Some(text_field(field).await?),
            "fileName" => file_name = Some(text_field(field).await?),
            "file" => {
                original_name = field.file_name().map(|s| s.to_string());
                data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                );
            }
            "tenantId" => tenant = Some(text_field(field).await?),
            "spaceId" => space = Some(text_field(field).await?),
            "isolationType" => iso = Some(text_field(field).await?),
            _ => {}
        }
    }
    let fields = UploadAttachmentFields { project_id, data };
    fields.validate().map_err(crate::error::from_garde)?;
    // 校验已保证必填; 取数 (失败逻辑不可达, 防御性处理)
    let project_id = fields
        .project_id
        .ok_or_else(|| AppError::system("project_id missing after garde validation"))?;
    let data = fields
        .data
        .ok_or_else(|| AppError::system("file missing after garde validation"))?;
    let original = original_name.unwrap_or_else(|| "attachment".to_string());
    let effective_name = file_name.as_deref().unwrap_or(&original);
    let extension = std::path::Path::new(effective_name)
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| format!(".{}", v.to_ascii_lowercase()))
        .unwrap_or_default();
    // 扩展名白名单依赖运行时 config, 非纯字段校验, 保留手写
    if !state
        .config
        .attachment_allowed_extensions
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(&extension))
    {
        return Err(AppError::validation(format!(
            "attachment extension is not allowed: {extension}"
        )));
    }
    let ctx = ctx_from(project_id.trim(), tenant, space, iso);
    let result = upload_service::upload_attachment_file(
        &*state.resolver,
        &ctx,
        file_name.as_deref(),
        &original,
        data.path(),
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "fileName": result.file_name,
        "relativePath": result.relative_path,
    })))
}

// ── upload-project (multipart zip) ──────────────────────────────────────────────

/// 上传项目 zip 覆盖
///
/// 上传 zip 覆盖项目。
#[utoipa::path(post, path = "/upload-project", request_body(content = UploadProjectForm, content_type = "multipart/form-data"), description = r#"
上传 zip 整包**覆盖**项目目录（自动解压；含 wrapper 目录剥除参数）。用于整版本替换导入——增量修改请走 single/batch 或 files-update。
"#,
    responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn upload_project(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut code_version = None;
    let mut data = None;
    let mut file_name = None;
    let mut pid = None;
    let mut tenant = None;
    let mut space = None;
    let mut iso = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "projectId" => project_id = Some(text_field(field).await?),
            "codeVersion" => code_version = Some(text_field(field).await?),
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
            "pid" => pid = text_field(field).await.ok(),
            "tenantId" => tenant = Some(text_field(field).await?),
            "spaceId" => space = Some(text_field(field).await?),
            "isolationType" => iso = Some(text_field(field).await?),
            _ => {}
        }
    }
    let fields = UploadProjectFields {
        project_id,
        code_version,
        data,
    };
    fields.validate().map_err(crate::error::from_garde)?;
    // 校验已保证必填; 取数 (失败逻辑不可达, 防御性处理)
    let project_id = fields
        .project_id
        .ok_or_else(|| AppError::system("project_id missing after garde validation"))?;
    let code_version = fields
        .code_version
        .ok_or_else(|| AppError::system("code_version missing after garde validation"))?;
    let data = fields
        .data
        .ok_or_else(|| AppError::system("zip file missing after garde validation"))?;
    // 扩展名 (仅 zip) + 大小上限校验 (对齐 nuwax multer fileFilter/limits)
    validate_zip_ext(file_name.as_deref())?;
    // 停旧版 dev server (对齐 nuwax: pid 可用时 stopDevServer, 失败不阻塞)
    if let Some(p) = pid.as_deref().and_then(|s| s.trim().parse::<u32>().ok()) {
        tracing::debug!(project_id = %project_id, pid = p, "upload-project: stop old dev server");
        if let Err(e) = state.dev_server.stop_dev(project_id.trim()).await {
            tracing::warn!(error = %e, project_id = %project_id, "stop old dev server on upload failed (skipping)");
        }
    }
    let ctx = ctx_from(project_id.trim(), tenant, space, iso);
    let result = project_service::upload_project(
        &*state.resolver,
        &state.config,
        &ctx,
        &code_version,
        data.path(),
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": format!("Project {} uploaded successfully", result.project_id),
        "projectId": result.project_id,
        "codeVersion": result.code_version,
    })))
}
