//! Git 复杂写操作 (reset / checkout / revert / switch; 对齐 nuwax)。
//!
//! 实现策略: 复用 `index_from_tree` 拿到目标 tree 的完整文件列表 (含 blob id),
//! 逐个写回 worktree + 落 index。避免直接调用 gix `worktree::state::checkout`
//! (需 progress/objects-arc/options 机器), 且与 nuwax 手搓逻辑 (listFiles +
//! readBlob + writeFileSync) 行为一致 (含 nuwax 的语义差异, 见各函数注释)。

use std::path::Path;

use gix::Repository;
use gix::hash::{ObjectId, oid};
use gix::index::{entry::Stage, write::Options as IndexWriteOptions};
use gix::path::from_bstr;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};

use crate::error::{AppError, AppResult};
use crate::path_safety::ensure_within_path;

use super::{commit_indexed, ensure_gitignore, get_status, map_git_err, stage_path};

// ── reset ──────────────────────────────────────────────────────────────────────

/// reset mode (对齐 nuwax reset.mode)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ResetMode {
    Soft,
    Mixed,
    Hard,
}

impl ResetMode {
    /// 小写字符串形式 (Display / FromStr 共用, 集中维护避免与变体定义分散)。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Soft => "soft",
            Self::Mixed => "mixed",
            Self::Hard => "hard",
        }
    }

    /// 解析为 ResetMode, 错误转 AppError::validation (兼容 handler `?`)。
    pub fn parse(s: &str) -> AppResult<Self> {
        s.parse::<Self>()
            .map_err(|e| AppError::validation(e.to_string()))
    }
}

impl std::fmt::Display for ResetMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// FromStr 解析错误。
#[derive(Debug, Clone)]
pub struct ResetModeParseError(pub String);

impl std::fmt::Display for ResetModeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "mode must be soft|mixed|hard, got {}", self.0)
    }
}

impl std::error::Error for ResetModeParseError {}

impl std::str::FromStr for ResetMode {
    type Err = ResetModeParseError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "" | "mixed" => Ok(Self::Mixed),
            "soft" => Ok(Self::Soft),
            "hard" => Ok(Self::Hard),
            other => Err(ResetModeParseError(other.to_string())),
        }
    }
}

pub struct ResetOutcome {
    pub previous_head: Option<String>,
}

/// `reset` (对齐 nuwax reset):
/// - 移动当前分支 ref → target
/// - mixed/hard: 重建 index 为 target tree
/// - hard: 额外重写 worktree (写 target 文件 + 删除 target 之外文件) + 补 .gitignore
pub fn reset(repo: &Repository, target: &str, mode: ResetMode) -> AppResult<ResetOutcome> {
    let target_id = repo
        .rev_parse_single(target)
        .map_err(|e| map_git_err(e, "git rev_parse target"))?
        .detach();
    // index_from_tree 需 tree id (非 commit id)
    let target_tree_id = repo
        .find_commit(target_id)
        .map_err(|e| map_git_err(e, "git find_commit target"))?
        .tree()
        .map_err(|e| map_git_err(e, "git target tree"))?
        .id()
        .detach();
    let previous_head = repo.head_id().ok().map(|id| id.to_string());
    let old_head_tree = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?
        .detach();

    move_branch_ref(repo, target_id, "reset")?;

    match mode {
        ResetMode::Soft => {}
        ResetMode::Mixed => {
            // 重建 index = target tree (workdir 不变)
            reset_index_to_tree(repo, &target_tree_id)?;
        }
        ResetMode::Hard => {
            // apply_tree_to_worktree 内部已设 index = target tree + 写 worktree + 删多余
            let workdir = repo
                .workdir()
                .ok_or_else(|| AppError::system("git repo has no workdir"))?;
            apply_tree_to_worktree(repo, workdir, &target_tree_id, Some(&old_head_tree))?;
            ensure_gitignore(workdir)?;
            stage_path(repo, ".gitignore")?;
        }
    }
    Ok(ResetOutcome { previous_head })
}

/// 把 index 重置为 tree (mixed reset 用; 不动 worktree)。
fn reset_index_to_tree(repo: &Repository, tree_id: &oid) -> AppResult<()> {
    let mut idx = repo
        .index_from_tree(tree_id)
        .map_err(|e| map_git_err(e, "git index_from_tree"))?;
    idx.remove_tree();
    idx.write(IndexWriteOptions::default())
        .map_err(|e| map_git_err(e, "git index write"))?;
    Ok(())
}

// ── checkout (tree restore) ────────────────────────────────────────────────────

