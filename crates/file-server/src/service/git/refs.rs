//! Git refs CRUD (branch/tag create/delete/switch)。

use crate::error::AppResult;

use super::map_git_err;
use super::read::{head_id_required, resolve_rev_required};

use gix::Repository;
use gix::actor::Signature;
use gix::bstr::BString;
use gix::date::{Time, parse::TimeBuf};
use gix::object::Kind as ObjectKind;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};

/// 创建分支 (对齐 nuwax createBranch; start_point 默认 HEAD)。
/// `switch=true` 时创建后立即 checkout (对齐 nuwax `git.branch({ checkout: true })`):
/// 先 clean_tree_check (避免创建后切换失败的半成品), 再建 ref, 再 switch。
pub fn create_branch(
    repo: &Repository,
    name: &str,
    start_point: Option<&str>,
    switch: bool,
    author_name: &str,
    author_email: &str,
) -> AppResult<()> {
    if switch {
        super::ops::clean_tree_check(repo)?;
    }
    let target = match start_point {
        Some(sp) => resolve_rev_required(repo, sp, "git branch start_point")?,
        // G-B4: unborn 仓库无起点可依——结构化判定, 不让 gix 原文直打调用方
        None => head_id_required(repo, "create branch without start_point")?,
    };
    let full = format!("refs/heads/{name}");
    let reference_name = FullName::try_from(full.as_str())
        .map_err(|e| map_git_err(e, "git branch reference name"))?;
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "create branch".into(),
            },
            expected: PreviousValue::MustNotExist,
            new: Target::Object(target),
        },
        name: reference_name,
        deref: false,
    };
    let committer = Signature {
        name: BString::from(author_name),
        email: BString::from(author_email),
        time: Time::now_local_or_utc(),
    };
    let mut time_buf = TimeBuf::default();
    repo.edit_references_as(std::iter::once(edit), Some(committer.to_ref(&mut time_buf)))
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
    let head_id = head_id_required(repo, "create tag")?;
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

#[cfg(test)]
mod tests {
    //! refs 写操作契约（G-B3/G-B4/N4）：
    //! - unborn 建分支/标签 = 显式 system 错误且不泄漏 gix 原文（app-169 同类）
    //! - start_point 缺席/坏表达式 = 显式错误
    //! - 缺分支/缺标签 = 显式 system 错误（类别保持, 不静默）

    use super::*;
    use crate::error::AppError;
    use crate::service::git::{commit_indexed, init_repo, stage_path};
    use gix::open;

    fn system_message(err: &AppError) -> String {
        match err {
            AppError::System(m) => m.clone(),
            other => panic!("expected System, got {other:?}"),
        }
    }

    #[test]
    fn create_branch_and_tag_on_unborn_fail_with_clean_message() {
        let dir = tempfile::tempdir().expect("create test directory");
        crate::service::git::ensure_repo(dir.path()).expect("init unborn repo");
        let repo = open(dir.path()).expect("open repo");

        let err = create_branch(&repo, "feature", None, false, "Test", "test@example.com")
            .expect_err("unborn 建分支必须报错");
        let msg = system_message(&err);
        assert!(
            !msg.contains("does not have any commits"),
            "不得泄漏 gix 原文: {msg}"
        );
        assert!(msg.contains("no commits yet"), "应说明缺提交: {msg}");

        let err = create_tag(&repo, "v1", None, "Test", "test@example.com")
            .expect_err("unborn 打标签必须报错");
        let msg = system_message(&err);
        assert!(
            !msg.contains("does not have any commits"),
            "不得泄漏 gix 原文: {msg}"
        );
    }

    #[test]
    fn create_branch_accepts_revision_expression_start_point() {
        let dir = tempfile::tempdir().expect("create test directory");
        init_repo(dir.path(), "Test", "test@example.com").expect("init repo");
        let repo = open(dir.path()).expect("open repo");
        std::fs::write(dir.path().join("a.txt"), "v1\n").expect("write v1");
        stage_path(&repo, "a.txt").expect("stage v1");
        let c1 = commit_indexed(&repo, "c1", "Test", "test@example.com").expect("commit c1");
        std::fs::write(dir.path().join("a.txt"), "v2\n").expect("write v2");
        stage_path(&repo, "a.txt").expect("stage v2");
        commit_indexed(&repo, "c2", "Test", "test@example.com").expect("commit c2");

        create_branch(
            &repo,
            "feature",
            Some("HEAD~1"),
            true,
            "Test",
            "test@example.com",
        )
        .expect("HEAD~1 start_point 必须可解析");
        let id = repo
            .find_reference("refs/heads/feature")
            .expect("branch exists")
            .id()
            .detach()
            .to_string();
        assert_eq!(id, c1, "feature 应指向 HEAD~1 即 c1");
        let reflog = std::fs::read_to_string(dir.path().join(".git/logs/refs/heads/feature"))
            .expect("branch creation should write a reflog");
        assert!(
            reflog.contains("Test <test@example.com>"),
            "branch reflog should use the configured committer: {reflog}"
        );

        let err = create_branch(
            &repo,
            "broken",
            Some("no-such-ref"),
            false,
            "Test",
            "test@example.com",
        )
        .expect_err("缺 start_point 必须报错");
        assert!(matches!(err, AppError::System(..)), "{err:?}");
        let err = create_branch(
            &repo,
            "broken2",
            Some("HEAD~x"),
            false,
            "Test",
            "test@example.com",
        )
        .expect_err("坏表达式必须报错");
        assert!(matches!(err, AppError::Validation(..)), "{err:?}");
    }

    #[test]
    fn missing_branch_and_tag_deletion_is_explicit_error() {
        let dir = tempfile::tempdir().expect("create test directory");
        init_repo(dir.path(), "Test", "test@example.com").expect("init repo");
        let repo = open(dir.path()).expect("open repo");

        let err = delete_branch(&repo, "nope", false).expect_err("缺分支必须显式报错");
        assert!(matches!(err, AppError::System(..)), "{err:?}");
        let err = delete_tag(&repo, "nope").expect_err("缺标签必须显式报错");
        assert!(matches!(err, AppError::System(..)), "{err:?}");
        let err =
            crate::service::git::switch_branch(&repo, "nope").expect_err("切缺分支必须显式报错");
        assert!(matches!(err, AppError::System(..)), "{err:?}");
    }
}
