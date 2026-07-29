use std::path::Path;

use crate::{DiscoverError, DiscoveredProject, parse_project, validate_topology};

pub fn discover_projects(ws_root: &Path) -> Result<Vec<DiscoveredProject>, DiscoverError> {
    let mut projects = Vec::new();
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
        projects.push(DiscoveredProject { dir, manifest });
    }
    projects.sort_by(|a, b| a.service_id().cmp(b.service_id()));
    validate_topology(&projects).map_err(|error| DiscoverError::Validation(error.to_string()))?;
    Ok(projects)
}
