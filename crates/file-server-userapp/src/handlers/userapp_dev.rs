//! `/api/v1/userapp` 开发执行镜像族: 执行/日志/依赖安装/打包下载/模板初始化/技能推送。
//!
//! 与 [`super::userapp_files`] 同约定: computer 域同参镜像（`app_id`/`user_id`）,
//! 定位 `resolve_userapp_dev` = `{USERAPP_WORKSPACE_DIR}/{app_id}`; 编排复用
//! file-server `*_core`，**响应在本壳层自拼 snake JSON**（computer 域 TS 驼峰
//! 契约不经本域）。例外: `ensure-workspace` 为 Rust 独有新契约（TS 无对应端点）,
//! 响应用 `HttpResult` 信封（同 build/dev-server 域）。

use axum::extract::{Path, State};
use axum::response::Response;
use garde::Validate;
use serde_json::{Value, json};
use shared_types::HttpResult;

use super::userapp::UserAppReply;
use super::userapp::reply;
use super::userapp_files::require_app_field;
use crate::UserAppState;
use crate::models::{
    UserappDownloadQuery, UserappEnsureWorkspaceBody, UserappEnsureWorkspaceData,
    UserappExecCommandBody, UserappGetLogsQuery, UserappInitTemplateForm, UserappInstallBody,
    UserappPushSkillsForm, UserappZipBody,
};
use file_server::error::AppError;
use file_server::extract::{AppJson as Json, AppMultipart as Multipart, AppQuery as Query};
use file_server::ops::archive::{download_all_files_impl, zip_workspace_impl};
use file_server::ops::exec::{LogsOutcome, execute_command_core, get_logs_core};
use file_server::ops::multipart::{file_field, text_field};
use file_server::ops::packages::install_project_core;
use file_server::ops::workspace::{init_project_template_core, push_skills_core};
use file_server::workspace::resolve_userapp_dev;

// ── ensure-workspace ────────────────────────────────────────────────────────────

/// 幂等建 workspace 目录
///
/// create-workspace 链路首建；execute-command 等接口要求 cwd 已存在，
/// 故目录创建须先于业务调用。
#[utoipa::path(post, path = "/ensure-workspace", request_body = UserappEnsureWorkspaceBody, responses((status = 200, body = HttpResult<UserappEnsureWorkspaceData>, description = "workspace 目录已就绪（含绝对路径）")), tag = "UserApp · dev · 工作区与工具链")]
pub(crate) async fn ensure_workspace(
    State(state): State<UserAppState>,
    Json(body): Json<UserappEnsureWorkspaceBody>,
) -> UserAppReply<UserappEnsureWorkspaceData> {
    let result = async {
        body.validate().map_err(file_server::error::from_garde)?;
        let ws = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
        tokio::fs::create_dir_all(&ws)
            .await
            .map_err(|e| AppError::system(format!("create workspace {}: {e}", ws.display())))?;
        Ok(UserappEnsureWorkspaceData {
            workspace: ws.to_string_lossy().to_string(),
        })
    };
    reply(result.await)
}

// ── execute-command ─────────────────────────────────────────────────────────────

/// 终端命令执行（cwd=workspace）
///
/// 经 shell -c 执行，带超时捕获。
#[utoipa::path(post, path = "/execute-command", request_body = UserappExecCommandBody, responses(file_server::openapi::JsonApiResponses), tag = "UserApp · dev · 工作区与工具链")]
pub(crate) async fn execute_command(
    State(state): State<UserAppState>,
    Json(body): Json<UserappExecCommandBody>,
) -> Result<Json<Value>, AppError> {
    body.validate().map_err(file_server::error::from_garde)?;
    let cwd = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
    let r = execute_command_core(&state.fs, cwd, &body.command).await?;
    // 外层恒 success=true，命令结果由 exit_code 表达（语义同 computer 域 TS 契约）
    Ok(Json(json!({
        "success": true,
        "stdout": r.stdout,
        "stderr": r.stderr,
        "exit_code": r.exit_code,
    })))
}

// ── get-logs ────────────────────────────────────────────────────────────────────

