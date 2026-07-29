//! Runtime manifest boundary.
//!
//! Runtime identity, dependency order, ports, routes and log sources are read
//! exclusively from the immutable build-time `release.lock.toml`.

use std::path::Path;

use anyhow::{Context, Result};
use workspace_manifest::{LockedService, ReleaseLock, SCHEMA_VERSION};

pub type ServiceSpec = LockedService;

pub fn read_release_lock(workspace: &Path) -> Result<ReleaseLock> {
    let path = workspace.join("release.lock.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("read release lock {}", path.display()))?;
    let lock: ReleaseLock = toml::from_str(&content)
        .with_context(|| format!("parse release lock {}", path.display()))?;
    if lock.schema_version != SCHEMA_VERSION {
        anyhow::bail!(
            "unsupported release lock schema {}; expected {}",
            lock.schema_version,
            SCHEMA_VERSION
        );
    }
    if lock.services.is_empty() {
        anyhow::bail!("release lock has no enabled services");
    }
    Ok(lock)
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
