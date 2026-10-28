//! Shared workspace inodes are the deployment boundary even when controllers
//! see different container paths. Memory admission cannot coordinate processes.
use crate::config::{AppConfig, StorageBackend};
use anyhow::{Context, Result};
use shared_types::ServiceType;
use std::{
    fs::{File, OpenOptions},
    path::Path,
};

pub(super) fn lock(config: &AppConfig) -> Result<Vec<File>> {
    let manager = crate::docker_init::build_docker_manager_config(config);
    let mut paths = Vec::new();
    for family in [
        ServiceType::WebAgentRunner,
        ServiceType::ComputerAgentRunner,
    ] {
        if let Some(service) = manager.multi_image_config.get_service_config(&family) {
            let root = service.effective_workspace_resolution_path();
            #[cfg(feature = "deploy-host")]
            let root = if shared_types::is_deploy_host() {
                let map = docker_manager::path::host_map::resolve_map()?;
                map.get(Path::new(&root))
                    .context("Controller workspace has no host path mapping")?
                    .to_string_lossy()
                    .into_owned()
            } else {
                root
            };
            paths.push(root);
        }
    }
    paths.sort();
    paths.dedup();
    let exclusive = match config.storage.backend {
        StorageBackend::Memory => true,
        StorageBackend::Postgres => false,
    };
    paths
        .into_iter()
        .map(|root| acquire(Path::new(&root), exclusive))
        .collect()
}

fn acquire(root: &Path, exclusive: bool) -> Result<File> {
    std::fs::create_dir_all(root).context("Create controller workspace scope")?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".rcoder-controller.lock"))
        .context("Open controller workspace scope lock")?;
    if exclusive {
        file.try_lock().context("Workspace already has a controller; memory workload admission requires a single controller")?;
    } else {
        file.try_lock_shared().context("Workspace has a memory controller; shared control requires common PostgreSQL admission")?;
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn memory_controller_excludes_other_memory_and_postgres_controllers_until_drop() {
        let root = tempfile::tempdir().unwrap();
        let first = acquire(root.path(), true).unwrap();
        assert!(acquire(root.path(), true).is_err());
        assert!(acquire(root.path(), false).is_err());
        drop(first);
        let pg = acquire(root.path(), false).unwrap();
        let peer = acquire(root.path(), false).unwrap();
        assert!(acquire(root.path(), true).is_err());
        drop((pg, peer));
        assert!(acquire(root.path(), true).is_ok());
    }
}
