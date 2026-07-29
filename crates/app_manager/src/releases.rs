//! Workspace release package persistence and atomic code switching.

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

use chrono::Utc;
use download_utils::{DownloadConfig, DownloadError, Downloader, extract_zip};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use tracing::{info, instrument};

use crate::models::{
    AppOperationError, AppResult, PrepareReleaseRequest, ReleaseIndex, ReleaseInfo,
    ReleaseListResponse, ReleaseStatus,
};
use crate::service::AppService;
use crate::utils::{map_archive_error, map_io_error, validate_app_id};

const RETENTION_MIN: u16 = 2;
const RETENTION_FALLBACK: u16 = 15;
const RETENTION_MAX_FALLBACK: u16 = 100;

impl AppService {
    #[instrument(skip(self, request))]
    pub async fn prepare_release(
        &self,
        app_id: &str,
        request: PrepareReleaseRequest,
    ) -> AppResult<ReleaseInfo> {
        validate_app_id(app_id)?;
        validate_release_id(&request.release_id)?;
        validate_sha256(&request.sha256)?;
        let retention = release_retention(request.retention)?;
        self.ensure_app_workspace_ready(app_id, None).await?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        ensure_release_dirs(&releases_dir).await?;
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, retention).await?;
        if let Some(existing) = index
            .releases
            .iter()
            .find(|release| release.release_id == request.release_id)
        {
            if existing.sha256 == request.sha256 && existing.size_bytes == request.size_bytes {
                return Ok(existing.clone());
            }
            return Err(AppOperationError::Conflict(format!(
                "release {} already exists with a different digest or size",
                request.release_id
            )));
        }

