//! Git diff (对齐 nuwax gitService.diff)。
//!
//! 三种 source:
//! - `worktree` (默认): HEAD ↔ 工作区文件
//! - `staged`: HEAD ↔ 暂存区 (index)
//! - `commit`: `from` ↔ `to` (to 缺省取 from 的首个 parent; 无 parent 则对空 tree)
//!
//! unified diff 文本由 gix [`UnifiedDiff`] 渲染 (上下文 3 行, Myers 算法),
//! 文件级头 (`diff --git` / `index` / `---` / `+++` / `new file mode` / `deleted file mode`)
//! 按 git CLI 规则自行拼装 (对齐 nuwax `makeDiffPatch`)。
//!
//! gix [`UnifiedDiff`] 负责 hunk 边界，定制 [`ConsumeHunk`] delegate 负责 Git 无尾换行标记与统计；
//! 文件级头和 JSON summary 由本模块按 API 契约组织。

use std::collections::BTreeSet;

use crate::error::{AppError, AppResult};

use super::{get_status, map_git_err};

use gix::Repository;
use gix::diff::tree_with_rewrites::Change;

mod content;
mod render;
mod types;

pub use types::{DiffParams, DiffResult, DiffSource, FileSummary};

use content::{head_tree, read_blob, read_head_blob, read_index_blob, read_worktree_file};
use render::render_changes;
#[cfg(test)]
use render::{assemble_header, render_blob_diff};
use types::FileChange;

/// 计算工作区 diff (对齐 nuwax diff)。
pub fn compute_diff(repo: &Repository, params: &DiffParams) -> AppResult<DiffResult> {
    let mut changes = match params.source {
        DiffSource::Commit => collect_commit_changes(repo, params)?,
        DiffSource::Worktree => collect_worktree_changes(
            repo,
            &params.paths,
            params.max_file_size_bytes,
            params.max_total_bytes,
        )?,
        DiffSource::Staged => collect_staged_changes(
            repo,
            &params.paths,
            params.max_file_size_bytes,
            params.max_total_bytes,
        )?,
    };
    // isomorphic-git 的 listFiles/statusMatrix 以路径稳定排序；gix tree diff
    // 的事件顺序不作相同保证。在公共入口统一排序以保持 API 可比较。
    changes.sort_by(|a, b| a.path.cmp(&b.path));
    render_changes(repo, changes, params.max_output_bytes)
}

// ── 变更收集 ────────────────────────────────────────────────────────────────────

fn collect_commit_changes(repo: &Repository, params: &DiffParams) -> AppResult<Vec<FileChange>> {
    let from = params
        .from
        .as_deref()
        .ok_or_else(|| AppError::validation("commit diff requires `from`"))?;
    let from_id = repo
        .rev_parse_single(from)
        .map_err(|e| map_git_err(e, "git rev_parse from"))?;
    let requested_tree = repo
        .find_commit(from_id)
        .map_err(|e| map_git_err(e, "git find_commit from"))?
        .tree()
        .map_err(|e| map_git_err(e, "git from tree"))?
        .id()
        .detach();
    // 同时给 from/to: old=from, new=to。
    // 只给 from: old=from 的首个 parent (无 parent 则空树), new=from。
    let (old_tree_id, new_tree_id) = match &params.to {
        Some(to) => {
            let to_id = repo
                .rev_parse_single(to.as_str())
                .map_err(|e| map_git_err(e, "git rev_parse to"))?;
            let to_tree = repo
                .find_commit(to_id)
                .map_err(|e| map_git_err(e, "git find_commit to"))?
                .tree()
                .map_err(|e| map_git_err(e, "git to tree"))?
                .id()
                .detach();
            (requested_tree, to_tree)
        }
        None => {
            let commit = repo
                .find_commit(from_id)
                .map_err(|e| map_git_err(e, "git find_commit from (parent)"))?;
            let parent_tree = match commit.parent_ids().next() {
                Some(parent_id) => repo
                    .find_commit(parent_id)
                    .map_err(|e| map_git_err(e, "git find_commit parent"))?
                    .tree()
                    .map_err(|e| map_git_err(e, "git parent tree"))?
                    .id()
                    .detach(),
                None => repo.empty_tree().id().detach(),
            };
            (parent_tree, requested_tree)
        }
    };
    let old_tree = repo
        .find_tree(old_tree_id)
        .map_err(|e| map_git_err(e, "git find_tree old"))?;
    let new_tree = repo
        .find_tree(new_tree_id)
        .map_err(|e| map_git_err(e, "git find_tree new"))?;
    let changes = repo
        .diff_tree_to_tree(Some(&old_tree), &new_tree, None)
        .map_err(|e| map_git_err(e, "git diff_tree_to_tree"))?;
    let mut out = Vec::new();
    let mut total_bytes = 0_u64;
    for change in changes {
        let (path, old_id, old_mode, new_id, new_mode) = match change {
            Change::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (location, None, None, Some(id), Some(entry_mode))
            }
            Change::Deletion {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (location, Some(id), Some(entry_mode), None, None)
            }
            Change::Modification {
                location,
                previous_entry_mode,
                previous_id,
                entry_mode,
                id,
                ..
            } => {
                if previous_entry_mode.is_tree() || entry_mode.is_tree() {
                    continue;
                }
                (
                    location,
                    Some(previous_id),
                    Some(previous_entry_mode),
                    Some(id),
                    Some(entry_mode),
                )
            }
            Change::Rewrite {
                location,
                source_entry_mode,
                source_id,
                entry_mode,
                id,
                ..
            } => (
                location,
                Some(source_id),
                Some(source_entry_mode),
                Some(id),
                Some(entry_mode),
            ),
        };
        let path = path.to_string();
        if !includes_path(&params.paths, &path) {
            continue;
        }
        let old = read_blob(repo, old_id, old_mode, params.max_file_size_bytes)?;
        let new = read_blob(repo, new_id, new_mode, params.max_file_size_bytes)?;
        let change = FileChange { path, old, new };
        account_change(&change, &mut total_bytes, params.max_total_bytes)?;
        out.push(change);
    }
    Ok(out)
}

