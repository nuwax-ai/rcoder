use std::path::Path;

use crate::{
    DiscoverError, DiscoveredProject, ProjectManifest, parse_project, validate_topology,
};

/// 扫描 workspace 根的一级子目录，发现并解析所有 `project.manifest.toml`。
///
/// 文件系统层：只负责"找目录 + 读文件 + 解析"；解析后的排序与拓扑校验交给
/// [`assemble_discovered`]——后者与文件系统无关，可被"重锁"（从 zip 内 manifest
/// 重建 release.lock.toml）等无文件系统场景复用。
pub fn discover_projects(ws_root: &Path) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut discovered: Vec<(String, ProjectManifest)> = Vec::new();
    for entry in std::fs::read_dir(ws_root).map_err(|error| DiscoverError::ReadDir {
        path: ws_root.display().to_string(),
        source: error.to_string(),
    })? {
        let entry = entry.map_err(|error| DiscoverError::Io(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| DiscoverError::Io(error.to_string()))?
            .is_dir()
        {
            continue;
        }
        let dir = entry.file_name().to_string_lossy().to_string();
        let path = entry.path().join("project.manifest.toml");
        if !path.is_file() {
            continue;
        }
        let content =
            std::fs::read_to_string(&path).map_err(|error| DiscoverError::ReadManifest {
                path: path.display().to_string(),
                source: error.to_string(),
            })?;
        let manifest = parse_project(&content).map_err(|error| DiscoverError::ParseManifest {
            path: path.display().to_string(),
            source: error.to_string(),
        })?;
        discovered.push((dir, manifest));
    }
    assemble_discovered(discovered)
}

/// 装配已解析的项目集合：按 `service_id` 排序 + 拓扑校验。
///
/// 与文件系统无关。既供 [`discover_projects`] 复用，也供未来 Stage 2 的
/// `relock_from_package`（从版本包 zip 内的 manifest 重锁，无文件系统访问）复用。
pub fn assemble_discovered(
    projects: Vec<(String, ProjectManifest)>,
) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut discovered: Vec<DiscoveredProject> = projects
        .into_iter()
        .map(|(dir, manifest)| DiscoveredProject { dir, manifest })
        .collect();
    discovered.sort_by(|a, b| a.service_id().cmp(b.service_id()));
    validate_topology(&discovered).map_err(|error| DiscoverError::Validation(error.to_string()))?;
    Ok(discovered)
}
