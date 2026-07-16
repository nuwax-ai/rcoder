//! `/api/project` 路由 (对齐 nuwax projectRoutes + codeRoutes, 二级路径不冲突)。

use axum::extract::{Multipart, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::response;
use crate::service::{code as code_service, project as project_service, tree, upload as upload_service};
use crate::workspace::ProjectContext;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/get-project-content", get(get_project_content))
        // nuwax delete-project 用 GET (反直觉, 但保持兼容)
        .route("/delete-project", get(delete_project))
        .route("/create-project", post(create_project))
        .route("/copy-project", post(copy_project))
        .route("/upload-single-file", post(upload_single_file))
        .route("/upload-batch-files", post(upload_batch_files))
        .route("/upload-attachment-file", post(upload_attachment_file))
        .route("/specified-files-update", post(specified_files_update))
        .route("/all-files-update", post(all_files_update))
}

fn ctx_from(
    project_id: &str,
    tenant: Option<String>,
    space: Option<String>,
    iso: Option<String>,
) -> ProjectContext {
    ProjectContext {
        project_id: project_id.to_string(),
        tenant_id: tenant,
        space_id: space,
        isolation_type: iso,
    }
}

async fn text_field(field: axum::extract::multipart::Field<'_>) -> Result<String, AppError> {
    field
        .text()
        .await
        .map_err(|e| AppError::validation(format!("read multipart field: {e}")))
}

async fn bytes_field(field: axum::extract::multipart::Field<'_>) -> Result<Vec<u8>, AppError> {
    field
        .bytes()
        .await
        .map(|b| b.to_vec())
        .map_err(|e| AppError::validation(format!("read multipart file: {e}")))
}

// ── get-project-content ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GetContentParams {
    project_id: String,
    command: Option<String>,
    proxy_path: Option<String>,
    tenant_id: Option<String>,
    space_id: Option<String>,
    isolation_type: Option<String>,
}

/// `GET /api/project/get-project-content`
async fn get_project_content(
    State(state): State<AppState>,
    Query(params): Query<GetContentParams>,
) -> Response {
    let project_id = params.project_id.trim();
    if project_id.is_empty() {
        return AppError::validation("Project ID cannot be empty").into_response();
    }
    let ctx = ProjectContext {
        project_id: project_id.to_string(),
        tenant_id: params.tenant_id.clone(),
        space_id: params.space_id.clone(),
        isolation_type: params.isolation_type.clone(),
    };
    let project_path = state.resolver.resolve_project(&ctx);

    if !tokio::fs::try_exists(&project_path).await.unwrap_or(false) {
        return AppError::validation("Project does not exist").into_response();
    }

    match tree::get_project_content(
        &project_path,
        &state.config,
        params.command.as_deref(),
        params.proxy_path.as_deref(),
    )
    .await
    {
        Ok(content) => Json(json!({
            "success": true,
            "files": content.files,
            "frontendFramework": content.frontend_framework,
            "devFramework": content.dev_framework,
        }))
        .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            response::failure_msg(&e.to_string()),
        )
            .into_response(),
    }
}

// ── delete-project ───────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeleteParams {
    project_id: String,
    #[allow(dead_code)]
    pid: Option<String>,
    tenant_id: Option<String>,
    space_id: Option<String>,
    isolation_type: Option<String>,
}

/// `GET /api/project/delete-project`
async fn delete_project(
    State(state): State<AppState>,
    Query(params): Query<DeleteParams>,
) -> Result<Json<serde_json::Value>, AppError> {
    let project_id = params.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let ctx = ProjectContext {
        project_id: project_id.clone(),
        tenant_id: params.tenant_id.clone(),
        space_id: params.space_id.clone(),
        isolation_type: params.isolation_type.clone(),
    };
    let result = project_service::delete_project(&*state.resolver, &state.config, &ctx).await?;

    let message = if result.failed.is_empty() {
        "Project deleted successfully".to_string()
    } else {
        format!(
            "Project deleted, but {} directories deleted failed",
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CreateProjectBody {
    project_id: String,
    template_type: Option<String>,
    tenant_id: Option<String>,
    space_id: Option<String>,
    isolation_type: Option<String>,
}

/// `POST /api/project/create-project`
async fn create_project(
    State(state): State<AppState>,
    Json(body): Json<CreateProjectBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
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
        "message": "Project created successfully",
        "projectPath": result.project_path,
    })))
}

