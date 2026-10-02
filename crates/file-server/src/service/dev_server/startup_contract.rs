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
            let candidate = source
                .join("builds")
                .join(format!("workspace-package-{id}.zip"));
            match std::fs::File::open(&candidate) {
                Ok(file) => {
                    let mut archive =
                        zip::ZipArchive::new(file).context("read candidate artifact")?;
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
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    // The registered zip can be cleaned after activation. The
                    // activated `.run` still carries the confirmed artifact
                    // identity; accept its startup contract only when its
                    // release matches the requested artifact exactly. Any
                    // other state keeps the error: republish the artifact.
                    let activated =
                        std::fs::read_to_string(source.join(".run").join("release.lock.toml"))
                            .context(
                                "artifact cache is gone and no activated release lock exists; \
                         republish the artifact",
                            )?;
                    let lock = shared_types::load_release_lock(&activated)
                        .context("parse activated artifact release lock")?;
                    anyhow::ensure!(
                        lock.release_id == id,
                        "artifact cache is gone and the activated release is {}; \
                         republish artifact {}",
                        lock.release_id,
                        id
                    );
                    activated
                }
                Err(error) => {
                    return Err(error).context("open candidate artifact for startup preflight");
                }
            }
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

#[cfg(test)]
mod tests {
    use super::*;

    const LOCK_FIXTURE: &str =
        include_str!("../../../../workspace-manifest/tests/fixtures/lock_v1.toml");
    const FIXTURE_RELEASE_ID: &str = "01923a5f8c217abc9def0123456789ab";

    fn project_with_activated_run(dir: &Path, release_id: &str) -> std::path::PathBuf {
        let project = dir.join("proj-art");
        let run = project.join(".run");
        std::fs::create_dir_all(&run).expect("run dir");
        let lock = LOCK_FIXTURE.replace(FIXTURE_RELEASE_ID, release_id);
        std::fs::write(run.join("release.lock.toml"), lock).expect("activated lock");
        project
    }

    /// 制品 zip 被清理后，激活 `.run` 声明同一制品身份时，启动契约预检
    /// 读取激活目录的 lock（owner 即将在其上启动）。
    #[tokio::test]
    async fn missing_zip_falls_back_to_matching_activated_release() {
        let dir = tempfile::tempdir().expect("dir");
        let project = project_with_activated_run(dir.path(), FIXTURE_RELEASE_ID);
        require_owner_support(&project, Some(FIXTURE_RELEASE_ID), &[])
            .await
            .expect("activated identity satisfies the startup contract");
    }

    /// 激活目录声明别的 release（或不存在）时保持明确拒绝：要求重新发布
    /// 制品，不猜输入。
    #[tokio::test]
    async fn missing_zip_with_wrong_identity_still_rejected() {
        let dir = tempfile::tempdir().expect("dir");
        let project = project_with_activated_run(dir.path(), FIXTURE_RELEASE_ID);
        let error = require_owner_support(&project, Some("another-release"), &[])
            .await
            .expect_err("identity mismatch must be rejected");
        assert!(
            format!("{error:#}").contains("republish"),
            "unexpected error: {error:#}"
        );

        let empty = tempfile::tempdir().expect("empty");
        let bare = empty.path().join("proj-bare");
        std::fs::create_dir_all(&bare).expect("bare project");
        let error = require_owner_support(&bare, Some(FIXTURE_RELEASE_ID), &[])
            .await
            .expect_err("no activation must be rejected");
        assert!(
            format!("{error:#}").contains("republish"),
            "unexpected error: {error:#}"
        );
    }

    /// zip 仍在时契约仍从 zip 内 lock 读取（既有行为），激活目录不参与。
    #[tokio::test]
    async fn present_zip_contract_unchanged() {
        let dir = tempfile::tempdir().expect("dir");
        let project = project_with_activated_run(dir.path(), "activated-but-stale");
        let builds = project.join("builds");
        std::fs::create_dir_all(&builds).expect("builds");
        let mut zip = zip::ZipWriter::new(
            std::fs::File::create(
                builds.join(format!("workspace-package-{FIXTURE_RELEASE_ID}.zip")),
            )
            .expect("zip file"),
        );
        zip.start_file(
            "release.lock.toml",
            zip::write::SimpleFileOptions::default(),
        )
        .expect("entry");
        std::io::Write::write_all(&mut zip, LOCK_FIXTURE.as_bytes()).expect("lock entry");
        zip.finish().expect("finish");
        require_owner_support(&project, Some(FIXTURE_RELEASE_ID), &[])
            .await
            .expect("zip contract still wins");
    }
}
