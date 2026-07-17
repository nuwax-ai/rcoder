//! Git 写操作 (stage/commit)。

use std::collections::HashSet;

use crate::error::{AppError, AppResult};

use super::map_git_err;
use super::read::get_status;

fn index_bstr_path(worktree_path: &str) -> gix::bstr::BString {
    gix::path::into_bstr(std::path::PathBuf::from(worktree_path)).into_owned()
}

/// stage 单个路径: 文件存在 → 更新/新增 index entry; 不存在 → stage 删除 (对齐 nuwax stageFiles)。
pub fn stage_path(repo: &gix::Repository, worktree_path: &str) -> AppResult<()> {
    let mut index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| AppError::system("git repo has no workdir"))?;
    let abs = workdir.join(worktree_path);
    let bstr_owned = index_bstr_path(worktree_path);
    let bstr_path: &gix::bstr::BStr = bstr_owned.as_ref();
    if abs.exists() {
        let blob_id = repo
            .write_blob(std::fs::read(&abs)?)
            .map_err(|e| map_git_err(e, "git write_blob"))?
            .detach();
        let stat = gix::index::entry::Stat::default();
        if let Some(e) =
            index.entry_mut_by_path_and_stage(bstr_path, gix::index::entry::Stage::Unconflicted)
        {
            e.id = blob_id;
            e.stat = stat;
            e.mode = gix::index::entry::Mode::FILE;
        } else {
            index.dangerously_push_entry(
                stat,
                blob_id,
                gix::index::entry::Flags::empty(),
                gix::index::entry::Mode::FILE,
                bstr_path,
            );
            index.sort_entries();
        }
    } else {
        index.remove_entries(|_, p, _| p == bstr_path);
        index.sort_entries();
    }
    index.remove_tree();
    index
        .write(gix::index::write::Options::default())
        .map_err(|e| map_git_err(e, "git index write"))?;
    Ok(())
}

/// stage 多个文件 (对齐 nuwax stageFiles; files 为空则 addAll: 基于 status stage 全部变更)。
pub fn stage_files(repo: &gix::Repository, files: &[String]) -> AppResult<()> {
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

/// 提交当前 index (edit_tree: HEAD tree + index entries upsert → tree → commit_as)。
pub fn commit_indexed(
    repo: &gix::Repository,
    message: &str,
    author_name: &str,
    author_email: &str,
) -> AppResult<String> {
    let index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let head_tree_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| map_git_err(e, "git head_tree_id_or_empty"))?;
    let mut editor = repo
        .edit_tree(head_tree_id)
        .map_err(|e| map_git_err(e, "git edit_tree"))?;
    let path_backing = index.path_backing();
    for entry in index.entries() {
        if entry.stage() != gix::index::entry::Stage::Unconflicted {
            continue;
        }
        let path = entry.path_in(path_backing);
        let kind = match entry.mode {
            gix::index::entry::Mode::FILE => gix::object::tree::EntryKind::Blob,
            gix::index::entry::Mode::FILE_EXECUTABLE => {
                gix::object::tree::EntryKind::BlobExecutable
            }
            gix::index::entry::Mode::SYMLINK => gix::object::tree::EntryKind::Link,
            _ => continue,
        };
        editor
            .upsert(path, kind, entry.id)
            .map_err(|e| map_git_err(e, "git editor upsert"))?;
    }
    let tree_id = editor
        .write()
        .map_err(|e| map_git_err(e, "git editor write"))?;
    let parents: Vec<gix::hash::ObjectId> = match repo.head_id() {
        Ok(id) => vec![id.detach()],
        Err(_) => vec![],
    };
    let sig = gix::actor::Signature {
        name: gix::bstr::BString::from(author_name),
        email: gix::bstr::BString::from(author_email),
        time: gix::date::Time::now_local_or_utc(),
    };
    let mut buf_c = gix::date::parse::TimeBuf::default();
    let mut buf_a = gix::date::parse::TimeBuf::default();
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

use std::path::Path;

/// 初始化仓库 (对齐 nuwax init): 已存在返回 already=true; 否则 init + .gitignore + initial commit。
pub fn init_repo(path: &Path, author_name: &str, author_email: &str) -> AppResult<bool> {
    let already = super::is_git_repo(path);
    let repo = super::ensure_repo(path)?;
    super::ensure_gitignore(path)?;
    if !already {
        stage_path(&repo, ".gitignore")?;
        let _ = commit_indexed(&repo, "Initial commit", author_name, author_email);
    }
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
    let _ = commit_indexed(&repo, message, author_name, author_email);
    Ok(())
}

/// unstage (对齐 nuwax unstage): files 空 → unstage 全部 staged; 否则逐个从 index 移除。
pub fn unstage_files(repo: &gix::Repository, files: &[String]) -> AppResult<()> {
    let mut index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git open_index"))?;
    let paths: Vec<gix::bstr::BString> = if files.is_empty() {
        let st = get_status(repo)?;
        st.staged.iter().map(|f| index_bstr_path(f)).collect()
    } else {
        files.iter().map(|f| index_bstr_path(f)).collect()
    };
    for p in &paths {
        let bp: &gix::bstr::BStr = p.as_ref();
        index.remove_entries(|_, x, _| x == bp);
    }
    index.sort_entries();
    index.remove_tree();
    index
        .write(gix::index::write::Options::default())
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
pub fn discard_files(repo: &gix::Repository, files: &[String]) -> AppResult<DiscardBuckets> {
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
        st.modified
            .iter()
            .chain(st.deleted.iter())
            .chain(st.staged.iter())
            .cloned()
            .collect()
    } else {
        files.to_vec()
    };
    let mut buckets = DiscardBuckets::default();
    for f in &to_discard {
        match tree
            .lookup_entry_by_path(f)
            .map_err(|e| map_git_err(e, "git lookup_entry_by_path"))?
        {
            Some(entry) => {
                // tracked (modified/deleted): 恢复 worktree 到 HEAD 内容 + 同步 index
                let blob = repo
                    .find_blob(entry.id())
                    .map_err(|e| map_git_err(e, "git find_blob"))?;
                let dest = workdir.join(f);
                if let Some(p) = dest.parent() {
                    let _ = std::fs::create_dir_all(p);
                }
                std::fs::write(&dest, &blob.data)?;
                stage_path(repo, f)?;
                buckets.tracked_files.push(f.clone());
            }
            None => {
                // untracked / staged-new: 删 worktree + 同步 index (移除 staged-new entry)
                let _ = std::fs::remove_file(workdir.join(f));
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
