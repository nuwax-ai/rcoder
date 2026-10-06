//! Source-derived cache preparation, exclusively performed by an admitted
//! owner operation. Historical artifact restoration never enters this path.
use std::{io::Write, path::Path};

use anyhow::{Context, Result};
use workspace_manifest::{ReleaseLock, ReleaseMetadata, build_release_lock};

pub(crate) async fn prepare(workspace: &Path) -> Result<()> {
    let workspace = workspace.to_owned();
    let metadata = Metadata::current();
    tokio::task::spawn_blocking(move || prepare_with(&workspace, &metadata))
        .await
        .context("join current Source lock preparation")??;
    Ok(())
}

struct Metadata {
    pingap_version: Option<String>,
    pingap_commit: Option<String>,
    runtime_image_digest: Option<String>,
}

impl Metadata {
    fn current() -> Self {
        let read = |key| {
            std::env::var(key)
                .ok()
                .filter(|value| !value.trim().is_empty())
        };
        Self {
            pingap_version: read("RCODER_PINGAP_VERSION"),
            pingap_commit: read("RCODER_PINGAP_COMMIT"),
            runtime_image_digest: read("RCODER_RUNTIME_IMAGE_DIGEST"),
        }
    }

    fn build(
        &self,
        workspace: &workspace_manifest::WorkspaceManifest,
        projects: &[workspace_manifest::DiscoveredProject],
        id: &str,
        cached: Option<&ReleaseLock>,
    ) -> Result<ReleaseLock> {
        // Same local-dev fallback as gen-lock. Runtime images supply all three
        // actual metadata inputs; unchanged legacy caches retain their metadata.
        let (version, commit) = super::devtool::pingap_identity();
        build_release_lock(
            workspace,
            projects,
            ReleaseMetadata {
                release_id: id,
                pingap_version: self
                    .pingap_version
                    .as_deref()
                    .or_else(|| cached.map(|lock| lock.pingap.version.as_str()))
                    .unwrap_or(&version),
                pingap_commit: self
                    .pingap_commit
                    .as_deref()
                    .or_else(|| cached.map(|lock| lock.pingap.commit.as_str()))
                    .unwrap_or(&commit),
                runtime_image_digest: self
                    .runtime_image_digest
                    .as_deref()
                    .or_else(|| cached.map(|lock| lock.runtime_image_digest.as_str()))
                    .unwrap_or("local-dev"),
                minimum_app_cli_version: cached
                    .map_or(workspace_manifest::MINIMUM_APP_CLI_VERSION, |lock| {
                        lock.minimum_app_cli_version.as_str()
                    }),
            },
        )
        .context("derive release identity from current Source manifests")
    }
}

fn prepare_with(workspace: &Path, metadata: &Metadata) -> Result<()> {
    let manifest_path = workspace.join("workspace.manifest.toml");
    let manifest = workspace_manifest::parse_workspace(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("read current Source manifest {}", manifest_path.display()))?,
    )
    .with_context(|| format!("parse current Source manifest {}", manifest_path.display()))?;
    let projects = workspace_manifest::discover_projects(workspace)
        .context("discover current Source projects")?;
    let path = workspace.join("release.lock.toml");
    let cached = match std::fs::read_to_string(&path) {
        Ok(content) => workspace_manifest::load_release_lock(&content)
            .ok()
            .filter(|lock| {
                !lock.release_id.trim().is_empty()
                    && semver::Version::parse(&lock.minimum_app_cli_version).is_ok()
            }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) if error.kind() == std::io::ErrorKind::InvalidData => None,
        Err(error) => {
            return Err(error)
                .with_context(|| format!("read derived Source lock {}", path.display()));
        }
    };
    if let Some(cached) = cached.as_ref() {
        let candidate = metadata.build(&manifest, &projects, &cached.release_id, Some(cached))?;
        if toml::to_string_pretty(&candidate)? == toml::to_string_pretty(cached)? {
            return Ok(());
        }
    }
    let id = uuid::Uuid::new_v4().simple().to_string();
    let lock = metadata.build(&manifest, &projects, &id, None)?;
    let content = toml::to_string_pretty(&lock).context("serialize current Source release lock")?;
    let builder = tempfile::Builder::new();
    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::PermissionsExt;
        let mut builder = builder;
        builder.permissions(std::fs::Permissions::from_mode(0o666));
        builder
    };
    let mut output = builder
        .tempfile_in(workspace)
        .context("create Source lock staging file")?;
    match std::fs::metadata(&path) {
        Ok(previous) => output.as_file().set_permissions(previous.permissions())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect previous Source lock permissions"),
    }
    output
        .write_all(content.as_bytes())
        .context("write Source lock staging file")?;
    output
        .as_file()
        .sync_all()
        .context("flush Source lock staging file")?;
    process_utils::atomic_file::persist(output, &path).context("publish current Source lock")?;
    #[cfg(unix)]
    std::fs::File::open(workspace)?
        .sync_all()
        .context("flush Source lock directory")?;
    tracing::info!(release_id = %id, "prepared current Source release lock");
    Ok(())
}

