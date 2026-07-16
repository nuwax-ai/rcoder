//! Git refs CRUD (branch/tag create/delete/switch)。

use crate::error::AppResult;

use super::map_git_err;

use gix::refs::transaction::PreviousValue;

/// 创建分支 (对齐 nuwax createBranch; start_point 默认 HEAD)。
pub fn create_branch(repo: &gix::Repository, name: &str, start_point: Option<&str>) -> AppResult<()> {
    let target = match start_point {
        Some(sp) => repo
            .rev_parse_single(sp)
            .map_err(|e| map_git_err(e, "git rev_parse startPoint"))?,
        None => repo
            .head_id()
            .map_err(|e| map_git_err(e, "git head_id"))?,
    };
    let full = format!("refs/heads/{name}");
    repo.reference(
        full,
        target.detach(),
        PreviousValue::MustNotExist,
        "create branch",
    )
    .map_err(|e| map_git_err(e, "git reference (branch may already exist)"))?;
    Ok(())
}

/// 删除分支 (对齐 nuwax deleteBranch)。
pub fn delete_branch(repo: &gix::Repository, name: &str) -> AppResult<()> {
    let full = format!("refs/heads/{name}");
    let r = repo
        .find_reference(&full)
        .map_err(|e| map_git_err(e, "git find_reference (branch not found)"))?;
    r.delete().map_err(|e| map_git_err(e, "git delete"))?;
    Ok(())
}

/// 创建标签 (对齐 nuwax createTag; message 非空 → annotated, 否则 lightweight)。
pub fn create_tag(repo: &gix::Repository, name: &str, message: Option<&str>) -> AppResult<()> {
    let head_id = repo
        .head_id()
        .map_err(|e| map_git_err(e, "git head_id"))?
        .detach();
    if let Some(msg) = message {
        let tagger = gix::actor::Signature {
            name: gix::bstr::BString::from("tag"),
            email: gix::bstr::BString::from("tag@nuwax.local"),
            time: gix::date::Time::now_local_or_utc(),
        };
        let mut buf = gix::date::parse::TimeBuf::default();
        repo.tag(
            name,
            head_id,
            gix::object::Kind::Commit,
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
pub fn delete_tag(repo: &gix::Repository, name: &str) -> AppResult<()> {
    let full = format!("refs/tags/{name}");
    let r = repo
        .find_reference(&full)
        .map_err(|e| map_git_err(e, "git find_reference (tag not found)"))?;
    r.delete().map_err(|e| map_git_err(e, "git delete"))?;
    Ok(())
}

/// 校验不能删除当前分支 (对齐 nuwax deleteBranch 检查)。
pub fn is_current_branch(repo: &gix::Repository, name: &str) -> AppResult<bool> {
    let current = repo
        .head_name()
        .ok()
        .flatten()
        .and_then(|n| super::shorten_ref(&n.to_string()));
    Ok(current.as_deref() == Some(name))
}
