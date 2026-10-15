//! Git 复杂写操作 (reset / checkout / revert / switch; 对齐 nuwax)。
//!
//! 实现策略: 复用 `index_from_tree` 拿到目标 tree 的完整文件列表 (含 blob id),
//! 逐个写回 worktree + 落 index。避免直接调用 gix `worktree::state::checkout`
//! (需 progress/objects-arc/options 机器), 且与 nuwax 手搓逻辑 (listFiles +
//! readBlob + writeFileSync) 行为一致 (含 nuwax 的语义差异, 见各函数注释)。

use std::path::Path;

use gix::Repository;
use gix::actor::Signature;
use gix::bstr::BString;

use gix::date::{Time, parse::TimeBuf};
use gix::hash::{ObjectId, oid};
use gix::index::{entry::Stage, write::Options as IndexWriteOptions};
use gix::path::from_bstr;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};

use crate::error::{AppError, AppResult};
use crate::path_safety::ensure_within_path;

use super::read::{head_id_required, resolve_rev_required};
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

#[derive(Debug)]
pub struct ResetOutcome {
    pub previous_head: Option<String>,
}

/// `reset` (对齐 nuwax reset):
/// - 移动当前分支 ref → target
/// - mixed/hard: 重建 index 为 target tree
/// - hard: 额外重写 worktree (写 target 文件 + 删除 target 之外文件) + 补 .gitignore
pub fn reset(
    repo: &Repository,
    target: &str,
    mode: ResetMode,
    author_name: &str,
    author_email: &str,
) -> AppResult<ResetOutcome> {
    let target_id = resolve_rev_required(repo, target, "git reset target")?;
    // index_from_tree 需 tree id (非 commit id)
    let target_tree_id = repo
        .find_commit(target_id)
        .map_err(|e| map_git_err(e, "git find_commit target"))?
        .tree()
        .map_err(|e| map_git_err(e, "git target tree"))?
        .id()
        .detach();
    let previous_head = super::read::resolve_rev(repo, "HEAD")?.map(|id| id.to_string());
    let old_head_tree = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?
        .detach();

    // P1-4: Hard 模式会写工作区——支持性预检（词法界内 + mode 支持, 含非 Unix
    // symlink 整批拒绝）必须发生在任何 HEAD/ref 副作用之前: 预检失败时 HEAD、
    // symbolic ref、index 与工作树均未发生预检前修改。
    if mode == ResetMode::Hard {
        preflight_tree_materialization(repo, &target_tree_id, Some(&old_head_tree))?;
    }
    move_branch_ref(repo, target_id, "reset", author_name, author_email)?;

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
    let target_id = resolve_rev_required(repo, target, "git checkout target")?;
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

    // P1-4: overlay 逐条目也走共享安全物化器（整批预检 + 界内目录链 + leaf
    // 不跟随 + mode 如实分派）——不再 raw create_dir_all/fs::write。
    // overlay 语义保持: 不删除 target 之外的 index entry 与工作区文件。
    let mut planned = Vec::new();
    for entry in target_index.entries() {
        if entry.stage() != Stage::Unconflicted {
            continue;
        }
        let path = entry.path_in(target_backing);
        let plan =
            super::materialize::plan_index_entry(workdir, &from_bstr(path), entry.mode, entry.id)?;
        planned.push((plan, entry.stat, entry.flags, path.to_owned()));
    }
    for (plan, stat, flags, path) in &planned {
        super::materialize::materialize_planned(repo, workdir, plan)?;
        current_index.remove_entries(|_, candidate, _| candidate == path);
        current_index.dangerously_push_entry(
            *stat,
            plan.blob_id,
            *flags,
            plan.index_mode(),
            path.as_ref(),
        );
    }
    current_index.sort_entries();
    current_index.remove_tree();
    current_index
        .write(IndexWriteOptions::default())
        .map_err(|e| map_git_err(e, "git index write (checkout overlay)"))?;
    Ok(())
}

// ── revert ─────────────────────────────────────────────────────────────────────

