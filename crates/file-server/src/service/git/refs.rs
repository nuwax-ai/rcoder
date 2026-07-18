//! Git refs CRUD (branch/tag create/delete/switch)。

use crate::error::AppResult;

use super::map_git_err;

use gix::Repository;
use gix::actor::Signature;
use gix::bstr::BString;
use gix::date::{Time, parse::TimeBuf};
use gix::object::Kind as ObjectKind;
use gix::refs::transaction::PreviousValue;

/// 创建分支 (对齐 nuwax createBranch; start_point 默认 HEAD)。
/// `switch=true` 时创建后立即 checkout (对齐 nuwax `git.branch({ checkout: true })`):
/// 先 clean_tree_check (避免创建后切换失败的半成品), 再建 ref, 再 switch。
pub fn create_branch(
    repo: &Repository,
    name: &str,
    start_point: Option<&str>,
    switch: bool,
) -> AppResult<()> {
    if switch {
        super::ops::clean_tree_check(repo)?;
    }
    let target = match start_point {
        Some(sp) => repo
            .rev_parse_single(sp)
            .map_err(|e| map_git_err(e, "git rev_parse startPoint"))?,
        None => repo.head_id().map_err(|e| map_git_err(e, "git head_id"))?,
    };
    let full = format!("refs/heads/{name}");
    repo.reference(
        full,
        target.detach(),
        PreviousValue::MustNotExist,
        "create branch",
    )
    .map_err(|e| map_git_err(e, "git reference (branch may already exist)"))?;
    if switch {
        super::ops::switch_branch(repo, name)?;
    }
    Ok(())
}

/// 删除分支 (对齐 nuwax deleteBranch)。
/// `force` 仅为字段契约对齐 nuwax (gix reference.delete 不校验合并状态, 始终删除)。
pub fn delete_branch(repo: &Repository, name: &str, _force: bool) -> AppResult<()> {
    let full = format!("refs/heads/{name}");
    let r = repo
        .find_reference(&full)
        .map_err(|e| map_git_err(e, "git find_reference (branch not found)"))?;
    r.delete().map_err(|e| map_git_err(e, "git delete"))?;
    Ok(())
}

/// 创建标签 (对齐 nuwax createTag; message 非空 → annotated, 否则 lightweight)。
/// annotated tag 的 tagger 用传入的 author (对齐 nuwax getDefaultAuthor), 不再硬编码。
pub fn create_tag(
    repo: &Repository,
    name: &str,
    message: Option<&str>,
    author_name: &str,
    author_email: &str,
) -> AppResult<()> {
    let head_id = repo
        .head_id()
        .map_err(|e| map_git_err(e, "git head_id"))?
        .detach();
    if let Some(msg) = message {
        let tagger = Signature {
            name: BString::from(author_name),
            email: BString::from(author_email),
            time: Time::now_local_or_utc(),
        };
        let mut buf = TimeBuf::default();
        repo.tag(
            name,
            head_id,
            ObjectKind::Commit,
            Some(tagger.to_ref(&mut buf)),
            msg,
            PreviousValue::MustNotExist,
        )
        .map_err(|e| map_git_err(e, "git tag (annotated)"))?;
    } else {
        repo.tag_reference(name, head_id, PreviousValue::MustNotExist)
            .map_err(|e| map_git_err(e, "git tag_reference"))?;
    }
    Ok(())
}

/// 删除标签 (对齐 nuwax deleteTag)。
pub fn delete_tag(repo: &Repository, name: &str) -> AppResult<()> {
    let full = format!("refs/tags/{name}");
    let r = repo
        .find_reference(&full)
        .map_err(|e| map_git_err(e, "git find_reference (tag not found)"))?;
    r.delete().map_err(|e| map_git_err(e, "git delete"))?;
    Ok(())
}

/// 校验不能删除当前分支 (对齐 nuwax deleteBranch 检查)。
pub fn is_current_branch(repo: &Repository, name: &str) -> AppResult<bool> {
    let current = repo
        .head_name()
        .ok()
        .flatten()
        .and_then(|n| super::shorten_ref(&n.to_string()));
    Ok(current.as_deref() == Some(name))
}
