//! Git 读操作 (status/log/branches/tags/file-content)。

use crate::error::{AppError, AppResult};

use super::{map_git_err, shorten_ref};

use gix::diff::index::ChangeRef as IndexChange;
use gix::index::entry::Stage;
use gix::progress::Discard;
use gix::status::index_worktree::Item as WorktreeItem;
use gix::status::{Item as StatusItem, UntrackedFiles};
use gix::{Commit, ObjectId, Repository};

/// 修订链上的一步：`~n` = 首个 parent 走 n 次（裸 `~` = `~1`）；
/// `^n` = 第 n 个 parent（裸 `^` = `^1`，`^0` = 恒等）。
enum ParentStep {
    Tilde(usize),
    Caret(usize),
}

/// refspec → ObjectId。spec 为：
/// - "HEAD"（gix 的 unborn 状态判定）
/// - 分支/tag 名（gix 部分名展开，与 revparse delegate 内部解析 ref 名同路径）
/// - 完整 40 位 OID 或 ≥4 位 hex 前缀（类型化 lookup；歧义前缀显式报错）
/// - 以上任意项 + `~n`/`^n` 修订链（`HEAD~1`、`main~2^2` 等，对齐 git 语法）
///
/// `Ok(None)` = 空仓库（unborn HEAD）、ref/OID 不存在或修订链越界（如 `HEAD~99`）——
/// "缺席"由调用方按"无数据"契约映射（log → 空列表，file-content → None/空串）。
/// 坏修订表达式（`HEAD~x`）与歧义前缀 → `AppError::validation`；其余错误传播。
///
/// 不经 rev_parse_single：其错误经 gix-error 类型擦除无法结构化分类。
/// 明确不支持 revspec 的 `^{}`（peel）、`@{}`、`:/`、`rev:path` 等表达式
/// （Java/TS 契约均未承诺）：含 `~`/`^` 的非法链 → Validation 显式报错，
/// 其余按 ref 名走缺席/非法名路径。
pub(crate) fn resolve_rev(repo: &Repository, spec: &str) -> AppResult<Option<ObjectId>> {
    let (base, steps) = split_revision_chain(spec)?;
    let Some(mut oid) = resolve_base_rev(repo, base)? else {
        return Ok(None);
    };
    for step in steps {
        let Some(next) = apply_parent_step(repo, oid, step)? else {
            return Ok(None);
        };
        oid = next;
    }
    Ok(Some(oid))
}

/// 写/对比路径的解析入口：语义同 [`resolve_rev`]，但"缺席"显式失败
/// （system 类，保持既有 HTTP 错误类别）——写与对比操作没有"无数据"契约，
/// 静默成功/空结果会误导调用方。
pub(crate) fn resolve_rev_required(
    repo: &Repository,
    spec: &str,
    ctx: &str,
) -> AppResult<ObjectId> {
    resolve_rev(repo, spec)?
        .ok_or_else(|| AppError::system(format!("{ctx}: revision '{spec}' not found")))
}

/// HEAD 必须已出生（创建分支/标签、revert 等需要现有提交做基准）。
/// unborn → system 错误（保持既有 HTTP 类别），文案不泄漏 gix 内部信息
/// （app-169 事故同源："Branch ... does not have any commits" 原文直达调用方）。
pub(crate) fn head_id_required(repo: &Repository, ctx: &str) -> AppResult<ObjectId> {
    let head = repo.head().map_err(|e| map_git_err(e, "git head"))?;
    if head.is_unborn() {
        return Err(AppError::system(format!(
            "{ctx}: repository has no commits yet"
        )));
    }
    Ok(head
        .into_peeled_id()
        .map_err(|e| map_git_err(e, "git head_id"))?
        .detach())
}

