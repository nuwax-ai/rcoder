//! manifest 类型（来自 `workspace-manifest` crate）re-export + file-server 侧解析 helper。

use std::path::Path;

use crate::error::{AppError, AppResult};

pub use shared_types::{
    BuildSection, ProjectManifest, ProjectMeta, ProxySection, RunSection, WorkspaceManifest,
    WorkspaceMeta,
};
pub(super) use shared_types::{
    ReleaseLock, ReleaseMetadata, build_release_lock, discover_projects, parse_workspace,
};

/// 读取并严格校验 Manifest v1 `workspace.manifest.toml`。
pub(super) async fn read_workspace_manifest(ws: &Path) -> AppResult<WorkspaceManifest> {
    let path = ws.join("workspace.manifest.toml");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::resource(format!("read workspace.manifest.toml: {e}")))?;
    parse_workspace(&content)
        .map_err(|e| AppError::business(format!("parse workspace.manifest.toml: {e}")))
}