/// `checkout` (对齐 nuwax checkout): 把 target 的整棵 tree 恢复到 workdir + index,
/// **不删除** target 之外的文件, **不动** HEAD, 变更留 staged。
/// (对齐 nuwax: 不是切分支, 不是恢复单文件; 类似 `git checkout <commit> -- .` 的覆盖语义)
pub fn checkout_tree(repo: &Repository, target: &str) -> AppResult<()> {
    let target_id = repo
        .rev_parse_single(target)
        .map_err(|e| map_git_err(e, "git rev_parse target"))?
        .detach();
    let target_tree_id = repo
        .find_commit(target_id)
        .map_err(|e| map_git_err(e, "git find_commit target"))?
        .tree()
        .map_err(|e| map_git_err(e, "git target tree"))?
        .id()
        .detach();
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    overlay_tree_on_worktree_and_index(repo, workdir, &target_tree_id)?;
    ensure_gitignore(workdir)?;
    stage_path(repo, ".gitignore")?;
    Ok(())
}

/// 将 target tree 覆盖到 worktree 和现有 index，不删除 target 之外的 index entry。
/// 这与 nuwax 的 listFiles + writeFile + git.add 行为一致。
fn overlay_tree_on_worktree_and_index(
    repo: &Repository,
    workdir: &Path,
    tree_id: &oid,
) -> AppResult<()> {
    let target_index = repo
        .index_from_tree(tree_id)
        .map_err(|e| map_git_err(e, "git index_from_tree (checkout overlay)"))?;
    let target_backing = target_index.path_backing();
    let mut current_index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index (checkout overlay)"))?;

    for entry in target_index.entries() {
        if entry.stage() != Stage::Unconflicted {
            continue;
        }
        let path = entry.path_in(target_backing);
        let blob = repo
            .find_blob(entry.id)
            .map_err(|e| map_git_err(e, "git find_blob (checkout overlay)"))?;
        let dest = ensure_within_path(workdir, from_bstr(path))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &blob.data)?;

        current_index.remove_entries(|_, candidate, _| candidate == path);
        current_index.dangerously_push_entry(entry.stat, entry.id, entry.flags, entry.mode, path);
    }
    current_index.sort_entries();
    current_index.remove_tree();
    current_index
        .write(IndexWriteOptions::default())
        .map_err(|e| map_git_err(e, "git index write (checkout overlay)"))?;
    Ok(())
}

// ── revert ─────────────────────────────────────────────────────────────────────

pub struct RevertOutcome {
    /// 新提交 hash (None = no-op, HEAD 已等于 target)。
    pub commit: Option<String>,
    pub previous_head: String,
    pub target: String,
}

/// `revert` (对齐 nuwax revert): 把 tree 重置到 target **但用新 commit 保留历史**。
/// (注意: 不是 `git revert <commit>` 反转单提交; 是"让文件树等于 target 再提交一次")
pub fn revert_to_commit(
    repo: &Repository,
    target: &str,
    message: Option<&str>,
    author_name: &str,
    author_email: &str,
) -> AppResult<RevertOutcome> {
    clean_tree_check(repo)?;
    let target_id = repo
        .rev_parse_single(target)
        .map_err(|e| map_git_err(e, "git rev_parse target"))?
        .detach();
    let target_tree_id = repo
        .find_commit(target_id)
        .map_err(|e| map_git_err(e, "git find_commit target"))?
        .tree()
        .map_err(|e| map_git_err(e, "git target tree"))?
        .id()
        .detach();
    let previous_head = repo
        .head_id()
        .map_err(|e| map_git_err(e, "git head_id"))?
        .detach()
        .to_string();
    let old_head_tree = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?
        .detach();
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;

    // 写 target tree → workdir + index, 并删除 target 之外的旧文件
    apply_tree_to_worktree(repo, workdir, &target_tree_id, Some(&old_head_tree))?;
    ensure_gitignore(workdir)?;
    stage_path(repo, ".gitignore")?;

    let st = get_status(repo)?;
    if st.staged.is_empty() {
        return Ok(RevertOutcome {
            commit: None,
            previous_head,
            target: target_id.to_string(),
        });
    }
    let full_target = target_id.to_string();
    let short = full_target.get(..7).unwrap_or(&full_target);
    let msg = match message {
        Some(m) => m.to_string(),
        None => format!("Revert to {short}"),
    };
    let hash = commit_indexed(repo, &msg, author_name, author_email)?;
    Ok(RevertOutcome {
        commit: Some(hash),
        previous_head,
        target: target_id.to_string(),
    })
}

// ── switch branch ──────────────────────────────────────────────────────────────

/// `switch_branch` (对齐 nuwax branch-switch): 切到已存在分支。
/// - clean-tree 检查
/// - HEAD symbolic ref → `refs/heads/<name>`
/// - index + worktree 重置为分支 tree (删除多余文件)
pub fn switch_branch(repo: &Repository, name: &str) -> AppResult<()> {
    clean_tree_check(repo)?;
    let branch_full = format!("refs/heads/{name}");
    let branch_ref = repo
        .find_reference(&branch_full)
        .map_err(|e| map_git_err(e, "git find_reference (branch not found)"))?;
    let target_id = branch_ref.id().detach();
    let target_tree_id = repo
        .find_commit(target_id)
        .map_err(|e| map_git_err(e, "git find_commit branch"))?
        .tree()
        .map_err(|e| map_git_err(e, "git branch tree"))?
        .id()
        .detach();
    let old_head_tree = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?
        .detach();
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;

    set_head_symbolic(repo, &branch_full)?;
    apply_tree_to_worktree(repo, workdir, &target_tree_id, Some(&old_head_tree))?;
    Ok(())
}

