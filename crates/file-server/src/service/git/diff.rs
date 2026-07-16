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
//! gix 的 [`UnifiedDiff`] 不产出 `\ No newline at end of file` 标记
//! (它统一补 `\n`), 与 nuwax/git CLI 存在细微差异; 仅影响显示, 不影响 diff 正确性。

use std::collections::BTreeSet;
use std::path::Path;

use crate::error::{AppError, AppResult};

use super::{get_status, map_git_err};

use gix::diff::blob::sources;
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, Diff, InternedInput, UnifiedDiff};

/// diff 数据来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffSource {
    Worktree,
    Staged,
    Commit,
}

impl DiffSource {
    pub fn parse(s: &str) -> AppResult<Self> {
        match s {
            "" | "worktree" => Ok(Self::Worktree),
            "staged" => Ok(Self::Staged),
            "commit" => Ok(Self::Commit),
            other => Err(AppError::validation(format!(
                "source must be worktree|staged|commit, got {other}"
            ))),
        }
    }
}

/// diff 请求参数。
pub struct DiffParams {
    pub source: DiffSource,
    pub from: Option<String>,
    pub to: Option<String>,
    pub paths: Vec<String>,
}

/// 单文件统计。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FileSummary {
    pub file: String,
    pub changes: usize,
    pub insertions: usize,
    pub deletions: usize,
    pub binary: bool,
}

/// diff 结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DiffResult {
    pub diff: String,
    pub files: Vec<FileSummary>,
    pub insertions: usize,
    pub deletions: usize,
}

/// 计算工作区 diff (对齐 nuwax diff)。
pub fn compute_diff(repo: &gix::Repository, params: &DiffParams) -> AppResult<DiffResult> {
    let changes = match params.source {
        DiffSource::Commit => collect_commit_changes(repo, params)?,
        DiffSource::Worktree => collect_worktree_changes(repo)?,
        DiffSource::Staged => collect_staged_changes(repo)?,
    };
    render_changes(repo, changes, &params.paths)
}

// ── 变更收集 ────────────────────────────────────────────────────────────────────

/// 一侧内容: 有 blob id 时优先用 id (避免重复 write_blob), 否则用裸 bytes。
struct Side {
    bytes: Option<Vec<u8>>,
}

/// 单文件两侧变更。
struct FileChange {
    path: String,
    old: Side,
    new: Side,
}

fn collect_commit_changes(
    repo: &gix::Repository,
    params: &DiffParams,
) -> AppResult<Vec<FileChange>> {
    let from = params
        .from
        .as_deref()
        .ok_or_else(|| AppError::validation("commit diff requires `from`"))?;
    let from_id = repo
        .rev_parse_single(from)
        .map_err(|e| map_git_err(e, "git rev_parse from"))?;
    let from_tree = repo
        .find_commit(from_id)
        .map_err(|e| map_git_err(e, "git find_commit from"))?
        .tree()
        .map_err(|e| map_git_err(e, "git from tree"))?
        .id()
        .detach();
    // to 缺省取 from 的首个 parent; 无 parent (初始提交) → 空 tree (全部视为新增)
    let to_tree = match &params.to {
        Some(to) => {
            let to_id = repo
                .rev_parse_single(to.as_str())
                .map_err(|e| map_git_err(e, "git rev_parse to"))?;
            repo.find_commit(to_id)
                .map_err(|e| map_git_err(e, "git find_commit to"))?
                .tree()
                .map_err(|e| map_git_err(e, "git to tree"))?
                .id()
                .detach()
        }
        None => {
            let commit = repo
                .find_commit(from_id)
                .map_err(|e| map_git_err(e, "git find_commit from (parent)"))?;
            match commit.parent_ids().next() {
                Some(parent_id) => repo
                    .find_commit(parent_id)
                    .map_err(|e| map_git_err(e, "git find_commit parent"))?
                    .tree()
                    .map_err(|e| map_git_err(e, "git parent tree"))?
                    .id()
                    .detach(),
                None => repo.head_tree_id_or_empty().map_err(|e| map_git_err(e, "git empty tree"))?.detach(),
            }
        }
    };
    // old = to_tree (基准), new = from_tree (目标) — 对齐 nuwax: diff to..from
    let old_tree = repo
        .find_tree(to_tree)
        .map_err(|e| map_git_err(e, "git find_tree old"))?;
    let new_tree = repo
        .find_tree(from_tree)
        .map_err(|e| map_git_err(e, "git find_tree new"))?;
    let changes = repo
        .diff_tree_to_tree(Some(&old_tree), &new_tree, None)
        .map_err(|e| map_git_err(e, "git diff_tree_to_tree"))?;
    let mut out = Vec::new();
    for change in changes {
        let (path, old_id, new_id) = match change {
            gix::diff::tree_with_rewrites::Change::Addition { location, id, .. } => {
                (location, None, Some(id))
            }
            gix::diff::tree_with_rewrites::Change::Deletion { location, id, .. } => {
                (location, Some(id), None)
            }
            gix::diff::tree_with_rewrites::Change::Modification {
                location,
                previous_id,
                id,
                ..
            } => (location, Some(previous_id), Some(id)),
            // rewrite 归为 modification (old=source, new=dest); source_id 取值需额外字段, 这里保守跳过
            gix::diff::tree_with_rewrites::Change::Rewrite { location, id, .. } => {
                (location, None, Some(id))
            }
        };
        let path = path.to_string();
        let old = read_blob(repo, old_id)?;
        let new = read_blob(repo, new_id)?;
        out.push(FileChange { path, old: Side { bytes: old }, new: Side { bytes: new } });
    }
    Ok(out)
}

