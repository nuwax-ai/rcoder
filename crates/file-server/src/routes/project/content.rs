//! project 内容读取路由: get-project-content / get-project-content-by-version。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::response;
use crate::service::{tree, version as version_service};
use crate::workspace::ProjectContext;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetContentParams {
    pub project_id: String,
    pub command: Option<String>,
    pub proxy_path: Option<String>,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
}

/// `GET /api/project/get-project-content`
#[utoipa::path(
    get,
    path = "/get-project-content",
    params(GetContentParams),
    responses(crate::openapi::JsonApiResponses),
    tag = "Project"
)]
pub(super) async fn get_project_content(
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

    match crate::service::fs_util::path_exists(&project_path).await {
        Ok(true) => {}
        Ok(false) => return AppError::validation("Project does not exist").into_response(),
        Err(error) => return error.into_response(),
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(super) struct GetByVersionParams {
    pub project_id: String,
    pub code_version: String,
    pub proxy_path: Option<String>,
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub tenant_id: Option<String>,
    #[serde(default)]
    pub space_id: Option<String>,
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// `GET /api/project/get-project-content-by-version`
#[utoipa::path(
    get,
    path = "/get-project-content-by-version",
    params(GetByVersionParams),
    responses(crate::openapi::JsonApiResponses),
    tag = "Project"
)]
pub(super) async fn get_project_content_by_version(
    State(state): State<AppState>,
    Query(params): Query<GetByVersionParams>,
) -> Response {
    if state.config.git_enabled {
        return response::deprecated(
            "此接口已废弃,请使用 /api/git/log + /api/git/diff 查看历史版本内容",
        )
        .into_response();
    }
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
    match version_service::get_content_by_version(
        &*state.resolver,
        &state.config,
        &ctx,
        &params.code_version,
        params.proxy_path.as_deref(),
        params.command.as_deref(),
    )
    .await
    {
        Ok(files) => Json(json!({ "success": true, "files": files })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            response::failure_msg(&e.to_string()),
        )
            .into_response(),
    }
}
