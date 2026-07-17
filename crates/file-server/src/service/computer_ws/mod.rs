//! computer 工作区装配 (对齐 nuwax `computerUtils.createWorkspace` +
//! `computerFileUtils.importProject` + `AgentWorkspaceUtils`)。
//!
//! 拆分: [`import_ws`] (import-project) / [`create_ws`] (create-workspace) /
//! [`helpers`] (move_dir / temp_sibling / find_dir / remove_top_level_dir 等共享)。
//! 本 mod.rs 仅做模块声明 + 公共 API re-export + 共享常量。

mod create_ws;
mod helpers;
mod import_ws;

pub use create_ws::{CreateWorkspaceResult, create_workspace};
pub use helpers::remove_top_level_dir;
pub use import_ws::{ImportResult, import_project};

/// 导入项目时保留的目录/文件 (对齐 nuwax IMPORT_PROJECT_PRESERVED_ENTRIES)。
pub(super) const IMPORT_PRESERVED: &[&str] = &[
    ".git",
    ".agents",
    ".claude",
    ".codex",
    ".opencode",
    ".tmp",
    ".logs",
];

/// `.dynamic_add.lock` 标记 (对齐 nuwax DYNAMIC_ADD_LOCK; 含此锁的 skill 子目录不删)。
pub(super) const DYNAMIC_ADD_LOCK: &str = ".dynamic_add.lock";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserved_entries_match_nuwax() {
        // 7 项白名单 (对齐 nuwax IMPORT_PROJECT_PRESERVED_ENTRIES)
        assert_eq!(IMPORT_PRESERVED.len(), 7);
        for e in [
            ".git",
            ".agents",
            ".claude",
            ".codex",
            ".opencode",
            ".tmp",
            ".logs",
        ] {
            assert!(IMPORT_PRESERVED.contains(&e), "missing {e}");
        }
    }
}