// ── 共享 helper ─────────────────────────────────────────────────────────────────

/// 移动当前分支 ref → target_id (对齐 nuwax writeRef force=true)。
/// HEAD 须是 symbolic (在某分支上); detached → BusinessError。
fn move_branch_ref(repo: &Repository, target_id: ObjectId, log_msg: &str) -> AppResult<()> {
    let branch_full = repo
        .head_name()
        .ok()
        .flatten()
        .ok_or_else(|| AppError::business("cannot reset/checkout in detached HEAD state"))?;
    let mut r = repo
        .find_reference(branch_full.as_bstr())
        .map_err(|e| map_git_err(e, "git find_reference (current branch)"))?;
    r.set_target_id(target_id, log_msg)
        .map_err(|e| map_git_err(e, "git set_target_id"))?;
    Ok(())
}

/// 把 HEAD 改为 symbolic → branch_full (切分支用)。
fn set_head_symbolic(repo: &Repository, branch_full: &str) -> AppResult<()> {
    let target_ref = FullName::try_from(branch_full)
        .map_err(|e| AppError::system(format!("invalid branch ref name: {e}")))?;
    let head_name = FullName::try_from("HEAD")
        .map_err(|e| AppError::system(format!("invalid HEAD name: {e}")))?;
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: format!("checkout: moving to {branch_full}").into(),
            },
            expected: PreviousValue::Any,
            new: Target::Symbolic(target_ref),
        },
        name: head_name,
        deref: false,
    };
    repo.edit_references(std::iter::once(edit))
        .map_err(|e| map_git_err(e, "git edit_references (HEAD symbolic)"))?;
    Ok(())
}

/// 把 tree_id 的所有文件写到 workdir, 并把 index 设为该 tree。
/// - 写每个 blob → workdir (含 mkdir parent)
/// - 落 index = tree_id (index_from_tree + write)
/// - 若 `old_tree_id` 给定: 删除 old_tree 有但 tree_id 没有的 worktree 文件 (reset-hard/revert/switch 用)
fn apply_tree_to_worktree(
    repo: &Repository,
    workdir: &Path,
    tree_id: &oid,
    old_tree_id: Option<&oid>,
) -> AppResult<()> {
    let mut new_index = repo
        .index_from_tree(tree_id)
        .map_err(|e| map_git_err(e, "git index_from_tree"))?;
    let new_backing = new_index.path_backing();
    // 写所有 target 文件到 worktree
    for entry in new_index.entries() {
        let path = entry.path_in(new_backing);
        let blob = repo
            .find_blob(entry.id)
            .map_err(|e| map_git_err(e, "git find_blob (checkout)"))?;
        let dest = ensure_within_path(workdir, from_bstr(path))?;
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &blob.data)?;
    }
    // 删除 old_tree 有但 target 没有的文件
    if let Some(old_id) = old_tree_id {
        let old_index = repo
            .index_from_tree(old_id)
            .map_err(|e| map_git_err(e, "git index_from_tree (old)"))?;
        let old_backing = old_index.path_backing();
        for entry in old_index.entries() {
            let path = entry.path_in(old_backing);
            if new_index
                .entry_by_path_and_stage(path, Stage::Unconflicted)
                .is_none()
            {
                // 防御：恶意 old-tree entry 含 `..` 时跳过删除（绝不删工作区外文件）
                if let Ok(abs) = ensure_within_path(workdir, from_bstr(path))
                    && let Err(e) = std::fs::remove_file(&abs)
                {
                    tracing::warn!(error = %e, "remove stale worktree file during checkout apply failed (skipping)");
                }
            }
        }
    }
    // 落 index = target tree
    new_index.remove_tree();
    new_index
        .write(IndexWriteOptions::default())
        .map_err(|e| map_git_err(e, "git index write (checkout)"))?;
    Ok(())
}

/// clean-tree 检查: 有 staged/modified/deleted 跟踪变更 → BusinessError (revert/switch/branch-create 前置)。
/// 对齐 nuwax `beforeMatrix.some(([f,H,W,S]) => H===0&&S===0 ? false : W!==1||S!==1)` ——
/// workdir 删除的跟踪文件 (H=1,W=0) 也算未提交变更, 须阻止; 仅未跟踪文件 (H=0,S=0) 不阻止。
pub(crate) fn clean_tree_check(repo: &Repository) -> AppResult<()> {
    let st = get_status(repo)?;
    if !st.staged.is_empty() || !st.modified.is_empty() || !st.deleted.is_empty() {
        return Err(AppError::business(
            "working tree has uncommitted changes (stage or discard first)",
        ));
    }
    Ok(())
}
