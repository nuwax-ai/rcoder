//! Workspace release 包存储层（从 releases.rs 拆出的无状态自由函数）。
//!
//! release 目录结构/index.json 读写/包校验/staging/保留策略等文件系统操作，
//! 与 [`crate::service::AppService`] 无状态耦合；发布流程方法留在 releases.rs。

use std::fs::OpenOptions;
use std::io::Read;
use std::path::{Path, PathBuf};

use download_utils::{DownloadError, extract_zip};
use fs2::FileExt;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::warn;

use crate::models::{AppOperationError, AppResult, ReleaseIndex, ReleaseStatus};
use crate::utils::{map_archive_error, map_io_error};

const RETENTION_MIN: u16 = 2;
const RETENTION_FALLBACK: u16 = 15;
const RETENTION_MAX_FALLBACK: u16 = 100;

pub(crate) async fn ensure_release_dirs(root: &Path) -> AppResult<()> {
    for directory in ["packages", ".incoming", ".staging", ".rollback"] {
        tokio::fs::create_dir_all(root.join(directory))
            .await
            .map_err(|error| map_io_error("create releases directory", error, false))?;
    }
    Ok(())
}

/// 解压并校验 release staging。任一步失败都立即清理，避免损坏包持续占用 PVC。
pub(crate) async fn stage_release_package(
    package: &Path,
    staging: &Path,
    release_id: &str,
) -> AppResult<()> {
    remove_dir_if_exists(staging).await?;
    tokio::fs::create_dir_all(staging)
        .await
        .map_err(|error| map_io_error("create release staging directory", error, false))?;

    let package = package.to_path_buf();
    let staging_owned = staging.to_path_buf();
    let result = async {
        tokio::task::spawn_blocking(move || extract_zip(&package, &staging_owned))
            .await
            .map_err(|error| AppOperationError::Backend(format!("release extract task: {error}")))?
            .map_err(map_archive_error)?;
        validate_staging(staging, release_id).await
    }
    .await;

    if result.is_err()
        && let Err(cleanup_error) = remove_dir_if_exists(staging).await
    {
        warn!(
            staging = %staging.display(),
            error = %cleanup_error,
            "failed to cleanup invalid release staging directory"
        );
    }
    result
}

