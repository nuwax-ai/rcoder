//! Git 操作 (对齐 nuwax gitService, gix 组合)。
//!
//! 拆分: 本模块 (公共) / `read` (读) / `write` (写); 后续 diff/reset/checkout 再加子模块。
//! gix 同步库且 `Repository` `!Send`, 函数均同步; axum handler 经 `spawn_blocking` 调用。

pub mod diff;
pub(crate) mod materialize;
pub mod ops;
pub mod read;
pub mod refs;
pub mod write;

pub use diff::*;
pub use ops::*;
pub use read::*;
pub use refs::*;
pub use write::*;

use std::path::{Path, PathBuf};

use gix::index::write::Options as IndexWriteOptions;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
use gix::{Repository, init, open};

use crate::error::{AppError, AppResult};
use crate::workspace::{ProjectContext, WorkspaceResolver};

/// `.gitignore` 默认条目 (对齐 nuwax appConfig GIT_GITIGNORE_ENTRIES)。
pub const DEFAULT_GITIGNORE_ENTRIES: &[&str] = &[
    "node_modules/",
    ".pnpm-store/",
    "dist/",
    "dist-packages/",
    "build/",
    // userApp 整体包产物（{ws}/builds/——ensure_gitignore append-only,
    // 存量 workspace 下次 git 操作自动补齐）
    "builds/",
    ".idea/",
    ".vscode/",
    ".DS_Store",
    ".npmrc",
    ".agents/",
    ".claude/",
    ".opencode/",
    ".codex/",
    ".grok/",
    ".pi/",
    // 智能体技能实体库（对齐 TS 6ab47b7——store 不入 git 版本管理）
    ".agent-store/",
    ".tmp/",
    ".logs/",
    "pnpm-lock.yaml",
    "yarn.lock",
    "package-lock.json",
];

/// pageApp 项目隔离模型解析（对齐 nuwax a29cbc0/1.4.7 `resolveAndCheck`
/// pageApp 分支——归一类型为 pageApp 时项目模型优先，serviceContext 定位
/// 参数不再能覆盖）：projectId 必填 → `resolver.resolve_project` → 目录
/// 必须已存在（git 操作只面向已存在工作区，不创建）。
pub async fn resolve_page_project(
    resolver: &dyn WorkspaceResolver,
    ctx: &ProjectContext,
) -> AppResult<PathBuf> {
    if ctx.project_id.trim().is_empty() {
        return Err(AppError::validation("pageApp mode requires projectId"));
    }
    let path = resolver.resolve_project(ctx).await?;
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(AppError::resource("Project does not exist"));
    }
    Ok(path)
}

/// 是否已是 git 仓库 (对齐 nuwax isGitRepo)。
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// env 布尔 (对齐 nuwax config env 开关)。
fn env_bool(name: &str, default: bool) -> bool {
    match std::env::var(name) {
        Ok(v) => matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
        Err(_) => default,
    }
}

/// 解析 .gitignore 条目列表: env `GIT_GITIGNORE_ENTRIES`(`|` 分隔) 覆盖默认 (对齐 nuwax appConfig)。
fn gitignore_entries() -> Vec<String> {
    match std::env::var("GIT_GITIGNORE_ENTRIES") {
        Ok(s) if !s.trim().is_empty() => s
            .split('|')
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect(),
        _ => DEFAULT_GITIGNORE_ENTRIES
            .iter()
            .map(|s| s.to_string())
            .collect(),
    }
}

