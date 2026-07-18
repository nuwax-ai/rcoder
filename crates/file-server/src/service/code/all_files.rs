//! 全量覆盖 (对齐 nuwax codeService.allFilesUpdate):
//! all_files_update (project 入口, 含版本备份) + apply_all_files (path 制核心) +
//! normalize_relative + pruneMissingFiles。

use std::collections::HashSet;
use std::path::{Component, Path};

use base64::Engine;
use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::path_safety::safe_within_or_skip;
use crate::service::version;
use crate::workspace::{ProjectContext, WorkspaceResolver};

use super::types::FileEntry;

pub struct AllResult {
    pub project_id: String,
}

/// 全量覆盖 + 清理缺失文件。is_dir/rename/binary/text 分支 + pruneMissingFiles。
/// nuwax 原版无路径防穿越, 此处补 (越界跳过, 安全加固)。
pub async fn all_files_update(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    files: &[FileEntry],
) -> AppResult<AllResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    version::parse_version(code_version)?;
    let project_path = resolver.resolve_project(ctx);
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    version::backup_project(config, project_id, &project_path, code_version).await?;

    apply_all_files(&project_path, config, files).await?;
    Ok(AllResult {
        project_id: project_id.to_string(),
    })
}

/// 全量覆盖核心 (path 制, 无版本备份): is_dir/rename/binary/text + prune 缺失 + 重建空目录。
/// project 路由 (all_files_update) 与 computer 路由共用。
pub async fn apply_all_files(base: &Path, config: &Config, files: &[FileEntry]) -> AppResult<()> {
    for file in files {
        let Some(target) = safe_within_or_skip(base, &file.name) else {
            tracing::warn!(path = %file.name, "unsafe path, skipping");
            continue;
        };
        // (A) 目录
        if file.is_dir == Some(true) {
            if !crate::service::fs_util::path_exists(&target).await? {
                fs::create_dir_all(&target).await?;
            }
            continue;
        }
        // (B) rename
        if let Some(from) = file.rename_from.as_deref()
            && let Some(old) = safe_within_or_skip(base, from)
            && crate::service::fs_util::path_exists(&old).await?
        {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).await?;
            }
            fs::rename(&old, &target).await?;
            continue;
        }
        let is_binary = file.binary == Some(true);
        let is_text = file.binary == Some(false);
        let size_exceeded = file.size_exceeded.unwrap_or(false);
        let has_contents = file
            .contents
            .as_deref()
            .map(|c| !c.is_empty())
            .unwrap_or(false);
        // (C) 二进制
        if is_binary {
            if crate::service::fs_util::path_exists(&target).await? {
                continue;
            }
            if has_contents {
                if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).await?;
                }
                match base64::engine::general_purpose::STANDARD
                    .decode(file.contents.as_deref().unwrap_or(""))
                {
                    Ok(bytes) => {
                        fs::write(&target, bytes).await?;
                    }
                    Err(e) => tracing::warn!(error = %e, "binary base64 decode failed"),
                }
            }
            continue;
        }
        // (D) 文本
        let should_replace = is_text && (!size_exceeded || has_contents);
        if !should_replace {
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).await?;
        }
        fs::write(&target, file.contents.as_deref().unwrap_or("")).await?;
    }

    // 清理缺失文件
    let keep_set: HashSet<String> = files
        .iter()
        .filter(|f| !f.name.is_empty())
        .map(|f| normalize_relative(&f.name))
        .collect();
    prune_missing_files(
        base,
        &keep_set,
        &config.traverse_exclude_dirs,
        &config.content_traverse_exclude_files,
    )
    .await?;
    // 重建前端提交的空目录 (prune 只删文件)
    for file in files {
        if file.is_dir == Some(true) {
            let dir_path = base.join(normalize_relative(&file.name));
            if !crate::service::fs_util::path_exists(&dir_path).await? {
                fs::create_dir_all(&dir_path).await?;
            }
        }
    }
    Ok(())
}

/// 相对路径规范化 (keep_set key 与 prune 的 relative 必须一致)。
fn normalize_relative(name: &str) -> String {
    let mut segs: Vec<String> = Vec::new();
    for comp in Path::new(name).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                segs.pop();
            }
            Component::Normal(n) => segs.push(n.to_string_lossy().to_string()),
            _ => {}
        }
    }
    segs.join("/")
}

async fn prune_missing_files(
    base: &Path,
    keep_set: &HashSet<String>,
    exclude_dirs: &[String],
    protected_files: &[String],
) -> AppResult<()> {
    prune_walk(base, base, keep_set, exclude_dirs, protected_files).await
}

async fn prune_walk(
    root: &Path,
    dir: &Path,
    keep_set: &HashSet<String>,
    exclude_dirs: &[String],
    protected_files: &[String],
) -> AppResult<()> {
    let mut entries = fs::read_dir(dir).await?;
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().to_string();
        let full = entry.path();
        let ft = entry.file_type().await?;
        if ft.is_dir() {
            if exclude_dirs.iter().any(|d| d == &name) {
                continue;
            }
            Box::pin(prune_walk(
                root,
                &full,
                keep_set,
                exclude_dirs,
                protected_files,
            ))
            .await?;
        } else if ft.is_file() {
            // 保护: 隐藏文件 / 内容排除名单
            if name.starts_with('.') {
                continue;
            }
            if protected_files.iter().any(|p| p == &name) {
                continue;
            }
            let rel = full
                .strip_prefix(root)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            if !keep_set.contains(&rel) {
                let _ = fs::remove_file(&full).await; // 单文件失败忽略
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_relative_handles_dots() {
        assert_eq!(normalize_relative("a/b/c"), "a/b/c");
        assert_eq!(normalize_relative("./a/b"), "a/b");
        assert_eq!(normalize_relative("/a/b"), "a/b");
    }
}