pub(crate) async fn acquire_lock(path: PathBuf) -> AppResult<std::fs::File> {
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

pub(crate) async fn read_index(root: &Path, retention: u16) -> AppResult<ReleaseIndex> {
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

pub(crate) async fn write_index(root: &Path, index: &ReleaseIndex) -> AppResult<()> {
    let bytes = serde_json::to_vec_pretty(index)
        .map_err(|error| AppOperationError::Backend(format!("serialize release index: {error}")))?;
    let temp = root.join("index.json.tmp");
    let mut file = tokio::fs::File::create(&temp)
        .await
        .map_err(|error| map_io_error("write release index", error, true))?;
    file.write_all(&bytes)
        .await
        .map_err(|error| map_io_error("write release index", error, true))?;
    file.sync_all()
        .await
        .map_err(|error| map_io_error("sync release index", error, true))?;
    drop(file);
    tokio::fs::rename(&temp, root.join("index.json"))
        .await
        .map_err(|error| map_io_error("commit release index", error, true))?;
    let root = root.to_path_buf();
    match tokio::task::spawn_blocking(move || std::fs::File::open(root)?.sync_all()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            // rename 已提交，目录 fsync 在部分 PVC/NFS 上不受支持。此时不能
            // 返回失败，否则调用方可能把 index 已引用的 package 当成孤儿删除。
            warn!(%error, "release index committed but directory sync is unsupported or failed");
        }
        Err(error) => {
            warn!(%error, "release index committed but directory sync task failed");
        }
    }
    Ok(())
}

pub(crate) async fn verify_package(
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

pub(crate) fn plan_retention(index: &mut ReleaseIndex) -> Vec<String> {
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
    // Failed 不参与 retention 计数、永不自动删除（失败现场保留供排查；清理由用户
    // 手动 delete 触发）。此前的过滤让 Failed 既不进 keep 也不占名额，反而必然
    // 落入 remove——每次成功激活都会清空全部失败版本，违反"失败现场保留"承诺。
    for release in &index.releases {
        if release.status == ReleaseStatus::Failed {
            keep.insert(release.release_id.clone());
        }
    }
    if let Some(active) = active {
        keep.insert(active.to_owned());
    }
    let remove: Vec<String> = index
        .releases
        .iter()
        .filter(|release| !keep.contains(&release.release_id))
        .map(|release| release.release_id.clone())
        .collect();
    index
        .releases
        .retain(|release| !remove.contains(&release.release_id));
    remove
}

pub(crate) async fn remove_release_packages(root: &Path, release_ids: &[String]) {
    for release_id in release_ids {
        let path = root.join("packages").join(format!("{release_id}.zip"));
        if let Err(error) = tokio::fs::remove_file(path).await
            && error.kind() != std::io::ErrorKind::NotFound
        {
            warn!(release_id, %error, "failed to remove unreferenced release package");
        }
    }
}

pub(crate) async fn remove_dir_if_exists(path: &Path) -> AppResult<()> {
    if let Err(error) = tokio::fs::remove_dir_all(path).await
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(map_io_error("remove release directory", error, true));
    }
    Ok(())
}

pub(crate) fn release_retention(requested: Option<u16>) -> AppResult<u16> {
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

pub(crate) fn validate_release_id(release_id: &str) -> AppResult<()> {
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

pub(crate) fn validate_sha256(value: &str) -> AppResult<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(AppOperationError::Validation(
            "sha256 must contain exactly 64 hexadecimal characters".into(),
        ));
    }
    Ok(())
}

pub(crate) fn map_download_error(error: DownloadError) -> AppOperationError {
    match error {
        DownloadError::InvalidUrl(message) => AppOperationError::Validation(message),
        other => AppOperationError::Backend(format!("download release failed: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::ReleaseInfo;

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
        let removals = plan_retention(&mut index);
        remove_release_packages(root.path(), &removals).await;
        let ids: std::collections::BTreeSet<_> = index
            .releases
            .iter()
            .map(|item| item.release_id.as_str())
            .collect();
        assert_eq!(ids, std::collections::BTreeSet::from(["active", "recent"]));
        assert!(!root.path().join("packages/old.zip").exists());
    }

    /// Failed 版本不占 retention 名额、永不自动删除（成功激活后失败现场仍在）。
    #[tokio::test]
    async fn retention_keeps_failed_releases_without_consuming_slots() {
        let mut index = ReleaseIndex {
            active_release_id: Some("v3".into()),
            pending_release_id: None,
            previous_release_id: None,
            retention: 2,
            releases: vec![
                release("v1", ReleaseStatus::Prepared, "2026-01-01"),
                release("v2-failed", ReleaseStatus::Failed, "2026-01-02"),
                release("v3", ReleaseStatus::Active, "2026-01-03"),
                release("v4", ReleaseStatus::Prepared, "2026-01-04"),
            ],
        };
        let removals = plan_retention(&mut index);
        // retention=2 → active(v3) + 最近一个非 Failed(v4)；v1 淘汰；v2-failed 保留
        // 且不占名额（否则被淘汰的会是 v4）。
        assert_eq!(removals, vec!["v1".to_string()]);
        let ids: std::collections::BTreeSet<_> = index
            .releases
            .iter()
            .map(|item| item.release_id.as_str())
            .collect();
        assert_eq!(
            ids,
            std::collections::BTreeSet::from(["v2-failed", "v3", "v4"])
        );
    }

    #[tokio::test]
    async fn retention_fifteen_prunes_the_oldest_non_active_release() {
        let root = tempfile::tempdir().expect("release tempdir");
        tokio::fs::create_dir_all(root.path().join("packages"))
            .await
            .expect("packages");
        let mut releases = Vec::new();
        for sequence in 0..16 {
            let id = format!("release-{sequence:02}");
            let status = if sequence == 0 {
                ReleaseStatus::Active
            } else {
                ReleaseStatus::Prepared
            };
            releases.push(release(
                &id,
                status,
                &format!("2026-01-{day:02}", day = sequence + 1),
            ));
            tokio::fs::write(root.path().join("packages").join(format!("{id}.zip")), &id)
                .await
                .expect("package");
        }
        let mut index = ReleaseIndex {
            active_release_id: Some("release-00".into()),
            pending_release_id: None,
            previous_release_id: None,
            retention: 15,
            releases,
        };

        let removals = plan_retention(&mut index);
        remove_release_packages(root.path(), &removals).await;

        assert_eq!(index.releases.len(), 15);
        assert!(
            index
                .releases
                .iter()
                .any(|release| release.release_id == "release-00")
        );
        assert!(
            !index
                .releases
                .iter()
                .any(|release| release.release_id == "release-01")
        );
        assert!(!root.path().join("packages/release-01.zip").exists());
    }

    #[test]
    fn validates_release_identity_and_digest() {
        assert!(validate_release_id("01j-release").is_ok());
        assert!(validate_release_id("../release").is_err());
        assert!(validate_sha256(&"a".repeat(64)).is_ok());
        assert!(validate_sha256("abc").is_err());
    }

    #[tokio::test]
    async fn invalid_staging_is_removed_immediately() {
        let root = tempfile::tempdir().expect("release tempdir");
        let package = root.path().join("broken.zip");
        let staging = root.path().join("staging");
        tokio::fs::write(&package, b"not a zip archive")
            .await
            .expect("write broken package");

        let result = stage_release_package(&package, &staging, "release-1").await;

        assert!(result.is_err());
        assert!(!staging.exists(), "invalid staging must not remain on PVC");
    }
}