/// 在首个 `~`/`^` 处拆出 base 与修订链（git refname 规则禁止这两个字符，
/// 拆分无歧义）。链语法非法（缺 base、非数字后缀）→ Validation。
fn split_revision_chain(spec: &str) -> AppResult<(&str, Vec<ParentStep>)> {
    let Some(pos) = spec.find(['~', '^']) else {
        return Ok((spec, Vec::new()));
    };
    let (base, rest) = spec.split_at(pos);
    if base.is_empty() {
        return Err(AppError::validation(format!(
            "invalid revision '{spec}': missing base before '~'/'^'"
        )));
    }
    let mut steps = Vec::new();
    let bytes = rest.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let tilde = match bytes[i] {
            b'~' => true,
            b'^' => false,
            _ => {
                return Err(AppError::validation(format!("invalid revision '{spec}'")));
            }
        };
        i += 1;
        let digits_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        let n: usize = if i == digits_start {
            1 // 裸 `~`/`^` 等价 `~1`/`^1`
        } else {
            rest[digits_start..i].parse().map_err(|_| {
                AppError::validation(format!("invalid revision '{spec}': parent index too large"))
            })?
        };
        steps.push(if tilde {
            ParentStep::Tilde(n)
        } else {
            ParentStep::Caret(n)
        });
    }
    Ok((base, steps))
}

/// base 段（不含修订链）的类型化解析。
fn resolve_base_rev(repo: &Repository, base: &str) -> AppResult<Option<ObjectId>> {
    if base == "HEAD" {
        let head = repo.head().map_err(|e| map_git_err(e, "git head"))?;
        return if head.is_unborn() {
            Ok(None)
        } else {
            Ok(Some(
                head.into_peeled_id()
                    .map_err(|e| map_git_err(e, "git head_id"))?
                    .detach(),
            ))
        };
    }
    match repo.find_reference(base) {
        Ok(mut r) => Ok(Some(
            r.peel_to_id()
                .map_err(|e| map_git_err(e, "git peel ref"))?
                .detach(),
        )),
        Err(gix::reference::find::existing::Error::NotFound { .. }) => resolve_oid(repo, base),
        Err(e) => Err(map_git_err(e, "git find_reference")),
    }
}

