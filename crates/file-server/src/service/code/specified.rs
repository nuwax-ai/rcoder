//! 增量文件操作 (对齐 nuwax codeService.specifiedFilesUpdate):
//! specified_files_update (project 入口, 含版本备份 + 结构校验) + apply_file_ops
//! (path 制核心, project/computer 共用) + modify 策略 + 行级 diff。

use std::path::Path;

use serde_json::json;
use tokio::fs;

use crate::config::Config;
use crate::error::{AppError, AppResult};
use crate::path_safety::safe_within_or_skip;
use crate::service::version;
use crate::workspace::{ProjectContext, WorkspaceResolver};

use super::types::{FileOp, FileOperation};

/// modify 策略 (project 与 computer 路由口径不同):
/// - `Diff`: 行级 diff 合并 (project specifiedFilesUpdate; 换行符继承 existing)。
/// - `ByteCompare`: 字节相等跳写, 不等直写 (computer updateFiles; 避免 \r\n 归一改内容)。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModifyStrategy {
    Diff,
    ByteCompare,
}

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
    let validated_ops = validate_file_ops(files)?;

    let project_path = resolver.resolve_project(ctx).await?;
    if !crate::service::fs_util::path_exists(&project_path).await? {
        return Err(AppError::resource("Project does not exist"));
    }
    version::backup_project(config, project_id, &project_path, code_version).await?;

    apply_validated_file_ops(&project_path, &validated_ops, ModifyStrategy::Diff).await?;
    Ok(SpecifiedResult {
        project_id: project_id.to_string(),
        files_count: files.len(),
    })
}

/// 在产生任何文件副作用之前校验增量操作结构。
fn validate_file_ops(files: &[FileOp]) -> AppResult<Vec<ValidatedFileOp<'_>>> {
    let mut validated = Vec::with_capacity(files.len());
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
        let operation = op.operation.parse::<FileOperation>().map_err(|_| {
            AppError::validation(format!(
                "files[{i}].operation must be one of create, delete, rename or modify"
            ))
        })?;
        if operation == FileOperation::Rename
            && op
                .rename_from
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            return Err(AppError::validation(format!(
                "files[{i}].renameFrom cannot be empty (rename operation requires)"
            )));
        }
        if operation == FileOperation::Modify && op.contents.is_none() {
            return Err(AppError::validation(format!(
                "files[{i}].contents must be a string (modify operation requires)"
            )));
        }
        validated.push(ValidatedFileOp {
            operation,
            input: op,
        });
    }
    Ok(validated)
}

struct ValidatedFileOp<'a> {
    operation: FileOperation,
    input: &'a FileOp,
}

/// 文件操作核心 (path 制, 无版本备份): create/delete/rename/modify, 路径防穿越。
/// project 路由 (specified_files_update, Diff) 与 computer 路由 (ByteCompare) 共用此核心。
pub async fn apply_file_ops(
    base: &Path,
    files: &[FileOp],
    strategy: ModifyStrategy,
) -> AppResult<()> {
    let validated_ops = validate_file_ops(files)?;
    apply_validated_file_ops(base, &validated_ops, strategy).await
}

async fn apply_validated_file_ops(
    base: &Path,
    files: &[ValidatedFileOp<'_>],
    strategy: ModifyStrategy,
) -> AppResult<()> {
    for validated in files {
        let op = validated.input;
        let Some(target) = safe_within_or_skip(base, &op.name) else {
            tracing::warn!(path = %op.name, "unsafe path, skipping");
            continue;
        };
        match validated.operation {
            FileOperation::Create => {
                if op.is_dir == Some(true) {
                    if !crate::service::fs_util::path_exists(&target).await? {
                        fs::create_dir_all(&target).await?;
                    }
                } else if let Some(parent) = target.parent() {
                    fs::create_dir_all(parent).await?;
                    fs::write(&target, op.contents.as_deref().unwrap_or("")).await?;
                } else {
                    fs::write(&target, op.contents.as_deref().unwrap_or("")).await?;
                }
            }
            FileOperation::Delete => {
                if crate::service::fs_util::path_exists(&target).await? {
                    let is_dir = fs::metadata(&target)
                        .await
                        .map(|m| m.is_dir())
                        .unwrap_or(false);
                    let remove_res = if is_dir {
                        fs::remove_dir_all(&target).await
                    } else {
                        fs::remove_file(&target).await
                    };
                    // NotFound = 文件在 path_exists 后、remove 前被删（TOCTOU）= 删除目标已达成，
                    // 良性跳过；其他错误（权限/IO/磁盘等）是真实失败 → 传播给调用方（Fail Fast），不吞。
                    if let Err(e) = remove_res
                        && e.kind() != std::io::ErrorKind::NotFound
                    {
                        return Err(AppError::system(format!(
                            "delete {}: {e}",
                            target.display()
                        )));
                    }
                }
            }
            FileOperation::Rename => {
                let Some(from) = op.rename_from.as_deref() else {
                    continue;
                };
                let Some(old) = safe_within_or_skip(base, from) else {
                    tracing::warn!(path = %from, "unsafe rename source, skipping");
                    continue;
                };
                if crate::service::fs_util::path_exists(&old).await? {
                    if let Some(parent) = target.parent() {
                        fs::create_dir_all(parent).await?;
                    }
                    fs::rename(&old, &target).await?;
                }
            }
            FileOperation::Modify => {
                if !crate::service::fs_util::path_exists(&target).await? {
                    continue;
                }
                let new = op.contents.as_deref().unwrap_or("");
                match strategy {
                    ModifyStrategy::ByteCompare => {
                        // computer updateFiles: 字节相等跳写, 不等直写 (避免 \r\n 归一改内容)
                        let existing = fs::read(&target).await.map_err(|error| {
                            AppError::system(format!(
                                "read {} before update: {error}",
                                target.display()
                            ))
                        })?;
                        if existing == new.as_bytes() {
                            continue;
                        }
                        fs::write(&target, new).await?;
                    }
                    ModifyStrategy::Diff => {
                        // project specifiedFilesUpdate: 行级 diff 合并, 换行符继承 existing
                        let existing = fs::read_to_string(&target).await.map_err(|error| {
                            AppError::system(format!(
                                "read {} before update: {error}",
                                target.display()
                            ))
                        })?;
                        let (final_content, changes) = diff_content_by_lines(&existing, new);
                        if changes == 0 {
                            continue; // 内容未变跳写, 避免 HMR
                        }
                        fs::write(&target, final_content).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

/// 行级对齐合并 (对齐 nuwax diffContentByLines); 换行符继承 existing; changes=0 跳写。
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
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    (out.join(newline), changes)
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
}