fn collect_worktree_changes(repo: &gix::Repository) -> AppResult<Vec<FileChange>> {
    let st = get_status(repo)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let head_tree = head_tree(repo)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    // 工作区 diff = HEAD ↔ 工作区: 覆盖 staged + modified + created + deleted
    for f in st.staged.iter().chain(&st.modified).chain(&st.created).chain(&st.deleted) {
        if !seen.insert(f.clone()) {
            continue;
        }
        let old = read_head_blob(repo, &head_tree, f)?;
        let new = read_worktree_file(workdir, f);
        out.push(FileChange {
            path: f.clone(),
            old: Side { bytes: old },
            new: Side { bytes: new },
        });
    }
    Ok(out)
}

fn collect_staged_changes(repo: &gix::Repository) -> AppResult<Vec<FileChange>> {
    let st = get_status(repo)?;
    let head_tree = head_tree(repo)?;
    let index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let mut out = Vec::new();
    // 暂存区 diff = HEAD ↔ index: 仅 staged 桶 (index 相对 HEAD 的变更)
    for f in &st.staged {
        let old = read_head_blob(repo, &head_tree, f)?;
        let new = read_index_blob(repo, &index, f);
        out.push(FileChange {
            path: f.clone(),
            old: Side { bytes: old },
            new: Side { bytes: new },
        });
    }
    Ok(out)
}

// ── 渲染 ────────────────────────────────────────────────────────────────────────

/// 渲染所有变更 → 完整 unified diff 文本 + summary。
fn render_changes(
    repo: &gix::Repository,
    changes: Vec<FileChange>,
    path_filter: &[String],
) -> AppResult<DiffResult> {
    let mut diff = String::new();
    let mut files = Vec::new();
    let mut total_ins = 0usize;
    let mut total_del = 0usize;
    for ch in changes {
        if !path_filter.is_empty() && !path_filter.iter().any(|p| p == &ch.path) {
            continue;
        }
        let old_bytes = ch.old.bytes.as_deref();
        let new_bytes = ch.new.bytes.as_deref();
        let rendered = render_blob_diff(old_bytes, new_bytes)?;
        let old_hash = match ch.old.bytes.as_deref() {
            Some(b) => short_hash(repo, b)?,
            None => "0000000".to_string(),
        };
        let new_hash = match ch.new.bytes.as_deref() {
            Some(b) => short_hash(repo, b)?,
            None => "0000000".to_string(),
        };
        // 内容完全相同 → 跳过 (无变更)
        if !rendered.binary && rendered.hunks.is_empty() {
            continue;
        }
        let header = assemble_header(&ch.path, ch.old.bytes.is_some(), ch.new.bytes.is_some(), &old_hash, &new_hash);
        if rendered.binary {
            // 二进制: 只出文件头 + "Binary files ... differ", 无 hunk
            diff.push_str(&header);
            diff.push_str(&format!("Binary files a/{} and b/{} differ\n", ch.path, ch.path));
        } else {
            diff.push_str(&header);
            diff.push_str(&rendered.hunks);
            if !rendered.hunks.ends_with('\n') {
                diff.push('\n');
            }
        }
        files.push(FileSummary {
            file: ch.path.clone(),
            changes: rendered.insertions + rendered.deletions,
            insertions: rendered.insertions,
            deletions: rendered.deletions,
            binary: rendered.binary,
        });
        total_ins += rendered.insertions;
        total_del += rendered.deletions;
    }
    Ok(DiffResult {
        diff,
        files,
        insertions: total_ins,
        deletions: total_del,
    })
}

struct Rendered {
    hunks: String,
    insertions: usize,
    deletions: usize,
    binary: bool,
}