#[derive(Debug)]
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
    let target_id = resolve_rev_required(repo, target, "git revert target")?;
    let target_tree_id = repo
        .find_commit(target_id)
        .map_err(|e| map_git_err(e, "git find_commit target"))?
        .tree()
        .map_err(|e| map_git_err(e, "git target tree"))?
        .id()
        .detach();
    let previous_head = head_id_required(repo, "git revert")?.to_string();
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

    // P1-4: 同 reset——物化预检先于 set_head_symbolic 的任何副作用。
    preflight_tree_materialization(repo, &target_tree_id, Some(&old_head_tree))?;
    set_head_symbolic(repo, &branch_full)?;
    apply_tree_to_worktree(repo, workdir, &target_tree_id, Some(&old_head_tree))?;
    Ok(())
}

/// 整批支持性预检: 对 tree 的全部 index entry 走 plan_index_entry（词法界内 +
/// mode 支持; 非 Unix 的 symlink 在此整批拒绝）, 不产生任何文件系统副作用。
fn preflight_tree_materialization(
    repo: &Repository,
    tree_id: &oid,
    old_tree_id: Option<&oid>,
) -> AppResult<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let index = repo
        .index_from_tree(tree_id)
        .map_err(|e| map_git_err(e, "git index_from_tree (preflight)"))?;
    let backing = index.path_backing();
    for entry in index.entries() {
        super::materialize::plan_index_entry(
            workdir,
            &from_bstr(entry.path_in(backing)),
            entry.mode,
            entry.id,
        )?;
    }
    preflight_stale_files(repo, workdir, &index, old_tree_id)?;
    Ok(())
}

/// Plan removals before any ref/worktree mutation; an outside parent link is an
/// error, not a warning that could leave a successful but unsafe reset.
fn preflight_stale_files(
    repo: &Repository,
    workdir: &Path,
    new_index: &gix::index::File,
    old_tree_id: Option<&oid>,
) -> AppResult<Vec<std::path::PathBuf>> {
    let Some(old_id) = old_tree_id else {
        return Ok(Vec::new());
    };
    let old_index = repo
        .index_from_tree(old_id)
        .map_err(|e| map_git_err(e, "git index_from_tree (old)"))?;
    let old_backing = old_index.path_backing();
    let mut removals = Vec::new();
    for entry in old_index.entries() {
        let path = entry.path_in(old_backing);
        if new_index
            .entry_by_path_and_stage(path, Stage::Unconflicted)
            .is_none()
        {
            let relative = from_bstr(path).into_owned();
            ensure_within_path(workdir, &relative)?;
            super::materialize::preflight_parent_chain(workdir, &relative)?;
            removals.push(relative);
        }
    }
    Ok(removals)
}

fn remove_stale_file(workdir: &Path, relative: &Path) -> AppResult<()> {
    // Re-resolve immediately before unlink. Unix captures the checked parent
    // handle so a directory replacement cannot redirect the actual deletion.
    #[cfg(unix)]
    let result = {
        let Some(parent) = crate::path_safety::ScopedParent::capture(workdir, relative, false)?
        else {
            return Ok(());
        };
        parent.remove_file()
    };
    #[cfg(not(unix))]
    let result = {
        super::materialize::preflight_parent_chain(workdir, relative)?;
        std::fs::remove_file(ensure_within_path(workdir, relative)?)
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AppError::system(format!(
            "remove stale git file {}: {error}",
            relative.display()
        ))),
    }
}

// ── 共享 helper ─────────────────────────────────────────────────────────────────

