//! manifest 类型（来自 `workspace-manifest` crate）re-export + file-server 侧解析 helper。

use std::path::Path;

use file_server::error::{AppError, AppResult};
use shared_types::{DiscoverError, DiscoveredProject, discover_projects};

pub use shared_types::{
    BuildSection, ProjectManifest, ProjectMeta, ProxySection, RunSection, WorkspaceManifest,
    WorkspaceMeta,
};
pub(super) use shared_types::{ReleaseLock, ReleaseMetadata, build_release_lock, parse_workspace};

/// 读取并严格校验 Manifest v1 `workspace.manifest.toml`。
pub(super) async fn read_workspace_manifest(ws: &Path) -> AppResult<WorkspaceManifest> {
    let path = ws.join("workspace.manifest.toml");
    let content = tokio::fs::read_to_string(&path)
        .await
        .map_err(|e| AppError::resource(format!("read workspace.manifest.toml: {e}")))?;
    parse_workspace(&content)
        .map_err(|e| AppError::business(format!("parse workspace.manifest.toml: {e}")))
}

/// 扫描 workspace 一级子目录，发现并解析所有 `project.manifest.toml`。
///
/// `workspace-manifest` 是纯同步 crate（内部 `read_dir` + 逐子目录 `read_to_string`），
/// 直接调用会阻塞 tokio worker，故移进阻塞池。`DiscoverError` 原样透传，由各调用点
/// 按自身语义映射（构建/启动走 `AppError::system`，识别接口走 `AppError::business`）。
pub(crate) async fn discover_projects_async(
    ws: &Path,
) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let ws = ws.to_path_buf();
    tokio::task::spawn_blocking(move || discover_projects(&ws))
        .await
        // 阻塞池 join 失败（panic）归为 IO 类：调用点只关心"扫不出来"，
        // 不新增错误变体以免各调用点重复接线
        .map_err(|e| DiscoverError::Io(format!("discover projects task: {e}")))?
}