/// 确保 `.gitignore` 含必要条目 (对齐 nuwax ensureGitignore, append-only)。
/// env `GIT_AUTO_GITIGNORE=false` 可整体关闭 (对齐 nuwax gitUtils GIT_AUTO_GITIGNORE)。
pub fn ensure_gitignore(path: &Path) -> AppResult<()> {
    if !env_bool("GIT_AUTO_GITIGNORE", true) {
        return Ok(());
    }
    let gitignore = path.join(".gitignore");
    let current = match std::fs::read_to_string(&gitignore) {
        Ok(current) => current,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => {
            return Err(AppError::system(format!(
                "read .gitignore {}: {error}",
                gitignore.display()
            )));
        }
    };
    let existing: Vec<String> = current.lines().map(|l| l.trim().to_string()).collect();
    let entries = gitignore_entries();
    let mut to_append: Vec<&str> = Vec::new();
    for entry in &entries {
        if !existing.iter().any(|e| e == entry) {
            to_append.push(entry);
        }
    }
    if to_append.is_empty() {
        return Ok(());
    }
    let mut new_content = current;
    if !new_content.is_empty() && !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(&to_append.join("\n"));
    new_content.push('\n');
    std::fs::write(&gitignore, new_content)?;
    Ok(())
}

/// 打开或初始化仓库 (对齐 nuwax ensureGitRepo 的 open/init 部分; initial commit 见 write::commit_indexed)。
pub fn ensure_repo(path: &Path) -> AppResult<Repository> {
    if is_git_repo(path) {
        let repo = open(path).map_err(|e| AppError::system(format!("git open failed: {e}")))?;
        ensure_index_file(&repo)?;
        Ok(repo)
    } else {
        let repo = init(path).map_err(|e| AppError::system(format!("git init failed: {e}")))?;
        // nuwax 显式 defaultBranch=main；覆盖宿主机/镜像中的 init.defaultBranch 配置。
        set_unborn_head_to_main(&repo)?;
        ensure_index_file(&repo)?;
        Ok(repo)
    }
}

/// gix `open_index` 严格失败，而 git CLI 把缺失的 index 视为合法空态——TS 时代
/// (git CLI/isomorphic-git init) 与外部工具创建的仓库只有 `.git` 没有 index 文件，
/// 首次 gix 读操作即报 "An IO error occurred while opening the index"。在所有
/// git 操作的必经收口自愈一次：unborn HEAD → 空 index；有提交 → 从 HEAD tree
/// 重建 (git reset 语义，未提交改动按 modified/untracked 正常呈现)。
fn ensure_index_file(repo: &Repository) -> AppResult<()> {
    ensure_index_file_with_probe(repo, |path| std::fs::symlink_metadata(path))
}

fn ensure_index_file_with_probe(
    repo: &Repository,
    mut probe: impl FnMut(&Path) -> std::io::Result<std::fs::Metadata>,
) -> AppResult<()> {
    let index_path = repo.index_path();
    let exists = |result: std::io::Result<std::fs::Metadata>| match result {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(AppError::system(format!(
            "git stat index {}: {error}",
            index_path.display()
        ))),
    };
    if exists(probe(&index_path))? {
        return Ok(());
    }

    // 与 git add / gix index.write 使用同一个 index.lock。初次探测之后可能
    // 已有请求补齐并暂存了文件，必须持锁复核，不能再覆盖成 HEAD tree。
    let lock = gix::lock::File::acquire_to_update_resource(
        &index_path,
        gix::lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|error| map_git_err(error, "git lock initial index"))?;
    if exists(probe(&index_path))? {
        return Ok(());
    }
    let tree_id = repo
        .head_tree_id_or_empty()
        .map_err(|e| AppError::system(format!("git head_tree: {e}")))?
        .detach();
    let mut idx = repo
        .index_from_tree(&tree_id)
        .map_err(|e| AppError::system(format!("git index_from_tree: {e}")))?;
    idx.remove_tree();
    // 锁已由本函数持有，write_to 不会像 idx.write 那样再次申请锁。
    let mut writer = std::io::BufWriter::with_capacity(64 * 1024, lock);
    idx.write_to(&mut writer, IndexWriteOptions::default())
        .map_err(|error| map_git_err(error, "git write initial index"))?;
    let lock = writer
        .into_inner()
        .map_err(|error| map_git_err(error, "git flush initial index"))?;
    lock.commit()
        .map_err(|error| map_git_err(error, "git commit initial index"))?;
    Ok(())
}