pub(crate) fn require_owner_capability(workspace: &Path, capabilities: &[String]) -> Result<()> {
    if capabilities
        .iter()
        .any(|c| c == workspace_manifest::STARTUP_PROBE_CAPABILITY)
    {
        return Ok(());
    }
    let projects = workspace_manifest::discover_projects(workspace)
        .context("inspect current Source startup capability requirements")?;
    anyhow::ensure!(
        !projects
            .iter()
            .any(|p| p.manifest.project.enabled && p.manifest.health.startup_probe.is_some()),
        "runtime owner lacks {}; upgrade app-cli before starting this Source input",
        workspace_manifest::STARTUP_PROBE_CAPABILITY
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_cache_rebuilds_corruption_and_changed_inputs_but_preserves_matching_identity() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("web")).unwrap();
        std::fs::write(
            root.path().join("workspace.manifest.toml"),
            "schema_version=1\n[workspace]\nname='source-cache'\n",
        )
        .unwrap();
        let project = root.path().join("web/project.manifest.toml");
        let manifest = "schema_version=1\n[project]\nservice_id='web'\nname='one'\ntype='python'\n[build]\ncommand=['true']\nartifact='out.zip'\n[run]\ncommand=['python3','main.py']\n";
        std::fs::write(&project, manifest).unwrap();
        let metadata = Metadata {
            pingap_version: Some("0.14.3".into()),
            pingap_commit: Some("fixture-commit".into()),
            runtime_image_digest: Some("fixture-image".into()),
        };
        let lock_path = root.path().join("release.lock.toml");
        prepare_with(root.path(), &metadata).unwrap();
        let original = std::fs::read(&lock_path).unwrap();
        prepare_with(root.path(), &metadata).unwrap();
        assert_eq!(std::fs::read(&lock_path).unwrap(), original);
        std::fs::write(&project, manifest.replace("name='one'", "name='two'")).unwrap();
        prepare_with(root.path(), &metadata).unwrap();
        let changed = std::fs::read(&lock_path).unwrap();
        assert_ne!(changed, original);
        prepare_with(root.path(), &metadata).unwrap();
        assert_eq!(std::fs::read(&lock_path).unwrap(), changed);
        std::fs::write(&lock_path, [0xff, 0xfe]).unwrap();
        prepare_with(root.path(), &metadata).unwrap();
        assert!(crate::manifest::read_release_lock(root.path()).is_ok());
        let valid = std::fs::read(&lock_path).unwrap();
        std::fs::write(&project, "invalid = [").unwrap();
        assert!(prepare_with(root.path(), &metadata).is_err());
        assert_eq!(
            std::fs::read(&lock_path).unwrap(),
            valid,
            "real project errors must preserve the last cache"
        );
    }
}
