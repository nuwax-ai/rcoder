//! project 增删改/上传 handlers: delete / create / copy / upload-single / upload-batch /
//! upload-attachment / upload-project / push-skills。

use axum::extract::State;
use serde::Deserialize;
use serde_json::json;

use super::{ctx_from, file_field, text_field, validate_zip_ext};
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart, AppQuery as Query};
use crate::service::{
    project as project_service, skills as skills_service, upload as upload_service,
};
use crate::workspace::ProjectContext;

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadSingleFileForm {
    pub project_id: String,
    pub code_version: String,
    pub file_path: String,
    #[schema(format = Binary)]
    pub file: String,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadBatchFilesForm {
    pub project_id: String,
    pub code_version: String,
    pub file_paths: Vec<String>,
    pub files: Vec<crate::openapi::BinaryFile>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadAttachmentForm {
    pub project_id: String,
    pub file_name: Option<String>,
    #[schema(format = Binary)]
    pub file: String,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UploadProjectForm {
    pub project_id: String,
    pub code_version: String,
    #[schema(format = Binary)]
    pub file: String,
    pub pid: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PushProjectSkillsForm {
    pub project_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
    pub skill_urls: Option<Vec<String>>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

// ── delete-project ───────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteParams {
    pub project_id: String,
    #[serde(default)]
    pub pid: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

/// `GET /api/project/delete-project`
#[utoipa::path(
    get,
    path = "/delete-project",
    params(DeleteParams),
    responses(crate::openapi::JsonApiResponses),
    tag = "Project"
)]
pub(crate) async fn delete_project(
    State(state): State<AppState>,
    Query(params): Query<DeleteParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let project_id = params.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    // 停 dev server (对齐 nuwax: pid 可用时 stopDevServer, 失败不阻塞)
    if let Some(p) = params
        .pid
        .as_deref()
        .and_then(|s| s.trim().parse::<u32>().ok())
    {
        tracing::debug!(project_id = %project_id, pid = p, "delete-project: stop dev server");
        if let Err(e) = state.dev_server.stop_dev(&project_id).await {
            tracing::warn!(error = %e, project_id = %project_id, "stop dev server on delete failed (skipping)");
        }
    }
    let ctx = ProjectContext {
        project_id: project_id.clone(),
        tenant_id: params.tenant_id.clone(),
        space_id: params.space_id.clone(),
        isolation_type: params.isolation_type.clone(),
    };
    let result = project_service::delete_project(&*state.resolver, &state.config, &ctx).await?;

    let message = if result.failed.is_empty() {
        format!("Project {project_id} deleted successfully")
    } else {
        format!(
            "Project {project_id} deleted, but {} directories deleted failed",
            result.failed.len()
        )
    };
    Ok(Json(json!({
        "success": true,
        "message": message,
        "projectId": project_id,
        "deletedDirectories": result.deleted,
        "failedDirectories": result.failed,
    })))
}

// ── create-project ───────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateProjectBody {
    #[serde(default, deserialize_with = "crate::extract::deserialize_id_string")]
    #[schema(required = true)]
    pub project_id: String,
    pub template_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

/// `POST /api/project/create-project`
#[utoipa::path(post, path = "/create-project", request_body = CreateProjectBody, responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn create_project(
    State(state): State<AppState>,
    Json(body): Json<CreateProjectBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation_with(
            "Project ID cannot be empty",
            json!({ "field": "projectId" }),
        ));
    }
    let template_type = body.template_type.as_deref().unwrap_or("react").to_string();
    let ctx = ctx_from(
        &project_id,
        body.tenant_id.clone(),
        body.space_id.clone(),
        body.isolation_type.clone(),
    );
    let result =
        project_service::create_project(&*state.resolver, &state.config, &ctx, &template_type)
            .await?;
    Ok(Json(json!({
        "success": true,
        "message": format!("Project {project_id} created successfully"),
        "projectPath": result.project_path,
    })))
}

// ── copy-project ─────────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CopyProjectBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub source_project_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub target_project_id: String,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub source_tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub source_space_id: Option<String>,
    #[serde(default)]
    pub source_isolation_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub target_tenant_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::extract::deserialize_optional_id_string"
    )]
    pub target_space_id: Option<String>,
    #[serde(default)]
    pub target_isolation_type: Option<String>,
}