/// 移动当前分支 ref → target_id (对齐 nuwax writeRef force=true)。
/// HEAD 须是 symbolic (在某分支上); detached → BusinessError。
fn move_branch_ref(
    repo: &Repository,
    target_id: ObjectId,
    log_msg: &str,
    author_name: &str,
    author_email: &str,
) -> AppResult<()> {
    let branch_full = repo
        .head_name()
        .ok()
        .flatten()
        .ok_or_else(|| AppError::business("cannot reset/checkout in detached HEAD state"))?;
    let reference = repo
        .find_reference(branch_full.as_bstr())
        .map_err(|e| map_git_err(e, "git find_reference (current branch)"))?;
    let previous_id = reference.id().detach();
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: log_msg.into(),
            },
            expected: PreviousValue::MustExistAndMatch(Target::Object(previous_id)),
            new: Target::Object(target_id),
        },
        name: branch_full,
        deref: false,
    };
    let committer = Signature {
        name: BString::from(author_name),
        email: BString::from(author_email),
        time: Time::now_local_or_utc(),
    };
    let mut time_buf = TimeBuf::default();
    repo.edit_references_as(std::iter::once(edit), Some(committer.to_ref(&mut time_buf)))
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
    // 整批预检（FS-03/06）: 词法界内 + mode 支持全部通过后才产生任何写副作用;
    // gitlink 等未承诺对象在这里整批拒绝, 不留半写工作树。
    let mut planned = Vec::with_capacity(new_index.entries().len());
    for entry in new_index.entries() {
        let path = entry.path_in(new_backing);
        planned.push(super::materialize::plan_index_entry(
            workdir,
            &from_bstr(path),
            entry.mode,
            entry.id,
        )?);
    }
    let removals = preflight_stale_files(repo, workdir, &new_index, old_tree_id)?;
    // 写所有 target 文件到 worktree（共享安全物化器: 界内目录链 + leaf 不跟随 + mode）
    for entry in &planned {
        super::materialize::materialize_planned(repo, workdir, entry)?;
    }
    // 删除 old_tree 有但 target 没有的文件
    for relative in &removals {
        remove_stale_file(workdir, relative)?;
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

#[cfg(test)]
mod tests {
    //! ops 写操作 target 解析契约（G-B2/T-d7）：
    //! - target 缺席 = 显式 system 错误（类别保持, 不静默成功）
    //! - 坏修订表达式 = 显式 Validation
    //! - `HEAD~1`/短 hash target 能力保留

    use super::*;
    use crate::error::AppError;
    use crate::service::git::{commit_indexed, init_repo, stage_path};

    use gix::open;

    fn fixture() -> (tempfile::TempDir, Repository, String, String) {
        let dir = tempfile::tempdir().expect("create test directory");
        init_repo(dir.path(), "Test", "test@example.com").expect("init repo");
        let repo = open(dir.path()).expect("open repo");
        std::fs::write(dir.path().join("a.txt"), "v1\n").expect("write v1");
        stage_path(&repo, "a.txt").expect("stage v1");
        let c1 = commit_indexed(&repo, "c1", "Test", "test@example.com").expect("commit c1");
        std::fs::write(dir.path().join("a.txt"), "v2\n").expect("write v2");
        stage_path(&repo, "a.txt").expect("stage v2");
        let c2 = commit_indexed(&repo, "c2", "Test", "test@example.com").expect("commit c2");
        (dir, repo, c1, c2)
    }

    #[test]
    fn reset_target_missing_is_explicit_and_bad_expression_is_validation() {
        let (_dir, repo, _c1, _c2) = fixture();
        let err = reset(
            &repo,
            "no-such-ref",
            ResetMode::Mixed,
            "Test",
            "test@example.com",
        )
        .expect_err("缺 target 必须显式报错");
        assert!(matches!(err, AppError::System(..)), "{err:?}");
        let err = reset(
            &repo,
            "HEAD~x",
            ResetMode::Mixed,
            "Test",
            "test@example.com",
        )
        .expect_err("坏表达式必须显式报错");
        assert!(matches!(err, AppError::Validation(..)), "{err:?}");
    }

    #[test]
    fn reset_and_checkout_accept_revision_expression_targets() {
        let (dir, repo, c1, _c2) = fixture();
        let out = reset(&repo, "HEAD~1", ResetMode::Soft, "Test", "test@example.com")
            .expect("HEAD~1 target 必须可解析");
        assert_eq!(out.previous_head.as_deref(), Some(_c2.as_str()));
        let head = repo.head_id().expect("head").detach().to_string();
        assert_eq!(head, c1, "soft reset 后 HEAD 应停在父提交");
        let reflog = std::fs::read_to_string(dir.path().join(".git/logs/refs/heads/main"))
            .expect("soft reset should write a branch reflog");
        let reset_entry = reflog.lines().last().expect("reset reflog entry");
        assert!(
            reset_entry.contains("Test <test@example.com>") && reset_entry.ends_with("\treset"),
            "reset reflog should use the configured committer and operation: {reset_entry}"
        );

        checkout_tree(&repo, &c1[..7]).expect("短 hash checkout 必须可解析");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).expect("read a.txt"),
            "v1\n",
            "checkout 短 hash 后工作区应回到 v1"
        );
    }

    #[test]
    fn revert_missing_target_is_explicit_error() {
        let (_dir, repo, _c1, _c2) = fixture();
        let err = revert_to_commit(&repo, "no-such-ref", None, "Test", "test@example.com")
            .expect_err("缺 target 必须显式报错");
        assert!(matches!(err, AppError::System(..)), "{err:?}");
    }

    /// 事故锚点（TS 版 nativeRevertToTree，nuwax-k8s-test cId=1695139）：回退
    /// 目标之后**新增**的文件让 `git rm -- ':(literal)x'` 在 index 里无匹配
    /// → fatal pathspec did not match → revert 500 且留下半完成脏状态。
    /// Rust 树级实现（apply_tree_to_worktree 删除 target 之外的旧文件）必须
    /// 覆盖该场景：新增文件被移除、工作区干净、revert 提交完整落地。
    /// 未来若对齐 TS 的 CLI 组合实现，此测试守住该语义不回归。
    #[test]
    fn revert_removes_files_added_after_target_and_stays_clean() {
        let (dir, repo, c1, _c2) = fixture();
        // 目标（c1）之后新增的文件 b.txt。
        std::fs::write(dir.path().join("b.txt"), "added later\n").expect("write b.txt");
        stage_path(&repo, "b.txt").expect("stage b.txt");
        let c3 =
            commit_indexed(&repo, "c3 add b.txt", "Test", "test@example.com").expect("commit c3");

        let outcome = revert_to_commit(&repo, &c1, None, "Test", "test@example.com")
            .expect("revert across added files succeeds");

        assert_eq!(outcome.previous_head, c3);
        assert_eq!(outcome.target, c1);
        let commit = outcome.commit.expect("revert commit created");
        assert_eq!(repo.head_id().expect("HEAD").to_string(), commit);
        let reverted = repo
            .find_commit(ObjectId::from_hex(commit.as_bytes()).expect("revert id"))
            .expect("revert commit");
        assert_eq!(
            reverted
                .parent_ids()
                .map(|id| id.to_string())
                .collect::<Vec<_>>(),
            [c3],
            "revert must preserve history instead of resetting HEAD to the target"
        );
        let target = repo
            .find_commit(ObjectId::from_hex(c1.as_bytes()).expect("target id"))
            .expect("target commit");
        assert_eq!(
            reverted.tree().expect("revert tree").id(),
            target.tree().expect("target tree").id(),
            "the committed tree, including .gitignore, must match the target"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a.txt")).expect("a.txt exists"),
            "v1\n",
            "content restored to target"
        );
        assert!(
            !dir.path().join("b.txt").exists(),
            "file added after target must be removed from worktree"
        );
        let status = get_status(&repo).expect("status after revert");
        assert!(
            status.staged.is_empty()
                && status.created.is_empty()
                && status.modified.is_empty()
                && status.deleted.is_empty()
                && status.untracked.is_empty()
                && status.conflicted.is_empty(),
            "worktree must stay clean after revert, got {status:?}"
        );
    }
}

