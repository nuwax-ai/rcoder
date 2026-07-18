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

/// 从版本 zip 恢复 (对齐 nuwax `restoreProjectFromZip`): 清空项目目录但**保留** excluded
/// 目录 (TRAVERSE_EXCLUDE_DIRS, 如 node_modules/.git) 与 excluded 文件
/// (BACKUP_TRAVERSE_EXCLUDE_FILES, 如 lock 文件), 再解压 zip 覆盖。
/// (版本 zip 本身不含这些条目, 故保留它们可避免 rollback 后重装依赖。)
pub async fn restore_from_zip(
    project_path: &Path,
    zip_path: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    clear_dir_keep_excluded(project_path, exclude_dirs, exclude_files).await?;
    crate::service::zip::extract_to(zip_path.to_path_buf(), project_path.to_path_buf()).await
}

/// 清空目录内容, 但保留名字命中 exclude_dirs (目录) / exclude_files (文件) 的顶层条目
/// (对齐 nuwax restoreProjectFromZip 的 "keep excluded" 清理)。
/// 删除失败必须中止恢复，避免旧文件残留后继续覆盖成混合版本。
async fn clear_dir_keep_excluded(
    dir: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    let mut rd = fs::read_dir(dir).await?;
    while let Some(entry) = rd.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let ft = entry.file_type().await?;
        if ft.is_dir() && exclude_dirs.iter().any(|d| d == &name) {
            continue;
        }
        if ft.is_file() && exclude_files.iter().any(|f| f == &name) {
            continue;
        }
        let path = entry.path();
        if ft.is_dir() {
            fs::remove_dir_all(&path).await
        } else {
            fs::remove_file(&path).await
        }
        .map_err(|error| {
            AppError::system(format!(
                "clear project entry before restore {}: {error}",
                path.display()
            ))
        })?;
    }
    Ok(())
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
    let project_path = resolver.resolve_project(ctx)?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
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
        return Err(AppError::validation(
            "rollbackTo must be less than codeVersion",
        ));
    }
    let project_path = resolver.resolve_project(ctx)?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    let target_zip = version_zip_path(config, project_id, to);
    if !crate::service::fs_util::path_exists(&target_zip).await? {
        return Err(AppError::resource(format!(
            "Rollback version v{to} zip not found"
        )));
    }
    // 当前版本若未备份, 先备份 (避免覆盖已有备份)
    let cur_zip = version_zip_path(config, project_id, cur);
    if !crate::service::fs_util::path_exists(&cur_zip).await? {
        backup_project(config, project_id, &project_path, code_version).await?;
    }
    // 从目标版本恢复; 失败则尝试从刚才备份的当前版本恢复 (对齐 nuwax rollbackVersion catch)
    if let Err(e) = restore_from_zip(
        &project_path,
        &target_zip,
        &config.traverse_exclude_dirs,
        &config.backup_traverse_exclude_files,
    )
    .await
    {
        if crate::service::fs_util::path_exists(&cur_zip).await? {
            tracing::warn!(error = %e, "rollback restore failed, restoring current version backup");
            if let Err(restore_error) = restore_from_zip(
                &project_path,
                &cur_zip,
                &config.traverse_exclude_dirs,
                &config.backup_traverse_exclude_files,
            )
            .await
            {
                return Err(AppError::system(format!(
                    "rollback to v{to} failed: {e}; restoring current version v{cur} also failed: {restore_error}"
                )));
            }
        }
        return Err(e);
    }
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
    command: Option<&str>,
) -> AppResult<Vec<crate::service::tree::FileEntry>> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    let version = parse_version(code_version)?;
    let project_path = resolver.resolve_project(ctx)?;
    let zip = version_zip_path(config, project_id, version);
    if !crate::service::fs_util::path_exists(&zip).await? {
        return Err(AppError::resource(format!(
            "Version v{version} zip not found"
        )));
    }
    // 每次请求使用独立临时目录，避免并发读取同一版本时互相清理。
    let history_root = match project_path.parent() {
        Some(parent) => parent.join("_his"),
        None => return Err(AppError::system("invalid project path")),
    };
    fs::create_dir_all(&history_root).await?;
    let history_temp = create_history_temp_dir(history_root).await?;
    let his_dir = history_temp.path().to_path_buf();

    // 解压与遍历结束后显式异步清理；TempDir 同时作为异常路径的 RAII 兜底。
    let mut files_result: AppResult<Vec<crate::service::tree::FileEntry>> = async {
        crate::service::zip::extract_to(zip, his_dir.clone()).await?;
        crate::service::tree::list_files(&his_dir, config, proxy_path).await
    }
    .await;
    let cleanup_result = fs::remove_dir_all(&his_dir).await;
    // command != "cpage_config" 时过滤 cpage_config.json (对齐 nuwax getContentUtils)
    if let Ok(files) = files_result.as_mut()
        && command != Some("cpage_config")
    {
        files.retain(|f| f.name != "cpage_config.json");
    }
    match (files_result, cleanup_result) {
        (Ok(files), Ok(())) => Ok(files),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(AppError::system(format!(
            "remove history temporary directory {}: {error}",
            his_dir.display()
        ))),
    }
}

async fn create_history_temp_dir(parent: PathBuf) -> AppResult<tempfile::TempDir> {
    tokio::task::spawn_blocking(move || {
        tempfile::Builder::new()
            .prefix("file-server-version-")
            .tempdir_in(&parent)
            .map_err(|error| {
                AppError::system(format!(
                    "create history temporary directory in {}: {error}",
                    parent.display()
                ))
            })
    })
    .await
    .map_err(|error| AppError::system(format!("history tempdir task failed: {error}")))?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clear_dir_keep_excluded_preserves_excluded_entries() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("fs_ver_{nanos}"));
        fs::create_dir_all(dir.join("node_modules")).await.unwrap();
        fs::create_dir_all(dir.join("src")).await.unwrap();
        fs::create_dir_all(dir.join(".git")).await.unwrap();
        fs::write(dir.join("src/app.js"), "x").await.unwrap();
        fs::write(dir.join("package-lock.yaml"), "x").await.unwrap();
        fs::write(dir.join("README.md"), "x").await.unwrap();

        clear_dir_keep_excluded(
            &dir,
            &["node_modules".into(), ".git".into()],
            &["package-lock.yaml".into()],
        )
        .await
        .unwrap();

        // excluded 保留 (node_modules / .git 目录 + lock 文件)
        assert!(dir.join("node_modules").exists());
        assert!(dir.join(".git").exists());
        assert!(dir.join("package-lock.yaml").exists());
        // 非 excluded 删除
        assert!(!dir.join("src").exists());
        assert!(!dir.join("README.md").exists());

        let _ = fs::remove_dir_all(&dir).await;
    }

    #[test]
    fn parse_version_validates_number() {
        assert!(parse_version("abc").is_err());
        assert_eq!(parse_version("12").unwrap(), 12);
        assert_eq!(parse_version(" 3 ").unwrap(), 3);
    }
}