fn set_unborn_head_to_main(repo: &Repository) -> AppResult<()> {
    let edit = RefEdit {
        change: Change::Update {
            log: LogChange {
                mode: RefLog::AndReference,
                force_create_reflog: false,
                message: "init: set default branch to main".into(),
            },
            expected: PreviousValue::Any,
            new: Target::Symbolic(
                FullName::try_from("refs/heads/main")
                    .map_err(|e| AppError::system(format!("invalid main ref: {e}")))?,
            ),
        },
        name: FullName::try_from("HEAD")
            .map_err(|e| AppError::system(format!("invalid HEAD ref: {e}")))?,
        deref: false,
    };
    repo.edit_references(std::iter::once(edit))
        .map_err(|e| map_git_err(e, "git set initial HEAD to main"))?;
    Ok(())
}

/// gix 错误 → AppError::system。
pub(crate) fn map_git_err(e: impl std::fmt::Display, ctx: &str) -> AppError {
    AppError::system(format!("{ctx}: {e}"))
}

/// `refs/heads/main` → `main`; `refs/tags/v1` → `v1`。
pub(crate) fn shorten_ref(full: &str) -> Option<String> {
    for prefix in ["refs/heads/", "refs/tags/"] {
        if let Some(s) = full.strip_prefix(prefix) {
            return Some(s.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::LocalWorkspaceResolver;

    #[tokio::test]
    async fn resolve_page_project_rejects_missing_workspace_without_creating_it() {
        let root =
            std::env::temp_dir().join(format!("file-server-git-resolve-{}", std::process::id()));
        let project_root = root.join("projects");
        let computer_root = root.join("computers");
        let resolver = LocalWorkspaceResolver::new(project_root.clone(), computer_root);
        let context = ProjectContext {
            project_id: "missing".to_string(),
            tenant_id: None,
            space_id: None,
            isolation_type: None,
        };

        let error = match resolve_page_project(&resolver, &context).await {
            Ok(_) => panic!("missing workspace must be rejected"),
            Err(error) => error,
        };

        assert!(matches!(error, AppError::Resource(_)));
        assert!(!project_root.join("missing").exists());
    }

    fn fresh_repo_dir(kind: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "file-server-git-mod-test-{kind}-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create test repo");
        path
    }

    /// 反例 (线上 app-1694246): TS 时代/git CLI 创建的仓库只有 .git 没有 index
    /// 文件 (gix init 亦然)，gix open_index 严格失败——修复前 get_status 报
    /// "git status open_index: An IO error occurred while opening the index"。
    #[test]
    fn ensure_repo_backfills_missing_index_for_index_less_unborn_repo() {
        let dir = fresh_repo_dir("ts-era");
        let repo = init(&dir).expect("gix init (不写 index)");
        set_unborn_head_to_main(&repo).expect("HEAD → main");
        drop(repo);
        std::fs::write(dir.join("note.md"), "工作区已有文件").expect("写工作区文件");
        assert!(
            !dir.join(".git/index").exists(),
            "前置: git CLI/gix init 均不写 .git/index"
        );

        let repo = ensure_repo(&dir).expect("打开无 index 的既有仓库");
        assert!(dir.join(".git/index").exists(), "自愈: 补写 index 文件");
        let status = get_status(&repo).expect("补写后 status 可用");
        assert_eq!(status.untracked, vec!["note.md".to_string()]);
        assert!(status.staged.is_empty());
        assert!(status.conflicted.is_empty());
    }

    /// 有提交的仓库丢了 index: 从 HEAD tree 重建 (git reset 语义)，
    /// 已提交文件回到干净态，未提交改动按 modified/untracked 呈现。
    #[test]
    fn ensure_repo_rebuilds_missing_index_from_head_tree_for_born_repo() {
        let dir = fresh_repo_dir("born-lost-index");
        let repo = ensure_repo(&dir).expect("init");
        std::fs::write(dir.join("app.txt"), "v1").expect("写文件");
        stage_path(&repo, "app.txt").expect("stage");
        commit_indexed(&repo, "c1", "Test", "test@example.com").expect("commit");
        std::fs::remove_file(dir.join(".git/index")).expect("模拟 index 丢失");
        std::fs::write(dir.join("app.txt"), "v2").expect("提交后修改");
        std::fs::write(dir.join("new.txt"), "n").expect("新增未跟踪");

        let repo = ensure_repo(&dir).expect("重开丢 index 仓库");
        let status = get_status(&repo).expect("status");
        assert_eq!(status.modified, vec!["app.txt".to_string()]);
        assert_eq!(status.untracked, vec!["new.txt".to_string()]);
        assert!(status.staged.is_empty());
        assert!(status.deleted.is_empty());
    }

    #[test]
    fn missing_index_recheck_preserves_concurrent_stage() {
        use std::sync::mpsc;
        use std::time::Duration;

        let directory = tempfile::tempdir().expect("test repository");
        let root = directory.path();
        let repo = ensure_repo(root).expect("init");
        std::fs::write(root.join("app.txt"), "head").expect("head content");
        stage_path(&repo, "app.txt").expect("stage head");
        commit_indexed(&repo, "head", "Test", "test@example.com").expect("commit");
        std::fs::remove_file(repo.index_path()).expect("remove index");

        let (observed_tx, observed_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let late_root = root.to_path_buf();
        let late_reader = std::thread::spawn(move || {
            let repo = open(late_root).expect("open late reader");
            let mut first = true;
            ensure_index_file_with_probe(&repo, |path| {
                let observed = std::fs::symlink_metadata(path);
                if first {
                    first = false;
                    assert_eq!(
                        observed.as_ref().expect_err("missing index").kind(),
                        std::io::ErrorKind::NotFound
                    );
                    observed_tx.send(()).expect("report missing index");
                    resume_rx
                        .recv_timeout(Duration::from_secs(5))
                        .expect("resume late reader");
                } else {
                    assert!(
                        path.with_extension("lock").exists(),
                        "second probe holds the Git index lock"
                    );
                }
                observed
            })
        });

        observed_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("reader observed missing index");
        let writer = ensure_repo(root).expect("other request repairs index");
        std::fs::write(root.join("app.txt"), "staged").expect("staged content");
        stage_path(&writer, "app.txt").expect("other request stages");
        let staged_index = std::fs::read(writer.index_path()).expect("saved staged index");
        std::fs::write(root.join("app.txt"), "later worktree edit").expect("unstaged edit");
        resume_tx.send(()).expect("resume original request");
        late_reader
            .join()
            .expect("reader thread")
            .expect("reader reuses existing index");

        assert_eq!(
            std::fs::read(writer.index_path()).expect("index after repair"),
            staged_index
        );
        assert!(
            !root.join(".git/index.lock").exists(),
            "repair releases its lock"
        );
    }

    #[test]
    fn index_stat_error_preserves_staged_bytes_without_attempting_repair() {
        let directory = tempfile::tempdir().expect("test repository");
        let repo = ensure_repo(directory.path()).expect("init");
        std::fs::write(directory.path().join("app.txt"), "staged").expect("file");
        stage_path(&repo, "app.txt").expect("stage");
        let before = std::fs::read(repo.index_path()).expect("index bytes");

        let error = ensure_index_file_with_probe(&repo, |_| {
            Err(std::io::Error::other("injected stat I/O failure"))
        })
        .expect_err("stat error must not authorize repair");
        assert!(matches!(error, AppError::System(_)));
        assert_eq!(
            std::fs::read(repo.index_path()).expect("unchanged index"),
            before
        );
        assert!(!directory.path().join(".git/index.lock").exists());
    }
}
