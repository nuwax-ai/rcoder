//! project 增删改 handlers: delete / create / copy。
//!
//! 上传类 (upload-single/batch/attachment/project) 见 [`super::upload`]；
//! skills 推送见 [`super::skills`]。

use axum::extract::State;
use garde::Validate;
use serde_json::json;

use super::ctx_from;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppQuery as Query};
use crate::models::{CopyProjectBody, CreateProjectBody, DeleteParams};
use crate::service::project as project_service;
use crate::workspace::ProjectContext;

// ── delete-project ───────────────────────────────────────────────────────────────

/// 删除项目
#[utoipa::path(
    get,
    path = "/delete-project",
    params(DeleteParams),
    description = r#"
删除整个项目目录（含停止其 dev server 前置动作）。**不可逆**：code/data/logs 一并移除；只删单文件用 files-update/delete 面。
"#,
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

/// 创建项目
#[utoipa::path(post, path = "/create-project", request_body = CreateProjectBody, description = r#"
创建项目骨架：在工作区建立 `{projectId}` 目录结构（app 根相对）。**注意是 GET 形态**——沿用 TS 契约，projectId 走 query。成功后即可用文件族接口写入内容或 upload-project 整包导入。
"#,
    responses(crate::openapi::JsonApiResponses), tag = "Project")]
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

/// 复制项目
///
/// 源/目标各自隔离上下文, 缺省回退公共字段。
#[utoipa::path(post, path = "/copy-project", request_body = CopyProjectBody, description = r#"
复制项目到新 projectId：源/目标各自取租户隔离上下文，字段缺省时逐级回退公共值。用于模板化克隆场景。
"#,
    responses(crate::openapi::JsonApiResponses), tag = "Project")]
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
