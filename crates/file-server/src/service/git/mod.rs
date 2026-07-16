//! Git 操作 (对齐 nuwax gitService, gix 组合)。
//!
//! 拆分: 本模块 (公共) / [`read`] (读) / [`write`] (写); 后续 diff/reset/checkout 再加子模块。
//! gix 同步库且 `Repository` `!Send`, 函数均同步; axum handler 经 `spawn_blocking` 调用。

pub mod read;
pub mod write;

pub use read::*;
pub use write::*;

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

/// 确保 `.gitignore` 含必要条目 (对齐 nuwax ensureGitignore, append-only)。
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

/// 打开或初始化仓库 (对齐 nuwax ensureGitRepo 的 open/init 部分; initial commit 见 write::commit_indexed)。
pub fn ensure_repo(path: &Path) -> AppResult<gix::Repository> {
    if is_git_repo(path) {
        gix::open(path).map_err(|e| AppError::system(format!("git open failed: {e}")))
    } else {
        let repo = gix::init(path).map_err(|e| AppError::system(format!("git init failed: {e}")))?;
        // 新 repo 无 .git/index 文件, 从空 tree 创建空 index (否则首次 stage 的 open_index 失败)
        let empty_id = repo
            .head_tree_id_or_empty()
            .map_err(|e| AppError::system(format!("git head_tree: {e}")))?
            .detach();
        let mut idx = repo
            .index_from_tree(&empty_id)
            .map_err(|e| AppError::system(format!("git index_from_tree: {e}")))?;
        idx.remove_tree();
        idx
            .write(gix::index::write::Options::default())
            .map_err(|e| AppError::system(format!("git index write: {e}")))?;
        Ok(repo)
    }
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
