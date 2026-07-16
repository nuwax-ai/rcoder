//! 版本管理: 备份/恢复/版本 zip 路径 (对齐 nuwax `backupUtils`)。
//!
//! 版本 zip 路径: `UPLOAD_PROJECT_DIR/{projectId}/{projectId}-v{N}.zip`

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};

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