/// 渲染单文件 blob diff (对齐 nuwax makeDiffPatch 的 hunk 部分)。
fn render_blob_diff(old: Option<&[u8]>, new: Option<&[u8]>) -> AppResult<Rendered> {
    let ob = old.unwrap_or(&[]);
    let nb = new.unwrap_or(&[]);
    if is_binary(ob) || is_binary(nb) {
        return Ok(Rendered { hunks: String::new(), insertions: 0, deletions: 0, binary: true });
    }
    if ob == nb {
        return Ok(Rendered { hunks: String::new(), insertions: 0, deletions: 0, binary: false });
    }
    let a = sources::byte_lines(ob);
    let b = sources::byte_lines(nb);
    let input = InternedInput::new(a, b);
    let diff = Diff::compute(Algorithm::Myers, &input);
    let hunks = UnifiedDiff::new(
        &diff,
        &input,
        ConsumeBinaryHunk::new(String::new(), "\n"),
        ContextSize::symmetrical(3),
    )
    .consume()
    .map_err(|e| AppError::system(format!("git unified diff render: {e}")))?;
    let (ins, del) = count_changes(&hunks);
    Ok(Rendered { hunks, insertions: ins, deletions: del, binary: false })
}

/// 拼装 git CLI 风格文件头 (对齐 nuwax makeDiffPatch part d)。
fn assemble_header(path: &str, has_old: bool, has_new: bool, old_hash: &str, new_hash: &str) -> String {
    let mut h = String::new();
    h.push_str(&format!("diff --git a/{path} b/{path}\n"));
    if !has_old {
        h.push_str("new file mode 100644\n");
        h.push_str(&format!("index 0000000..{new_hash}\n"));
    } else if !has_new {
        h.push_str("deleted file mode 100644\n");
        h.push_str(&format!("index {old_hash}..0000000\n"));
    } else {
        h.push_str(&format!("index {old_hash}..{new_hash} 100644\n"));
    }
    h.push_str(&format!("--- {}\n", if has_old { format!("a/{path}") } else { "/dev/null".to_string() }));
    h.push_str(&format!("+++ {}\n", if has_new { format!("b/{path}") } else { "/dev/null".to_string() }));
    h
}

// ── 读取 helper ─────────────────────────────────────────────────────────────────

fn read_blob(repo: &gix::Repository, id: Option<gix::hash::ObjectId>) -> AppResult<Option<Vec<u8>>> {
    match id {
        Some(id) => {
            let blob = repo
                .find_blob(id)
                .map_err(|e| map_git_err(e, "git find_blob"))?;
            Ok(Some(blob.data.to_vec()))
        }
        None => Ok(None),
    }
}

fn head_tree(repo: &gix::Repository) -> AppResult<gix::Tree<'_>> {
    let head_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?;
    repo.find_tree(head_id)
        .map_err(|e| map_git_err(e, "git find_tree (head)"))
}

fn read_head_blob(
    repo: &gix::Repository,
    tree: &gix::Tree<'_>,
    path: &str,
) -> AppResult<Option<Vec<u8>>> {
    let entry = tree
        .lookup_entry_by_path(path)
        .map_err(|e| map_git_err(e, "git lookup_entry_by_path"))?;
    match entry {
        Some(e) => {
            let blob = repo
                .find_blob(e.id())
                .map_err(|e| map_git_err(e, "git find_blob (head)"))?;
            Ok(Some(blob.data.to_vec()))
        }
        None => Ok(None),
    }
}

fn read_index_blob(repo: &gix::Repository, index: &gix::index::File, path: &str) -> Option<Vec<u8>> {
    let bstr_path = gix::path::into_bstr(std::path::PathBuf::from(path));
    let entry = index.entry_by_path_and_stage(bstr_path.as_ref(), gix::index::entry::Stage::Unconflicted)?;
    let blob = repo.find_blob(entry.id).ok()?;
    Some(blob.data.to_vec())
}

fn read_worktree_file(workdir: &Path, path: &str) -> Option<Vec<u8>> {
    std::fs::read(workdir.join(path)).ok()
}

// ── 工具 ─────────────────────────────────────────────────────────────────────────

/// 二进制检测: 前 8000 字节含 \0 (对齐 nuwax isBinaryBuffer)。
fn is_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(8000).any(|&b| b == 0)
}

/// 统计 hunk 中 +/- 行数。
/// UnifiedDiff 输出仅含 `@@ ` 头与 `+`/`-`/` ` 内容行, 不含 `+++`/`---` 文件头,
/// 故凡 `+`/`-` 起始行即为新增/删除。
fn count_changes(hunks: &str) -> (usize, usize) {
    let mut ins = 0usize;
    let mut del = 0usize;
    for line in hunks.lines() {
        if line.starts_with('+') {
            ins += 1;
        } else if line.starts_with('-') {
            del += 1;
        }
    }
    (ins, del)
}

/// blob 7 字符短 hash (对齐 nuwax gitBlobHash[..7])。
/// write_blob 对已存在内容去重, 不会重复存储。
fn short_hash(repo: &gix::Repository, bytes: &[u8]) -> AppResult<String> {
    let id = repo
        .write_blob(bytes)
        .map_err(|e| map_git_err(e, "git write_blob (hash)"))?;
    let hex = id.to_hex().to_string();
    Ok(hex.chars().take(7).collect())
}
