//! 共享 fs 操作 (目录复制过滤 / npmrc)。

use std::path::Path;

use tokio::fs;

use crate::error::AppResult;

/// 检查路径是否存在；与 `Path::exists` 不同，权限/底层 I/O 错误不会伪装成“不存在”。
pub async fn path_exists(path: &Path) -> AppResult<bool> {
    fs::try_exists(path).await.map_err(Into::into)
}

/// 递归复制目录, 跳过 `exclude_dirs` 中的目录名与 `exclude_files` 中的文件名
/// (对齐 nuwax `copyDirectoryFiltered`)。
pub async fn copy_dir_filtered(
    src: &Path,
    dst: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    fs::create_dir_all(dst).await?;
    let mut entries = fs::read_dir(src).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let path = entry.path();
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            if exclude_dirs.iter().any(|d| d == &name) {
                continue;
            }
            // async 递归需 Box::pin
            Box::pin(copy_dir_filtered(
                &path,
                &dst.join(&name),
                exclude_dirs,
                exclude_files,
            ))
            .await?;
        } else if ft.is_file() {
            if exclude_files.iter().any(|f| f == &name) {
                continue;
            }
            fs::copy(&path, &dst.join(&name)).await?;
        }
    }
    Ok(())
}

/// 写 `.npmrc` 到项目根 (对齐 nuwax `createPnpmNpmrc`):
/// 固定 `package-import-method=copy` (JuiceFS/FUSE 上 hardlink 会失败) +
/// `auto-install-peers=true` + npmmirror registry + 可选 `store-dir` (环境变量设置时)。
pub async fn write_npmrc(project_path: &Path) -> AppResult<()> {
    super::pnpm_config::create_pnpm_npmrc(project_path).await
}
