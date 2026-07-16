//! code 文件写操作 (对齐 nuwax `codeService.specifiedFilesUpdate` / `allFilesUpdate`)。

use std::collections::HashSet;
use std::path::{Component, Path};

use base64::Engine;
use serde_json::json;
use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::path_safety::safe_within_or_skip;
use crate::service::version;
use crate::workspace::{ProjectContext, WorkspaceResolver};

// ── 请求结构 ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileOp {
    pub operation: String,
    pub name: String,
    #[serde(default)]
    pub is_dir: Option<bool>,
    #[serde(default)]
    pub contents: Option<String>,
    #[serde(default)]
    pub rename_from: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    pub name: String,
    #[serde(default)]
    pub contents: Option<String>,
    #[serde(default)]
    pub binary: Option<bool>,
    #[serde(default)]
    pub size_exceeded: Option<bool>,
    #[serde(default)]
    pub is_dir: Option<bool>,
    #[serde(default)]
    pub rename_from: Option<String>,
}

// ── decodeURIComponent (对齐 nuwax 路由层: 非法 % 保留原串, 不抛错) ──

pub fn decode_uri_component(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (hex_digit(b[i + 1]), hex_digit(b[i + 2]))
        {
            out.push(h * 16 + l);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8(out)
        .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

// ── diffContentByLines (行级对齐合并; 换行符继承 existing; changes=0 跳写) ──

fn diff_content_by_lines(existing: &str, new: &str) -> (String, i64) {
    // 统一 \r\n → \n 后按行切分 (等价 JS split(/\r?\n/))
    let norm_old = existing.replace("\r\n", "\n");
    let norm_new = new.replace("\r\n", "\n");
    let old_lines: Vec<&str> = norm_old.split('\n').collect();
    let new_lines: Vec<&str> = norm_new.split('\n').collect();
    let old_len = old_lines.len();
    let new_len = new_lines.len();
    let min_len = old_len.min(new_len);
    let mut out: Vec<String> = old_lines.iter().map(|s| (*s).to_string()).collect();
    let mut changes: i64 = 0;
    // 对齐区逐行替换
    for i in 0..min_len {
        if out[i] != new_lines[i] {
            out[i] = new_lines[i].to_string();
            changes += 1;
        }
    }
    // old 更长: 删尾
    if old_len > new_len {
        for _ in new_len..old_len {
            out.pop();
            changes += 1;
        }
    }
    // new 更长: 追加
    if new_len > old_len {
        for line in &new_lines[old_len..new_len] {
            out.push(line.to_string());
            changes += 1;
        }
    }
    let newline = if existing.contains("\r\n") { "\r\n" } else { "\n" };
    (out.join(newline), changes)
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

// ── specifiedFilesUpdate ────────────────────────────────────────────────────────

pub struct SpecifiedResult {
    pub project_id: String,
    pub files_count: usize,
}

/// 增量文件操作 (create/delete/rename/modify)。modify 用 diffContentByLines, changes=0 跳写。
/// 路径越界跳过 (对齐 nuwax specifiedFilesUpdate)。非 GIT 先备份。
pub async fn specified_files_update(
    resolver: &dyn WorkspaceResolver,
    config: &Config,
    ctx: &ProjectContext,
    code_version: &str,
    files: &[FileOp],
) -> AppResult<SpecifiedResult> {
    let project_id = ctx.project_id.trim();
    if project_id.is_empty() {
        return Err(AppError::validation("Project ID cannot be empty"));
    }
    if code_version.trim().is_empty() {
        return Err(AppError::validation("codeVersion cannot be empty"));
    }
    version::parse_version(code_version)?; // 校验为数字
    // 结构校验
    for (i, op) in files.iter().enumerate() {
        if op.operation.trim().is_empty() {
            return Err(AppError::validation_with(
                format!("files[{i}].operation cannot be empty"),
                json!({ "field": format!("files[{i}].operation") }),
            ));
        }
        if op.name.trim().is_empty() {
            return Err(AppError::validation_with(
                format!("files[{i}].name cannot be empty"),
                json!({ "field": format!("files[{i}].name") }),
            ));
        }
        let op_l = op.operation.to_lowercase();
        if !matches!(op_l.as_str(), "create" | "delete" | "rename" | "modify") {
            return Err(AppError::validation(format!(
                "files[{i}].operation must be one of create, delete, rename or modify"
            )));
        }
        if op_l == "rename"
            && op.rename_from.as_deref().map(str::trim).unwrap_or("").is_empty()
        {
            return Err(AppError::validation(format!(
                "files[{i}].renameFrom cannot be empty (rename operation requires)"
            )));
        }
        if op_l == "modify" && op.contents.is_none() {
            return Err(AppError::validation(format!(
                "files[{i}].contents must be a string (modify operation requires)"
            )));
        }
    }

    let project_path = resolver.resolve_project(ctx);
    if !fs::try_exists(&project_path).await.unwrap_or(false) {
        return Err(AppError::resource("Project does not exist"));
    }
    version::backup_project(config, project_id, &project_path, code_version).await?;

    for op in files {
        let op_l = op.operation.to_lowercase();
        let Some(target) = safe_within_or_skip(&project_path, &op.name) else {
            tracing::warn!(path = %op.name, "unsafe path, skipping");
            continue;
        };
        match op_l.as_str() {
            "create" => {
                if op.is_dir == Some(true) {
                    if !fs::try_exists(&target).await.unwrap_or(false) {
                        fs::create_dir_all(&target).await?;
                    }
                } else {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    fs::write(&target, op.contents.as_deref().unwrap_or("")).await?;
                }
            }
            "delete" => {
                if fs::try_exists(&target).await.unwrap_or(false) {
                    let is_dir = fs::metadata(&target).await.map(|m| m.is_dir()).unwrap_or(false);
                    if is_dir {
                        let _ = fs::remove_dir_all(&target).await;
                    } else {
                        let _ = fs::remove_file(&target).await;
                    }
                }
            }
            "rename" => {
                let Some(from) = op.rename_from.as_deref() else {
                    continue;
                };
                let Some(old) = safe_within_or_skip(&project_path, from) else {
                    tracing::warn!(path = %from, "unsafe rename source, skipping");
                    continue;
                };
                if fs::try_exists(&old).await.unwrap_or(false) {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    fs::rename(&old, &target).await?;
                }
            }
            "modify" => {
                if !fs::try_exists(&target).await.unwrap_or(false) {
                    continue;
                }
                let existing = fs::read_to_string(&target).await.unwrap_or_default();
                let new = op.contents.as_deref().unwrap_or("");
                let (final_content, changes) = diff_content_by_lines(&existing, new);
                if changes == 0 {
                    continue; // 内容未变跳写, 避免 HMR
                }
                fs::write(&target, final_content).await?;
            }
            _ => {}
        }
    }
    Ok(SpecifiedResult {
        project_id: project_id.to_string(),
        files_count: files.len(),
    })
}

// ── allFilesUpdate ──────────────────────────────────────────────────────────────

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
    if !fs::try_exists(&project_path).await.unwrap_or(false) {
        return Err(AppError::resource("Project does not exist"));
    }
    version::backup_project(config, project_id, &project_path, code_version).await?;

    for file in files {
        let Some(target) = safe_within_or_skip(&project_path, &file.name) else {
            tracing::warn!(path = %file.name, "unsafe path, skipping");
            continue;
        };
        // (A) 目录
        if file.is_dir == Some(true) {
            if !fs::try_exists(&target).await.unwrap_or(false) {
                fs::create_dir_all(&target).await?;
            }
            continue;
        }
        // (B) rename
        if let Some(from) = file.rename_from.as_deref()
            && let Some(old) = safe_within_or_skip(&project_path, from)
            && fs::try_exists(&old).await.unwrap_or(false)
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
        let has_contents = file.contents.as_deref().map(|c| !c.is_empty()).unwrap_or(false);
        // (C) 二进制
        if is_binary {
            if fs::try_exists(&target).await.unwrap_or(false) {
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
        &project_path,
        &keep_set,
        &config.traverse_exclude_dirs,
        &config.content_traverse_exclude_files,
    )
    .await?;
    // 重建前端提交的空目录 (prune 只删文件)
    for file in files {
        if file.is_dir == Some(true) {
            let dir_path = project_path.join(normalize_relative(&file.name));
            if !fs::try_exists(&dir_path).await.unwrap_or(false) {
                let _ = fs::create_dir_all(&dir_path).await;
            }
        }
    }
    Ok(AllResult {
        project_id: project_id.to_string(),
    })
}

// ── pruneMissingFiles ───────────────────────────────────────────────────────────

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
            Box::pin(prune_walk(root, &full, keep_set, exclude_dirs, protected_files)).await?;
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
    fn diff_replaces_all_lines() {
        let (out, c) = diff_content_by_lines("a\nb\nc", "x\ny\nz");
        assert_eq!(out, "x\ny\nz");
        assert_eq!(c, 3);
    }

    #[test]
    fn diff_truncates_when_new_shorter() {
        let (out, c) = diff_content_by_lines("a\nb\nc\nd", "a\nb");
        assert_eq!(out, "a\nb");
        assert_eq!(c, 2); // 删 c, d
    }

    #[test]
    fn diff_appends_when_new_longer() {
        let (out, c) = diff_content_by_lines("a", "a\nb\nc");
        assert_eq!(out, "a\nb\nc");
        assert_eq!(c, 2); // 追加 b, c
    }

    #[test]
    fn diff_zero_changes_when_identical() {
        let (out, c) = diff_content_by_lines("a\nb", "a\nb");
        assert_eq!(out, "a\nb");
        assert_eq!(c, 0); // 跳写
    }

    #[test]
    fn diff_inherits_crlf_from_existing() {
        let (out, _) = diff_content_by_lines("a\r\nb", "x\ny");
        assert_eq!(out, "x\r\ny");
    }

    #[test]
    fn decode_uri_percent() {
        assert_eq!(decode_uri_component("%E4%B8%AD"), "中");
        assert_eq!(decode_uri_component("hello%20world"), "hello world");
        assert_eq!(decode_uri_component("plain"), "plain");
        assert_eq!(decode_uri_component("bad%ZZ"), "bad%ZZ"); // 非法保留
    }

    #[test]
    fn normalize_relative_handles_dots() {
        assert_eq!(normalize_relative("a/b/c"), "a/b/c");
        assert_eq!(normalize_relative("./a/b"), "a/b");
        assert_eq!(normalize_relative("/a/b"), "a/b");
    }
}
