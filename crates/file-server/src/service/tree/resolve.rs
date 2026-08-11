//! resolve-file: 校验目标根目录下文件是否存在 (对齐 TS `resolveExistingFile`)。
//!
//! 从 `tree` 模块拆出: 与遍历/搜索是独立职责。

use std::path::{Path, PathBuf};

use path_clean::PathClean;
use tokio::fs;

use crate::error::AppResult;

use super::build_file_proxy_url;

/// [`resolve_existing_file`] 命中时的返回值。
pub struct FileResolveResult {
    /// 相对根目录的 POSIX 风格路径 (与 TS `name` 一致)。
    pub name: String,
    /// fileProxyUrl (含 customTargetDir 后缀); proxy_path 为空时为 None。
    pub file_proxy_url: Option<String>,
}

/// 校验目标根目录下文件是否存在 (对齐 TS `resolveExistingFile` + `resolveFilePathWithinWorkspace`)。
///
/// - `root`: 目标根目录 (默认工作区或 customTargetDir)。
/// - `file_path`: 相对 `root` 的路径; 绝对路径须落在 `root` 下 (否则被拒)。
///   兼容 TS: 以 `/` 开头但实为相对根的写法 (如 `/src/a.md`) 会剥前导斜杠重试。
/// - 命中且为**文件** (跟随符号链接) → `Some`; 不存在 / 越界 / 是目录 → `None`。
///
/// 注: 返回的 `file_proxy_url` 不含 customTargetDir 后缀, 由 handler 统一追加。
pub async fn resolve_existing_file(
    root: &Path,
    file_path: &str,
    proxy_path: Option<&str>,
) -> AppResult<Option<FileResolveResult>> {
    let resolved = resolve_file_path_within_workspace(root, file_path);
    // 兼容以 / 开头、实为相对目标根的写法 (如 /src/a.md)
    let resolved = match resolved {
        Some(r) => Some(r),
        None => {
            let as_relative = file_path.trim_start_matches(['/', '\\']);
            if as_relative.is_empty() || as_relative == file_path.trim() {
                None
            } else {
                resolve_file_path_within_workspace(root, as_relative)
            }
        }
    };
    let Some(ResolvedPath { abs_path, name }) = resolved else {
        return Ok(None);
    };
    // 与静态文件 sendFile 一致: 跟随符号链接, 只要最终是文件即可 (TS 用 fs.stat)
    let meta = match fs::metadata(&abs_path).await {
        Ok(m) => m,
        Err(_) => return Ok(None),
    };
    if !meta.is_file() {
        return Ok(None);
    }
    let file_proxy_url = build_file_proxy_url(proxy_path, &name);
    Ok(Some(FileResolveResult {
        name,
        file_proxy_url,
    }))
}

/// 路径解析结果 (绝对路径 + 相对根的 POSIX 名)。
struct ResolvedPath {
    abs_path: PathBuf,
    name: String,
}

/// 将 `file_path` 解析到 `root` 内 (对齐 TS `resolveFilePathWithinWorkspace`)。
///
/// 与 [`super::resolve_subdir`] 用统一的标准化策略 ([`.clean()`] + 残留 `..` 检测),
/// 差异仅在于: 本函数接受落在 `root` 内的**绝对路径** (供 IM 直出场景), 且 root 本身不算
/// (要解析到具体文件)。越界 / 空 → `None`。
fn resolve_file_path_within_workspace(root: &Path, file_path: &str) -> Option<ResolvedPath> {
    let trimmed = file_path.trim();
    if trimmed.is_empty() {
        return None;
    }

    let normalized_root = root.clean();
    let abs_path = if Path::new(trimmed).is_absolute() {
        // 绝对路径: clean 后须落在 root 下 (TS resolveFilePathWithinWorkspace 允许)
        PathBuf::from(trimmed).clean()
    } else {
        // 相对路径: 剥前导斜杠 + clean 标准化 (与 resolve_subdir 一致)
        let stripped = trimmed.trim_start_matches(['/', '\\']);
        if stripped.is_empty() {
            return None;
        }
        let normalized = Path::new(stripped).clean();
        // 标准化后仍含 .. → 越界
        if normalized
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return None;
        }
        normalized_root.join(normalized)
    };

    // 必须落在 root 下 (root 本身不算, 因为要解析到具体文件)
    if abs_path == normalized_root || !abs_path.starts_with(&normalized_root) {
        return None;
    }
    let name = abs_path
        .strip_prefix(&normalized_root)
        .map(|p| p.to_string_lossy().replace('\\', "/"))
        .unwrap_or_default();
    if name.is_empty() || name.starts_with("..") {
        return None;
    }
    Some(ResolvedPath { abs_path, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_file_path_within_workspace_logic() {
        let root = Path::new("/app/ws");
        // 相对路径 OK
        let r = resolve_file_path_within_workspace(root, "src/a.txt").unwrap();
        assert_eq!(r.abs_path, PathBuf::from("/app/ws/src/a.txt"));
        assert_eq!(r.name, "src/a.txt");
        // 越界 → None
        assert!(resolve_file_path_within_workspace(root, "../escape").is_none());
        // 空 → None
        assert!(resolve_file_path_within_workspace(root, "").is_none());
        // 绝对路径在 root 外 → None
        assert!(resolve_file_path_within_workspace(root, "/etc/passwd").is_none());
    }

    /// 构造测试目录结构 (与 tree/mod.rs tests 同构):
    /// ```text
    /// root/
    ///   a.txt
    ///   b.md
    ///   sub/
    ///     c.txt
    ///     d.log
    ///     nested/
    ///       e.txt
    /// ```
    async fn make_test_tree(root: &Path) {
        fs::create_dir_all(root.join("sub").join("nested"))
            .await
            .unwrap();
        fs::write(root.join("a.txt"), "a").await.unwrap();
        fs::write(root.join("b.md"), "b").await.unwrap();
        fs::write(root.join("sub").join("c.txt"), "c")
            .await
            .unwrap();
        fs::write(root.join("sub").join("d.log"), "d")
            .await
            .unwrap();
        fs::write(root.join("sub").join("nested").join("e.txt"), "e")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn resolve_existing_file_hits_relative_path() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let r = resolve_existing_file(tmp.path(), "sub/c.txt", Some("/proxy"))
            .await
            .unwrap()
            .expect("should resolve");
        assert_eq!(r.name, "sub/c.txt");
        assert_eq!(r.file_proxy_url.as_deref(), Some("/proxy/sub/c.txt"));
    }

    #[tokio::test]
    async fn resolve_existing_file_hits_leading_slash_compat() {
        // 兼容 /src/a.md 这类前导斜杠写法 (对齐 TS)
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let r = resolve_existing_file(tmp.path(), "/a.txt", Some("/proxy"))
            .await
            .unwrap()
            .expect("should resolve with leading slash");
        assert_eq!(r.name, "a.txt");
    }

    #[tokio::test]
    async fn resolve_existing_file_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        // 越界 → None (非 Err, 对齐 TS 返回 exists:false)
        let r = resolve_existing_file(tmp.path(), "../escape.txt", None)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn resolve_existing_file_directory_returns_none() {
        // 目录 (非文件) → None
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let r = resolve_existing_file(tmp.path(), "sub", None)
            .await
            .unwrap();
        assert!(r.is_none());
    }

    #[tokio::test]
    async fn resolve_existing_file_missing_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        make_test_tree(tmp.path()).await;
        let r = resolve_existing_file(tmp.path(), "nope.txt", None)
            .await
            .unwrap();
        assert!(r.is_none());
    }
}