/// `POST /api/project/copy-project` (源/目标各自隔离上下文, 缺省回退公共字段)
#[utoipa::path(post, path = "/copy-project", request_body = CopyProjectBody, responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn copy_project(
    State(state): State<AppState>,
    Json(body): Json<CopyProjectBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let common_t = body.tenant_id.clone();
    let common_s = body.space_id.clone();
    let common_i = body.isolation_type.clone();
    let source_ctx = ProjectContext {
        project_id: body.source_project_id.trim().to_string(),
        tenant_id: body.source_tenant_id.clone().or_else(|| common_t.clone()),
        space_id: body.source_space_id.clone().or_else(|| common_s.clone()),
        isolation_type: body
            .source_isolation_type
            .clone()
            .or_else(|| common_i.clone()),
    };
    let target_ctx = ProjectContext {
        project_id: body.target_project_id.trim().to_string(),
        tenant_id: body.target_tenant_id.clone().or_else(|| common_t.clone()),
        space_id: body.target_space_id.clone().or_else(|| common_s.clone()),
        isolation_type: body
            .target_isolation_type
            .clone()
            .or_else(|| common_i.clone()),
    };
    let result =
        project_service::copy_project(&*state.resolver, &state.config, &source_ctx, &target_ctx)
            .await?;
    Ok(Json(json!({
        "success": true,
        "message": format!(
            "Project {} successfully copied to {}",
            result.source_project_id, result.target_project_id
        ),
        "sourceProjectId": result.source_project_id,
        "targetProjectId": result.target_project_id,
        "targetProjectPath": result.target_project_path,
    })))
}

// ── upload-single-file (multipart) ───────────────────────────────────────────────

/// `POST /api/project/upload-single-file`
#[utoipa::path(post, path = "/upload-single-file", request_body(content = UploadSingleFileForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Code")]
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
    let project_id = project_id.ok_or_else(|| AppError::validation("projectId is required"))?;
    let code_version =
        code_version.ok_or_else(|| AppError::validation("codeVersion is required"))?;
    let file_path = file_path.ok_or_else(|| AppError::validation("filePath is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;

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

/// `POST /api/project/upload-batch-files`
#[utoipa::path(post, path = "/upload-batch-files", request_body(content = UploadBatchFilesForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Code")]
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
    let project_id = project_id.ok_or_else(|| AppError::validation("projectId is required"))?;
    let code_version =
        code_version.ok_or_else(|| AppError::validation("codeVersion is required"))?;
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

/// `POST /api/project/upload-attachment-file`
#[utoipa::path(post, path = "/upload-attachment-file", request_body(content = UploadAttachmentForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Project")]
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
    let project_id = project_id.ok_or_else(|| AppError::validation("projectId is required"))?;
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let original = original_name.unwrap_or_else(|| "attachment".to_string());
    let effective_name = file_name.as_deref().unwrap_or(&original);
    let extension = std::path::Path::new(effective_name)
        .extension()
        .and_then(|v| v.to_str())
        .map(|v| format!(".{}", v.to_ascii_lowercase()))
        .unwrap_or_default();
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

/// `POST /api/project/upload-project` (上传 zip 覆盖项目)
#[utoipa::path(post, path = "/upload-project", request_body(content = UploadProjectForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Project")]
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
    let project_id = project_id.ok_or_else(|| AppError::validation("projectId is required"))?;
    let code_version =
        code_version.ok_or_else(|| AppError::validation("codeVersion is required"))?;
    let data = data.ok_or_else(|| AppError::validation("Please upload a zip file"))?;
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

// ── push-skills-to-workspace (multipart: file zip 和/或 skillUrls) ───────────────

/// `POST /api/project/push-skills-to-workspace`
#[utoipa::path(post, path = "/push-skills-to-workspace", request_body(content = PushProjectSkillsForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn push_skills_to_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut zip_data = None;
    let mut skill_urls: Vec<String> = Vec::new();
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
            "file" => {
                zip_data = Some(
                    file_field(
                        field,
                        state.config.upload_max_file_size_bytes,
                        &state.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            "skillUrls" => {
                let t = text_field(field).await?;
                // 兼容 JSON 数组字符串 / 单 URL
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            "tenantId" => tenant = Some(text_field(field).await?),
            "spaceId" => space = Some(text_field(field).await?),
            "isolationType" => iso = Some(text_field(field).await?),
            _ => {}
        }
    }
    let project_id = project_id.ok_or_else(|| AppError::validation("projectId is required"))?;
    state
        .skill_downloader
        .validate_url_count(skill_urls.len())?;
    let ctx = ctx_from(project_id.trim(), tenant, space, iso);
    let result = skills_service::push_skills(
        &*state.resolver,
        &ctx,
        zip_data.as_ref().map(|file| file.path()),
        skill_urls,
        &state.skill_downloader,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Skills pushed to workspace",
        "projectPath": result.project_path,
        "updatedSkills": result.updated_skills,
    })))
}
