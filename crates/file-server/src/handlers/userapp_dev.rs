//! `/api/userapp` 开发执行镜像族: 执行/日志/依赖安装/打包下载/模板初始化/技能推送。
//!
//! 与 [`super::userapp_files`] 同约定: computer 域同参镜像 (`appId`/`userId`),
//! 定位 `resolve_userapp_dev` = `{USERAPP_WORKSPACE_DIR}/{appId}`, 复用 computer impl。

use axum::extract::State;
use axum::response::Response;
use garde::Validate;
use serde::Deserialize;
use serde_json::Value;

use super::computer::archive::{download_all_files_impl, zip_workspace_impl};
use super::computer::exec::{execute_command_impl, get_logs_impl};
use super::computer::packages::install_project_impl;
use super::computer::workspace::init_template::init_project_template_impl;
use super::computer::workspace::push_skills::push_skills_impl;
use super::multipart::{file_field, text_field};
use super::userapp_files::require_app_field;
use crate::AppState;
use crate::error::AppError;
use crate::extract::{AppJson as Json, AppMultipart as Multipart, AppQuery as Query};
use crate::workspace::resolve_userapp_dev;

// ── execute-command ─────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::ToSchema)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappExecCommandBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub app_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub command: String,
}

/// `POST /api/userapp/execute-command`: 终端命令执行 (cwd=workspace, shell -c + 超时捕获)。
#[utoipa::path(post, path = "/execute-command", request_body = UserappExecCommandBody, responses(crate::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn execute_command(
    State(state): State<AppState>,
    Json(body): Json<UserappExecCommandBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(crate::error::from_garde)?;
    let cwd = resolve_userapp_dev(&body.app_id, None, &state.config)?;
    execute_command_impl(&state, cwd, &body.command).await
}

// ── get-logs ────────────────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[garde(allow_unvalidated)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappGetLogsQuery {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub app_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[serde(default = "default_tail_lines")]
    pub tail_lines: usize,
}
fn default_tail_lines() -> usize {
    200
}

/// `GET /api/userapp/get-logs`: 读 `{ws}/.logs` 下 mtime 最新日志末尾 N 行。
#[utoipa::path(
    get,
    path = "/get-logs",
    params(UserappGetLogsQuery),
    responses(crate::openapi::JsonApiResponses),
    tag = "UserApp"
)]
pub(crate) async fn get_logs(
    State(state): State<AppState>,
    Query(q): Query<UserappGetLogsQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(crate::error::from_garde)?;
    let log_dir = resolve_userapp_dev(&q.app_id, None, &state.config)?.join(".logs");
    get_logs_impl(&state, log_dir, q.tail_lines).await
}

// ── install-project ─────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappInstallBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub app_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    pub programming_language: String,
}

/// `POST /api/userapp/install-project`: 依赖安装 (typescript→pnpm / python→pip)。
#[utoipa::path(post, path = "/install-project", request_body = UserappInstallBody, responses(crate::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn install_project(
    State(state): State<AppState>,
    Json(body): Json<UserappInstallBody>,
) -> Result<Json<Value>, AppError> {
    tracing::debug!(app_id = %body.app_id, user_id = %body.user_id, "userapp install-project");
    let ws = resolve_userapp_dev(&body.app_id, None, &state.config)?;
    install_project_impl(&state, ws, &body.programming_language).await
}

// ── zip-workspace ───────────────────────────────────────────────────────────────

#[derive(Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappZipBody {
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub app_id: String,
    #[serde(deserialize_with = "crate::extract::deserialize_id_string")]
    pub user_id: String,
    #[serde(default)]
    pub exclude_dirs: Option<Vec<String>>,
}

/// `POST /api/userapp/zip-workspace`: workspace 打包下载 (二进制 zip)。
#[utoipa::path(
    post,
    path = "/zip-workspace",
    request_body = UserappZipBody,
    responses(
        (status = 200, description = "Workspace ZIP archive", body = crate::openapi::BinaryFile, content_type = "application/zip"),
        crate::openapi::ErrorApiResponses
    ),
    tag = "UserApp"
)]
pub(crate) async fn zip_workspace(
    State(state): State<AppState>,
    Json(body): Json<UserappZipBody>,
) -> Result<Response, AppError> {
    let src = resolve_userapp_dev(&body.app_id, None, &state.config)?;
    let filename = format!("{}_{}.zip", body.user_id, body.app_id);
    zip_workspace_impl(&state, src, body.exclude_dirs.unwrap_or_default(), filename).await
}

// ── download-all-files ──────────────────────────────────────────────────────────

