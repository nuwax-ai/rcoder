//! Git 读操作 (status/log/branches/tags/file-content)。

use crate::error::AppResult;

use super::{map_git_err, shorten_ref};

/// 列本地分支 + 当前分支名 (对齐 nuwax listBranches + currentBranch)。
pub fn list_branches(repo: &gix::Repository) -> AppResult<(Vec<String>, Option<String>)> {
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
pub fn list_tags(repo: &gix::Repository) -> AppResult<Vec<String>> {
    let mut tags = Vec::new();
    let refs = repo
        .references()
        .map_err(|e| map_git_err(e, "git references"))?;
    let iter = refs
        .tags()
        .map_err(|e| map_git_err(e, "git tags"))?;
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
pub fn log_history(
    repo: &gix::Repository,
    max_count: usize,
    skip: usize,
) -> AppResult<Vec<CommitInfo>> {
    let head_id = repo
        .head_id()
        .map_err(|e| map_git_err(e, "git head_id"))?
        .detach();
    let walk = repo
        .rev_walk([head_id])
        .first_parent_only()
        .all()
        .map_err(|e| map_git_err(e, "git walk all"))?;
    let mut commits = Vec::new();
    let mut seen = 0usize;
    for info in walk {
        let info = info.map_err(|e| map_git_err(e, "git walk item"))?;
        if seen < skip {
            seen += 1;
            continue;
        }
        if commits.len() >= max_count {
            break;
        }
        let commit = info
            .object()
            .map_err(|e| map_git_err(e, "git commit object"))?;
        let message = commit
            .message_raw()
            .map_err(|e| map_git_err(e, "git message"))?
            .to_string();
        let author = commit
            .author()
            .map_err(|e| map_git_err(e, "git author"))?;
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

fn iso_from_secs(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

/// 读 ref 处的文件内容 (对齐 nuwax fileContent)。
pub fn file_content_at_ref(
    repo: &gix::Repository,
    ref_spec: &str,
    file_path: &str,
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
pub fn get_status(repo: &gix::Repository) -> AppResult<StatusResult> {
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
        .status(gix::progress::Discard)
        .map_err(|e| map_git_err(e, "git status"))?
        .untracked_files(gix::status::UntrackedFiles::Files)
        .into_iter(None)
        .map_err(|e| map_git_err(e, "git status into_iter"))?;
    while let Some(item) = iter
        .next()
        .transpose()
        .map_err(|e| map_git_err(e, "git status item"))?
    {
        match item {
            gix::status::Item::TreeIndex(change) => {
                let (loc, is_add, is_del) = match &change {
                    gix::diff::index::ChangeRef::Addition { location, .. } => {
                        (location, true, false)
                    }
                    gix::diff::index::ChangeRef::Deletion { location, .. } => {
                        (location, false, true)
                    }
                    gix::diff::index::ChangeRef::Modification { location, .. } => {
                        (location, false, false)
                    }
                    gix::diff::index::ChangeRef::Rewrite { location, .. } => {
                        (location, false, false)
                    }
                };
                let s = loc.to_string();
                r.staged.push(s.clone());
                if is_add {
                    r.created.push(s);
                } else if is_del {
                    r.deleted.push(s);
                }
            }
            gix::status::Item::IndexWorktree(change) => match change {
                gix::status::index_worktree::Item::Modification { rela_path, .. } => {
                    r.modified.push(rela_path.to_string());
                }
                gix::status::index_worktree::Item::DirectoryContents { entry, .. } => {
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