/// 读取最新应用日志
///
/// 取 `{ws}/.logs` 下 mtime 最新日志末尾 N 行。
#[utoipa::path(
    get,
    path = "/get-logs",
    params(UserappGetLogsQuery),
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp · dev · 工作区与工具链"
)]
pub(crate) async fn get_logs(
    State(state): State<UserAppState>,
    Query(q): Query<UserappGetLogsQuery>,
) -> Result<Json<Value>, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let log_dir = resolve_userapp_dev(&q.app_id, None, &state.fs.config)?.join(".logs");
    match get_logs_core(&state.fs, log_dir, q.tail_lines).await? {
        LogsOutcome::Empty { reason } => Ok(Json(json!({
            "success": true,
            "message": reason,
            "logs": [],
            "total_lines": 0,
            "start_index": 1,
            "log_file_name": None::<String>,
        }))),
        LogsOutcome::Tail {
            logs,
            total_lines,
            start_index,
            log_file_name,
        } => {
            let logs: Vec<Value> = logs
                .into_iter()
                .map(|l| json!({ "line": l.line, "content": l.content }))
                .collect();
            Ok(Json(json!({
                "success": true,
                "message": "Get log successfully",
                "logs": logs,
                "total_lines": total_lines,
                "start_index": start_index,
                "log_file_name": log_file_name,
            })))
        }
    }
}

// ── install-project ─────────────────────────────────────────────────────────────

/// 依赖安装
///
/// 按语言执行依赖安装：typescript → pnpm install；python → pip install。
/// 前置：projects/detect + confirm 已完成（workspace 内已有项目 manifest）。
/// 响应 `{ success, message, project_dir, programming_language }`。
#[utoipa::path(
    post,
    path = "/{app_id}/{app_stage}/install-project",
    params(
        ("app_id" = String, Path, description = "应用 ID"),
        ("app_stage" = String, Path, description = "目标环境：`dev`=开发容器（UserAppBuilder）")
    ),
    request_body = UserappInstallBody,
    responses(file_server::openapi::JsonApiResponses),
    tag = "UserApp · dev · 工作区与工具链"
)]
pub(crate) async fn install_project(
    State(state): State<UserAppState>,
    Path((app_id, _app_stage)): Path<(String, String)>,
    Json(body): Json<UserappInstallBody>,
) -> Result<Json<Value>, AppError> {
    tracing::debug!(app_id = %app_id, user_id = %body.user_id, "userapp install-project");
    let ws = resolve_userapp_dev(&app_id, None, &state.fs.config)?;
    let r = install_project_core(&state.fs, ws, &body.programming_language).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project dependencies installed successfully",
        "project_dir": r.project_dir.display().to_string(),
        "programming_language": r.programming_language,
    })))
}

// ── zip-workspace ───────────────────────────────────────────────────────────────

/// workspace 打包下载（二进制 zip）
///
/// 将开发卷 workspace 整体打包为 zip 返回（`application/zip` 二进制体），
/// 文件名 `{user_id}_{app_id}.zip`；body `exclude_dirs` 可选排除目录清单。
#[utoipa::path(
    post,
    path = "/zip-workspace",
    request_body = UserappZipBody,
    responses(
        (status = 200, description = "Workspace ZIP archive", body = file_server::models::BinaryFile, content_type = "application/zip"),
        file_server::openapi::ErrorApiResponses
    ),
    tag = "UserApp · dev · 工作区与工具链"
)]
pub(crate) async fn zip_workspace(
    State(state): State<UserAppState>,
    Json(body): Json<UserappZipBody>,
) -> Result<Response, AppError> {
    let src = resolve_userapp_dev(&body.app_id, None, &state.fs.config)?;
    let filename = format!("{}_{}.zip", body.user_id, body.app_id);
    zip_workspace_impl(
        &state.fs,
        src,
        body.exclude_dirs.unwrap_or_default(),
        filename,
    )
    .await
}

// ── download-all-files ──────────────────────────────────────────────────────────

/// 全量文件下载
///
/// 打包下载 workspace 全部文件（zip 内顶层加 `{user_id}_{app_id}/` 前缀，
/// 防解压时覆盖本地目录）；空 workspace 返回空 zip 兜底；超大小上限报错。
#[utoipa::path(
    get,
    path = "/download-all-files",
    params(UserappDownloadQuery),
    responses(
        (status = 200, description = "Workspace ZIP archive", body = file_server::models::BinaryFile, content_type = "application/zip"),
        file_server::openapi::ErrorApiResponses
    ),
    tag = "UserApp · dev · 工作区与工具链"
)]
pub(crate) async fn download_all_files(
    State(state): State<UserAppState>,
    Query(q): Query<UserappDownloadQuery>,
) -> Result<Response, AppError> {
    q.validate().map_err(file_server::error::from_garde)?;
    let src = resolve_userapp_dev(&q.app_id, q.custom_target_dir.as_deref(), &state.fs.config)?;
    let prefix = format!("{}_{}/", q.user_id, q.app_id);
    let filename = format!("{}_{}.zip", q.user_id, q.app_id);
    download_all_files_impl(&state.fs, src, prefix, filename).await
}

// ── init-project-template ───────────────────────────────────────────────────────