#[cfg(test)]
mod p14_preflight_tests {
    use super::*;
    use crate::service::git::{commit_indexed, init_repo, stage_path};
    use gix::bstr::BString;
    use gix::open;

    /// P1-4 反例(预检先于 HEAD 副作用): 目标 tree 含 gitlink(子模块) entry 时,
    /// reset --hard 必须在移动分支 ref/HEAD **之前**整批拒绝——修复前
    /// move_branch_ref 先执行, 预检失败后 HEAD 已指向新 target 而工作区未动。
    #[test]
    fn reset_hard_rejects_gitlink_before_any_head_mutation() {
        let dir = tempfile::tempdir().expect("dir");
        init_repo(dir.path(), "Test", "test@example.com").expect("init");
        let repo = open(dir.path()).expect("open");
        std::fs::write(dir.path().join("a.txt"), "v1\n").expect("file");
        stage_path(&repo, "a.txt").expect("stage");
        let c1 = commit_indexed(&repo, "c1", "Test", "test@example.com").expect("commit");

        // 在 index 不变的情况下构造含 gitlink 的 commit（借底层 tree editor）。
        let index = repo.open_index().expect("index");
        let mut editor = repo.edit_tree(repo.empty_tree().id()).expect("editor");
        let backing = index.path_backing();
        for entry in index.entries() {
            editor
                .upsert(
                    entry.path_in(backing).to_owned(),
                    gix::objs::tree::EntryKind::Blob,
                    entry.id,
                )
                .expect("upsert blob");
        }
        let base = ObjectId::from_hex(c1.as_bytes()).expect("c1 oid");
        editor
            .upsert(
                BString::from("vendor/lib"),
                gix::objs::tree::EntryKind::Commit,
                base,
            )
            .expect("upsert gitlink");
        let tree2 = editor.write().expect("tree2");
        let head = repo.head().expect("head");
        let parent = head.into_peeled_id().expect("parent").detach();
        let sig = Signature {
            name: BString::from("Test"),
            email: BString::from("t@e.com"),
            time: Time::now_local_or_utc(),
        };
        let mut buf_c = TimeBuf::default();
        let mut buf_a = TimeBuf::default();
        let c2 = repo
            .commit_as(
                sig.to_ref(&mut buf_c),
                sig.to_ref(&mut buf_a),
                "HEAD",
                "c2 gitlink",
                tree2,
                std::iter::once(parent),
            )
            .expect("commit2")
            .to_string();

        // The target must differ from HEAD, otherwise the old ordering moves
        // the ref to its existing value and this assertion cannot expose it.
        reset(&repo, &c1, ResetMode::Soft, "Test", "test@example.com").unwrap();

        let head_before = crate::service::git::read::resolve_rev(&repo, "HEAD")
            .expect("head")
            .map(|id| id.to_string())
            .expect("head exists");
        assert_eq!(head_before, c1);
        assert_ne!(head_before, c2);
        let index_before = std::fs::read(repo.index_path()).unwrap();
        let branch_before = repo
            .find_reference("HEAD")
            .expect("head ref")
            .follow()
            .expect("follow")
            .map(|target| target.id().to_string())
            .expect("branch target");

        let error = reset(&repo, &c2, ResetMode::Hard, "Test", "test@example.com")
            .expect_err("gitlink target must be rejected");

        assert!(matches!(error, AppError::Business(_)), "{error:?}");
        let head_after = crate::service::git::read::resolve_rev(&repo, "HEAD")
            .expect("head")
            .map(|id| id.to_string())
            .expect("head exists");
        assert_eq!(
            head_before, head_after,
            "HEAD must not move when preflight rejects"
        );
        let branch_after = repo
            .find_reference("HEAD")
            .expect("head ref")
            .follow()
            .expect("follow")
            .map(|target| target.id().to_string())
            .expect("branch target");
        assert_eq!(branch_before, branch_after, "branch ref must not move");
        assert_eq!(std::fs::read(repo.index_path()).unwrap(), index_before);
        assert_eq!(
            std::fs::read(dir.path().join("a.txt")).unwrap(),
            b"v1\n",
            "worktree untouched"
        );
        assert!(!dir.path().join("vendor").exists());
    }

