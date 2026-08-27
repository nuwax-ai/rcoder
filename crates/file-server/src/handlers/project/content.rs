//! project 内容读取 handlers: get-project-content / get-project-content-by-version。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use garde::Validate;
use serde_json::json;

use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{GetByVersionParams, GetContentParams};
use crate::response;
use crate::service::{tree, version as version_service};
use crate::workspace::ProjectContext;

/// 获取项目内容
#[utoipa::path(
    get,
    path = "/get-project-content",
    params(GetContentParams),
    description = r#"
拉取项目内容树 + 前端框架探测结果：返回 `files` 树、`frontendFramework`/`devFramework`（探测失败时可传 `command` 兜底执行自定义命令）——打开工作台的第一数据源。
"#,
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

/// 按版本获取项目内容
#[utoipa::path(
    get,
    path = "/get-project-content-by-version",
    params(GetByVersionParams),
    description = r#"
同 [`get-project-content`](#) 但以 `codeVersion` 指定历史版本读取内容树（版本对比/回滚预览场景）。
"#,
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