fn collect_worktree_changes(
    repo: &Repository,
    paths: &[String],
    max_bytes: u64,
    max_total_bytes: u64,
) -> AppResult<Vec<FileChange>> {
    let st = get_status(repo)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let head_tree = head_tree(repo)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    let mut total_bytes = 0_u64;
    // 工作区 diff = HEAD ↔ 工作区: 覆盖 staged + modified + created + deleted
    for f in st
        .staged
        .iter()
        .chain(&st.modified)
        .chain(&st.created)
        .chain(&st.deleted)
    {
        if !seen.insert(f.clone()) {
            continue;
        }
        if !includes_path(paths, f) {
            continue;
        }
        let old = read_head_blob(repo, &head_tree, f, max_bytes)?;
        let new = read_worktree_file(workdir, f, max_bytes)?;
        let change = FileChange {
            path: f.clone(),
            old,
            new,
        };
        account_change(&change, &mut total_bytes, max_total_bytes)?;
        out.push(change);
    }
    Ok(out)
}

fn collect_staged_changes(
    repo: &Repository,
    paths: &[String],
    max_bytes: u64,
    max_total_bytes: u64,
) -> AppResult<Vec<FileChange>> {
    let st = get_status(repo)?;
    let head_tree = head_tree(repo)?;
    let index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let mut out = Vec::new();
    let mut total_bytes = 0_u64;
    // 暂存区 diff = HEAD ↔ index: 仅 staged 桶 (index 相对 HEAD 的变更)
    for f in &st.staged {
        if !includes_path(paths, f) {
            continue;
        }
        let old = read_head_blob(repo, &head_tree, f, max_bytes)?;
        let new = read_index_blob(repo, &index, f, max_bytes)?;
        let change = FileChange {
            path: f.clone(),
            old,
            new,
        };
        account_change(&change, &mut total_bytes, max_total_bytes)?;
        out.push(change);
    }
    Ok(out)
}

fn account_change(change: &FileChange, total: &mut u64, max_total_bytes: u64) -> AppResult<()> {
    let change_bytes = change
        .old
        .bytes
        .as_ref()
        .map_or(0, Vec::len)
        .checked_add(change.new.bytes.as_ref().map_or(0, Vec::len))
        .and_then(|size| u64::try_from(size).ok())
        .ok_or_else(|| AppError::validation("git diff size overflow"))?;
    *total = total
        .checked_add(change_bytes)
        .ok_or_else(|| AppError::validation("git diff total size overflow"))?;
    if *total > max_total_bytes {
        return Err(AppError::validation(format!(
            "git diff total content exceeds limit (max {max_total_bytes} bytes)"
        )));
    }
    Ok(())
}

// ── 工具 ─────────────────────────────────────────────────────────────────────────

/// 二进制检测: 前 8000 字节含 \0 (对齐 nuwax isBinaryBuffer)。
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

fn ensure_output_size(output: &str, max_bytes: u64) -> AppResult<()> {
    let output_bytes = u64::try_from(output.len())
        .map_err(|_| AppError::validation("git diff output size overflow"))?;
    if output_bytes > max_bytes {
        return Err(AppError::validation(format!(
            "git diff output exceeds limit (max {max_bytes} bytes)"
        )));
    }
    Ok(())
}

fn includes_path(paths: &[String], path: &str) -> bool {
    paths.is_empty() || paths.iter().any(|candidate| candidate == path)
}

/// blob 7 字符短 hash (对齐 nuwax gitBlobHash[..7])。
/// 纯计算对象 ID，不修改对象数据库。
fn short_hash(repo: &Repository, bytes: &[u8]) -> AppResult<String> {
    let id = gix::objs::compute_hash(repo.object_hash(), gix::objs::Kind::Blob, bytes)
        .map_err(|error| AppError::system(format!("git compute blob hash: {error}")))?;
    let hex = id.to_hex().to_string();
    Ok(hex.chars().take(7).collect())
}

#[cfg(test)]
mod tests;
