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
#[derive(Debug)]
pub(super) struct StagingLease {
    file: std::fs::File,
    released: bool,
}

impl StagingLease {
    fn release(&mut self) -> std::io::Result<()> {
        if !self.released {
            self.file.unlock()?;
            self.released = true;
        }
        Ok(())
    }
}

impl Drop for StagingLease {
    fn drop(&mut self) {
        if let Err(error) = self.release() {
            tracing::error!(%error, "Release dev preparation lease failed");
        }
    }
}

pub(super) fn staging_lease(ws: &Path) -> AppResult<StagingLease> {
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(ws.join(".dev-prepare.lock"))?;
    lease
        .try_lock()
        .map_err(|e| AppError::business(format!("dev preparation is busy: {e}")))?;
    Ok(StagingLease {
        file: lease,
        released: false,
    })
}

#[derive(Debug)]
pub struct PreparedRun {
    // Field order releases the directory before the lease.
    staging: Option<tempfile::TempDir>,
    _lease: StagingLease,
    workspace: PathBuf,
    requested_release_id: String,
    /// The activated `.run` already declares the requested release and the
    /// registered zip is gone: activation is a verified no-op instead of a
    /// directory swap.
    reuse_activated: bool,
}

impl PreparedRun {
    /// Must run inside the generation commit guard. No suspension between the
    /// two renames: cancellation cannot leave a half-promoted directory.
    pub fn activate(mut self) -> AppResult<PathBuf> {
        let run = self.workspace.join(RUN_DIR);
        if self.reuse_activated {
            // Re-verify at the commit boundary: identity was checked while the
            // preparation lease was held, but the window before activation may
            // have seen another actor replace `.run`.
            let lock = std::fs::read_to_string(run.join("release.lock.toml")).map_err(|e| {
                AppError::business(format!(
                    "activated run directory lost its release lock: {e}"
                ))
            })?;
            let release_id = shared_types::load_release_lock(&lock)
                .map_err(|e| {
                    AppError::business(format!(
                        "activated run directory has an unreadable release lock: {e}"
                    ))
                })?
                .release_id;
            if release_id != self.requested_release_id {
                return Err(AppError::business(format!(
                    "activated run directory now holds release {release_id}"
                )));
            }
            self.staging = None;
            self._lease.release()?;
            return Ok(run);
        }
        let previous = self.workspace.join(PREVIOUS_DIR);
        let had_run = run.try_exists()?;
        if had_run {
            if previous.try_exists()? {
                std::fs::remove_dir_all(&previous)?;
            }
            std::fs::rename(&run, &previous)?;
        }
        let staging = self
            .staging
            .take()
            .expect("fresh preparation always owns a staging directory");
        if let Err(error) = std::fs::rename(staging.path(), &run) {
            if had_run && let Err(restore) = std::fs::rename(&previous, &run) {
                return Err(AppError::system(format!(
                    "activate dev directory: {error}; restore failed: {restore}"
                )));
            }
            return Err(AppError::system(format!(
                "activate dev directory: {error}; previous directory preserved"
            )));
        }
        // Closing alone can leave flock held by a transient fork/dup. The
        // staging path has been promoted, so cleanup no longer needs the lease.
        drop(staging);
        self._lease.release()?;
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
                // The registered zip can be cleaned after activation. When the
                // activated `.run` still carries the requested release (both
                // required manifests present), starting it is the recovery
                // path — do not guess a different input or demand the cache.
                // `activate` re-verifies the identity at the commit boundary.
                if activated_run_matches(&workspace, &release_id)? {
                    return Ok(PreparedRun {
                        staging: None,
                        _lease: lease,
                        workspace,
                        requested_release_id: release_id,
                        reuse_activated: true,
                    });
                }
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
            staging: Some(staging),
            _lease: lease,
            workspace,
            requested_release_id: release_id,
            reuse_activated: false,
        })
    })
    .await
    .map_err(|e| AppError::system(format!("dev preparation task failed: {e}")))?
}

