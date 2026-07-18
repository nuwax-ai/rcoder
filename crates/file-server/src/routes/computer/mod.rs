//! `/api/computer` 路由 (对齐 nuwax computerRoutes)。
//!
//! computer 工作区路径: `{COMPUTER_WORKSPACE_ROOT}/{userId}/{cId}/`。
//!
//! 拆分: [`files`] (get-file-list / files-update / upload / import / delete-workspace) /
//! [`archive`] (zip-workspace / download-all-files) / [`workspace`] (create-workspace /
//! push-skills / init-project-template) / [`exec`] (execute-command / install-project /
//! get-logs / build-agent-package / cleanup-build-artifacts)。本 mod.rs 装 router + 跨组共享 helper。

use std::path::{Path, PathBuf};

use serde::Deserialize;
use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ComputerContext;

mod archive;
mod exec;
mod files;
mod workspace;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(files::get_file_list))
        .routes(routes!(files::delete_workspace))
        .routes(routes!(exec::get_logs))
        .routes(routes!(exec::execute_command))
        .routes(routes!(exec::install_project))
        .routes(routes!(archive::zip_workspace))
        .routes(routes!(archive::download_all_files))
        .routes(routes!(files::files_update))
        .routes(routes!(files::upload_file))
        .routes(routes!(files::upload_files))
        .routes(routes!(files::import_project))
        .routes(routes!(exec::cleanup_build_artifacts))
        .routes(routes!(workspace::create_workspace))
        .routes(routes!(workspace::create_workspace_v2))
        .routes(routes!(workspace::push_skills_to_workspace))
        .routes(routes!(workspace::push_skills_to_workspace_v2))
        .routes(routes!(workspace::init_project_template))
        .routes(routes!(exec::build_agent_package))
}

// ── 跨组共享 helper (子模块经 super:: 访问) ──────────────────────────────────────

async fn text_field(field: axum::extract::multipart::Field<'_>) -> Result<String, AppError> {
    field
        .text()
        .await
        .map_err(|e| AppError::validation(format!("read multipart field: {e}")))
}

async fn bytes_field(
    mut field: axum::extract::multipart::Field<'_>,
    max_bytes: u64,
) -> Result<Vec<u8>, AppError> {
    let mut data = Vec::new();
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|e| AppError::validation(format!("read multipart file: {e}")))?
    {
        let next_len = (data.len() as u64)
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| AppError::validation("multipart file size overflow"))?;
        if next_len > max_bytes {
            return Err(AppError::validation(format!(
                "File size exceeds limit (max {max_bytes} bytes)"
            )));
        }
        data.try_reserve(chunk.len())
            .map_err(|e| AppError::system(format!("reserve multipart buffer: {e}")))?;
        data.extend_from_slice(&chunk);
    }
    Ok(data)
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

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
#[serde(rename_all = "camelCase")]
pub(super) struct UserCidQuery {
    pub user_id: String,
    pub c_id: String,
    #[serde(default)]
    pub proxy_path: Option<String>,
    #[serde(default)]
    pub custom_target_dir: Option<String>,
}
