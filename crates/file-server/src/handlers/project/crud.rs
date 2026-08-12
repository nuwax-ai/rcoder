//! project 增删改 handlers: delete / create / copy。
//!
//! 上传类 (upload-single/batch/attachment/project) 见 [`super::upload`]；
//! skills 推送见 [`super::skills`]。

use axum::extract::State;
use garde::Validate;
use serde::Deserialize;
use serde_json::json;

use super::ctx_from;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::service::project as project_service;
use crate::workspace::ProjectContext;

// ── delete-project ───────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DeleteParams {
    #[garde(custom(crate::validation_rules::not_blank))]
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
    params.validate().map_err(crate::error::from_garde)?;
    let project_id = params.project_id.trim().to_string();
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

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CreateProjectBody {
    #[serde(default, deserialize_with = "crate::extract::deserialize_id_string")]
    #[schema(required = true)]
    #[garde(custom(crate::validation_rules::not_blank))]
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
    body.validate().map_err(crate::error::from_garde)?;
    let project_id = body.project_id.trim().to_string();
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
