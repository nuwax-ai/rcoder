//! Git 写操作 (stage/commit)。

use std::collections::HashSet;
use std::path::Path;

use gix::Repository;
use gix::actor::Signature;
use gix::bstr::{BStr, BString};
use gix::date::{Time, parse::TimeBuf};
use gix::hash::ObjectId;
use gix::index::{
    entry::{Flags, Mode as IndexMode, Stage, Stat},
    write::Options as IndexWriteOptions,
};
use gix::object::tree::EntryKind as TreeEntryKind;
use gix::objs::Write;
use gix::path::into_bstr;

use crate::error::{AppError, AppResult};

use super::map_git_err;
use super::read::get_status;

fn index_bstr_path(worktree_path: &str) -> BString {
    into_bstr(std::path::PathBuf::from(worktree_path)).into_owned()
}

/// stage 单个路径: 文件存在 → 更新/新增 index entry; 不存在 → stage 删除 (对齐 nuwax stageFiles)。
pub fn stage_path(repo: &Repository, worktree_path: &str) -> AppResult<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let abs = crate::path_safety::ensure_within(workdir, worktree_path)?;
    if std::fs::symlink_metadata(&abs).is_ok_and(|metadata| metadata.is_dir()) {
        let mut files = Vec::new();
        collect_stage_paths(workdir, &abs, &mut files)?;
        for path in files {
            stage_file(repo, &path)?;
        }
        return Ok(());
    }
    stage_file(repo, worktree_path)
}

fn collect_stage_paths(workdir: &Path, dir: &Path, out: &mut Vec<String>) -> AppResult<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if entry.file_name() != ".git" {
                collect_stage_paths(workdir, &path, out)?;
            }
        } else {
            let relative = path
                .strip_prefix(workdir)
                .map_err(|_| AppError::validation("git path is outside worktree"))?;
            out.push(relative.to_string_lossy().into_owned());
        }
    }
    Ok(())
}

fn stage_file(repo: &Repository, worktree_path: &str) -> AppResult<()> {
    let mut index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let abs = crate::path_safety::ensure_within(workdir, worktree_path)?;
    let bstr_owned = index_bstr_path(worktree_path);
    let bstr_path: &BStr = bstr_owned.as_ref();
    let metadata = std::fs::symlink_metadata(&abs).ok();
    if let Some(metadata) = metadata {
        let (blob_id, mode) = if metadata.file_type().is_symlink() {
            let target = std::fs::read_link(&abs)?;
            let data = into_bstr(target).into_owned();
            let id = repo
                .write_blob(data.as_slice())
                .map_err(|e| map_git_err(e, "git write symlink blob"))?
                .detach();
            (id, IndexMode::SYMLINK)
        } else if metadata.is_file() {
            #[cfg(unix)]
            let executable = {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            };
            #[cfg(not(unix))]
            let executable = false;
            let mode = if executable {
                IndexMode::FILE_EXECUTABLE
            } else {
                IndexMode::FILE
            };
            // Repository::write_blob_stream() 在 gix 0.85 中仍先聚合到内存；
            // 直接写公开 ODB handle 才会流式压缩并落到 loose object。
            let mut file = std::fs::File::open(&abs)?;
            let id = repo
                .objects
                .write_stream(gix::objs::Kind::Blob, metadata.len(), &mut file)
                .map_err(|e| map_git_err(e, "git stream worktree blob"))?;
            (id, mode)
        } else {
            return Err(AppError::validation(format!(
                "unsupported file type: {worktree_path}"
            )));
        };
        let stat = Stat::default();
        if let Some(e) = index.entry_mut_by_path_and_stage(bstr_path, Stage::Unconflicted) {
            e.id = blob_id;
            e.stat = stat;
            e.mode = mode;
        } else {
            index.dangerously_push_entry(stat, blob_id, Flags::empty(), mode, bstr_path);
            index.sort_entries();
        }
    } else {
        index.remove_entries(|_, p, _| p == bstr_path);
        index.sort_entries();
    }
    index.remove_tree();
    index
        .write(IndexWriteOptions::default())
        .map_err(|e| map_git_err(e, "git index write"))?;
    Ok(())
}