        let incoming = releases_dir
            .join(".incoming")
            .join(format!("{}.zip.part", request.release_id));
        let package = releases_dir
            .join("packages")
            .join(format!("{}.zip", request.release_id));
        let result = async {
            let downloader = Downloader::new(DownloadConfig::default());
            downloader
                .download_to_file(
                    &request.url,
                    &incoming,
                    Some(&request.sha256),
                    &CancellationToken::new(),
                )
                .await
                .map_err(map_download_error)?;
            verify_package(
                &incoming,
                &request.release_id,
                &request.sha256,
                request.size_bytes,
            )
            .await?;
            tokio::fs::rename(&incoming, &package)
                .await
                .map_err(|error| map_io_error("move prepared release package", error, true))?;
            Ok::<(), AppOperationError>(())
        }
        .await;
        if let Err(error) = result {
            let _ = tokio::fs::remove_file(&incoming).await;
            return Err(error);
        }
        let release = ReleaseInfo {
            release_id: request.release_id,
            sha256: request.sha256.to_ascii_lowercase(),
            size_bytes: request.size_bytes,
            status: ReleaseStatus::Prepared,
            created_at: Utc::now().to_rfc3339(),
            activated_at: None,
            failure_message: None,
        };
        index.retention = retention;
        index.releases.push(release.clone());
        write_index(&releases_dir, &index).await?;
        info!(app_id, release_id = %release.release_id, "release prepared");
        Ok(release)
    }

    #[instrument(skip(self))]
    pub async fn activate_release(&self, app_id: &str, release_id: &str) -> AppResult<ReleaseInfo> {
        validate_app_id(app_id)?;
        validate_release_id(release_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        let release_position = index
            .releases
            .iter()
            .position(|release| release.release_id == release_id)
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("release not found: {release_id}"))
            })?;
        if index.active_release_id.as_deref() == Some(release_id)
            || index.pending_release_id.as_deref() == Some(release_id)
        {
            return Ok(index.releases[release_position].clone());
        }
        if let Some(pending) = &index.pending_release_id {
            return Err(AppOperationError::InvalidState(format!(
                "release {pending} is still pending confirmation"
            )));
        }
        let package = releases_dir
            .join("packages")
            .join(format!("{release_id}.zip"));
        if !package.is_file() {
            return Err(AppOperationError::FileNotFound(format!(
                "release package missing: {}",
                package.display()
            )));
        }
        let staging = releases_dir.join(".staging").join(release_id);
        remove_dir_if_exists(&staging).await?;
        tokio::fs::create_dir_all(&staging)
            .await
            .map_err(|error| map_io_error("create release staging directory", error, false))?;
        let package_clone = package.clone();
        let staging_clone = staging.clone();
        tokio::task::spawn_blocking(move || extract_zip(&package_clone, &staging_clone))
            .await
            .map_err(|error| AppOperationError::Backend(format!("release extract task: {error}")))?
            .map_err(map_archive_error)?;
        validate_staging(&staging, release_id).await?;

        let code = app_dir.join("code");
        let rollback = releases_dir.join(".rollback").join("code");
        remove_dir_if_exists(&rollback).await?;
        let app_exists = self
            .runtime
            .get_deployment_status(app_id)
            .await
            .ok()
            .flatten()
            .is_some();
        if app_exists {
            self.stop_app(app_id).await?;
        }
        if code.exists() {
            tokio::fs::rename(&code, &rollback)
                .await
                .map_err(|error| map_io_error("move active code to rollback", error, true))?;
        }
        if let Err(error) = tokio::fs::rename(&staging, &code).await {
            if rollback.exists() {
                let _ = tokio::fs::rename(&rollback, &code).await;
            }
            return Err(map_io_error("activate staged release", error, true));
        }
        if app_exists && let Err(error) = self.start_app(app_id).await {
            remove_dir_if_exists(&code).await?;
            if rollback.exists() {
                tokio::fs::rename(&rollback, &code)
                    .await
                    .map_err(|restore| map_io_error("restore previous release", restore, true))?;
                let _ = self.start_app(app_id).await;
            }
            index.releases[release_position].status = ReleaseStatus::Failed;
            index.releases[release_position].failure_message = Some(error.to_string());
            let _ = tokio::fs::remove_file(&package).await;
            write_index(&releases_dir, &index).await?;
            return Err(error);
        }
        index.previous_release_id = index.active_release_id.clone();
        index.pending_release_id = Some(release_id.to_owned());
        index.releases[release_position].status = ReleaseStatus::PendingStart;
        index.releases[release_position].activated_at = Some(Utc::now().to_rfc3339());
        let release = index.releases[release_position].clone();
        write_index(&releases_dir, &index).await?;
        Ok(release)
    }

    pub async fn confirm_release(
        &self,
        app_id: &str,
        release_id: &str,
        healthy: bool,
        message: Option<String>,
    ) -> AppResult<ReleaseInfo> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        if index.pending_release_id.as_deref() != Some(release_id) {
            return Err(AppOperationError::InvalidState(format!(
                "release is not pending confirmation: {release_id}"
            )));
        }
        let position = index
            .releases
            .iter()
            .position(|release| release.release_id == release_id)
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("release not found: {release_id}"))
            })?;
        if !healthy {
            let code = app_dir.join("code");
            let rollback = releases_dir.join(".rollback").join("code");
            if rollback.exists() {
                let _ = self.stop_app(app_id).await;
                remove_dir_if_exists(&code).await?;
                tokio::fs::rename(&rollback, &code).await.map_err(|error| {
                    map_io_error(
                        "restore previous release after readiness failure",
                        error,
                        true,
                    )
                })?;
                self.start_app(app_id).await?;
            }
            index.releases[position].status = ReleaseStatus::Failed;
            index.releases[position].failure_message =
                message.or_else(|| Some("readiness confirmation failed".into()));
            index.pending_release_id = None;
            let package = releases_dir
                .join("packages")
                .join(format!("{release_id}.zip"));
            let _ = tokio::fs::remove_file(package).await;
            write_index(&releases_dir, &index).await?;
            return Err(AppOperationError::InvalidState(format!(
                "release readiness failed: {release_id}"
            )));
        }
        for release in &mut index.releases {
            if release.status == ReleaseStatus::Active {
                release.status = ReleaseStatus::Prepared;
            }
        }
        index.releases[position].status = ReleaseStatus::Active;
        index.releases[position].failure_message = None;
        index.active_release_id = Some(release_id.to_owned());
        index.pending_release_id = None;
        let release = index.releases[position].clone();
        cleanup_retention(&releases_dir, &mut index).await?;
        remove_dir_if_exists(&releases_dir.join(".rollback").join("code")).await?;
        write_index(&releases_dir, &index).await?;
        Ok(release)
    }

    pub async fn list_releases(&self, app_id: &str) -> AppResult<ReleaseListResponse> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let index = read_index(&app_dir.join("releases"), release_retention(None)?).await?;
        Ok(ReleaseListResponse {
            active_release_id: index.active_release_id,
            pending_release_id: index.pending_release_id,
            retention: index.retention,
            releases: index.releases,
        })
    }

    pub async fn delete_release(&self, app_id: &str, release_id: &str) -> AppResult<()> {
        validate_app_id(app_id)?;
        validate_release_id(release_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        if index.active_release_id.as_deref() == Some(release_id)
            || index.pending_release_id.as_deref() == Some(release_id)
        {
            return Err(AppOperationError::InvalidState(format!(
                "active or pending release cannot be deleted: {release_id}"
            )));
        }
        let before = index.releases.len();
        index
            .releases
            .retain(|release| release.release_id != release_id);
        if before == index.releases.len() {
            return Err(AppOperationError::NotFound(format!(
                "release not found: {release_id}"
            )));
        }
        let package = releases_dir
            .join("packages")
            .join(format!("{release_id}.zip"));
        if let Err(error) = tokio::fs::remove_file(&package).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(map_io_error("delete release package", error, true));
        }
        write_index(&releases_dir, &index).await
    }
}

