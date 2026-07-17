//! `/api/project` 路由 (对齐 nuwax projectRoutes + codeRoutes, 二级路径不冲突)。
//!
//! 拆分: [`content`] (get-project-content / get-by-version) / [`crud`] (create / copy /
//! delete / upload 系列 / push-skills) / [`code`] (specified/all-files-update) /
//! [`version`] (backup / rollback / export)。本 mod.rs 装 router + 跨组共享 helper。

use std::path::Path;

use axum::Router;
use axum::routing::{get, post};

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ProjectContext;

mod code;
mod content;
mod crud;
mod version;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/get-project-content", get(content::get_project_content))
        // nuwax delete-project 用 GET (反直觉, 但保持兼容)
        .route("/delete-project", get(crud::delete_project))
        .route("/create-project", post(crud::create_project))
        .route("/copy-project", post(crud::copy_project))
        .route("/upload-single-file", post(crud::upload_single_file))
        .route("/upload-batch-files", post(crud::upload_batch_files))
        .route(
            "/upload-attachment-file",
            post(crud::upload_attachment_file),
        )
        .route(
            "/specified-files-update",
            post(code::specified_files_update),
        )
        .route("/all-files-update", post(code::all_files_update))
        .route("/upload-project", post(crud::upload_project))
        .route(
            "/backup-current-version",
            post(version::backup_current_version),
        )
        .route("/rollback-version", post(version::rollback_version))
        .route(
            "/get-project-content-by-version",
            get(content::get_project_content_by_version),
        )
        .route("/export-project", post(version::export_project))
        .route(
            "/push-skills-to-workspace",
            post(crud::push_skills_to_workspace),
        )
}

// ── 跨组共享 helper (子模块经 super:: 访问) ──────────────────────────────────────

fn ctx_from(
    project_id: &str,
    tenant: Option<String>,
    space: Option<String>,
    iso: Option<String>,
) -> ProjectContext {
    ProjectContext {
        project_id: project_id.to_string(),
        tenant_id: tenant,
        space_id: space,
        isolation_type: iso,
    }
}

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

/// `.zip` 扩展名校验 (对齐 nuwax multer fileFilter: 仅允许 zip)。
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
