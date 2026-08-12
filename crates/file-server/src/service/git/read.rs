//! Git 读操作 (status/log/branches/tags/file-content)。

use crate::error::{AppError, AppResult};

use super::{map_git_err, shorten_ref};

use gix::diff::index::ChangeRef as IndexChange;
use gix::progress::Discard;
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::{Item as StatusItem, UntrackedFiles};
use gix::{Commit, Repository};

/// 列本地分支 + 当前分支名 (对齐 nuwax listBranches + currentBranch)。
pub fn list_branches(repo: &Repository) -> AppResult<(Vec<String>, Option<String>)> {
    let current = repo
        .head_name()
        .ok()
        .flatten()
        .and_then(|n| shorten_ref(&n.to_string()));
    let mut branches = Vec::new();
    let refs = repo
        .references()
        .map_err(|e| map_git_err(e, "git references"))?;
    let iter = refs
        .local_branches()
        .map_err(|e| map_git_err(e, "git local_branches"))?;
    for r in iter {
        let r = r.map_err(|e| map_git_err(e, "git ref iter"))?;
        let name = r.name().as_bstr().to_string();
        if let Some(short) = shorten_ref(&name) {
            branches.push(short);
        }
    }
    Ok((branches, current))
}

/// 列标签 (对齐 nuwax listTags)。
pub fn list_tags(repo: &Repository) -> AppResult<Vec<String>> {
    let mut tags = Vec::new();
    let refs = repo
        .references()
        .map_err(|e| map_git_err(e, "git references"))?;
    let iter = refs.tags().map_err(|e| map_git_err(e, "git tags"))?;
    for r in iter {
        let r = r.map_err(|e| map_git_err(e, "git tag iter"))?;
        let name = r.name().as_bstr().to_string();
        if let Some(short) = shorten_ref(&name) {
            tags.push(short);
        }
    }
    Ok(tags)
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitInfo {
    pub hash: String,
    pub date: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

/// 提交历史 (对齐 nuwax logHistory; first-parent)。
/// `branch` 非空 → 从该 ref 起 walk (对齐 nuwax git.log({ ref: branch })); 默认 HEAD。
///
/// 仓库刚 init 尚无任何 commit 时 (HEAD 无法解析) → 返回空列表 (对齐 TS d1e5c8a)。
pub fn log_history(
    repo: &Repository,
    max_count: usize,
    skip: usize,
    branch: Option<&str>,
    file_path: Option<&str>,
) -> AppResult<Vec<CommitInfo>> {
    // 解析起始 ref; 失败时若是"无 commit"场景 (空仓库), 返回空列表而非报错。
    let start_id = match branch {
        Some(b) if !b.trim().is_empty() => match repo.rev_parse_single(b) {
            Ok(id) => id.detach(),
            Err(e) => {
                if is_no_commit_error(&e) {
                    return Ok(Vec::new());
                }
                return Err(map_git_err(e, "git rev_parse branch"));
            }
        },
        _ => match repo.head_id() {
            Ok(id) => id.detach(),
            Err(e) => {
                if is_no_commit_error(&e) {
                    return Ok(Vec::new());
                }
                return Err(map_git_err(e, "git head_id"));
            }
        },
    };
    let walk = repo
        .rev_walk([start_id])
        .first_parent_only()
        .all()
        .map_err(|e| map_git_err(e, "git walk all"))?;
    let mut commits = Vec::new();
    let mut seen = 0usize;
    for info in walk {
        let info = info.map_err(|e| map_git_err(e, "git walk item"))?;
        let commit = info
            .object()
            .map_err(|e| map_git_err(e, "git commit object"))?;
        if let Some(path) = file_path.filter(|p| !p.trim().is_empty())
            && !commit_changes_path(repo, &commit, path)?
        {
            continue;
        }
        if seen < skip {
            seen += 1;
            continue;
        }
        if commits.len() >= max_count {
            break;
        }
        let mut message = commit
            .message_raw()
            .map_err(|e| map_git_err(e, "git message"))?
            .to_string();
        // isomorphic-git 的 `readCommit()` 会把提交消息作为完整消息段返回，
        // 其序列化的 commit message 末尾带换行。API 保留该可见格式。
        if !message.ends_with('\n') {
            message.push('\n');
        }
        let author = commit.author().map_err(|e| map_git_err(e, "git author"))?;
        let secs = commit
            .time()
            .map_err(|e| map_git_err(e, "git commit time"))?
            .seconds;
        commits.push(CommitInfo {
            hash: info.id().to_string(),
            date: iso_from_secs(secs),
            message,
            author_name: author.name.to_string(),
            author_email: author.email.to_string(),
        });
    }
    Ok(commits)
}

fn commit_changes_path(repo: &Repository, commit: &Commit<'_>, path: &str) -> AppResult<bool> {
    let tree = commit
        .tree()
        .map_err(|e| map_git_err(e, "git commit tree"))?;
    let current = tree
        .lookup_entry_by_path(path)
        .map_err(|e| map_git_err(e, "git lookup path in commit"))?
        .map(|entry| (entry.id().detach(), entry.mode()));
    let parent = match commit.parent_ids().next() {
        Some(parent_id) => {
            let parent_tree = repo
                .find_commit(parent_id)
                .map_err(|e| map_git_err(e, "git find parent"))?
                .tree()
                .map_err(|e| map_git_err(e, "git parent tree"))?;
            parent_tree
                .lookup_entry_by_path(path)
                .map_err(|e| map_git_err(e, "git lookup path in parent"))?
                .map(|entry| (entry.id().detach(), entry.mode()))
        }
        None => None,
    };
    Ok(current != parent)
}

fn iso_from_secs(secs: i64) -> String {
    // 对齐 nuwax `new Date(ts*1000).toISOString()` → "YYYY-MM-DDTHH:mm:ss.sssZ" (毫秒 + UTC Z)
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
        .unwrap_or_default()
}

/// 读 ref 处的文件内容 (对齐 nuwax fileContent)。
pub fn file_content_at_ref(
    repo: &Repository,
    ref_spec: &str,
    file_path: &str,
    max_bytes: u64,
) -> AppResult<Option<String>> {
    let oid = repo
        .rev_parse_single(ref_spec)
        .map_err(|e| map_git_err(e, "git rev_parse"))?;
    let commit = repo
        .find_commit(oid)
        .map_err(|e| map_git_err(e, "git find_commit"))?;
    let tree = commit.tree().map_err(|e| map_git_err(e, "git tree"))?;
    match tree
        .lookup_entry_by_path(file_path)
        .map_err(|e| map_git_err(e, "git lookup_entry_by_path"))?
    {
        Some(entry) => {
            let size = repo
                .find_header(entry.id())
                .map_err(|e| map_git_err(e, "git find file-content header"))?
                .size();
            if size > max_bytes {
                return Err(AppError::validation(format!(
                    "git file content exceeds limit (max {max_bytes} bytes)"
                )));
            }
            let blob = repo
                .find_blob(entry.id())
                .map_err(|e| map_git_err(e, "git find_blob"))?;
            Ok(Some(String::from_utf8_lossy(&blob.data).into_owned()))
        }
        None => Ok(None),
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StatusResult {
    pub current: Option<String>,
    pub staged: Vec<String>,
    pub modified: Vec<String>,
    pub created: Vec<String>,
    pub deleted: Vec<String>,
    pub untracked: Vec<String>,
}

/// 工作区状态 (对齐 nuwax status; gix status platform 折叠到 5-bucket)。
/// IndexWorktree Modification 暂统一归 modified (worktree delete 细分需 gix_status EntryStatus)。
pub fn get_status(repo: &Repository) -> AppResult<StatusResult> {
    let current = repo
        .head_name()
        .ok()
        .flatten()
        .and_then(|n| shorten_ref(&n.to_string()));
    let mut r = StatusResult {
        current,
        staged: vec![],
        modified: vec![],
        created: vec![],
        deleted: vec![],
        untracked: vec![],
    };
    let mut iter = repo
        .status(Discard)
        .map_err(|e| map_git_err(e, "git status"))?
        .untracked_files(UntrackedFiles::Files)
        .into_iter(None)
        .map_err(|e| map_git_err(e, "git status into_iter"))?;
    while let Some(item) = iter
        .next()
        .transpose()
        .map_err(|e| map_git_err(e, "git status item"))?
    {
        match item {
            StatusItem::TreeIndex(change) => {
                let (loc, is_add, is_del) = match &change {
                    IndexChange::Addition { location, .. } => (location, true, false),
                    IndexChange::Deletion { location, .. } => (location, false, true),
                    IndexChange::Modification { location, .. } => (location, false, false),
                    IndexChange::Rewrite { location, .. } => (location, false, false),
                };
                let s = loc.to_string();
                r.staged.push(s.clone());
                if is_add {
                    r.created.push(s);
                } else if is_del {
                    r.deleted.push(s);
                }
            }
            StatusItem::IndexWorktree(change) => match change {
                WorktreeItem::Modification { rela_path, .. } => {
                    // workdir 文件不存在 → workdir 删除归 deleted, 否则 modified
                    // (对齐 nuwax W===0&&S!==0 → deleted 桶)
                    let s = rela_path.to_string();
                    let deleted = repo
                        .workdir()
                        .map(|w| !w.join(&s).exists())
                        .unwrap_or(false);
                    if deleted {
                        r.deleted.push(s);
                    } else {
                        r.modified.push(s);
                    }
                }
                WorktreeItem::DirectoryContents { entry, .. } => {
                    r.untracked.push(entry.rela_path.to_string());
                }
                _ => {}
            },
        }
    }
    for v in [
        &mut r.staged,
        &mut r.modified,
        &mut r.created,
        &mut r.deleted,
        &mut r.untracked,
    ] {
        v.sort();
        v.dedup();
    }
    Ok(r)
}

/// 判断 gix 错误是否为"空仓库无 commit" (HEAD 无法解析)。
/// 对齐 TS d1e5c8a: hasGitHead 预检 + catch 兜底 "does not have any commits"。
fn is_no_commit_error(e: &impl std::fmt::Display) -> bool {
    let msg = e.to_string().to_lowercase();
    msg.contains("does not have any commits yet")
        || msg.contains("unborn")
        || msg.contains("could not find")
        || msg.contains("not found")
}