async fn ensure_release_dirs(root: &Path) -> AppResult<()> {
    for directory in ["packages", ".incoming", ".staging", ".rollback"] {
        tokio::fs::create_dir_all(root.join(directory))
            .await
            .map_err(|error| map_io_error("create releases directory", error, false))?;
    }
    Ok(())
}

async fn acquire_lock(path: PathBuf) -> AppResult<std::fs::File> {
    tokio::task::spawn_blocking(move || {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|error| map_io_error("open release operation lock", error, false))?;
        file.lock_exclusive()
            .map_err(|error| map_io_error("lock release operation", error, false))?;
        Ok(file)
    })
    .await
    .map_err(|error| AppOperationError::Backend(format!("release lock task: {error}")))?
}

async fn read_index(root: &Path, retention: u16) -> AppResult<ReleaseIndex> {
    let path = root.join("index.json");
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| AppOperationError::Backend(format!("parse release index: {error}"))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ReleaseIndex {
            retention,
            ..ReleaseIndex::default()
        }),
        Err(error) => Err(map_io_error("read release index", error, false)),
    }
}

async fn write_index(root: &Path, index: &ReleaseIndex) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(index)
        .map_err(|error| AppOperationError::Backend(format!("serialize release index: {error}")))?;
    let temp = root.join("index.json.tmp");
    tokio::fs::write(&temp, bytes)
        .await
        .map_err(|error| map_io_error("write release index", error, true))?;
    tokio::fs::rename(&temp, root.join("index.json"))
        .await
        .map_err(|error| map_io_error("commit release index", error, true))
}