    #[cfg(unix)]
    #[test]
    fn reset_hard_refuses_outward_parent_before_deleting_old_files() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("ws");
        std::fs::create_dir(&workdir).unwrap();
        init_repo(&workdir, "Test", "test@example.com").unwrap();
        let repo = open(&workdir).unwrap();
        std::fs::create_dir(workdir.join("sub")).unwrap();
        std::fs::write(workdir.join("sub/secret"), b"tracked old content").unwrap();
        std::fs::write(workdir.join("a.txt"), b"old").unwrap();
        stage_path(&repo, "sub/secret").unwrap();
        stage_path(&repo, "a.txt").unwrap();
        let old = commit_indexed(&repo, "old", "Test", "test@example.com").unwrap();
        std::fs::remove_file(workdir.join("sub/secret")).unwrap();
        std::fs::write(workdir.join("a.txt"), b"new").unwrap();
        stage_path(&repo, "sub/secret").unwrap();
        stage_path(&repo, "a.txt").unwrap();
        let target = commit_indexed(&repo, "target", "Test", "test@example.com").unwrap();
        reset(&repo, &old, ResetMode::Hard, "Test", "test@example.com").unwrap();

        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("secret"), b"external sentinel").unwrap();
        std::fs::remove_dir_all(workdir.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, workdir.join("sub")).unwrap();
        let index_before = std::fs::read(repo.index_path()).unwrap();
        assert_ne!(old, target);

        let result = reset(&repo, &target, ResetMode::Hard, "Test", "test@example.com");
        assert_eq!(
            std::fs::read(outside.join("secret")).unwrap(),
            b"external sentinel"
        );
        assert!(
            matches!(result, Err(AppError::Validation(_, _))),
            "{result:?}"
        );
        assert_eq!(head_id_required(&repo, "head").unwrap().to_string(), old);
        assert_eq!(std::fs::read(repo.index_path()).unwrap(), index_before);
        assert_eq!(std::fs::read(workdir.join("a.txt")).unwrap(), b"old");
        assert_eq!(std::fs::read_link(workdir.join("sub")).unwrap(), outside);
    }

    #[cfg(unix)]
    #[test]
    fn reset_hard_refuses_outward_write_parent_before_head_or_worktree_changes() {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().join("ws");
        std::fs::create_dir(&workdir).unwrap();
        init_repo(&workdir, "Test", "test@example.com").unwrap();
        let repo = open(&workdir).unwrap();
        std::fs::write(workdir.join("a.txt"), b"old").unwrap();
        stage_path(&repo, "a.txt").unwrap();
        let old = commit_indexed(&repo, "old", "Test", "test@example.com").unwrap();
        std::fs::write(workdir.join("a.txt"), b"new").unwrap();
        std::fs::create_dir(workdir.join("sub")).unwrap();
        std::fs::write(workdir.join("sub/file.txt"), b"new file").unwrap();
        stage_path(&repo, "a.txt").unwrap();
        stage_path(&repo, "sub/file.txt").unwrap();
        let target = commit_indexed(&repo, "target", "Test", "test@example.com").unwrap();
        reset(&repo, &old, ResetMode::Hard, "Test", "test@example.com").unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("sentinel"), b"external sentinel").unwrap();
        std::fs::remove_dir_all(workdir.join("sub")).unwrap();
        std::os::unix::fs::symlink(&outside, workdir.join("sub")).unwrap();
        let index_before = std::fs::read(repo.index_path()).unwrap();
        assert_ne!(old, target);

        let result = reset(&repo, &target, ResetMode::Hard, "Test", "test@example.com");
        assert!(
            matches!(result, Err(AppError::Validation(_, _))),
            "{result:?}"
        );
        assert_eq!(head_id_required(&repo, "head").unwrap().to_string(), old);
        assert_eq!(std::fs::read(repo.index_path()).unwrap(), index_before);
        assert_eq!(std::fs::read(workdir.join("a.txt")).unwrap(), b"old");
        assert!(!outside.join("file.txt").exists());
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"external sentinel"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reset_hard_deletes_through_inside_parent_and_accepts_missing_old_file() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path(), "Test", "test@example.com").unwrap();
        let repo = open(dir.path()).unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/file.txt"), b"old").unwrap();
        stage_path(&repo, "sub/file.txt").unwrap();
        let old = commit_indexed(&repo, "old", "Test", "test@example.com").unwrap();
        std::fs::remove_file(dir.path().join("sub/file.txt")).unwrap();
        stage_path(&repo, "sub/file.txt").unwrap();
        let target = commit_indexed(&repo, "target", "Test", "test@example.com").unwrap();
        reset(&repo, &old, ResetMode::Hard, "Test", "test@example.com").unwrap();
        std::fs::rename(dir.path().join("sub"), dir.path().join("inside")).unwrap();
        std::os::unix::fs::symlink("inside", dir.path().join("sub")).unwrap();

        reset(&repo, &target, ResetMode::Hard, "Test", "test@example.com").unwrap();
        assert!(!dir.path().join("inside/file.txt").exists());
        assert!(
            std::fs::symlink_metadata(dir.path().join("sub"))
                .unwrap()
                .is_symlink()
        );
        reset(&repo, &old, ResetMode::Hard, "Test", "test@example.com").unwrap();
        std::fs::remove_file(dir.path().join("inside/file.txt")).unwrap();
        reset(&repo, &target, ResetMode::Hard, "Test", "test@example.com").unwrap();
        assert!(!dir.path().join("inside/file.txt").exists());
    }

    /// P1-4: checkout overlay 走共享物化器——工作区已有外向目录链接时, 目标
    /// tree 中该前缀下的文件必须被拒绝, 不再沿链接写出界。
    #[cfg(unix)]
    #[test]
    fn checkout_overlay_rejects_outward_directory_link_via_shared_materializer() {
        let dir = tempfile::tempdir().expect("dir");
        let workdir = dir.path().join("ws");
        std::fs::create_dir_all(&workdir).expect("ws");
        init_repo(&workdir, "Test", "test@example.com").expect("init");
        let repo = open(&workdir).expect("open");
        std::fs::write(workdir.join("a.txt"), "v1\n").expect("file");
        stage_path(&repo, "a.txt").expect("stage");
        let c1 = commit_indexed(&repo, "c1", "Test", "test@example.com").expect("commit");
        // 第二个提交在 sub/ 下放文件（overlay 目标）。
        std::fs::create_dir_all(workdir.join("sub")).expect("sub");
        std::fs::write(workdir.join("sub/file.txt"), "v2\n").expect("sub file");
        stage_path(&repo, "sub/file.txt").expect("stage sub");
        let c2 = commit_indexed(&repo, "c2", "Test", "test@example.com").expect("commit2");
        // 把 sub 变成指向工作区外的目录链接（工作区父目录 = 真正外向）。
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).expect("outside");
        std::fs::write(outside.join("sentinel"), b"KEEP").expect("sentinel");
        std::fs::remove_dir_all(workdir.join("sub")).expect("remove sub");
        std::os::unix::fs::symlink(&outside, workdir.join("sub")).expect("outward link");

        let error = checkout_tree(&repo, &c2).expect_err("overlay must refuse the link path");
        assert!(
            matches!(error, AppError::Validation(..)),
            "expected validation, got: {error:?}"
        );
        assert!(
            !outside.join("file.txt").exists(),
            "no file may be written through the link"
        );
        assert_eq!(
            std::fs::read(outside.join("sentinel")).unwrap(),
            b"KEEP",
            "outside content intact"
        );
        // 非 sub 前缀的条目照常物化（overlay 语义保持）。
        assert_eq!(
            std::fs::read_to_string(workdir.join("a.txt")).unwrap(),
            "v1\n"
        );
        drop(c1);
    }
}