/// stage 多个文件 (对齐 nuwax stageFiles; files 为空则 addAll: 基于 status stage 全部变更)。
pub fn stage_files(repo: &Repository, files: &[String]) -> AppResult<()> {
    if !files.is_empty() {
        for f in files {
            stage_path(repo, f)?;
        }
        return Ok(());
    }
    let st = get_status(repo)?;
    for f in st.modified.iter().chain(st.untracked.iter()) {
        stage_path(repo, f)?;
    }
    for f in st.deleted.iter() {
        let abs = repo
            .workdir()
            .ok_or_else(|| AppError::system("git repo has no workdir"))?
            .join(f);
        if !abs.exists() {
            stage_path(repo, f)?;
        }
    }
    Ok(())
}

/// 提交当前 index。新 tree 完全由 index 构造，确保暂存删除不会被 HEAD tree 带回。
pub fn commit_indexed(
    repo: &Repository,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> AppResult<String> {
    let index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let mut editor = repo
        .edit_tree(repo.empty_tree().id())
        .map_err(|e| map_git_err(e, "git edit_tree"))?;
    let path_backing = index.path_backing();
    for entry in index.entries() {
        if entry.stage() != Stage::Unconflicted {
            continue;
        }
        let path = entry.path_in(path_backing);
        let kind = match entry.mode {
            IndexMode::FILE => TreeEntryKind::Blob,
            IndexMode::FILE_EXECUTABLE => TreeEntryKind::BlobExecutable,
            IndexMode::SYMLINK => TreeEntryKind::Link,
            IndexMode::COMMIT => TreeEntryKind::Commit,
            _ => continue,
        };
        editor
            .upsert(path, kind, entry.id)
            .map_err(|e| map_git_err(e, "git editor upsert"))?;
    }
    let tree_id = editor
        .write()
        .map_err(|e| map_git_err(e, "git editor write"))?;
    let parents: Vec<ObjectId> = match repo.head_id() {
        Ok(id) => vec![id.detach()],
        Err(_) => vec![],
    };
    let sig = Signature {
        name: BString::from(author_name),
        email: BString::from(author_email),
        time: Time::now_local_or_utc(),
    };
    let mut buf_c = TimeBuf::default();
    let mut buf_a = TimeBuf::default();
    let commit_id = repo
        .commit_as(
            sig.to_ref(&mut buf_c),
            sig.to_ref(&mut buf_a),
            "HEAD",
            message,
            tree_id,
            parents.iter().copied(),
        )
        .map_err(|e| map_git_err(e, "git commit"))?;
    Ok(commit_id.to_string())
}

// ── init / unstage / discard ───────────────────────────────────────────────────

/// 初始化仓库 (对齐 nuwax init): 已存在返回 already=true; 否则 init + .gitignore + initial commit。
pub fn init_repo(path: &Path, author_name: &str, author_email: &str) -> AppResult<bool> {
    let already = super::is_git_repo(path);
    let repo = super::ensure_repo(path)?;
    super::ensure_gitignore(path)?;
    if !already {
        stage_path(&repo, ".gitignore")?;
        if let Err(e) = commit_indexed(&repo, "Initial commit", author_name, author_email) {
            tracing::warn!(error = %e, "initial commit failed (best-effort, skipping)");
        }
    }
    Ok(already)
}

/// 公开 `/api/git/init` 使用：只初始化仓库并维护 .gitignore，不创建提交。
pub fn init_repo_only(path: &Path) -> AppResult<bool> {
    let already = super::is_git_repo(path);
    let _repo = super::ensure_repo(path)?;
    super::ensure_gitignore(path)?;
    Ok(already)
}

/// 初始化 (幂等) + stage 全部变更 + 提交 (对齐 nuwax gitService.init + commit 组合,
/// 用于 createProject/copyProject/uploadProject 在 GIT_ENABLED 下首次落地工作区)。
/// 与 [`init_repo`] 区别: 总是 stage all + 用自定义 message 提交 (非 "Initial commit")。
pub fn init_and_commit(
    path: &Path,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> AppResult<()> {
    let repo = super::ensure_repo(path)?;
    super::ensure_gitignore(path)?;
    // stage 全部变更 (addAll: modified + untracked + deleted)
    stage_files(&repo, &[])?;
    // 提交 (无变更也会产生提交, 与 nuwax 一致; commit 失败 best-effort 不阻断业务)
    if let Err(e) = commit_indexed(&repo, message, author_name, author_email) {
        tracing::warn!(error = %e, "commit failed (best-effort, skipping)");
    }
    Ok(())
}

/// unstage (git restore --staged): HEAD 有路径则恢复其 entry，HEAD 无路径则从 index 移除。
pub fn unstage_files(repo: &Repository, files: &[String]) -> AppResult<()> {
    let head_tree = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?
        .detach();
    if files.is_empty() {
        let mut index = repo
            .index_from_tree(&head_tree)
            .map_err(|e| map_git_err(e, "git index_from_tree (unstage all)"))?;
        index.remove_tree();
        index
            .write(IndexWriteOptions::default())
            .map_err(|e| map_git_err(e, "git index write"))?;
        return Ok(());
    }

    let mut index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let head_index = repo
        .index_from_tree(&head_tree)
        .map_err(|e| map_git_err(e, "git index_from_tree (unstage)"))?;
    let head_backing = head_index.path_backing();
    let paths: Vec<BString> = files.iter().map(|f| index_bstr_path(f)).collect();
    for p in &paths {
        let bp: &BStr = p.as_ref();
        index.remove_entries(|_, x, _| x == bp);
        if let Some(entry) = head_index.entry_by_path_and_stage(bp, Stage::Unconflicted) {
            index.dangerously_push_entry(
                entry.stat,
                entry.id,
                entry.flags,
                entry.mode,
                entry.path_in(head_backing),
            );
        }
    }
    index.sort_entries();
    index.remove_tree();
    index
        .write(IndexWriteOptions::default())
        .map_err(|e| map_git_err(e, "git index write"))?;
    Ok(())
}

/// discard 分桶结果 (对齐 nuwax discard 响应明细)。
#[derive(Debug, Default, Clone)]
pub struct DiscardBuckets {
    pub tracked_files: Vec<String>,
    pub new_files: Vec<String>,
    pub untracked_files: Vec<String>,
}

impl DiscardBuckets {
    pub fn len(&self) -> usize {
        self.tracked_files.len() + self.new_files.len() + self.untracked_files.len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// discard (对齐 nuwax discard): files 空 → discard 全部 modified/staged/deleted。
/// - HEAD 有该文件 (tracked, modified/deleted) → 恢复 worktree 到 HEAD + `stage_path` 同步 index
/// - HEAD 无 (untracked/staged-new) → 删除 worktree + `stage_path` 同步 index (移除 staged-new entry)
///
/// 返回三桶明细 (trackedFiles / newFiles / untrackedFiles), 与 nuwax 一致。
pub fn discard_files(repo: &Repository, files: &[String]) -> AppResult<DiscardBuckets> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let head_tree_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?;
    let tree = repo
        .find_tree(head_tree_id)
        .map_err(|e| map_git_err(e, "git find_tree"))?;
    let st = get_status(repo)?;
    let staged_set: HashSet<String> = st.staged.iter().cloned().collect();
    let to_discard: Vec<String> = if files.is_empty() {
        // 空 files → discard 全部变更 (对齐 nuwax discard: tracked-modified/deleted + staged-new
        // + **untracked**; 漏 untracked 会导致 discard-all 后仍有未跟踪文件残留)
        st.modified
            .iter()
            .chain(st.deleted.iter())
            .chain(st.staged.iter())
            .chain(st.untracked.iter())
            .cloned()
            .collect()
    } else {
        files.to_vec()
    };
    let mut buckets = DiscardBuckets::default();
    for f in &to_discard {
        // 安全：用户可控的 files 必须在任何 fs 操作前做路径校验，拒绝 `..` / 绝对路径穿越。
        // stage_path 内部的 ensure_within 发生在 write/remove 之后（太晚），故在此先行拦截。
        let abs = crate::path_safety::ensure_within(workdir, f)?;
        match tree
            .lookup_entry_by_path(f)
            .map_err(|e| map_git_err(e, "git lookup_entry_by_path"))?
        {
            Some(entry) => {
                // tracked (modified/deleted): 恢复 worktree 到 HEAD 内容 + 同步 index
                let blob = repo
                    .find_blob(entry.id())
                    .map_err(|e| map_git_err(e, "git find_blob"))?;
                if let Some(p) = abs.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::write(&abs, &blob.data)?;
                stage_path(repo, f)?;
                buckets.tracked_files.push(f.clone());
            }
            None => {
                // untracked / staged-new: 删 worktree + 同步 index (移除 staged-new entry)
                if let Err(e) = std::fs::remove_file(&abs) {
                    tracing::warn!(error = %e, "remove untracked worktree file failed (skipping)");
                }
                stage_path(repo, f)?;
                if staged_set.contains(f) {
                    buckets.new_files.push(f.clone());
                } else {
                    buckets.untracked_files.push(f.clone());
                }
            }
        }
    }
    Ok(buckets)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::open;

    struct TestRepo(std::path::PathBuf);

    impl TestRepo {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "file-server-git-test-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test repo");
            init_repo(&path, "Test", "test@example.com").expect("init test repo");
            Self(path)
        }

        fn open(&self) -> Repository {
            open(&self.0).expect("open test repo")
        }
    }

    impl Drop for TestRepo {
        fn drop(&mut self) {
            drop(std::fs::remove_dir_all(&self.0));
        }
    }

    fn commit_file(test: &TestRepo, path: &str, data: &str, message: &str) -> String {
        std::fs::write(test.0.join(path), data).expect("write fixture");
        let repo = test.open();
        stage_path(&repo, path).expect("stage fixture");
        commit_indexed(&repo, message, "Test", "test@example.com").expect("commit fixture")
    }

    #[test]
    fn commit_tree_honours_staged_deletion() {
        let test = TestRepo::new();
        commit_file(&test, "delete-me.txt", "one", "add file");
        std::fs::remove_file(test.0.join("delete-me.txt")).expect("remove fixture");
        let repo = test.open();
        stage_path(&repo, "delete-me.txt").expect("stage deletion");
        commit_indexed(&repo, "delete file", "Test", "test@example.com").expect("commit deletion");
        let tree = repo.head_tree().expect("head tree");
        assert!(
            tree.lookup_entry_by_path("delete-me.txt")
                .expect("tree lookup")
                .is_none(),
            "deleted index entry must not be copied back from HEAD"
        );
    }

    #[test]
    fn discard_restores_tracked_file_from_head() {
        let test = TestRepo::new();
        commit_file(&test, "tracked.txt", "head", "add tracked");
        std::fs::write(test.0.join("tracked.txt"), "modified").expect("modify fixture");
        let repo = test.open();
        let buckets = discard_files(&repo, &["tracked.txt".to_string()]).expect("discard tracked");
        assert_eq!(buckets.tracked_files, vec!["tracked.txt".to_string()]);
        // worktree 恢复到 HEAD 内容
        assert_eq!(
            std::fs::read_to_string(test.0.join("tracked.txt")).unwrap(),
            "head"
        );
    }

    #[test]
    fn discard_rejects_path_traversal() {
        let test = TestRepo::new();
        commit_file(&test, "tracked.txt", "head", "add tracked");
        let repo = test.open();
        // 穿越路径必须在任何 fs 操作前被 ensure_within 拦截
        let outside = test.0.parent().unwrap().join("evil-outside.txt");
        drop(std::fs::remove_file(&outside));
        let result = discard_files(&repo, &["../evil-outside.txt".to_string()]);
        assert!(
            result.is_err(),
            "path traversal must be rejected before any fs op"
        );
        assert!(!outside.exists(), "no fs side effect outside workdir");
    }

    #[test]
    fn unstage_restores_tracked_entry_from_head() {
        let test = TestRepo::new();
        commit_file(&test, "tracked.txt", "head", "add tracked");
        std::fs::write(test.0.join("tracked.txt"), "worktree").expect("modify fixture");
        let repo = test.open();
        stage_path(&repo, "tracked.txt").expect("stage modification");
        unstage_files(&repo, &["tracked.txt".to_string()]).expect("unstage modification");

        let index = repo.open_index().expect("open index");
        let index_entry = index
            .entry_by_path_and_stage("tracked.txt".into(), Stage::Unconflicted)
            .expect("tracked index entry");
        let head_entry = repo
            .head_tree()
            .expect("head tree")
            .lookup_entry_by_path("tracked.txt")
            .expect("head lookup")
            .expect("tracked head entry");
        assert_eq!(index_entry.id, head_entry.id().detach());
        assert_eq!(
            std::fs::read_to_string(test.0.join("tracked.txt")).expect("read worktree"),
            "worktree",
            "unstage must not modify the worktree"
        );
    }

    #[test]
    fn checkout_overlay_preserves_non_target_index_entries() {
        let test = TestRepo::new();
        let target = commit_file(&test, "a.txt", "a1", "add a");
        commit_file(&test, "b.txt", "b1", "add b");
        let repo = test.open();
        super::super::ops::checkout_tree(&repo, &target).expect("checkout old tree");
        let index = repo.open_index().expect("open index");
        assert!(
            index
                .entry_by_path_and_stage("b.txt".into(), Stage::Unconflicted)
                .is_some(),
            "checkout overlay must not stage deletion of paths absent from target"
        );
    }
}
