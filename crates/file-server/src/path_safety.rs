//! 路径安全: 用 Rust 原生 `Path` API 拦截路径注入 (不做字符串 replace / 正则)。
//!
//! - `PathBuf::join` 对**绝对路径**参数会替换 base → 天然拦截绝对路径注入
//!   (如 `base.join("/etc/passwd")` 直接得到 `/etc/passwd`, 落在 base 外即被拒);
//! - `path-clean` 按成熟的词法规约规则消除 `.`/`..`，不访问文件系统;
//! - 规范化后用 `starts_with` 比较 → 确保落在 base 下。
//!
//! 全程不依赖文件系统存在性 (不 `canonicalize`), 不做字符串 `replace` / 手动 `split`。

use std::path::{Path, PathBuf};

use path_clean::PathClean;

use crate::error::{AppError, AppResult};

/// 业务文件路径校验: `relative` 解析后必须落在 `base` 下, 越界返回 `Err`。
/// 对齐 nuwax `uploadSingleFile` (抛错风格)。
pub fn ensure_within(base: &Path, relative: &str) -> AppResult<PathBuf> {
    let target = base.join(relative).clean();
    let normalized_base = base.clean();
    if target.starts_with(&normalized_base) {
        Ok(target)
    } else {
        Err(AppError::validation(
            "File path is not safe, cannot exceed project directory",
        ))
    }
}

/// Path 级校验（用于 git tree entry 等可能非 UTF-8 的路径）：`target` 解析后必须落在
/// `base` 下，越界返回 `Err`。语义同 [`ensure_within`]，区别是接受 `Path` 而非 `&str`。
pub fn ensure_within_path(base: &Path, target: impl AsRef<Path>) -> AppResult<PathBuf> {
    let target = base.join(target).clean();
    let normalized_base = base.clean();
    if target.starts_with(&normalized_base) {
        Ok(target)
    } else {
        Err(AppError::validation(
            "File path is not safe, cannot exceed project directory",
        ))
    }
}

/// 业务文件路径校验 (跳过风格): 越界返回 `None`, 由调用方决定跳过。
/// 对齐 nuwax `specifiedFilesUpdate` / `uploadBatchFiles`。
pub fn safe_within_or_skip(base: &Path, relative: &str) -> Option<PathBuf> {
    let target = base.join(relative).clean();
    let normalized_base = base.clean();
    if target.starts_with(&normalized_base) {
        Some(target)
    } else {
        None
    }
}

/// Zip 解压条目路径校验 (对齐 nuwax `assertSafeZipEntryPath`)。
/// `join` 拦截绝对路径，`path-clean` 规约 `..`，`starts_with` 兜底。
pub fn safe_zip_entry(extract_path: &Path, entry_name: &str) -> AppResult<PathBuf> {
    let target = extract_path.join(entry_name).clean();
    let normalized_base = extract_path.clean();
    if target == normalized_base || target.starts_with(&normalized_base) {
        Ok(target)
    } else {
        Err(AppError::file(format!(
            "Unsafe zip entry path: {entry_name}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn ensure_within_accepts_nested() {
        let base = PathBuf::from("/app/project_workspace/p1");
        assert_eq!(
            ensure_within(&base, "src/main.rs").unwrap(),
            PathBuf::from("/app/project_workspace/p1/src/main.rs")
        );
        // 多层嵌套
        assert_eq!(
            ensure_within(&base, "a/b/c/d.txt").unwrap(),
            PathBuf::from("/app/project_workspace/p1/a/b/c/d.txt")
        );
    }

    #[test]
    fn ensure_within_rejects_traversal_and_absolute() {
        let base = PathBuf::from("/app/project_workspace/p1");
        // `..` 越界
        assert!(ensure_within(&base, "../../etc/passwd").is_err());
        assert!(ensure_within(&base, "../secret").is_err());
        // 绝对路径: PathBuf::join 替换 base → 落在 base 外 → 拦截
        assert!(ensure_within(&base, "/etc/passwd").is_err());
        assert!(ensure_within(&base, "/app/other").is_err());
    }

    #[test]
    fn ensure_within_path_accepts_and_rejects() {
        let base = PathBuf::from("/app/p");
        assert_eq!(
            ensure_within_path(&base, "a/b.txt").unwrap(),
            PathBuf::from("/app/p/a/b.txt")
        );
        // `..` 越界 / 绝对路径均拒绝
        assert!(ensure_within_path(&base, "../secret").is_err());
        assert!(ensure_within_path(&base, "/etc/passwd").is_err());
    }

    #[test]
    fn safe_within_or_skip_returns_none_on_traversal() {
        let base = PathBuf::from("/app/p");
        assert!(safe_within_or_skip(&base, "../secret").is_none());
        assert!(safe_within_or_skip(&base, "/abs/path").is_none());
        assert!(safe_within_or_skip(&base, "ok.txt").is_some());
    }

    #[test]
    fn safe_zip_entry_rejects_absolute_and_traversal() {
        let ex = PathBuf::from("/tmp/extract");
        assert!(safe_zip_entry(&ex, "/etc/passwd").is_err());
        assert!(safe_zip_entry(&ex, "../escape").is_err());
        assert!(safe_zip_entry(&ex, "skills/foo/SKILL.md").is_ok());
        assert_eq!(
            safe_zip_entry(&ex, "skills/foo/SKILL.md").unwrap(),
            PathBuf::from("/tmp/extract/skills/foo/SKILL.md")
        );
    }
}
