//! Git 操作 (对齐 nuwax gitService, 用 gix 组合实现)。
//!
//! gix 是同步库且 `Repository` 为 `!Send`, 故本模块函数均为同步;
//! axum handler 经 `tokio::task::spawn_blocking` 调用。

use std::path::{Path, PathBuf};

use crate::error::{AppError, AppResult};
use crate::workspace::{ComputerContext, ProjectContext, WorkspaceResolver};

/// `.gitignore` 默认条目 (对齐 nuwax appConfig GIT_GITIGNORE_ENTRIES)。
pub const DEFAULT_GITIGNORE_ENTRIES: &[&str] = &[
    "node_modules/",
    ".pnpm-store/",
    "dist/",
    "build/",
    ".idea/",
    ".vscode/",
    ".DS_Store",
    ".npmrc",
    ".agents/",
    ".claude/",
    ".opencode/",
    ".codex/",
    ".tmp/",
    ".logs/",
    "pnpm-lock.yaml",
    "yarn.lock",
    "package-lock.json",
];

/// workspaceType 分发目标 (对齐 nuwax resolveAndCheck)。
pub enum GitTarget {
    PageApp {
        project_id: String,
        path: PathBuf,
    },
    TaskAgent {
        user_id: String,
        cid: String,
        path: PathBuf,
    },
}

impl GitTarget {
    pub fn path(&self) -> &Path {
        match self {
            GitTarget::PageApp { path, .. } | GitTarget::TaskAgent { path, .. } => path,
        }
    }
    pub fn log_id(&self) -> String {
        match self {
            GitTarget::PageApp { project_id, .. } => project_id.clone(),
            GitTarget::TaskAgent { user_id, cid, .. } => format!("computer:{user_id}:{cid}"),
        }
    }
}

/// 解析 workspaceType + 路径 (对齐 nuwax resolveAndCheck)。
pub fn resolve_target(
    resolver: &dyn WorkspaceResolver,
    workspace_type: &str,
    project_ctx: Option<&ProjectContext>,
    computer_ctx: Option<&ComputerContext>,
) -> AppResult<GitTarget> {
    match workspace_type {
        "taskAgent" => {
            let ctx = computer_ctx
                .ok_or_else(|| AppError::validation("taskAgent mode requires userId and cId"))?;
            if ctx.user_id.trim().is_empty() || ctx.cid.trim().is_empty() {
                return Err(AppError::validation("taskAgent mode requires userId and cId"));
            }
            let path = resolver.resolve_computer(ctx);
            Ok(GitTarget::TaskAgent {
                user_id: ctx.user_id.clone(),
                cid: ctx.cid.clone(),
                path,
            })
        }
        "pageApp" => {
            let ctx = project_ctx
                .ok_or_else(|| AppError::validation("pageApp mode requires projectId"))?;
            if ctx.project_id.trim().is_empty() {
                return Err(AppError::validation("pageApp mode requires projectId"));
            }
            let path = resolver.resolve_project(ctx);
            Ok(GitTarget::PageApp {
                project_id: ctx.project_id.clone(),
                path,
            })
        }
        _ => Err(AppError::validation(
            "workspaceType is required and must be pageApp or taskAgent",
        )),
    }
}

/// 是否已是 git 仓库 (对齐 nuwax isGitRepo)。
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// 确保 `.gitignore` 含必要条目 (对齐 nuwax ensureGitignore, append-only; 同步版)。
pub fn ensure_gitignore(path: &Path) -> AppResult<()> {
    let gitignore = path.join(".gitignore");
    let current = std::fs::read_to_string(&gitignore).unwrap_or_default();
    let existing: Vec<&str> = current.lines().map(str::trim).collect();
    let mut to_append = Vec::new();
    for entry in DEFAULT_GITIGNORE_ENTRIES {
        if !existing.iter().any(|l| *l == *entry) {
            to_append.push(*entry);
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

// ── gix 仓库 ────────────────────────────────────────────────────────────────────

/// 打开或初始化仓库 (对齐 nuwax ensureGitRepo 的 open/init 部分; initial commit 见 commit 路由)。
pub fn ensure_repo(path: &Path) -> AppResult<gix::Repository> {
    if is_git_repo(path) {
        gix::open(path).map_err(|e| AppError::system(format!("git open failed: {e}")))
    } else {
        gix::init(path).map_err(|e| AppError::system(format!("git init failed: {e}")))
    }
}

fn map_git_err(e: impl std::fmt::Display, ctx: &str) -> AppError {
    AppError::system(format!("{ctx}: {e}"))
}

// ── branches ────────────────────────────────────────────────────────────────────

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

/// `refs/heads/main` → `main`; `refs/tags/v1` → `v1`。
fn shorten_ref(full: &str) -> Option<String> {
    for prefix in ["refs/heads/", "refs/tags/"] {
        if let Some(s) = full.strip_prefix(prefix) {
            return Some(s.to_string());
        }
    }
    None
}

// ── tags ────────────────────────────────────────────────────────────────────────

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

// ── log ─────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct CommitInfo {
    pub hash: String,
    pub date: String,
    pub message: String,
    pub author_name: String,
    pub author_email: String,
}

/// 提交历史 (对齐 nuwax logHistory; first-parent, 按提交时间倒序)。
pub fn log_history(repo: &gix::Repository, max_count: usize, skip: usize) -> AppResult<Vec<CommitInfo>> {
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
        let commit = info.object().map_err(|e| map_git_err(e, "git commit object"))?;
        let message = commit
            .message_raw()
            .map_err(|e| map_git_err(e, "git message"))?
            .to_string();
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

fn iso_from_secs(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_default()
}

// ── file-content ────────────────────────────────────────────────────────────────

/// 读 ref 处的文件内容 (对齐 nuwax fileContent; ref 非 worktree/staged 时从 commit 读)。
/// 返回 None 表示文件不存在 (对齐 nuwax: 错误返回空)。
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
