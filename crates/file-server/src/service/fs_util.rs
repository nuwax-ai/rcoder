//! 共享 fs 操作 (目录复制过滤 / npmrc)。

use std::path::Path;

use tokio::fs;

use crate::error::AppResult;

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

/// 写 `.npmrc` 到项目根 (对齐 nuwax `createPnpmNpmrc`; 简化: 固定 npmmirror registry)。
pub async fn write_npmrc(project_path: &Path) -> AppResult<()> {
    const NPMRC: &str = "registry=https://registry.npmmirror.com/\n";
    fs::write(project_path.join(".npmrc"), NPMRC).await?;
    Ok(())
}
