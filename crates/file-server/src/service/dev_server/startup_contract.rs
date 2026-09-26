//! Read the exact source/artifact contract before submitting work to an older owner.
use std::{io::Read, path::Path};

use anyhow::{Context, Result};

pub(super) async fn require_owner_support(
    project: &Path,
    artifact_id: Option<&str>,
    capabilities: &[String],
) -> Result<()> {
    let project = project.to_owned();
    let artifact_id = artifact_id.map(str::to_owned);
    let lock = tokio::task::spawn_blocking(move || -> Result<Option<shared_types::ReleaseLock>> {
        let content = if let Some(id) = artifact_id {
            anyhow::ensure!(
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid artifact identifier"
            );
            let source = runtime_state_layout::canonical_project_root(&project);
            let file = std::fs::File::open(
                source
                    .join("builds")
                    .join(format!("workspace-package-{id}.zip")),
            )
            .context("open candidate artifact for startup preflight")?;
            let mut archive = zip::ZipArchive::new(file).context("read candidate artifact")?;
            let entry = archive
                .by_name("release.lock.toml")
                .context("candidate artifact has no release lock")?;
            let mut content = String::new();
            entry
                .take(1024 * 1024 + 1)
                .read_to_string(&mut content)
                .context("read artifact release lock")?;
            anyhow::ensure!(
                content.len() <= 1024 * 1024,
                "artifact release lock exceeds 1 MiB"
            );
            content
        } else {
            match std::fs::read_to_string(project.join("release.lock.toml")) {
                Ok(content) => content,
                // Legacy callers can ask the owner to start before producing a lock.
                // No new probe contract exists here; the owner reports its normal error.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error).context("read source release lock"),
            }
        };
        shared_types::load_release_lock(&content)
            .map(Some)
            .map_err(Into::into)
    })
    .await
    .context("join startup preflight")??;
    if let Some(lock) = lock {
        shared_types::require_startup_probe_capability(&lock, capabilities)?;
    }
    Ok(())
}
