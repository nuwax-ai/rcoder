//! Prepare dev artifacts without changing the serving directory. Activation belongs
//! inside the application's generation/commit guard.

use super::workspace_artifact_rel_path;
use file_server::error::{AppError, AppResult};
use file_server::service::zip;
use std::path::{Path, PathBuf};

pub const RUN_DIR: &str = ".run";
pub const PREVIOUS_DIR: &str = ".previous";
pub const STAGING_DIR: &str = ".staging";

/// Shared with hygiene: a live worker keeps this lease even if its async caller
/// is cancelled. Never unlink the lock file (that would split the lock domain).
pub(super) fn staging_lease(ws: &Path) -> AppResult<std::fs::File> {
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(ws.join(".dev-prepare.lock"))?;
    lease
        .try_lock()
        .map_err(|e| AppError::business(format!("dev preparation is busy: {e}")))?;
    Ok(lease)
}

#[derive(Debug)]
pub struct PreparedRun {
    // Field order releases the directory before the lease.
    staging: tempfile::TempDir,
    _lease: std::fs::File,
    workspace: PathBuf,
}

impl PreparedRun {
    /// Must run inside the generation commit guard. No suspension between the
    /// two renames: cancellation cannot leave a half-promoted directory.
    pub fn activate(self) -> AppResult<PathBuf> {
        let run = self.workspace.join(RUN_DIR);
        let previous = self.workspace.join(PREVIOUS_DIR);
        let had_run = run.try_exists()?;
        if had_run {
            if previous.try_exists()? {
                std::fs::remove_dir_all(&previous)?;
            }
            std::fs::rename(&run, &previous)?;
        }
        if let Err(error) = std::fs::rename(self.staging.path(), &run) {
            if had_run && let Err(restore) = std::fs::rename(&previous, &run) {
                return Err(AppError::system(format!(
                    "activate dev directory: {error}; restore failed: {restore}"
                )));
            }
            return Err(AppError::system(format!(
                "activate dev directory: {error}; previous directory preserved"
            )));
        }
        Ok(run)
    }
}

