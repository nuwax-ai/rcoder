//! project 版本管理 handlers: backup-current-version / rollback-version / export-project。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use super::ctx_from;
use crate::AppState;
use crate::error::AppError;
use crate::extract::AppJson as Json;
use crate::response;
use crate::service::{project as project_service, version as version_service};

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct BackupVersionBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub project_id: String,
    pub code_version: String,
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
}

/// `POST /api/project/backup-current-version`
#[utoipa::path(post, path = "/backup-current-version", request_body = BackupVersionBody, responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn backup_current_version(
    State(state): State<AppState>,
    Json(body): Json<BackupVersionBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if state.config.git_enabled {
        return Ok(response::deprecated(
            "此接口已废弃,请使用 Git 版本管理 API（/api/git/*）",
        ));
    }
    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if body.code_version.trim().is_empty() {
        return Err(AppError::validation("codeVersion cannot be empty"));
    }
    let ctx = ctx_from(
        &project_id,
        body.tenant_id,
        body.space_id,
        body.isolation_type,
    );
    let result = version_service::backup_current_version(
        &*state.resolver,
        &state.config,
        &ctx,
        &body.code_version,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "projectId": result.project_id,
        "zipPath": result.zip_path,
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RollbackBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub project_id: String,
    pub code_version: String,
    pub rollback_to: String,
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
}

/// `POST /api/project/rollback-version`
#[utoipa::path(post, path = "/rollback-version", request_body = RollbackBody, responses(crate::openapi::JsonApiResponses), tag = "Project")]
pub(crate) async fn rollback_version(
    State(state): State<AppState>,
    Json(body): Json<RollbackBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    if state.config.git_enabled {
        return Ok(response::deprecated(
            "此接口已废弃,请使用 /api/git/rollback 进行版本回滚",
        ));
    }
    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let ctx = ctx_from(
        &project_id,
        body.tenant_id,
        body.space_id,
        body.isolation_type,
    );
    let result = version_service::rollback_version(
        &*state.resolver,
        &state.config,
        &ctx,
        &body.code_version,
        &body.rollback_to,
    )
    .await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project rolled back successfully",
        "newVersion": result.new_version,
        "rollbackTo": result.rollback_to,
    })))
}

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ExportBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub project_id: String,
    pub code_version: String,
    #[serde(default)]
    pub export_type: Option<String>,
    #[serde(default)]
    #[schema(value_type = Object)]
    pub config: Option<serde_json::Value>,
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
}

/// `POST /api/project/export-project` (返回 application/zip 文件流)
#[utoipa::path(
    post,
    path = "/export-project",
    request_body = ExportBody,
    responses(
        (status = 200, description = "Project ZIP archive", body = crate::openapi::BinaryFile, content_type = "application/zip"),
        crate::openapi::ErrorApiResponses
    ),
    tag = "Project"
)]
pub(crate) async fn export_project(
    State(state): State<AppState>,
    Json(body): Json<ExportBody>,
) -> Result<Response, AppError> {
    let project_id = body.project_id.trim().to_string();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let ctx = ctx_from(
        &project_id,
        body.tenant_id,
        body.space_id,
        body.isolation_type,
    );
    let zip_path = project_service::export_project(
        &*state.resolver,
        &state.config,
        &ctx,
        &body.code_version,
        body.export_type.as_deref(),
        body.config.as_ref(),
    )
    .await?;
    let archive_size = tokio::fs::metadata(&zip_path).await?.len();
    let archive_body = crate::service::temp_file::file_body(&zip_path).await?;
    let filename = zip_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("project.zip");
    Ok((
        StatusCode::OK,
        [
            (
                axum::http::header::CONTENT_TYPE,
                axum::http::HeaderValue::from_static("application/zip"),
            ),
            (
                axum::http::header::CONTENT_DISPOSITION,
                axum::http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
                    .unwrap_or_else(|_| axum::http::HeaderValue::from_static("attachment")),
            ),
            (
                axum::http::header::CONTENT_LENGTH,
                axum::http::HeaderValue::from_str(&archive_size.to_string()).map_err(|error| {
                    AppError::system(format!("build export content length header: {error}"))
                })?,
            ),
        ],
        archive_body,
    )
        .into_response())
}
