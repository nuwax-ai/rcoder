//! 版本管理: 备份/恢复/版本 zip 路径 (对齐 nuwax `backupUtils`)。
//!
//! 版本 zip 路径: `UPLOAD_PROJECT_DIR/{projectId}/{projectId}-v{N}.zip`

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::workspace::{ProjectContext, WorkspaceResolver};

/// codeVersion 字符串 → u64 (对齐 nuwax: 须为有限正数)。
pub fn parse_version(code_version: &str) -> AppResult<u64> {
    code_version
        .trim()
        .parse::<u64>()
        .map_err(|_| AppError::validation("Code version must be a number"))
}

/// 版本 zip 路径: `UPLOAD_PROJECT_DIR/{projectId}/{projectId}-v{N}.zip`
pub fn version_zip_path(config: &Config, project_id: &str, version: u64) -> PathBuf {
    config
        .upload_project_dir
        .join(project_id)
        .join(format!("{project_id}-v{version}.zip"))
}

/// 备份项目到版本 zip (非 GIT 模式); `GIT_ENABLED` 时跳过返回 `None`。
pub async fn backup_project(
    config: &Config,
    project_id: &str,
    project_path: &Path,
    code_version: &str,
) -> AppResult<Option<PathBuf>> {
    if config.git_enabled {
        return Ok(None);
    }
    let version = parse_version(code_version)?;
    let zip_path = version_zip_path(config, project_id, version);
    crate::service::zip::pack_dir(
        project_path.to_path_buf(),
        zip_path.clone(),
        config.traverse_exclude_dirs.clone(),
        config.backup_traverse_exclude_files.clone(),
    )
    .await?;
    Ok(Some(zip_path))
}

/// 从版本 zip 恢复 (清空项目目录 + 解压覆盖, 对齐 nuwax `restoreProjectFromZip`)。
pub async fn restore_from_zip(project_path: &Path, zip_path: &Path) -> AppResult<()> {
    fs::remove_dir_all(project_path).await?;
    fs::create_dir_all(project_path).await?;
    crate::service::zip::extract_to(zip_path.to_path_buf(), project_path.to_path_buf()).await
}

// ── backup-current-version ──────────────────────────────────────────────────────

pub struct BackupVersionResult {
    pub project_id: String,
    pub zip_path: String,
}

/// 备份当前版本到 `{projectId}-v{N}.zip` (GIT_ENABLED 由 handler 拦截为 deprecated)。
pub async fn backup_current_version(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
) -> AppResult<BackupVersionResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let project_path = resolver.resolve_project(ctx);
    if !fs::try_exists(&project_path).await.unwrap_or(false) {
        return Err(AppError::resource("Project does not exist"));
    }
    let zip = backup_project(config, project_id, &project_path, code_version)
        .await?
        .ok_or_else(|| AppError::business("backup disabled in GIT mode"))?;
    Ok(BackupVersionResult {
        project_id: project_id.to_string(),
        zip_path: zip.to_string_lossy().to_string(),
    })
}

// ── rollback-version ────────────────────────────────────────────────────────────

pub struct RollbackResult {
    pub new_version: u64,
    pub rollback_to: u64,
}

/// 回滚到 rollbackTo 版本 (先备份当前, 再从历史 zip 恢复)。
pub async fn rollback_version(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    rollback_to: &str,
) -> AppResult<RollbackResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let cur = parse_version(code_version)?;
    let to = parse_version(rollback_to)?;
    if to >= cur {
        return Err(AppError::validation("rollbackTo must be less than codeVersion"));
    }
    let project_path = resolver.resolve_project(ctx);
    if !fs::try_exists(&project_path).await.unwrap_or(false) {
        return Err(AppError::resource("Project does not exist"));
    }
    let target_zip = version_zip_path(config, project_id, to);
    if !fs::try_exists(&target_zip).await.unwrap_or(false) {
        return Err(AppError::resource(format!(
            "Rollback version v{to} zip not found"
        )));
    }
    // 当前版本若未备份, 先备份
    let cur_zip = version_zip_path(config, project_id, cur);
    if !fs::try_exists(&cur_zip).await.unwrap_or(false) {
        backup_project(config, project_id, &project_path, code_version).await?;
    }
    restore_from_zip(&project_path, &target_zip).await?;
    Ok(RollbackResult {
        new_version: cur,
        rollback_to: to,
    })
}

// ── get-project-content-by-version ──────────────────────────────────────────────

/// 解压历史版本 zip 到 `_his` 临时目录, 遍历后清理 (对齐 nuwax getProjectContentByVersion)。
pub async fn get_content_by_version(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    proxy_path: Option<&str>,
) -> AppResult<Vec<crate::service::tree::FileEntry>> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let version = parse_version(code_version)?;
    let project_path = resolver.resolve_project(ctx);
    let zip = version_zip_path(config, project_id, version);
    if !fs::try_exists(&zip).await.unwrap_or(false) {
        return Err(AppError::resource(format!(
            "Version v{version} zip not found"
        )));
    }
    // 临时解压目录: project 父目录下 _his/{projectId}
    let his_dir = match project_path.parent() {
        Some(p) => p.join("_his").join(project_id),
        None => return Err(AppError::system("invalid project path")),
    };
    let _ = fs::remove_dir_all(&his_dir).await;
    crate::service::zip::extract_to(zip, his_dir.clone()).await?;

    // 遍历 (无论成败都清理临时目录)
    let files_result = crate::service::tree::list_files(&his_dir, config, proxy_path).await;
    let _ = fs::remove_dir_all(&his_dir).await;
    files_result
}