/// Only extract and validate. The blocking worker owns both staging and its
/// lease, so cancellation cannot race extraction against temporary cleanup.
pub async fn prepare_run_dir(ws: &Path, release_id: &str) -> AppResult<PreparedRun> {
    let workspace = ws.to_path_buf();
    let release_id = release_id.to_owned();
    tokio::task::spawn_blocking(move || {
        let lease = staging_lease(&workspace)?;
        let zip_path = workspace.join(workspace_artifact_rel_path(&release_id));
        match std::fs::metadata(&zip_path) {
            Ok(m) if m.is_file() => {}
            Ok(_) => return Err(AppError::resource("deploy package is not a file")),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(AppError::resource(format!(
                    "deploy package missing: {}",
                    zip_path.display()
                )));
            }
            Err(e) => return Err(AppError::system(format!("inspect deploy package: {e}"))),
        }
        let staging_root = workspace.join(STAGING_DIR);
        std::fs::create_dir_all(&staging_root)?;
        let staging = tempfile::Builder::new()
            .prefix("run-")
            .tempdir_in(staging_root)?;
        zip::extract_blocking(&zip_path, staging.path())?;
        for required in ["workspace.manifest.toml", "release.lock.toml"] {
            match std::fs::metadata(staging.path().join(required)) {
                Ok(m) if m.is_file() => {}
                Ok(_) => {
                    return Err(AppError::business(format!(
                        "deploy package {required} is not a file"
                    )));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return Err(AppError::business(format!(
                        "deploy package missing {required} at zip root (release_id={release_id})"
                    )));
                }
                Err(e) => {
                    return Err(AppError::system(format!(
                        "inspect deploy package {required}: {e}"
                    )));
                }
            }
        }
        Ok(PreparedRun {
            staging,
            _lease: lease,
            workspace,
        })
    })
    .await
    .map_err(|e| AppError::system(format!("dev preparation task failed: {e}")))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::service::userapp::WORKSPACE_BUILDS_DIR;
    use std::io::Write;

    /// 造一个最小合法制品 zip（workspace.manifest.toml + release.lock.toml + start.sh）。
    fn make_package(dir: &Path, release_id: &str) -> PathBuf {
        let builds = dir.join(WORKSPACE_BUILDS_DIR);
        std::fs::create_dir_all(&builds).expect("builds dir");
        let zip_path = builds.join(format!("workspace-package-{release_id}.zip"));
        let file = std::fs::File::create(&zip_path).expect("zip file");
        let mut writer = ::zip::ZipWriter::new(file);
        let options = ::zip::write::SimpleFileOptions::default()
            .compression_method(::zip::CompressionMethod::Stored);
        writer
            .start_file("workspace.manifest.toml", options)
            .expect("start manifest");
        writer
            .write_all(b"schema_version = 1\n")
            .expect("write manifest");
        writer
            .start_file("release.lock.toml", options)
            .expect("start lock");
        writer
            .write_all(b"release_id = \"x\"\n")
            .expect("write lock");
        writer.start_file("start.sh", options).expect("start sh");
        writer.write_all(b"#!/bin/sh\n").expect("write sh");
        writer.finish().expect("finish zip");
        zip_path
    }

    #[tokio::test]
    async fn first_deploy_extracts_and_promotes_run_dir() {
        let ws = tempfile::tempdir().expect("ws");
        make_package(ws.path(), "rel-1");
        let run = prepare_run_dir(ws.path(), "rel-1")
            .await
            .expect("prepare")
            .activate()
            .expect("activate");
        assert!(run.ends_with(RUN_DIR));
        assert!(run.join("workspace.manifest.toml").is_file());
        assert!(run.join("release.lock.toml").is_file());
        assert!(run.join("start.sh").is_file());
        // staging 已被 promote 走（不存在）
        assert!(!ws.path().join(STAGING_DIR).join("rel-1").exists());
    }

    #[tokio::test]
    async fn second_deploy_rotates_previous() {
        let ws = tempfile::tempdir().expect("ws");
        make_package(ws.path(), "rel-1");
        make_package(ws.path(), "rel-2");
        prepare_run_dir(ws.path(), "rel-1")
            .await
            .expect("first")
            .activate()
            .expect("activate");
        // 在 rel-1 的 .run 里放标记文件
        std::fs::write(ws.path().join(RUN_DIR).join("marker-rel-1"), "1").expect("marker");
        let run = prepare_run_dir(ws.path(), "rel-2")
            .await
            .expect("second")
            .activate()
            .expect("activate");
        // 新 .run 来自 rel-2（无 marker），旧内容轮换进 .previous
        assert!(!run.join("marker-rel-1").exists());
        assert!(ws.path().join(PREVIOUS_DIR).join("marker-rel-1").is_file());
    }

    #[tokio::test]
    async fn missing_package_keeps_existing_run_dir_untouched() {
        let ws = tempfile::tempdir().expect("ws");
        make_package(ws.path(), "rel-1");
        prepare_run_dir(ws.path(), "rel-1")
            .await
            .expect("first")
            .activate()
            .expect("activate");
        let err = prepare_run_dir(ws.path(), "rel-missing")
            .await
            .expect_err("must fail");
        assert!(err.to_string().contains("deploy package missing"));
        // 旧 .run 原样
        assert!(ws.path().join(RUN_DIR).join("start.sh").is_file());
        assert!(!ws.path().join(PREVIOUS_DIR).exists());
    }
    #[tokio::test]
    async fn invalid_package_cleans_its_staging() {
        let ws = tempfile::tempdir().expect("ws");
        let path = make_package(ws.path(), "invalid");
        let file = std::fs::File::create(path).expect("zip");
        let mut zip = ::zip::ZipWriter::new(file);
        zip.start_file(
            "workspace.manifest.toml",
            ::zip::write::SimpleFileOptions::default(),
        )
        .expect("entry");
        zip.write_all(b"schema_version = 1").expect("write");
        zip.finish().expect("finish");
        assert!(prepare_run_dir(ws.path(), "invalid").await.is_err());
        assert_eq!(
            std::fs::read_dir(ws.path().join(STAGING_DIR))
                .expect("staging")
                .count(),
            0
        );
    }
    #[tokio::test]
    async fn stale_preparation_neither_promotes_nor_leaks() {
        use crate::models::BuildTaskKind;
        use crate::service::userapp::tasks::BuildTaskStore;
        let ws = tempfile::tempdir().expect("ws");
        make_package(ws.path(), "old");
        prepare_run_dir(ws.path(), "old")
            .await
            .expect("prepare")
            .activate()
            .expect("activate");
        std::fs::write(ws.path().join(RUN_DIR).join("marker"), "old").expect("marker");
        make_package(ws.path(), "new");
        let store = BuildTaskStore::new();
        let task = store
            .create("app".into(), BuildTaskKind::DevStart)
            .await
            .expect("task");
        let lifecycle = store.dev_lifecycle("app").await;
        let generation = *lifecycle.lock().await;
        let prepared = prepare_run_dir(ws.path(), "new").await.expect("prepare");
        assert!(ws.path().join(RUN_DIR).join("marker").exists());
        // A sweep while preparation is active must leave its directory intact.
        super::super::hygiene::sweep_workspace(ws.path(), &ws.path().join("logs"), 5, 7).await;
        assert!(prepared.staging.path().join("release.lock.toml").exists());
        *lifecycle.lock().await += 1;
        let committed = task
            .commit_start(&lifecycle, generation, async move {
                prepared.activate()?;
                Ok::<(), AppError>(())
            })
            .await
            .expect("check");
        assert!(!committed);
        assert!(ws.path().join(RUN_DIR).join("marker").exists());
        assert_eq!(
            std::fs::read_dir(ws.path().join(STAGING_DIR))
                .expect("staging")
                .count(),
            0
        );
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn dev_artifact_preserves_links_and_executable_bits() {
        use std::os::unix::fs::PermissionsExt;
        let ws = tempfile::tempdir().expect("ws");
        let path = make_package(ws.path(), "links");
        let mut zip = ::zip::ZipWriter::new(std::fs::File::create(path).expect("zip"));
        for (name, content) in [
            ("workspace.manifest.toml", "schema_version=1"),
            ("release.lock.toml", "release_id='links'"),
            ("lib/index.js", "module.exports=42"),
            ("start.sh", "#!/bin/sh"),
        ] {
            zip.start_file(
                name,
                ::zip::write::SimpleFileOptions::default().unix_permissions(0o755),
            )
            .expect("entry");
            zip.write_all(content.as_bytes()).expect("content");
        }
        zip.add_symlink(
            "node_modules/next",
            "../lib",
            ::zip::write::SimpleFileOptions::default(),
        )
        .expect("link");
        zip.finish().expect("zip");
        let run = prepare_run_dir(ws.path(), "links")
            .await
            .expect("prepare")
            .activate()
            .expect("activate");
        assert!(run.join("node_modules/next").is_symlink());
        assert_eq!(
            std::fs::read_to_string(run.join("node_modules/next/index.js")).expect("module"),
            "module.exports=42"
        );
        assert_ne!(
            std::fs::metadata(run.join("start.sh"))
                .expect("mode")
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }
}
