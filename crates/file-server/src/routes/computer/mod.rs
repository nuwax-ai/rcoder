//! `/api/computer` 路由 (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`。
//!
//! 拆分: [`files`] (get-file-list / files-update / upload / import / delete-workspace) /
//! [`archive`] (zip-workspace / download-all-files) / [`workspace`] (create-workspace /
//! push-skills / init-project-template) / [`exec`] (execute-command / install-project /
//! get-logs / build-agent-package / cleanup-build-artifacts)。本 mod.rs 装 router + 跨组共享 helper。

use std::path::{Path, PathBuf};

use axum::Router;
use axum::routing::{get, post};
use serde::Deserialize;

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ComputerContext;

mod archive;
mod exec;
mod files;
mod workspace;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/get-file-list", get(files::get_file_list))
        .route("/delete-workspace", post(files::delete_workspace))
        .route("/get-logs", get(exec::get_logs))
        .route("/execute-command", post(exec::execute_command))
        .route("/install-project", post(exec::install_project))
        .route("/zip-workspace", post(archive::zip_workspace))
        .route("/download-all-files", get(archive::download_all_files))
        .route("/files-update", post(files::files_update))
        .route("/upload-file", post(files::upload_file))
        .route("/upload-files", post(files::upload_files))
        .route("/import-project", post(files::import_project))
        .route(
            "/cleanup-build-artifacts",
            post(exec::cleanup_build_artifacts),
        )
        .route("/create-workspace", post(workspace::create_workspace))
        .route("/create-workspace-v2", post(workspace::create_workspace_v2))
        .route(
            "/push-skills-to-workspace",
            post(workspace::push_skills_to_workspace),
        )
        .route(
            "/push-skills-to-workspace-v2",
            post(workspace::push_skills_to_workspace),
        )
        .route(
            "/init-project-template",
            post(workspace::init_project_template),
        )
        .route("/build-agent-package", post(exec::build_agent_package))
}

// ── 跨组共享 helper (子模块经 super:: 访问) ──────────────────────────────────────

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

/// `.zip` 扩展名校验 (对齐 nuwax: 仅允许 zip)。
fn validate_zip_ext(filename: Option<&str>) -> Result<(), AppError> {
    let ext = filename
        .and_then(|n| {
            Path::new(n)
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
        })
        .unwrap_or_default();
    if ext == "zip" {
        Ok(())
    } else {
        Err(AppError::validation("Only zip files are supported"))
    }
}

fn ws_path(state: &AppState, user_id: &str, cid: &str) -> PathBuf {
    state.resolver.resolve_computer(&ComputerContext {
        user_id: user_id.to_string(),
        cid: cid.to_string(),
    })
}

/// computer 目标路径: `customTargetDir` trim 后非空则用之, 否则回退默认工作区 (对齐 nuwax)。
fn resolve_computer_target(
    state: &AppState,
    user_id: &str,
    cid: &str,
    custom_target_dir: Option<&str>,
) -> PathBuf {
    match custom_target_dir.map(str::trim).filter(|s| !s.is_empty()) {
        Some(ct) => PathBuf::from(ct),
        None => ws_path(state, user_id, cid),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserCidQuery {
    pub user_id: String,
    pub c_id: String,
    #[serde(default)]
    pub proxy_path: Option<String>,
    #[serde(default)]
    pub custom_target_dir: Option<String>,
}
