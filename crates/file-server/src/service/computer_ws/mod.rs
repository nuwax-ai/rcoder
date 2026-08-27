//! computer 工作区装配 (对齐 nuwax `computerUtils.createWorkspace` +
//! `computerFileUtils.importProject` + `AgentWorkspaceUtils`)。
//!
//! 拆分: [`import_ws`] (import-project) / [`create_ws`] (create-workspace 旧路径) /
//! [`agent_store_ws`] (create-workspace agent-store 路径) /
//! [`helpers`] (move_dir / temp_sibling / find_dir / remove_top_level_dir 等共享)。
//! 本 mod.rs 仅做模块声明 + 公共 API re-export + 共享常量。

mod agent_store_ws;
mod create_ws;
mod helpers;
mod import_ws;

pub use agent_store_ws::{CreateAgentStoreParams, create_workspace_with_agent_store};
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
    ".grok",
    ".pi",
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
        // 9 项白名单 (对齐 nuwax IMPORT_PROJECT_PRESERVED_ENTRIES + grok/pi)
        assert_eq!(IMPORT_PRESERVED.len(), 9);
        for e in [
            ".git",
            ".agents",
            ".claude",
            ".codex",
            ".opencode",
            ".grok",
            ".pi",
            ".tmp",
            ".logs",
        ] {
            assert!(IMPORT_PRESERVED.contains(&e), "missing {e}");
        }
    }
}