async fn verify_package(
    path: &Path,
    release_id: &str,
    expected_sha256: &str,
    expected_size: u64,
) -> AppResult<()> {
    let path = path.to_path_buf();
    let release_id = release_id.to_owned();
    let expected_sha256 = expected_sha256.to_ascii_lowercase();
    tokio::task::spawn_blocking(move || {
        let metadata = std::fs::metadata(&path)
            .map_err(|error| map_io_error("stat release package", error, false))?;
        if metadata.len() != expected_size {
            return Err(AppOperationError::Validation(format!(
                "release size mismatch: expected {expected_size}, got {}",
                metadata.len()
            )));
        }
        let mut file = std::fs::File::open(&path)
            .map_err(|error| map_io_error("open release package", error, false))?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|error| map_io_error("hash release package", error, false))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let actual = hex::encode(hasher.finalize());
        if actual != expected_sha256 {
            return Err(AppOperationError::Validation(format!(
                "release sha256 mismatch: expected {expected_sha256}, got {actual}"
            )));
        }
        let file = std::fs::File::open(&path)
            .map_err(|error| map_io_error("open release zip", error, false))?;
        let mut archive = zip::ZipArchive::new(file).map_err(|error| {
            AppOperationError::Validation(format!("invalid release zip: {error}"))
        })?;
        let mut lock = archive.by_name("release.lock.toml").map_err(|error| {
            AppOperationError::Validation(format!("release lock missing: {error}"))
        })?;
        let mut content = String::new();
        lock.read_to_string(&mut content)
            .map_err(|error| map_io_error("read release lock", error, false))?;
        let value: toml::Value = toml::from_str(&content).map_err(|error| {
            AppOperationError::Validation(format!("invalid release lock: {error}"))
        })?;
        if value.get("release_id").and_then(toml::Value::as_str) != Some(release_id.as_str()) {
            return Err(AppOperationError::Validation(
                "release lock ID does not match requested release ID".into(),
            ));
        }
        Ok(())
    })
    .await
    .map_err(|error| AppOperationError::Backend(format!("verify release task: {error}")))?
}

async fn validate_staging(path: &Path, release_id: &str) -> AppResult<()> {
    if !path.join("workspace.manifest.toml").is_file() || !path.join("release.lock.toml").is_file()
    {
        return Err(AppOperationError::Validation(
            "release must contain workspace.manifest.toml and release.lock.toml".into(),
        ));
    }
    let content = tokio::fs::read_to_string(path.join("release.lock.toml"))
        .await
        .map_err(|error| map_io_error("read staged release lock", error, false))?;
    let value: toml::Value = toml::from_str(&content).map_err(|error| {
        AppOperationError::Validation(format!("parse staged release lock: {error}"))
    })?;
    if value.get("release_id").and_then(toml::Value::as_str) != Some(release_id) {
        return Err(AppOperationError::Validation(
            "staged release lock ID mismatch".into(),
        ));
    }
    Ok(())
}

async fn cleanup_retention(root: &Path, index: &mut ReleaseIndex) -> AppResult<()> {
    let active = index.active_release_id.as_deref();
    let mut ordered: Vec<_> = index
        .releases
        .iter()
        .filter(|release| {
            release.status != ReleaseStatus::Failed && active != Some(release.release_id.as_str())
        })
        .map(|release| (release.created_at.clone(), release.release_id.clone()))
        .collect();
    ordered.sort_by(|left, right| right.0.cmp(&left.0));
    let mut keep: std::collections::BTreeSet<String> = ordered
        .into_iter()
        .take(usize::from(index.retention.saturating_sub(1)))
        .map(|(_, id)| id)
        .collect();
    if let Some(active) = active {
        keep.insert(active.to_owned());
    }
    let remove: Vec<String> = index
        .releases
        .iter()
        .filter(|release| !keep.contains(&release.release_id))
        .map(|release| release.release_id.clone())
        .collect();
    for release_id in &remove {
        let path = root.join("packages").join(format!("{release_id}.zip"));
        if let Err(error) = tokio::fs::remove_file(path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            return Err(map_io_error("clean retained release", error, true));
        }
    }
    index
        .releases
        .retain(|release| !remove.contains(&release.release_id));
    Ok(())
}

async fn remove_dir_if_exists(path: &Path) -> AppResult<()> {
    if let Err(error) = tokio::fs::remove_dir_all(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(map_io_error("remove release directory", error, true));
    }
    Ok(())
}

