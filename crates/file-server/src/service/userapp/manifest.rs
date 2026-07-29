//! 两级 manifest 类型（下沉到 `shared_types::workspace_manifest`）+ file-server 侧解析 helper。
//!
//! 类型定义在 shared_types（供 app-cli 复用，避免拖入 file-server 的 gix/zip 重依赖）；
//! 此处 `pub use` re-export（保持 `file_server::service::userapp::*` 引用不变）+ 提供
//! 读 manifest 的异步 helper（用 file-server 的 AppError）。类型的解析测试在 shared_types 侧。

use std::path::Path;

use crate::error::{AppError, AppResult};

pub use shared_types::{
    BuildSection, ProjectManifest, ProjectMeta, ProjectRef, RunSection, WorkspaceManifest,
    WorkspaceMeta,
};

/// 读 `workspace.manifest.toml`。
pub(super) async fn read_workspace_manifest(ws: &Path) -> AppResult<WorkspaceManifest> {
    let path = ws.join("workspace.manifest.toml");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::resource(format!("read workspace.manifest.toml: {e}")))?;
    toml::from_str(&content)
        .map_err(|e| AppError::business(format!("parse workspace.manifest.toml: {e}")))
}

/// 读子项目的 `project.manifest.toml`。
pub(super) async fn read_project_manifest(proj_dir: &Path) -> AppResult<ProjectManifest> {
    let path = proj_dir.join("project.manifest.toml");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::resource(format!("read project.manifest.toml: {e}")))?;
    toml::from_str(&content)
        .map_err(|e| AppError::business(format!("parse project.manifest.toml: {e}")))
}
