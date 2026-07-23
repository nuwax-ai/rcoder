//! Git 操作 (对齐 nuwax gitService, gix 组合)。
//!
//! 拆分: 本模块 (公共) / [`read`] (读) / [`write`] (写); 后续 diff/reset/checkout 再加子模块。
//! gix 同步库且 `Repository` `!Send`, 函数均同步; axum handler 经 `spawn_blocking` 调用。

pub mod diff;
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
use crate::workspace::{ComputerContext, ProjectContext, WorkspaceResolver};

/// `.gitignore` 默认条目 (对齐 nuwax appConfig GIT_GITIGNORE_ENTRIES)。
pub const DEFAULT_GITIGNORE_ENTRIES: &[&str] = &[
    "node_modules/",
    ".pnpm-store/",
    "dist/",
    "dist-packages/",
    "build/",
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
pub async fn resolve_target(
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
                return Err(AppError::validation(
                    "taskAgent mode requires userId and cId",
                ));
            }
            let path = resolver.resolve_computer(ctx).await?;
            if !path.exists() {
                return Err(AppError::resource("Computer workspace does not exist"));
            }
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
            let path = resolver.resolve_project(ctx).await?;
            if !path.exists() {
                return Err(AppError::resource("Project does not exist"));
            }
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
        open(path).map_err(|e| AppError::system(format!("git open failed: {e}")))
    } else {
        let repo = init(path).map_err(|e| AppError::system(format!("git init failed: {e}")))?;
        // nuwax 显式 defaultBranch=main；覆盖宿主机/镜像中的 init.defaultBranch 配置。
        set_unborn_head_to_main(&repo)?;
        // 新 repo 无 .git/index 文件, 从空 tree 创建空 index (否则首次 stage 的 open_index 失败)
        let empty_id = repo
            .head_tree_id_or_empty()
            .map_err(|e| AppError::system(format!("git head_tree: {e}")))?
            .detach();
        let mut idx = repo
            .index_from_tree(&empty_id)
            .map_err(|e| AppError::system(format!("git index_from_tree: {e}")))?;
        idx.remove_tree();
        idx.write(IndexWriteOptions::default())
            .map_err(|e| AppError::system(format!("git index write: {e}")))?;
        Ok(repo)
    }
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
    async fn resolve_target_rejects_missing_workspace_without_creating_it() {
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

        let error = match resolve_target(&resolver, "pageApp", Some(&context), None).await {
            Ok(_) => panic!("missing workspace must be rejected"),
            Err(error) => error,
        };

        assert!(matches!(error, AppError::Resource(_)));
        assert!(!project_root.join("missing").exists());
    }
}
