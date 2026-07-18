//! `/api/project` 路由 (对齐 nuwax projectRoutes + codeRoutes, 二级路径不冲突)。
//!
//! 拆分: [`content`] (get-project-content / get-by-version) / [`crud`] (create / copy /
//! delete / upload 系列 / push-skills) / [`code`] (specified/all-files-update) /
//! [`version`] (backup / rollback / export)。本 mod.rs 装 router + 跨组共享 helper。

use std::path::Path;

use utoipa_axum::router::OpenApiRouter;
use utoipa_axum::routes;

use crate::AppState;
use crate::error::AppError;
use crate::workspace::ProjectContext;

mod code;
mod content;
mod crud;
mod version;

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(content::get_project_content))
        .routes(routes!(crud::delete_project))
        .routes(routes!(crud::create_project))
        .routes(routes!(crud::copy_project))
        .routes(routes!(crud::upload_single_file))
        .routes(routes!(crud::upload_batch_files))
        .routes(routes!(crud::upload_attachment_file))
        .routes(routes!(code::specified_files_update))
        .routes(routes!(code::all_files_update))
        .routes(routes!(crud::upload_project))
        .routes(routes!(version::backup_current_version))
        .routes(routes!(version::rollback_version))
        .routes(routes!(content::get_project_content_by_version))
        .routes(routes!(version::export_project))
        .routes(routes!(crud::push_skills_to_workspace))
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