#[derive(Deserialize, Validate, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(crate) struct UserappDownloadQuery {
    #[garde(custom(crate::validation_rules::not_blank))]
    pub app_id: String,
    #[garde(custom(crate::validation_rules::not_blank))]
    pub user_id: String,
    #[serde(default)]
    #[garde(skip)]
    pub custom_target_dir: Option<String>,
}

/// `GET /api/userapp/download-all-files`: 全量文件下载 (顶层前缀 + 空 zip 兜底 + 大小上限)。
#[utoipa::path(
    get,
    path = "/download-all-files",
    params(UserappDownloadQuery),
    responses(
        (status = 200, description = "Workspace ZIP archive", body = crate::openapi::BinaryFile, content_type = "application/zip"),
        crate::openapi::ErrorApiResponses
    ),
    tag = "UserApp"
)]
pub(crate) async fn download_all_files(
    State(state): State<AppState>,
    Query(q): Query<UserappDownloadQuery>,
) -> Result<Response, AppError> {
    q.validate().map_err(crate::error::from_garde)?;
    let src = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.config)?;
    let prefix = format!("{}_{}/", q.user_id, q.app_id);
    let filename = format!("{}_{}.zip", q.user_id, q.app_id);
    download_all_files_impl(&state, src, prefix, filename).await
}

// ── init-project-template ───────────────────────────────────────────────────────

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserappInitTemplateForm {
    pub app_id: String,
    pub user_id: String,
    #[schema(format = Binary)]
    pub file: String,
    pub enable_git: Option<bool>,
}

/// `POST /api/userapp/init-project-template`: 模板 zip 解压初始化开发卷 workspace
/// (可选 git init, 双开关 GIT_ENABLED && enableGit)。UserApp 开发的起点接口。
#[utoipa::path(post, path = "/init-project-template", request_body(content = UserappInitTemplateForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn init_project_template(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut app_id = None;
    let mut user_id = None;
    let mut data = None;
    let mut enable_git = false;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "appId" => app_id = Some(text_field(field).await?),
            "userId" => user_id = Some(text_field(field).await?),
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
            "enableGit" => {
                enable_git = matches!(
                    text_field(field).await?.trim().to_lowercase().as_str(),
                    "true" | "1" | "yes"
                );
            }
            _ => {}
        }
    }
    let app_id = require_app_field(app_id, "appId")?;
    let user_id = require_app_field(user_id, "userId")?;
    tracing::debug!(app_id = %app_id, user_id, "userapp init-project-template");
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let ws = resolve_userapp_dev(&app_id, None, &state.config)?;
    init_project_template_impl(&state, ws, data, enable_git).await
}

// ── push-skills-to-workspace ────────────────────────────────────────────────────

#[allow(dead_code, reason = "OpenAPI-only multipart schema")]
#[derive(utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserappPushSkillsForm {
    pub app_id: String,
    pub user_id: String,
    #[schema(format = Binary)]
    pub file: Option<String>,
    pub skill_urls: Option<Vec<String>>,
    /// 智能体 ID (开发卷布局下不走 agent-store, 仅审计日志)
    pub agent_id: Option<String>,
}

/// `POST /api/userapp/push-skills-to-workspace`: 技能推送 (zip/skillUrls)。
/// 开发卷布局下技能一律推 `{ws}/.agents/skills` (legacy 路径)。
#[utoipa::path(post, path = "/push-skills-to-workspace", request_body(content = UserappPushSkillsForm, content_type = "multipart/form-data"), responses(crate::openapi::JsonApiResponses), tag = "UserApp")]
pub(crate) async fn push_skills_to_workspace(
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> Result<Json<Value>, AppError> {
    let mut app_id = None;
    let mut user_id = None;
    let mut zip_data = None;
    let mut skill_urls: Vec<String> = Vec::new();
    let mut agent_id: Option<String> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| AppError::validation(format!("multipart parse: {e}")))?
    {
        match field.name().unwrap_or("") {
            "appId" => app_id = Some(text_field(field).await?),
            "userId" => user_id = Some(text_field(field).await?),
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
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            "agentId" => agent_id = Some(text_field(field).await?),
            _ => {}
        }
    }
    let app_id = require_app_field(app_id, "appId")?;
    let user_id = require_app_field(user_id, "userId")?;
    tracing::debug!(app_id = %app_id, user_id, "userapp push-skills");
    let ws = resolve_userapp_dev(&app_id, None, &state.config)?;
    push_skills_impl(
        &state,
        &ws,
        &app_id,
        zip_data.as_ref(),
        skill_urls,
        agent_id.as_deref(),
        false,
    )
    .await
}
