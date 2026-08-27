//! project 内容读取 handlers: get-project-content / get-project-content-by-version。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use garde::Validate;
use serde::Deserialize;
use serde_json::json;

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::response;
use crate::service::{tree, version as version_service};
use crate::workspace::ProjectContext;

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetContentParams {
    /// 项目 ID
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    /// 框架探测命令 (可选；前端框架识别失败时兜底执行的自定义命令)
    pub command: Option<String>,
    /// 代理子路径 (可选；透传给框架探测的环境信息)
    pub proxy_path: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    pub isolation_type: Option<String>,
}

/// 获取项目内容
#[utoipa::path(
    get,
    path = "/get-project-content",
    params(GetContentParams),
    responses(crate::openapi::JsonApiResponses),
    tag = "Project"
)]
pub(crate) async fn get_project_content(
    State(state): State<AppState>,
    Query(params): Query<GetContentParams>,
) -> Response {
    if let Err(report) = params.validate() {
        return crate::error::from_garde(report).into_response();
    }
    let project_id = params.project_id.trim();
    let ctx = ProjectContext {
        project_id: project_id.to_string(),
        tenant_id: params.tenant_id.clone(),
        space_id: params.space_id.clone(),
        isolation_type: params.isolation_type.clone(),
    };
    let project_path = match state.resolver.resolve_project(&ctx).await {
        Ok(path) => path,
        Err(error) => return error.into_response(),
    };

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

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[garde(allow_unvalidated)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct GetByVersionParams {
    /// 项目 ID
    #[garde(custom(crate::validation_rules::not_blank))]
    pub project_id: String,
    /// 代码版本号（code_version 仓库的版本标签）
    #[garde(custom(crate::validation_rules::not_blank))]
    pub code_version: String,
    /// 代理子路径 (可选；透传给框架探测的环境信息)
    pub proxy_path: Option<String>,
    /// 框架探测命令 (可选；前端框架识别失败时兜底执行的自定义命令)
    #[serde(default)]
    pub command: Option<String>,
    /// 租户 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub tenant_id: Option<String>,
    /// 空间 ID（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub space_id: Option<String>,
    /// 隔离类型（多租户隔离；本地部署可缺省）
    #[serde(default)]
    pub isolation_type: Option<String>,
}

/// 按版本获取项目内容
#[utoipa::path(
    get,
    path = "/get-project-content-by-version",
    params(GetByVersionParams),
    responses(crate::openapi::JsonApiResponses),
    tag = "Project"
)]
pub(crate) async fn get_project_content_by_version(
    State(state): State<AppState>,
    Query(params): Query<GetByVersionParams>,
) -> Response {
    if state.config.git_enabled {
        return response::deprecated(
            "此接口已废弃,请使用 /api/git/log + /api/git/diff 查看历史版本内容",
        )
        .into_response();
    }
    if let Err(report) = params.validate() {
        return crate::error::from_garde(report).into_response();
    }
    let project_id = params.project_id.trim();
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
