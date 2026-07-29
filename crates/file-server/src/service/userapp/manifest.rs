//! manifest 类型（来自 `workspace-manifest` crate）re-export + file-server 侧解析 helper。

use std::path::Path;

use crate::error::{AppError, AppResult};

pub use shared_types::{
    BuildSection, DiscoveredProject, DiscoverError, ProjectManifest, ProjectMeta, ProxySection,
    RunSection, WorkspaceManifest, WorkspaceMeta, discover_projects,
};

/// 读 `workspace.manifest.toml`（只读 [workspace].name，[[projects]] 已废弃）。
pub(super) async fn read_workspace_manifest(ws: &Path) -> AppResult<WorkspaceManifest> {
    let path = ws.join("workspace.manifest.toml");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::resource(format!("read workspace.manifest.toml: {e}")))?;
    toml::from_str(&content)
        .map_err(|e| AppError::business(format!("parse workspace.manifest.toml: {e}")))
}