// ── copy-project ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CopyProjectBody {
    source_project_id: String,
    target_project_id: String,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    isolation_type: Option<String>,
    #[serde(default)]
    source_tenant_id: Option<String>,
    #[serde(default)]
    source_space_id: Option<String>,
    #[serde(default)]
    source_isolation_type: Option<String>,
    #[serde(default)]
    target_tenant_id: Option<String>,
    #[serde(default)]
    target_space_id: Option<String>,
    #[serde(default)]
    target_isolation_type: Option<String>,
}

/// `POST /api/project/copy-project` (源/目标各自隔离上下文, 缺省回退公共字段)
async fn copy_project(
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
        isolation_type: body.source_isolation_type.clone().or_else(|| common_i.clone()),
    };
    let target_ctx = ProjectContext {
        project_id: body.target_project_id.trim().to_string(),
        tenant_id: body.target_tenant_id.clone().or_else(|| common_t.clone()),
        space_id: body.target_space_id.clone().or_else(|| common_s.clone()),
        isolation_type: body.target_isolation_type.clone().or_else(|| common_i.clone()),
    };
    let result =
        project_service::copy_project(&*state.resolver, &state.config, &source_ctx, &target_ctx)
            .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project copied successfully",
        "sourceProjectId": result.source_project_id,
        "targetProjectId": result.target_project_id,
        "targetProjectPath": result.target_project_path,
    })))
}

// ── upload-single-file (multipart) ───────────────────────────────────────────────

/// `POST /api/project/upload-single-file`
async fn upload_single_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut code_version = None;
    let mut file_path = None;
    let mut data: Option<Vec<u8>> = None;
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
            "file" => data = Some(bytes_field(field).await?),
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
        data,
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
async fn upload_batch_files(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut code_version = None;
    let mut file_paths: Vec<String> = Vec::new();
    let mut files_vec: Vec<Vec<u8>> = Vec::new();
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
            "files" => files_vec.push(bytes_field(field).await?),
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
    let pairs: Vec<(String, Vec<u8>)> = file_paths.into_iter().zip(files_vec).collect();
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
async fn upload_attachment_file(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<serde_json::Value>, AppError> {
    let mut project_id = None;
    let mut file_name = None;
    let mut data: Option<Vec<u8>> = None;
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
                data = Some(bytes_field(field).await?);
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
    let ctx = ctx_from(project_id.trim(), tenant, space, iso);
    let result = upload_service::upload_attachment_file(
        &*state.resolver,
        &ctx,
        file_name.as_deref(),
        &original,
        data,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "fileName": result.file_name,
        "relativePath": result.relative_path,
    })))
}

// ── specified-files-update (JSON) ────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpecifiedBody {
    project_id: String,
    code_version: String,
    files: Vec<code_service::FileOp>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    isolation_type: Option<String>,
}

/// `POST /api/project/specified-files-update` (create/delete/rename/modify 增量)
async fn specified_files_update(
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
    let ctx = ctx_from(&project_id, body.tenant_id, body.space_id, body.isolation_type);
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

// ── all-files-update (JSON) ──────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AllFilesBody {
    project_id: String,
    code_version: String,
    files: Vec<code_service::FileEntry>,
    #[serde(default)]
    tenant_id: Option<String>,
    #[serde(default)]
    space_id: Option<String>,
    #[serde(default)]
    isolation_type: Option<String>,
    #[allow(dead_code)]
    #[serde(default)]
    base_path: Option<String>, // nuwax 接收但未使用
    #[allow(dead_code)]
    #[serde(default)]
    pid: Option<String>,
}

/// `POST /api/project/all-files-update` (全量覆盖 + 清理缺失)
async fn all_files_update(
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
    let ctx = ctx_from(&project_id, body.tenant_id, body.space_id, body.isolation_type);
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
