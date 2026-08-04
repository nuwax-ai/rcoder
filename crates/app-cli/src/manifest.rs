//! Runtime manifest boundary.
//!
//! Runtime identity, dependency order, ports, routes and log sources are read
//! exclusively from the immutable build-time `release.lock.toml`.

use std::path::Path;

use anyhow::{Context, Result};
use workspace_manifest::{LockedService, ReleaseLock, load_release_lock};

pub type ServiceSpec = LockedService;

pub fn read_release_lock(workspace: &Path) -> Result<ReleaseLock> {
    let path = workspace.join("release.lock.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read release lock {}", path.display()))?;
    // 版本感知加载与迁移 dispatch 收敛到 workspace_manifest::load_release_lock（单一权威）；
    // app-cli 与 app_manager 两条读路径都经它，LoadError 经 anyhow 上下文化后返回。
    load_release_lock(&content)
        .with_context(|| format!("load release lock {}", path.display()))
}

pub fn build_specs(workspace: &Path) -> Result<Vec<ServiceSpec>> {
    Ok(read_release_lock(workspace)?.services)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_release_lock_fails_fast() {
        let workspace = tempfile::tempdir().expect("workspace tempdir");
        assert!(read_release_lock(workspace.path()).is_err());
    }
}
