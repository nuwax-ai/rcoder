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
use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};

use super::{get_status, map_git_err};

use gix::diff::blob::sources;
use gix::diff::blob::unified_diff::{ConsumeBinaryHunk, ContextSize};
use gix::diff::blob::{Algorithm, Diff, InternedInput, UnifiedDiff};
use gix::diff::tree_with_rewrites::Change;
use gix::hash::ObjectId;
use gix::index::{File as IndexFile, entry::Stage as IndexStage};
use gix::path::into_bstr;
use gix::{Repository, Tree};

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
pub fn compute_diff(repo: &Repository, params: &DiffParams) -> AppResult<DiffResult> {
    let mut changes = match params.source {
        DiffSource::Commit => collect_commit_changes(repo, params)?,
        DiffSource::Worktree => collect_worktree_changes(repo)?,
        DiffSource::Staged => collect_staged_changes(repo)?,
    };
    // isomorphic-git 的 listFiles/statusMatrix 以路径稳定排序；gix tree diff
    // 的事件顺序不作相同保证。在公共入口统一排序以保持 API 可比较。
    changes.sort_by(|a, b| a.path.cmp(&b.path));
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
    for change in changes {
        let (path, old_id, new_id) = match change {
            Change::Addition {
                location,
                entry_mode,
                id,
                ..
            } => {
                if entry_mode.is_tree() {
                    continue;
                }
                (location, None, Some(id))
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
                (location, Some(id), None)
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
                (location, Some(previous_id), Some(id))
            }
            Change::Rewrite {
                location,
                source_id,
                id,
                ..
            } => (location, Some(source_id), Some(id)),
        };
        let path = path.to_string();
        let old = read_blob(repo, old_id)?;
        let new = read_blob(repo, new_id)?;
        out.push(FileChange {
            path,
            old: Side { bytes: old },
            new: Side { bytes: new },
        });
    }
    Ok(out)
}

fn collect_worktree_changes(repo: &Repository) -> AppResult<Vec<FileChange>> {
    let st = get_status(repo)?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let head_tree = head_tree(repo)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
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

fn collect_staged_changes(repo: &Repository) -> AppResult<Vec<FileChange>> {
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
    repo: &Repository,
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
        let header = assemble_header(
            &ch.path,
            ch.old.bytes.is_some(),
            ch.new.bytes.is_some(),
            &old_hash,
            &new_hash,
        );
        if rendered.binary {
            // 二进制: 只出文件头 + "Binary files ... differ", 无 hunk
            diff.push_str(&header);
            diff.push_str(&format!(
                "Binary files a/{} and b/{} differ\n",
                ch.path, ch.path
            ));
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
        return Ok(Rendered {
            hunks: String::new(),
            insertions: 0,
            deletions: 0,
            binary: true,
        });
    }
    if ob == nb {
        return Ok(Rendered {
            hunks: String::new(),
            insertions: 0,
            deletions: 0,
            binary: false,
        });
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
    Ok(Rendered {
        hunks,
        insertions: ins,
        deletions: del,
        binary: false,
    })
}

/// 拼装 git CLI 风格文件头 (对齐 nuwax makeDiffPatch part d)。
fn assemble_header(
    path: &str,
    has_old: bool,
    has_new: bool,
    old_hash: &str,
    new_hash: &str,
) -> String {
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
    h.push_str(&format!(
        "--- {}\n",
        if has_old {
            format!("a/{path}")
        } else {
            "/dev/null".to_string()
        }
    ));
    h.push_str(&format!(
        "+++ {}\n",
        if has_new {
            format!("b/{path}")
        } else {
            "/dev/null".to_string()
        }
    ));
    h
}

// ── 读取 helper ─────────────────────────────────────────────────────────────────

fn read_blob(repo: &Repository, id: Option<ObjectId>) -> AppResult<Option<Vec<u8>>> {
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

fn head_tree(repo: &Repository) -> AppResult<Tree<'_>> {
    let head_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?;
    repo.find_tree(head_id)
        .map_err(|e| map_git_err(e, "git find_tree (head)"))
}

fn read_head_blob(repo: &Repository, tree: &Tree<'_>, path: &str) -> AppResult<Option<Vec<u8>>> {
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

fn read_index_blob(repo: &Repository, index: &IndexFile, path: &str) -> Option<Vec<u8>> {
    let bstr_path = into_bstr(PathBuf::from(path));
    let entry = index.entry_by_path_and_stage(bstr_path.as_ref(), IndexStage::Unconflicted)?;
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
fn short_hash(repo: &Repository, bytes: &[u8]) -> AppResult<String> {
    let id = repo
        .write_blob(bytes)
        .map_err(|e| map_git_err(e, "git write_blob (hash)"))?;
    let hex = id.to_hex().to_string();
    Ok(hex.chars().take(7).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::git::{commit_indexed, init_repo, stage_path};
    use gix::open;

    #[test]
    fn commit_diff_handles_nested_tree_and_preserves_from_to_direction() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "file-server-diff-test-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(root.join("src")).expect("create fixture");
        init_repo(&root, "Test", "test@example.com").expect("init repo");
        let repo = open(&root).expect("open repo");

        std::fs::write(root.join("src/app.txt"), "old\n").expect("write old");
        stage_path(&repo, "src/app.txt").expect("stage old");
        let old = commit_indexed(&repo, "old", "Test", "test@example.com").expect("commit old");

        std::fs::write(root.join("src/app.txt"), "new\n").expect("write new");
        std::fs::write(root.join("src/added.txt"), "added\n").expect("write added");
        stage_path(&repo, "src").expect("stage nested tree");
        let new = commit_indexed(&repo, "new", "Test", "test@example.com").expect("commit new");

        let result = compute_diff(
            &repo,
            &DiffParams {
                source: DiffSource::Commit,
                from: Some(old),
                to: Some(new.clone()),
                paths: Vec::new(),
            },
        )
        .expect("explicit commit diff");
        assert_eq!(
            result
                .files
                .iter()
                .map(|file| file.file.as_str())
                .collect::<Vec<_>>(),
            ["src/added.txt", "src/app.txt"]
        );
        assert!(result.diff.contains("-old"));
        assert!(result.diff.contains("+new"));

        let from_only = compute_diff(
            &repo,
            &DiffParams {
                source: DiffSource::Commit,
                from: Some(new),
                to: None,
                paths: Vec::new(),
            },
        )
        .expect("from-only commit diff");
        assert_eq!(from_only.insertions, result.insertions);
        assert_eq!(from_only.deletions, result.deletions);

        drop(repo);
        let _ = std::fs::remove_dir_all(root);
    }
}
