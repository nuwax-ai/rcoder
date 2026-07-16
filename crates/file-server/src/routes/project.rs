//! `/api/project` 路由 (对齐 nuwax projectRoutes + codeRoutes, 二级路径不冲突)。

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::AppError;
use crate::response;
use crate::service::{project as project_service, tree};
use crate::workspace::ProjectContext;
use crate::AppState;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/get-project-content", get(get_project_content))
        // nuwax delete-project 用 GET (反直觉, 但保持兼容)
        .route("/delete-project", get(delete_project))
        .route("/create-project", post(create_project))
        .route("/copy-project", post(copy_project))
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
///
/// 错误处理对齐 nuwax: 前置校验 (projectId 空 / 项目不存在) 走 AppError (全局 shape);
/// 遍历失败路由自捕 → `{success:false, message}` + 500。
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
    let ctx = ProjectContext {
        project_id: project_id.clone(),
        tenant_id: body.tenant_id.clone(),
        space_id: body.space_id.clone(),
        isolation_type: body.isolation_type.clone(),
    };
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