/// OID 解析：完整 hash 逐对象确证存在；其余 hex 前缀走类型化 lookup。
/// NotFound/无前缀命中 = 缺席；歧义前缀 = 显式 Validation（不静默取第一个）；
/// ODB 真错误传播（不吞成缺席）。
fn resolve_oid(repo: &Repository, spec: &str) -> AppResult<Option<ObjectId>> {
    if let Ok(oid) = ObjectId::from_hex(spec.as_bytes()) {
        return match repo.find_object(oid) {
            Ok(_) => Ok(Some(oid)),
            Err(gix::object::find::existing::Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(map_git_err(e, "git find_object")),
        };
    }
    match gix::hash::Prefix::from_hex(spec) {
        Ok(prefix) => match repo.objects.lookup_prefix(prefix, None) {
            Ok(Some(Ok(id))) => Ok(Some(id)),
            Ok(Some(Err(()))) => Err(AppError::validation(format!(
                "ambiguous object prefix '{spec}'"
            ))),
            Ok(None) => Ok(None),
            Err(e) => Err(map_git_err(e, "git lookup_prefix")),
        },
        // 非 hex / 过短(<4) / 过长：按"非 OID 缺席"处理（保持既有空数据契约）
        Err(_) => Ok(None),
    }
}

/// 应用一步修订：越界 parent = 缺席（Ok(None)）；作用在非 commit 上 = Validation。
fn apply_parent_step(
    repo: &Repository,
    oid: ObjectId,
    step: ParentStep,
) -> AppResult<Option<ObjectId>> {
    match step {
        ParentStep::Tilde(0) | ParentStep::Caret(0) => Ok(Some(oid)),
        ParentStep::Tilde(n) => {
            let mut current = oid;
            for _ in 0..n {
                match parent_at(repo, current, 0)? {
                    Some(parent) => current = parent,
                    None => return Ok(None),
                }
            }
            Ok(Some(current))
        }
        ParentStep::Caret(n) => parent_at(repo, oid, n - 1),
    }
}

/// commit 的第 index 个 parent（0 基）。缺席/越界 → Ok(None)。
fn parent_at(repo: &Repository, oid: ObjectId, index: usize) -> AppResult<Option<ObjectId>> {
    let obj = match repo.find_object(oid) {
        Ok(obj) => obj,
        Err(gix::object::find::existing::Error::NotFound { .. }) => return Ok(None),
        Err(e) => return Err(map_git_err(e, "git find_object")),
    };
    if obj.kind != gix::object::Kind::Commit {
        return Err(AppError::validation(format!(
            "revision step '~'/'^' requires a commit at {oid}"
        )));
    }
    let commit = obj
        .try_into_commit()
        .map_err(|e| map_git_err(e, "git to commit"))?;
    Ok(commit.parent_ids().nth(index).map(|id| id.detach()))
}

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

// CommitInfo 移至 models（wire 契约 + ToSchema）；此处 re-export 保持
// `git::CommitInfo` 既有引用路径。
pub use crate::models::CommitInfo;

/// 提交历史 (对齐 nuwax logHistory; first-parent)。
/// `branch` 非空 → 从该 ref 起 walk (对齐 nuwax git.log({ ref: branch })); 默认 HEAD。
///
/// 仓库刚 init 尚无任何 commit (unborn) 或 ref 不存在时 → 返回空列表
/// (结构化判定: [`resolve_rev`] 返回 None; 对齐 TS d1e5c8a 的"无数据"契约)。
pub fn log_history(
    repo: &Repository,
    max_count: usize,
    skip: usize,
    branch: Option<&str>,
    file_path: Option<&str>,
) -> AppResult<Vec<CommitInfo>> {
    // 解析起始 ref; "缺席"(空仓库/ref 不存在)返回空列表而非报错。
    let spec = branch.filter(|b| !b.trim().is_empty()).unwrap_or("HEAD");
    let Some(start_id) = resolve_rev(repo, spec)? else {
        return Ok(Vec::new());
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
        // TS nativeLog 用 `--format=%B` 后 `.replace(/\n$/, "")` 去掉一个尾部换行;
        // gix message_raw 同样携带提交对象的尾部换行，去掉后与 TS API 契约一致。
        if message.ends_with('\n') {
            message.pop();
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

/// 读工作区文件内容（ref=worktree/staged 短路路径, 对齐 TS fileContent）。
/// 缺文件 = 空串（TS `existsSync ? read : ""` 的空数据契约——Diff 右侧取
/// 未存盘/已删文件时不得 500）；超限 = Validation；非 UTF-8 = lossy
/// （与 ref 路径 [`file_content_at_ref`] 的 from_utf8_lossy 一致）。
pub fn worktree_content(
    path: &std::path::Path,
    file_path: &str,
    max_bytes: u64,
) -> AppResult<String> {
    let full = crate::path_safety::ensure_within(path, file_path)?;
    let metadata = match std::fs::metadata(&full) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(AppError::system(format!(
                "read metadata {}: {error}",
                full.display()
            )));
        }
    };
    if metadata.len() > max_bytes {
        return Err(AppError::validation(format!(
            "git file content exceeds limit (max {max_bytes} bytes)"
        )));
    }
    let bytes = match std::fs::read(&full) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => {
            return Err(AppError::system(format!(
                "read {}: {error}",
                full.display()
            )));
        }
    };
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// 读 ref 处的文件内容 (对齐 nuwax fileContent)。
/// ref 缺席 (空仓库 unborn / ref 不存在) → `Ok(None)`，handler 既有契约映射为空串。
pub fn file_content_at_ref(
    repo: &Repository,
    ref_spec: &str,
    file_path: &str,
    max_bytes: u64,
) -> AppResult<Option<String>> {
    let Some(oid) = resolve_rev(repo, ref_spec)? else {
        return Ok(None);
    };
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
    pub conflicted: Vec<String>,
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
        conflicted: vec![],
    };
    let index = repo
        .open_index()
        .map_err(|e| map_git_err(e, "git status open_index"))?;
    let index_backing = index.path_backing();
    for entry in index.entries() {
        if entry.stage() != Stage::Unconflicted {
            r.conflicted.push(entry.path_in(index_backing).to_string());
        }
    }
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
    // An unmerged index path is staged and conflicted, not a normal worktree modification.
    // gix's status iterator currently reports common UU conflicts as IndexWorktree::Modification;
    // the index stages are the authoritative conflict source.
    for path in &r.conflicted {
        r.staged.push(path.clone());
    }
    r.modified
        .retain(|path| !r.conflicted.iter().any(|conflict| conflict == path));
    for v in [
        &mut r.staged,
        &mut r.modified,
        &mut r.created,
        &mut r.deleted,
        &mut r.untracked,
        &mut r.conflicted,
    ] {
        v.sort();
        v.dedup();
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    //! git 读服务回归网（read.rs 此前 0 测试）。
    //! 锁住的偏移敏感点：
    //! - file_content_at_ref 的 blob 缺失 = Ok(None)（handler 依赖此语义映射空串）
    //! - 超限 blob = Validation（防整库读爆内存）
    //! - get_status 的 5-bucket 分派（客户端按 bucket 渲染状态列表）
    //! - log_history 尊重 max_count（handler 层 clamp 后传值）
    //! - unborn 仓库/缺席 ref = 空数据而非报错（app-169 事故反例；
    //!   结构化判定 resolve_rev，不匹配错误文案）

    use super::*;
    use crate::service::git::write::{commit_indexed, init_repo, stage_path};
    use gix::open;

    struct TestRepo(std::path::PathBuf);

    impl TestRepo {
        fn new() -> Self {
            let path = Self::fresh_dir("born");
            init_repo(&path, "Test", "test@example.com").expect("init test repo");
            Self(path)
        }

        /// 只 init 不提交: unborn HEAD → refs/heads/main (与生产 app-169 同形)。
        fn new_unborn() -> Self {
            let path = Self::fresh_dir("unborn");
            crate::service::git::ensure_repo(&path).expect("init unborn repo");
            Self(path)
        }

        fn fresh_dir(kind: &str) -> std::path::PathBuf {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock after epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "file-server-git-read-test-{kind}-{}-{nonce}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("create test repo");
            path
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
    fn file_content_at_ref_reads_blob_and_missing_path_yields_none() {
        let t = TestRepo::new();
        std::fs::create_dir_all(t.0.join("src")).expect("建父目录");
        commit_file(&t, "src/app.txt", "hello", "c1");
        let repo = t.open();

        let content =
            file_content_at_ref(&repo, "HEAD", "src/app.txt", 1024).expect("读 blob 不应失败");
        assert_eq!(content.as_deref(), Some("hello"));

        // blob 缺失 = Ok(None) 而非 Err——handler 据此映射为空串（静默契约）
        let missing =
            file_content_at_ref(&repo, "HEAD", "no/such/file.txt", 1024).expect("缺失路径不应报错");
        assert_eq!(
            missing, None,
            "blob 缺失必须 Ok(None)（handler 空串语义的前提）"
        );
    }

    #[test]
    fn file_content_at_ref_rejects_blob_over_limit() {
        let t = TestRepo::new();
        commit_file(&t, "big.txt", "0123456789", "c1");
        let repo = t.open();

        let err = file_content_at_ref(&repo, "HEAD", "big.txt", 5).expect_err("超限 blob 必须拒绝");
        assert!(
            matches!(err, AppError::Validation(..)),
            "超限必须是 Validation（而非把大文件读进内存后再失败）"
        );
    }

    #[test]
    fn get_status_buckets_staged_modified_and_untracked() {
        let t = TestRepo::new();
        commit_file(&t, "tracked.txt", "v1", "c1");
        let repo = t.open();

        // staged：新增并 stage
        std::fs::write(t.0.join("staged_new.txt"), "x").expect("写文件");
        stage_path(&repo, "staged_new.txt").expect("stage");

        // modified：已跟踪文件改动（不 stage）
        std::fs::write(t.0.join("tracked.txt"), "v2-dirty").expect("改文件");

        // untracked：新文件不 stage
        std::fs::write(t.0.join("fresh.txt"), "y").expect("写未跟踪");

        let s = get_status(&repo).expect("status 不应失败");
        assert_eq!(s.current.as_deref(), Some("main"));
        assert!(
            s.staged.iter().any(|p| p == "staged_new.txt"),
            "stage 的新文件必须进 staged bucket: {:?}",
            s.staged
        );
        assert!(
            s.modified.iter().any(|p| p == "tracked.txt"),
            "工作区改动必须进 modified bucket: {:?}",
            s.modified
        );
        assert!(
            s.untracked.iter().any(|p| p == "fresh.txt"),
            "未跟踪文件必须进 untracked bucket: {:?}",
            s.untracked
        );
    }

    #[test]
    fn get_status_reports_unmerged_index_paths_as_staged_conflicts() {
        let t = TestRepo::new();
        commit_file(&t, "README.md", "base", "base");
        std::fs::write(
            t.0.join("README.md"),
            "<<<<<<< ours\nours\n=======\ntheirs\n>>>>>>> theirs\n",
        )
        .expect("write conflict markers");

        let repo = t.open();
        let path: &gix::bstr::BStr = "README.md".into();
        let base = {
            let index = repo.open_index().expect("open index");
            let entry = index
                .entry_by_path_and_stage(path, Stage::Unconflicted)
                .expect("find base index entry");
            (entry.stat, entry.id, entry.mode)
        };
        let ours = repo
            .write_blob(b"ours\n")
            .expect("write ours blob")
            .detach();
        let theirs = repo
            .write_blob(b"theirs\n")
            .expect("write theirs blob")
            .detach();
        let mut index = repo.open_index().expect("open index for conflict setup");
        index.remove_entries(|_, candidate, _| candidate == path);
        for (stage, id) in [
            (Stage::Base, base.1),
            (Stage::Ours, ours),
            (Stage::Theirs, theirs),
        ] {
            index.dangerously_push_entry(
                base.0,
                id,
                gix::index::entry::Flags::from(stage),
                base.2,
                path,
            );
        }
        index.sort_entries();
        index
            .write(super::super::IndexWriteOptions::default())
            .expect("write unmerged index");

        let status = get_status(&repo).expect("read status with conflict");
        assert_eq!(status.conflicted, ["README.md"]);
        assert_eq!(status.staged, ["README.md"]);
        assert!(
            status.modified.is_empty(),
            "conflict must not be reported as modified: {status:?}"
        );
    }

    #[test]
    fn log_history_respects_max_count() {
        let t = TestRepo::new();
        commit_file(&t, "a.txt", "1", "c1");
        commit_file(&t, "a.txt", "2", "c2");
        commit_file(&t, "a.txt", "3", "c3");
        let repo = t.open();

        let all = log_history(&repo, 50, 0, None, None).expect("全量 log");
        assert_eq!(all.len(), 4, "init_repo 的 initial commit + 3 次提交");

        let limited = log_history(&repo, 2, 0, None, None).expect("受限 log");
        assert_eq!(limited.len(), 2, "service 必须尊重传入的 max_count 上限");
    }

    #[test]
    fn log_history_on_unborn_repo_returns_empty() {
        // 修复前反例 (app-169 事故): 兜底谓词匹配不上 gix 真实文案 → 500
        let t = TestRepo::new_unborn();
        let repo = t.open();
        let head = log_history(&repo, 50, 0, None, None).expect("unborn HEAD 必须空列表而非报错");
        assert!(head.is_empty(), "unborn HEAD: {head:?}");
        let branch =
            log_history(&repo, 50, 0, Some("main"), None).expect("unborn 分支必须空列表而非报错");
        assert!(branch.is_empty(), "unborn main 分支: {branch:?}");
    }

    #[test]
    fn log_history_missing_branch_returns_empty() {
        let t = TestRepo::new();
        commit_file(&t, "a.txt", "1", "c1");
        let repo = t.open();
        // T-d4 锁: 有提交后分支短名（PartialNameRef 展开）必须命中
        let main_walk = log_history(&repo, 50, 0, Some("main"), None).expect("短名 main 应可 walk");
        assert!(
            !main_walk.is_empty(),
            "main 短名 walk 必须非空: {main_walk:?}"
        );
        let r = log_history(&repo, 50, 0, Some("no-such-branch"), None)
            .expect("缺分支必须空列表而非报错");
        assert!(r.is_empty(), "缺失分支: {r:?}");
    }

    #[test]
    fn log_history_branch_full_oid_resolves_and_missing_oid_is_empty() {
        let t = TestRepo::new();
        let oid = commit_file(&t, "a.txt", "1", "c1");
        let repo = t.open();
        // 完整 40 位 OID 作为 branch 保持既有解析能力
        let r = log_history(&repo, 50, 0, Some(&oid), None).expect("OID 起点应可解析");
        assert!(!r.is_empty(), "OID walk 必须有历史: {r:?}");
        let missing = "0".repeat(40);
        let r = log_history(&repo, 50, 0, Some(&missing), None).expect("缺 OID 必须空列表而非报错");
        assert!(r.is_empty(), "不存在 OID: {r:?}");
    }

    #[test]
    fn file_content_at_ref_on_unborn_or_missing_ref_returns_none() {
        let t = TestRepo::new_unborn();
        let repo = t.open();
        assert_eq!(
            file_content_at_ref(&repo, "HEAD", "any.txt", 1024)
                .expect("unborn HEAD 必须 Ok(None) 而非报错"),
            None,
            "unborn HEAD 走 handler 既有空串契约"
        );
        assert_eq!(
            file_content_at_ref(&repo, "refs/heads/nope", "any.txt", 1024)
                .expect("缺席 ref 必须 Ok(None) 而非报错"),
            None
        );
    }

    // ── 修订表达式 / 短 OID（G-A1/G-A2/G-C2 反例；修复前必挂） ──────────────────

    #[test]
    fn revision_expressions_resolve_relative_to_parents() {
        // G-A1: Java 契约 (GitController.fileContent) 明文 ref 可取 HEAD~1。
        let t = TestRepo::new();
        let c1 = commit_file(&t, "a.txt", "one", "c1");
        commit_file(&t, "a.txt", "two", "c2");
        let repo = t.open();

        let content = file_content_at_ref(&repo, "HEAD~1", "a.txt", 1024)
            .expect("HEAD~1 必须解析到父提交而非静默空");
        assert_eq!(content.as_deref(), Some("one"), "HEAD~1 应取父提交内容");

        // git 语法: 裸 `~`/`^` 等价于 `~1`/`^1`
        let bare =
            file_content_at_ref(&repo, "HEAD~", "a.txt", 1024).expect("HEAD~ 必须等价 HEAD~1");
        assert_eq!(bare.as_deref(), Some("one"));
        let caret =
            file_content_at_ref(&repo, "HEAD^", "a.txt", 1024).expect("HEAD^ 必须等价 HEAD~1");
        assert_eq!(caret.as_deref(), Some("one"));

        let log = log_history(&repo, 50, 0, Some("HEAD~1"), None).expect("log HEAD~1 应可解析");
        assert_eq!(
            log.first().map(|c| c.hash.as_str()),
            Some(c1.as_str()),
            "log 应自父提交开始 walk: {log:?}"
        );

        // 越界 parent = 缺席 → 空数据契约（不报错）
        let oob = log_history(&repo, 50, 0, Some("HEAD~99"), None)
            .expect("HEAD~99 越界必须按缺席返回空列表");
        assert!(oob.is_empty(), "越界 parent: {oob:?}");
        // 单亲链上 ^2 越界同为缺席
        let oob_caret = log_history(&repo, 50, 0, Some("HEAD^2"), None)
            .expect("HEAD^2 于单亲链必须按缺席返回空列表");
        assert!(oob_caret.is_empty(), "越界 nth-parent: {oob_caret:?}");
    }

    #[test]
    fn short_oid_resolves_and_missing_is_absent() {
        // G-A2: 短 hash 前缀解析（旧 rev_parse_single 能力）。
        let t = TestRepo::new();
        let oid = commit_file(&t, "a.txt", "1", "c1");
        let repo = t.open();
        let short = &oid[..7];
        let r = log_history(&repo, 50, 0, Some(short), None).expect("短 hash 应命中");
        assert!(!r.is_empty(), "短 hash walk 必须非空: {r:?}");
        assert_eq!(r.first().map(|c| c.hash.as_str()), Some(oid.as_str()));

        let missing = "0000000";
        let r = log_history(&repo, 50, 0, Some(missing), None)
            .expect("不存在的短前缀必须按缺席返回空列表");
        assert!(r.is_empty(), "不存在前缀: {r:?}");
    }

    #[test]
    fn ambiguous_object_prefix_is_explicit_error() {
        // G-A2: 歧义前缀必须显式报错，不得静默取第一个。
        let t = TestRepo::new();
        let repo = t.open();
        // 暴力构造两个共享 4-hex 前缀的 blob（16-bit 生日碰撞期望 ~300 次）
        let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
        let mut found: Option<(String, String, String)> = None;
        for i in 0..200_000_u32 {
            let content = format!("ambiguous-{i}");
            let id = gix::objs::compute_hash(
                repo.object_hash(),
                gix::objs::Kind::Blob,
                content.as_bytes(),
            )
            .expect("hash blob");
            let p4 = id.to_hex().to_string()[..4].to_string();
            if let Some(prev) = seen.get(&p4) {
                found = Some((p4, prev.clone(), content));
                break;
            }
            seen.insert(p4, content);
        }
        let (prefix, a, b) = found.expect("暴力构造 4-hex 碰撞对");
        repo.write_blob(a.as_bytes()).expect("write blob a");
        repo.write_blob(b.as_bytes()).expect("write blob b");

        let err = resolve_rev(&repo, &prefix).expect_err("歧义前缀必须显式报错");
        assert!(
            matches!(err, AppError::Validation(..)),
            "歧义前缀应是 Validation: {err:?}"
        );
    }

    #[test]
    fn malformed_revision_expression_is_explicit_error() {
        // G-C2: 坏修订表达式显式报错（当前静默空的坏输入, 拍板允许的新增显式错误面）。
        let t = TestRepo::new();
        let repo = t.open();
        for spec in ["HEAD~x", "HEAD^2x", "~1", "HEAD~2~y"] {
            let err = resolve_rev(&repo, spec).expect_err("坏表达式必须显式报错");
            assert!(
                matches!(err, AppError::Validation(..)),
                "{spec} 应是 Validation: {err:?}"
            );
        }
    }

    // ── N1: worktree file-content 空数据契约（修复前必挂） ─────────────────────

    #[test]
    fn worktree_content_missing_file_is_empty_and_non_utf8_is_lossy() {
        // TS 契约: fileContent 的 worktree 路径 `existsSync ? read : ""` ——
        // 缺文件不得 500（Diff 右侧取未存盘/已删文件时的空数据契约）;
        // 非 UTF-8 统一 lossy（与 ref 路径 from_utf8_lossy 一致）。
        let t = TestRepo::new();
        assert_eq!(
            worktree_content(&t.0, "no/such/file.txt", 1024).expect("缺文件必须空串"),
            "",
            "缺文件必须走空串契约"
        );
        std::fs::write(t.0.join("bin.dat"), [0xff_u8, 0xfe, b'a']).expect("write non-utf8");
        let s = worktree_content(&t.0, "bin.dat", 1024).expect("非 UTF-8 不得 500");
        assert!(s.contains('a'), "lossy 内容应保留可解码部分: {s:?}");
        std::fs::write(t.0.join("big.txt"), "0123456789").expect("write big");
        let err = worktree_content(&t.0, "big.txt", 5).expect_err("超限必须拒绝");
        assert!(matches!(err, AppError::Validation(..)), "{err:?}");
    }
}
