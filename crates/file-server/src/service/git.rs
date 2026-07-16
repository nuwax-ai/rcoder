//! Git 操作 (对齐 nuwax gitService, 用 gix 组合实现)。

use std::path::{Path, PathBuf};

use tokio::fs;

use crate::error::{AppError, AppResult};
use crate::workspace::{ComputerContext, ProjectContext, WorkspaceResolver};

/// .gitignore 默认条目 (对齐 nuwax appConfig GIT_GITIGNORE_ENTRIES)。
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

/// 解析 workspaceType + 路径 (对齐 nuwax resolveAndCheck)。存在性由 ensure_git_repo 处理。
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

/// 是否已是 git 仓库 (对齐 nuwax isGitRepo: `.git` 存在)。
pub fn is_git_repo(path: &Path) -> bool {
    path.join(".git").exists()
}

/// 确保 `.gitignore` 含必要条目 (对齐 nuwax ensureGitignore, append-only)。
pub async fn ensure_gitignore(path: &Path) -> AppResult<()> {
    let gitignore = path.join(".gitignore");
    let current = fs::read_to_string(&gitignore).await.unwrap_or_default();
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
    fs::write(&gitignore, new_content).await?;
    Ok(())
}
