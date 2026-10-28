//! 共享 fs 操作 (目录复制过滤 / npmrc)。

use std::path::Path;

use tokio::fs;
use tokio::io::AsyncReadExt;

use crate::error::{AppError, AppResult};

/// 检查路径是否存在；与 `Path::exists` 不同，权限/底层 I/O 错误不会伪装成“不存在”。
pub async fn path_exists(path: &Path) -> AppResult<bool> {
    fs::try_exists(path).await.map_err(Into::into)
}

/// 有界读取 UTF-8 文本；即使文件在 metadata 检查后继续增长，也只读取 `max_bytes + 1`。
pub async fn read_to_string_bounded(path: &Path, max_bytes: u64, label: &str) -> AppResult<String> {
    let file = fs::File::open(path)
        .await
        .map_err(|error| AppError::system(format!("open {label} {}: {error}", path.display())))?;
    let mut bytes = Vec::new();
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| AppError::system(format!("read {label} {}: {error}", path.display())))?;
    if u64::try_from(bytes.len()).is_ok_and(|size| size > max_bytes) {
        return Err(AppError::validation(format!(
            "{label} exceeds readable size limit (max {max_bytes} bytes)"
        )));
    }
    String::from_utf8(bytes).map_err(|error| {
        AppError::validation(format!("{label} {} is not UTF-8: {error}", path.display()))
    })
}

/// 递归复制目录, 跳过 `exclude_dirs` 中的目录名与 `exclude_files` 中的文件名
/// (对齐 nuwax `copyDirectoryFiltered`)。
///
/// 符号链接条目**物化为实体副本**（跟随解析目标复制内容），断链/非常规类型
/// 明确报错——历史行为静默跳过链接条目，会让 fan-out 产生缺项副本
/// （生产实测：源 15 个技能目录、副本只剩 13 个）。
/// 物化会跟随链接递归，环形链接由 [`COPY_MATERIALIZE_MAX_DEPTH`] 明确报错
/// 拦截，而不是无限递归栈溢出拖垮进程。
pub async fn copy_dir_filtered(
    src: &Path,
    dst: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
) -> AppResult<()> {
    copy_dir_filtered_depth(src, dst, exclude_dirs, exclude_files, 0).await
}

/// 物化/嵌套复制的递归深度上限。技能与 agent 目录的合法嵌套远低于此;
/// 触达上限按数据异常报错 (典型成因: 符号链接环)。
const COPY_MATERIALIZE_MAX_DEPTH: usize = 24;

async fn copy_dir_filtered_depth(
    src: &Path,
    dst: &Path,
    exclude_dirs: &[String],
    exclude_files: &[String],
    depth: usize,
) -> AppResult<()> {
    if depth > COPY_MATERIALIZE_MAX_DEPTH {
        return Err(AppError::system(format!(
            "copy recursion exceeds depth {} at {} (symbolic link loop?)",
            COPY_MATERIALIZE_MAX_DEPTH,
            src.display()
        )));
    }
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
            Box::pin(copy_dir_filtered_depth(
                &path,
                &dst.join(&name),
                exclude_dirs,
                exclude_files,
                depth + 1,
            ))
            .await?;
        } else if ft.is_file() {
            if exclude_files.iter().any(|f| f == &name) {
                continue;
            }
            fs::copy(&path, &dst.join(&name)).await?;
        } else {
            // 符号链接等条目 (entry.file_type 不跟随链接): 解析后按目标类型物化
            let resolved = fs::metadata(&path).await.map_err(|error| {
                AppError::system(format!(
                    "resolve symlinked entry {} while copying: {error}",
                    path.display()
                ))
            })?;
            if resolved.is_dir() {
                if exclude_dirs.iter().any(|d| d == &name) {
                    continue;
                }
                tracing::debug!(
                    entry = %path.display(),
                    "materializing symlinked directory entry"
                );
                Box::pin(copy_dir_filtered_depth(
                    &path,
                    &dst.join(&name),
                    exclude_dirs,
                    exclude_files,
                    depth + 1,
                ))
                .await?;
            } else if resolved.is_file() {
                if exclude_files.iter().any(|f| f == &name) {
                    continue;
                }
                tracing::debug!(
                    entry = %path.display(),
                    "materializing symlinked file entry"
                );
                fs::copy(&path, &dst.join(&name)).await?;
            } else {
                return Err(AppError::system(format!(
                    "cannot copy non-regular entry {}",
                    path.display()
                )));
            }
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