/// Identity check for the cache-missing recovery path: the activated `.run`
/// must declare exactly the requested release and still hold both manifests a
/// fresh package would carry.
fn activated_run_matches(workspace: &Path, release_id: &str) -> AppResult<bool> {
    let run = workspace.join(RUN_DIR);
    for required in ["workspace.manifest.toml", "release.lock.toml"] {
        match std::fs::metadata(run.join(required)) {
            Ok(m) if m.is_file() => {}
            _ => return Ok(false),
        }
    }
    let lock = match std::fs::read_to_string(run.join("release.lock.toml")) {
        Ok(lock) => lock,
        Err(_) => return Ok(false),
    };
    let declared = match shared_types::load_release_lock(&lock) {
        Ok(lock) => lock.release_id,
        Err(_) => return Ok(false),
    };
    Ok(declared == release_id)
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
    async fn activation_releases_lease_with_a_duplicated_file_description() {
        let ws = tempfile::tempdir().expect("workspace");
        make_package(ws.path(), "one");
        make_package(ws.path(), "two");
        let prepared = prepare_run_dir(ws.path(), "one").await.expect("prepare");
        // Model a transient fork/dup retaining the open file description.
        let inherited = prepared
            ._lease
            .file
            .try_clone()
            .expect("duplicate description");
        prepared.activate().expect("activate first");
        let second = prepare_run_dir(ws.path(), "two")
            .await
            .expect("completed worker must release lease explicitly");
        second.activate().expect("activate second");
        drop(inherited);
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

    /// 完整可解析 lock 的制品（真实构建产物形态）。
    const FULL_LOCK_FIXTURE: &str =
        include_str!("../../../../workspace-manifest/tests/fixtures/lock_v1.toml");

    fn make_full_package(dir: &Path, release_id: &str) -> PathBuf {
        let builds = dir.join(WORKSPACE_BUILDS_DIR);
        std::fs::create_dir_all(&builds).expect("builds dir");
        let zip_path = builds.join(format!("workspace-package-{release_id}.zip"));
        let file = std::fs::File::create(&zip_path).expect("zip file");
        let mut writer = ::zip::ZipWriter::new(file);
        let options = ::zip::write::SimpleFileOptions::default()
            .compression_method(::zip::CompressionMethod::Stored);
        let lock = FULL_LOCK_FIXTURE.replace("01923a5f8c217abc9def0123456789ab", release_id);
        for (name, content) in [
            ("workspace.manifest.toml", "schema_version = 1\n"),
            ("release.lock.toml", lock.as_str()),
            ("start.sh", "#!/bin/sh\n"),
        ] {
            writer.start_file(name, options).expect("start entry");
            writer.write_all(content.as_bytes()).expect("write entry");
        }
        writer.finish().expect("finish zip");
        zip_path
    }

    /// 注册 zip 被清理后，激活 `.run` 仍声明同一 release：准备与激活都
    /// 复用现有目录（内容原样保留，不产生 .previous）。
    #[tokio::test]
    async fn missing_zip_reuses_activated_run_identity() {
        let ws = tempfile::tempdir().expect("ws");
        let zip = make_full_package(ws.path(), "rel-cache-1");
        prepare_run_dir(ws.path(), "rel-cache-1")
            .await
            .expect("first")
            .activate()
            .expect("activate");
        std::fs::write(ws.path().join(RUN_DIR).join("data.txt"), "kept").expect("runtime data");
        std::fs::remove_file(&zip).expect("cache cleaned");

        let run = prepare_run_dir(ws.path(), "rel-cache-1")
            .await
            .expect("recovery prepare")
            .activate()
            .expect("recovery activate reuses the activated directory");
        assert_eq!(
            std::fs::read_to_string(run.join("data.txt")).expect("data"),
            "kept"
        );
        assert!(!ws.path().join(PREVIOUS_DIR).exists());
    }

    /// 缓存缺失且激活目录声明别的 release：保持明确失败，`.run` 原样。
    #[tokio::test]
    async fn missing_zip_with_foreign_release_still_fails() {
        let ws = tempfile::tempdir().expect("ws");
        let zip = make_full_package(ws.path(), "rel-cache-1");
        prepare_run_dir(ws.path(), "rel-cache-1")
            .await
            .expect("first")
            .activate()
            .expect("activate");
        std::fs::remove_file(&zip).expect("cache cleaned");

        let err = prepare_run_dir(ws.path(), "rel-cache-2")
            .await
            .expect_err("foreign identity must fail");
        assert!(err.to_string().contains("deploy package missing"));
        assert!(ws.path().join(RUN_DIR).join("start.sh").is_file());
    }

    /// 复用路径在激活（commit 边界）重核身份：窗口内 `.run` 被换成别的
    /// release 时激活失败，不启动身份不明的内容。
    #[tokio::test]
    async fn reuse_rechecks_identity_at_commit_boundary() {
        let ws = tempfile::tempdir().expect("ws");
        let zip = make_full_package(ws.path(), "rel-cache-1");
        prepare_run_dir(ws.path(), "rel-cache-1")
            .await
            .expect("first")
            .activate()
            .expect("activate");
        std::fs::remove_file(&zip).expect("cache cleaned");
        let prepared = prepare_run_dir(ws.path(), "rel-cache-1")
            .await
            .expect("recovery prepare");

        // 窗口内另一个角色把 `.run` 换成别的 release。
        let swapped =
            FULL_LOCK_FIXTURE.replace("01923a5f8c217abc9def0123456789ab", "rel-swapped-9");
        std::fs::write(ws.path().join(RUN_DIR).join("release.lock.toml"), swapped)
            .expect("swap lock");

        let err = prepared
            .activate()
            .expect_err("swapped identity must be refused at commit boundary");
        assert!(err.to_string().contains("rel-swapped-9"));
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
        assert!(
            prepared
                .staging
                .as_ref()
                .expect("fresh preparation owns staging")
                .path()
                .join("release.lock.toml")
                .exists()
        );
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

/// DEV-R6（本次制品实际生效）：restart_dev_staged 的 activate 夹在"确认旧
/// 执行停止"与"启动"之间——`.run` 被**本次**制品内容替换（旧版本移入
/// `.previous` 留档），编排器在**新**目录上启动并产出新内容。
#[cfg(all(test, unix))]
mod artifact_restart_tests {
    use super::*;
    use crate::service::userapp::WORKSPACE_BUILDS_DIR;
    use std::io::Write;

    /// 造带内容标记的最小合法制品 zip。
    fn make_package_with_marker(dir: &Path, release_id: &str, marker: &str) {
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
        writer.write_all(b"schema_version = 1\n").expect("manifest");
        writer
            .start_file("release.lock.toml", options)
            .expect("start lock");
        writer.write_all(b"release_id = \"x\"\n").expect("lock");
        writer.start_file("marker.txt", options).expect("marker");
        writer.write_all(marker.as_bytes()).expect("marker content");
        // 编排器：把 cwd 的 marker.txt 内容落到 served.txt（内容生效的可观测面）
        writer.start_file("orchestrator.sh", options).expect("sh");
        writer
            .write_all(b"#!/bin/sh\ncp marker.txt served.txt 2>/dev/null || true\nexec sleep 120\n")
            .expect("sh content");
        writer.finish().expect("finish zip");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&zip_path, std::fs::Permissions::from_mode(0o644))
                .expect("zip mode");
        }
    }

    #[tokio::test]
    async fn staged_restart_runs_on_this_release_content() {
        let harness = tempfile::tempdir().expect("harness");
        let ws = harness.path().join("ws-art");
        std::fs::create_dir_all(&ws).expect("ws");
        // 旧版本 .run（内容 A）：activate 必须替换而不是叠加。
        std::fs::create_dir_all(ws.join(".run")).expect("old run");
        std::fs::write(ws.join(".run/marker.txt"), "A").expect("old marker");
        // 本次制品（内容 B）。
        let release_id = "rel-art-1";
        make_package_with_marker(&ws, release_id, "B");

        // 受控编排器 = 制品内的 orchestrator.sh（cwd=.run）。
        let bin = harness.path().join("run-orchestrator.sh");
        std::fs::write(
            &bin,
            "#!/bin/sh\ncp \"$(pwd)/marker.txt\" \"$(pwd)/served.txt\"\nexec sleep 120\n",
        )
        .expect("write orchestrator");
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
                .expect("chmod orchestrator");
        }
        let unused = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let probe_addr = unused.local_addr().unwrap().to_string();
        drop(unused);
        let mut config = file_server::Config::from_env().expect("test config");
        config.app_cli_bin = Some(bin.display().to_string());
        config.app_cli_admin_probe_addr = probe_addr;
        config.log_base_dir = harness.path().join("logs");
        std::fs::create_dir_all(&config.log_base_dir).expect("logs");
        config.dev_alive_max_wait_ms = 1200;
        config.dev_alive_poll_interval_ms = 100;
        config.dev_alive_check_timeout_ms = 300;
        let manager = std::sync::Arc::new(file_server::service::dev_server::DevServerManager::new(
            std::sync::Arc::new(config),
        ));

        let key = "userapp:artifact-effective";
        let ws_for_stop = ws.clone();
        let prepared = prepare_run_dir(&ws, release_id)
            .await
            .expect("prepare staging");
        let started = manager
            .restart_dev_staged(
                key,
                &ws_for_stop,
                file_server::service::dev_server::DevLaunch {
                    base_path: None,
                    hooks: None,
                    pg: None,
                    request_context: Some("artifact-test"),
                    artifact_release_id: Some(release_id),
                },
                async move { prepared.activate() },
            )
            .await
            .expect("staged restart");
        assert!(started.pid > 0);
        // 本次制品实际生效：.run 内容 == B（旧 A 归档进 .previous）。
        assert_eq!(
            std::fs::read_to_string(ws.join(".run/marker.txt")).expect("new marker"),
            "B",
            "active run dir must be THIS release's content"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join(".previous/marker.txt")).expect("archived marker"),
            "A",
            "the replaced release must be archived, not merged"
        );
        // 编排器在新目录上运行并产出新内容。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        loop {
            if let Ok(content) = std::fs::read_to_string(ws.join(".run/served.txt"))
                && content.trim() == "B"
            {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "orchestrator never produced this release's content"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        manager
            .stop_userapp_dev(key, &ws)
            .await
            .expect("cleanup stop");
    }
}