/// 模板初始化开发卷
///
/// 模板 zip 解压到 workspace；可选 git init（双开关：GIT_ENABLED 且 enableGit 为 true 才执行）。
/// UserApp 开发的起点接口。
#[utoipa::path(post, path = "/init-project-template", request_body(content = UserappInitTemplateForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp · dev · 工作区与工具链")]
pub(crate) async fn init_project_template(
    State(state): State<UserAppState>,
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
            "app_id" => app_id = Some(text_field(field).await?),
            "user_id" => user_id = Some(text_field(field).await?),
            "file" => {
                data = Some(
                    file_field(
                        field,
                        state.fs.config.upload_max_file_size_bytes,
                        &state.fs.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            "enable_git" => {
                enable_git = matches!(
                    text_field(field).await?.trim().to_lowercase().as_str(),
                    "true" | "1" | "yes"
                );
            }
            _ => {}
        }
    }
    let app_id = require_app_field(app_id, "app_id")?;
    let user_id = require_app_field(user_id, "user_id")?;
    tracing::debug!(app_id = %app_id, user_id, "userapp init-project-template");
    let data = data.ok_or_else(|| AppError::validation("file is required"))?;
    let ws = resolve_userapp_dev(&app_id, None, &state.fs.config)?;
    let ws = init_project_template_core(&state.fs, ws, data, enable_git).await?;
    Ok(Json(json!({
        "success": true,
        "message": "Project template initialized successfully",
        "workspace_root": ws.display().to_string(),
    })))
}

// ── push-skills-to-workspace ────────────────────────────────────────────────────

/// 技能推送（zip/skillUrls）
///
/// 开发卷布局下技能一律推 `{ws}/.agents/skills`（legacy 路径）。
#[utoipa::path(post, path = "/push-skills-to-workspace", request_body(content = UserappPushSkillsForm, content_type = "multipart/form-data"), responses(file_server::openapi::JsonApiResponses), tag = "UserApp · dev · 工作区与工具链")]
pub(crate) async fn push_skills_to_workspace(
    State(state): State<UserAppState>,
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
            "app_id" => app_id = Some(text_field(field).await?),
            "user_id" => user_id = Some(text_field(field).await?),
            "file" => {
                zip_data = Some(
                    file_field(
                        field,
                        state.fs.config.upload_max_file_size_bytes,
                        &state.fs.config.upload_project_dir.join("temp"),
                    )
                    .await?,
                )
            }
            "skill_urls" => {
                let t = text_field(field).await?;
                if let Ok(urls) = serde_json::from_str::<Vec<String>>(&t) {
                    skill_urls.extend(urls);
                } else {
                    skill_urls.push(t);
                }
            }
            "agent_id" => agent_id = Some(text_field(field).await?),
            _ => {}
        }
    }
    let app_id = require_app_field(app_id, "app_id")?;
    let user_id = require_app_field(user_id, "user_id")?;
    tracing::debug!(app_id = %app_id, user_id, "userapp push-skills");
    let ws = resolve_userapp_dev(&app_id, None, &state.fs.config)?;
    let r = push_skills_core(
        &state.fs,
        &ws,
        &app_id,
        zip_data.as_ref(),
        skill_urls,
        agent_id.as_deref(),
        false,
    )
    .await?;
    let message = if r.updated.is_empty() {
        "No valid skill directories found in file or skillUrls".to_string()
    } else {
        format!(
            "Pushed {} skills: {}",
            r.updated.len(),
            r.updated.join(", ")
        )
    };
    Ok(Json(json!({
        "success": true,
        "message": message,
        "workspace_root": ws.display().to_string(),
        "updated_skills": r.updated,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handlers::userapp_files::tests_support::make_state;
    use file_server::extract::AppJson;

    /// execute-command 响应键 snake（exit_code）+ 命令真实执行语义。
    #[tokio::test]
    async fn execute_command_snake_response() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let state = make_state(tmp.path().to_path_buf());
        tokio::fs::create_dir_all(tmp.path().join("app-exec"))
            .await
            .unwrap();
        let res = execute_command(
            State(state),
            AppJson(UserappExecCommandBody {
                app_id: "app-exec".into(),
                user_id: "u".into(),
                command: "printf out-xyz".into(),
            }),
        )
        .await
        .expect("exec ok");
        assert_eq!(res.0["success"], serde_json::json!(true));
        assert_eq!(res.0["exit_code"], serde_json::json!(0));
        assert_eq!(res.0["stdout"].as_str().unwrap(), "out-xyz");
        // 旧 camel 键不再出现（concat 拼装避免键风格守卫自命中）
        let legacy_key = ["exit", "Code"].concat();
        assert!(res.0.get(&legacy_key).is_none());
    }
}