fn release_retention(requested: Option<u16>) -> AppResult<u16> {
    let default = env_u16("RCODER_APP_RELEASE_RETENTION_DEFAULT", RETENTION_FALLBACK)?;
    let maximum = env_u16("RCODER_APP_RELEASE_RETENTION_MAX", RETENTION_MAX_FALLBACK)?;
    if !(RETENTION_MIN..=RETENTION_MAX_FALLBACK).contains(&maximum) {
        return Err(AppOperationError::Validation(format!(
            "RCODER_APP_RELEASE_RETENTION_MAX must be {RETENTION_MIN}..={RETENTION_MAX_FALLBACK}"
        )));
    }
    let value = requested.unwrap_or(default);
    if value < RETENTION_MIN || value > maximum {
        return Err(AppOperationError::Validation(format!(
            "release retention must be {RETENTION_MIN}..={maximum}, got {value}"
        )));
    }
    Ok(value)
}

fn env_u16(name: &str, fallback: u16) -> AppResult<u16> {
    match std::env::var(name) {
        Ok(value) => value.parse().map_err(|error| {
            AppOperationError::Validation(format!("invalid {name}={value}: {error}"))
        }),
        Err(std::env::VarError::NotPresent) => Ok(fallback),
        Err(error) => Err(AppOperationError::Validation(format!(
            "read {name}: {error}"
        ))),
    }
}

fn validate_release_id(release_id: &str) -> AppResult<()> {
    if release_id.is_empty()
        || release_id.len() > 64
        || !release_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(AppOperationError::Validation(
            "release_id must be 1-64 ASCII alphanumeric/hyphen characters".into(),
        ));
    }
    Ok(())
}

fn validate_sha256(value: &str) -> AppResult<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppOperationError::Validation(
            "sha256 must contain exactly 64 hexadecimal characters".into(),
        ));
    }
    Ok(())
}

fn map_download_error(error: DownloadError) -> AppOperationError {
    match error {
        DownloadError::InvalidUrl(message) => AppOperationError::Validation(message),
        other => AppOperationError::Backend(format!("download release failed: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(id: &str, status: ReleaseStatus, created_at: &str) -> ReleaseInfo {
        ReleaseInfo {
            release_id: id.into(),
            sha256: "0".repeat(64),
            size_bytes: 1,
            status,
            created_at: created_at.into(),
            activated_at: None,
            failure_message: None,
        }
    }

    #[tokio::test]
    async fn retention_keeps_active_plus_recent_other_versions() {
        let root = tempfile::tempdir().expect("release tempdir");
        tokio::fs::create_dir_all(root.path().join("packages"))
            .await
            .expect("packages");
        let mut index = ReleaseIndex {
            active_release_id: Some("active".into()),
            pending_release_id: None,
            previous_release_id: None,
            retention: 2,
            releases: vec![
                release("active", ReleaseStatus::Active, "2026-01-01"),
                release("recent", ReleaseStatus::Prepared, "2026-01-03"),
                release("old", ReleaseStatus::Prepared, "2026-01-02"),
            ],
        };
        for id in ["active", "recent", "old"] {
            tokio::fs::write(root.path().join("packages").join(format!("{id}.zip")), id)
                .await
                .expect("package");
        }
        cleanup_retention(root.path(), &mut index)
            .await
            .expect("cleanup");
        let ids: std::collections::BTreeSet<_> = index
            .releases
            .iter()
            .map(|item| item.release_id.as_str())
            .collect();
        assert_eq!(ids, std::collections::BTreeSet::from(["active", "recent"]));
        assert!(!root.path().join("packages/old.zip").exists());
    }

    #[test]
    fn validates_release_identity_and_digest() {
        assert!(validate_release_id("01j-release").is_ok());
        assert!(validate_release_id("../release").is_err());
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256("abc").is_err());
    }
}
