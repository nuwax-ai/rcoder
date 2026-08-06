//! Workspace release package persistence and atomic code switching.

use std::sync::Arc;

use chrono::Utc;
use download_utils::{DownloadConfig, Downloader};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, instrument, warn};

use crate::models::{
    AppOperationError, AppResult, PrepareReleaseRequest, ReleaseInfo, ReleaseListResponse,
    ReleaseStatus,
};
use crate::release_store::{
    acquire_lock, code_release_id, ensure_release_dirs, map_download_error, plan_retention,
    read_index, release_retention, remove_dir_if_exists, remove_release_packages,
    stage_release_package, validate_release_id, validate_sha256, verify_package, write_index,
};
use crate::service::AppService;
use crate::utils::{map_io_error, validate_app_id};

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
        let _process_lock = self.acquire_process_release_lock(app_id).await;
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
            if let Err(cleanup_err) = tokio::fs::remove_file(&incoming).await {
                warn!(
                    "[app_manager] Failed to cleanup incoming file {} after prepare failure: {}",
                    incoming.display(),
                    cleanup_err
                );
            }
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
        if let Err(error) = write_index(&releases_dir, &index).await {
            if let Err(cleanup_error) = tokio::fs::remove_file(&package).await {
                warn!(
                    path = %package.display(),
                    %cleanup_error,
                    "failed to remove orphaned release package after index commit failure"
                );
            }
            return Err(error);
        }
        info!(app_id, release_id = %release.release_id, "release prepared");
        Ok(release)
    }

    #[instrument(skip(self))]
    pub async fn activate_release(&self, app_id: &str, release_id: &str) -> AppResult<ReleaseInfo> {
        validate_app_id(app_id)?;
        validate_release_id(release_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _process_lock = self.acquire_process_release_lock(app_id).await;
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        let release_position = index
            .releases
            .iter()
            .position(|release| release.release_id == release_id)
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("release not found: {release_id}"))
            })?;
        if index.active_release_id.as_deref() == Some(release_id) {
            return Ok(index.releases[release_position].clone());
        }
        if index.pending_release_id.as_deref() == Some(release_id) {
            let code = app_dir.join("code");
            let rollback = releases_dir.join(".rollback").join("code");
            if code_release_id(&code).await?.as_deref() == Some(release_id) {
                let app_exists = self
                    .runtime
                    .get_deployment_status(app_id)
                    .await
                    .map_err(|error| {
                        crate::utils::map_runtime_error(
                            &format!("[APP] recover pending activation failed app_id={app_id}"),
                            error,
                        )
                    })?
                    .is_some();
                if app_exists {
                    self.start_app(app_id).await?;
                }
                return Ok(index.releases[release_position].clone());
            }
            if !code.exists() && rollback.exists() {
                tokio::fs::rename(&rollback, &code)
                    .await
                    .map_err(|error| map_io_error("recover interrupted activation", error, true))?;
            }
            index.pending_release_id = None;
            index.releases[release_position].status = ReleaseStatus::Prepared;
            write_index(&releases_dir, &index).await?;
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
        stage_release_package(&package, &staging, release_id).await?;

        let code = app_dir.join("code");
        let rollback = releases_dir.join(".rollback").join("code");
        remove_dir_if_exists(&rollback).await?;
        let app_exists = match self.runtime.get_deployment_status(app_id).await {
            Ok(status) => status.is_some(),
            Err(error) => {
                remove_dir_if_exists(&staging).await?;
                return Err(crate::utils::map_runtime_error(
                    &format!("[APP] check app existence before activation failed app_id={app_id}"),
                    error,
                ));
            }
        };
        index.previous_release_id = index.active_release_id.clone();
        index.pending_release_id = Some(release_id.to_owned());
        index.releases[release_position].status = ReleaseStatus::PendingStart;
        index.releases[release_position].activated_at = Some(Utc::now().to_rfc3339());
        write_index(&releases_dir, &index).await?;
        if app_exists && let Err(error) = self.stop_app(app_id).await {
            remove_dir_if_exists(&staging).await?;
            index.pending_release_id = None;
            index.releases[release_position].status = ReleaseStatus::Prepared;
            write_index(&releases_dir, &index).await?;
            return Err(error);
        }
        if code.exists()
            && let Err(error) = tokio::fs::rename(&code, &rollback).await
        {
            index.pending_release_id = None;
            index.releases[release_position].status = ReleaseStatus::Prepared;
            write_index(&releases_dir, &index).await?;
            if app_exists && let Err(restart_error) = self.start_app(app_id).await {
                error!(app_id, %restart_error, "failed to restart app after code move failure");
            }
            return Err(map_io_error("move active code to rollback", error, true));
        }
        if let Err(error) = tokio::fs::rename(&staging, &code).await {
            let mut restore_failed = false;
            if rollback.exists()
                && let Err(rollback_error) = tokio::fs::rename(&rollback, &code).await
            {
                restore_failed = true;
                error!(app_id, %rollback_error, "rollback rename failed after activation error");
            }
            index.pending_release_id = None;
            index.releases[release_position].status = if restore_failed {
                ReleaseStatus::Failed
            } else {
                ReleaseStatus::Prepared
            };
            write_index(&releases_dir, &index).await?;
            if app_exists
                && !restore_failed
                && let Err(restart_error) = self.start_app(app_id).await
            {
                error!(app_id, %restart_error, "failed to restart restored app after activation error");
            }
            return Err(map_io_error("activate staged release", error, true));
        }
        if app_exists && let Err(error) = self.start_app(app_id).await {
            warn!(
                "[app_manager] Failed to restart app {} after activation: {}, attempting rollback",
                app_id, error
            );
            if let Err(cleanup_err) = remove_dir_if_exists(&code).await {
                warn!(
                    "[app_manager] Failed to cleanup code dir {} during rollback: {}",
                    code.display(),
                    cleanup_err
                );
            }
            if rollback.exists() {
                tokio::fs::rename(&rollback, &code)
                    .await
                    .map_err(|restore| map_io_error("restore previous release", restore, true))?;
                if let Err(restart_err) = self.start_app(app_id).await {
                    error!(
                        "[app_manager] CRITICAL: rollback restart failed for {}: {}. App may be down.",
                        app_id, restart_err
                    );
                }
            }
            index.releases[release_position].status = ReleaseStatus::Failed;
            index.releases[release_position].failure_message = Some(error.to_string());
            index.pending_release_id = None;
            write_index(&releases_dir, &index).await?;
            if let Err(e) = tokio::fs::remove_file(&package).await {
                debug!(
                    "[app_manager] Failed to cleanup package {}: {}",
                    package.display(),
                    e
                );
            }
            return Err(error);
        }
        let release = index.releases[release_position].clone();
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
        validate_release_id(release_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _process_lock = self.acquire_process_release_lock(app_id).await;
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        let position = index
            .releases
            .iter()
            .position(|release| release.release_id == release_id)
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("release not found: {release_id}"))
            })?;
        if index.pending_release_id.as_deref() != Some(release_id) {
            let release = &index.releases[position];
            if (healthy
                && index.active_release_id.as_deref() == Some(release_id)
                && release.status == ReleaseStatus::Active)
                || (!healthy && release.status == ReleaseStatus::Failed)
            {
                return Ok(release.clone());
            }
            return Err(AppOperationError::InvalidState(format!(
                "release is not pending confirmation: {release_id}"
            )));
        }
        if !healthy {
            warn!(
                "[app_manager] Release {} for app {} unhealthy, initiating rollback",
                release_id, app_id
            );
            let code = app_dir.join("code");
            let rollback = releases_dir.join(".rollback").join("code");
            // 不论是升级还是首次发布，都必须先停止失败运行时，再操作 code。
            match self.stop_app(app_id).await {
                Ok(_) | Err(AppOperationError::NotFound(_)) => {}
                Err(error) => {
                    return Err(AppOperationError::Backend(format!(
                        "failed to stop app before release rollback: {error}"
                    )));
                }
            }
            remove_dir_if_exists(&code).await?;
            let mut restart_error = None;
            if rollback.exists() {
                tokio::fs::rename(&rollback, &code).await.map_err(|error| {
                    map_io_error(
                        "restore previous release after readiness failure",
                        error,
                        true,
                    )
                })?;
                if let Err(restart_err) = self.start_app(app_id).await {
                    restart_error = Some(restart_err);
                }
            } else {
                // 首次发布失败（无旧版本可回滚）：code 已被上方清空，必须清理残留运行时资源，
                // 否则留下 Deployment 空壳 + 空 code 的坏状态。
                // 注意：此处**不可调 delete_app** —— 它会再次 acquire_process_release_lock
                // （tokio Mutex 不可重入），在 confirm 持锁分支内调用会死锁；
                // 故内联 delete_app 内核中的免锁清理序列（见 service.rs delete_app）。
                // 整个清理 best-effort：任何一步失败仅记日志，绝不 return Err，
                // 否则会阻断下方 `pending_release_id = None` + write_index，重蹈 pending 卡死。
                // PVC 保留（purge=false 语义）：K8s SSA apply 幂等，同名重建仍可成功。
                self.unregister_pingora_backends(app_id).await;
                if let Err(error) = self.runtime.delete_deployment(app_id).await {
                    // Deployment 可能从未建成（NotFound 属正常）；其他错误仅记 warn 不阻断
                    warn!(
                        "[app_manager] Failed to cleanup deployment for failed first release {} (NotFound tolerated): {}",
                        app_id, error
                    );
                }
                self.activity.forget_app(app_id);
            }
            index.releases[position].status = ReleaseStatus::Failed;
            index.releases[position].failure_message =
                message.or_else(|| Some("readiness confirmation failed".into()));
            index.pending_release_id = None;
            write_index(&releases_dir, &index).await?;
            let package = releases_dir
                .join("packages")
                .join(format!("{release_id}.zip"));
            if let Err(e) = tokio::fs::remove_file(package).await {
                debug!(
                    "[app_manager] Failed to cleanup package for failed release {}: {}",
                    release_id, e
                );
            }
            if let Some(restart_error) = restart_error {
                return Err(AppOperationError::Backend(format!(
                    "rollback restored previous code but restart failed for {app_id}: {restart_error}"
                )));
            }
            return Ok(index.releases[position].clone());
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
        // 先持久化 Active 权威状态。后续保留清理均可重试，不能让清理失败破坏发布确认。
        write_index(&releases_dir, &index).await?;
        let removals = plan_retention(&mut index);
        if !removals.is_empty() {
            write_index(&releases_dir, &index).await?;
            remove_release_packages(&releases_dir, &removals).await;
        }
        if let Err(error) = remove_dir_if_exists(&releases_dir.join(".rollback").join("code")).await
        {
            warn!(app_id, release_id, %error, "failed to remove rollback directory after confirm");
        }
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
        let _process_lock = self.acquire_process_release_lock(app_id).await;
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
        // 先从权威索引移除，再 best-effort 删除包；删除失败只留下可回收孤儿文件。
        write_index(&releases_dir, &index).await?;
        remove_release_packages(&releases_dir, &[release_id.to_owned()]).await;
        Ok(())
    }

    /// 中止 pending 发布（index-only compare-and-clear，运维自救 API）。
    ///
    /// 针对 `confirm_release(healthy=false)` 自身失败导致 `pending_release_id` 永久残留、
    /// activate 守卫挡住后续所有发布、且 delete_release 拒删 pending 的死局。
    /// 仅当 index 的 pending 恰好指向 `release_id` 时（CAS 语义）：置 Failed + 清 pending。
    /// **仅改 index，不做任何文件/运行时操作**——这是 confirm 自身失败后仍能成功的最小操作。
    pub async fn abort_release(
        &self,
        app_id: &str,
        release_id: &str,
        message: Option<String>,
    ) -> AppResult<ReleaseInfo> {
        validate_app_id(app_id)?;
        validate_release_id(release_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _process_lock = self.acquire_process_release_lock(app_id).await;
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        let position = index
            .releases
            .iter()
            .position(|release| release.release_id == release_id)
            .ok_or_else(|| {
                AppOperationError::NotFound(format!("release not found: {release_id}"))
            })?;
        if index.pending_release_id.as_deref() != Some(release_id) {
            // 幂等：目标 release 已 Failed 且非 pending → 视为已中止（对齐 confirm_release 先例）。
            let release = &index.releases[position];
            if release.status == ReleaseStatus::Failed {
                return Ok(release.clone());
            }
            return Err(AppOperationError::InvalidState(format!(
                "release is not pending confirmation: {release_id}"
            )));
        }
        index.releases[position].status = ReleaseStatus::Failed;
        index.releases[position].failure_message =
            message.or_else(|| Some("release aborted".into()));
        index.pending_release_id = None;
        write_index(&releases_dir, &index).await?;
        warn!(
            app_id,
            release_id, "release aborted: pending_release_id cleared"
        );
        Ok(index.releases[position].clone())
    }

    pub(super) async fn acquire_process_release_lock(
        &self,
        app_id: &str,
    ) -> tokio::sync::OwnedMutexGuard<()> {
        let lock = match self.release_locks.entry(app_id.to_owned()) {
            dashmap::mapref::entry::Entry::Occupied(entry) => entry.get().clone(),
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                let lock = Arc::new(tokio::sync::Mutex::new(()));
                entry.insert(lock.clone());
                lock
            }
        };
        lock.lock_owned().await
    }

    pub(super) fn remove_unused_process_release_lock(&self, app_id: &str) {
        if let dashmap::mapref::entry::Entry::Occupied(entry) =
            self.release_locks.entry(app_id.to_owned())
            && Arc::strong_count(entry.get()) == 1
        {
            entry.remove();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::models::ReleaseIndex;
    use crate::test_support::{MockRuntime, test_service};

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

    /// 铺设“首次发布 pending 中”现场：index 含 pending release，无 .rollback/code（首次发布）。
    async fn seed_pending_first_release(root: &Path, app_id: &str) -> PathBuf {
        let app_dir = root.join(app_id);
        let releases_dir = app_dir.join("releases");
        ensure_release_dirs(&releases_dir)
            .await
            .expect("ensure release dirs");
        let mut index = ReleaseIndex {
            retention: 15,
            pending_release_id: Some("release-1".into()),
            ..ReleaseIndex::default()
        };
        index.releases.push(release(
            "release-1",
            ReleaseStatus::PendingStart,
            "2026-08-01",
        ));
        write_index(&releases_dir, &index)
            .await
            .expect("write pending index");
        assert!(
            !releases_dir.join(".rollback").join("code").exists(),
            "first release must have no rollback source"
        );
        releases_dir
    }

    /// R1：首次发布 confirm(false) 且无 rollback——必须 best-effort 清理 Deployment，
    /// 且 pending_release_id 被清、write_index 成功。
    #[tokio::test]
    async fn confirm_unhealthy_first_release_cleans_up_without_rollback() {
        let root = tempfile::tempdir().expect("release tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime.clone());
        let app_id = "app-r1";
        let releases_dir = seed_pending_first_release(root.path(), app_id).await;

        let release = service
            .confirm_release(app_id, "release-1", false, Some("readiness failed".into()))
            .await
            .expect("confirm must succeed");

        assert_eq!(release.status, ReleaseStatus::Failed);
        assert_eq!(
            runtime
                .delete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "delete_deployment must be called for failed first release"
        );
        let index = read_index(&releases_dir, 15)
            .await
            .expect("write_index must have succeeded");
        assert!(
            index.pending_release_id.is_none(),
            "pending_release_id must be cleared"
        );
        assert!(
            !root.path().join(app_id).join("code").exists(),
            "failed first release must not leave code dir"
        );
    }

    /// R1：清理失败（delete_deployment 报错）绝不能阻断 pending 清理与 write_index。
    #[tokio::test]
    async fn confirm_unhealthy_first_release_cleanup_failure_does_not_block_commit() {
        let root = tempfile::tempdir().expect("release tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime
            .delete_fails
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let service = test_service(root.path(), runtime.clone());
        let app_id = "app-r1-fail";
        let releases_dir = seed_pending_first_release(root.path(), app_id).await;

        let release = service
            .confirm_release(app_id, "release-1", false, None)
            .await
            .expect("cleanup failure must not break confirm");

        assert_eq!(release.status, ReleaseStatus::Failed);
        assert_eq!(
            runtime
                .delete_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        let index = read_index(&releases_dir, 15)
            .await
            .expect("write_index must have succeeded despite cleanup failure");
        assert!(
            index.pending_release_id.is_none(),
            "pending must be cleared even when cleanup fails (no pending deadlock)"
        );
    }

    /// R3：CAS 匹配——pending 恰为目标 release → abort 成功、状态 Failed、pending 清空。
    #[tokio::test]
    async fn abort_release_clears_matching_pending() {
        let root = tempfile::tempdir().expect("release tempdir");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let app_id = "app-r3-abort";
        let releases_dir = seed_pending_first_release(root.path(), app_id).await;

        let release = service
            .abort_release(app_id, "release-1", Some("confirm failed hard".into()))
            .await
            .expect("abort must succeed when pending matches");

        assert_eq!(release.status, ReleaseStatus::Failed);
        assert_eq!(
            release.failure_message.as_deref(),
            Some("confirm failed hard")
        );
        let index = read_index(&releases_dir, 15).await.expect("read index");
        assert!(
            index.pending_release_id.is_none(),
            "pending_release_id must be cleared by abort"
        );
    }

    /// R3：CAS 不匹配——pending 指向别的 release → InvalidState。
    #[tokio::test]
    async fn abort_release_rejects_non_matching_release() {
        let root = tempfile::tempdir().expect("release tempdir");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let app_id = "app-r3-mismatch";
        let app_dir = root.path().join(app_id);
        let releases_dir = app_dir.join("releases");
        ensure_release_dirs(&releases_dir)
            .await
            .expect("ensure release dirs");
        let mut index = ReleaseIndex {
            retention: 15,
            pending_release_id: Some("release-1".into()),
            ..ReleaseIndex::default()
        };
        index.releases.push(release(
            "release-1",
            ReleaseStatus::PendingStart,
            "2026-08-01",
        ));
        index
            .releases
            .push(release("release-2", ReleaseStatus::Prepared, "2026-08-02"));
        write_index(&releases_dir, &index)
            .await
            .expect("write index");

        let error = service
            .abort_release(app_id, "release-2", None)
            .await
            .expect_err("abort must fail when pending points elsewhere");

        assert!(
            matches!(error, AppOperationError::InvalidState(_)),
            "expected InvalidState, got {error:?}"
        );
        let index = read_index(&releases_dir, 15).await.expect("read index");
        assert_eq!(index.pending_release_id.as_deref(), Some("release-1"));
    }

    /// R3：幂等——目标 release 已 Failed 且非 pending → 返回 Ok。
    #[tokio::test]
    async fn abort_release_is_idempotent_for_already_failed_release() {
        let root = tempfile::tempdir().expect("release tempdir");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let app_id = "app-r3-idempotent";
        let app_dir = root.path().join(app_id);
        let releases_dir = app_dir.join("releases");
        ensure_release_dirs(&releases_dir)
            .await
            .expect("ensure release dirs");
        let mut index = ReleaseIndex {
            retention: 15,
            ..ReleaseIndex::default()
        };
        index.releases.push(ReleaseInfo {
            failure_message: Some("earlier failure".into()),
            ..release("release-1", ReleaseStatus::Failed, "2026-08-01")
        });
        write_index(&releases_dir, &index)
            .await
            .expect("write index");

        let release = service
            .abort_release(app_id, "release-1", None)
            .await
            .expect("abort must be idempotent for already-failed release");

        assert_eq!(release.status, ReleaseStatus::Failed);
        assert_eq!(release.failure_message.as_deref(), Some("earlier failure"));
    }

    /// R3：release 不存在 → NotFound。
    #[tokio::test]
    async fn abort_release_returns_not_found_for_missing_release() {
        let root = tempfile::tempdir().expect("release tempdir");
        let service = test_service(root.path(), Arc::new(MockRuntime::default()));
        let app_id = "app-r3-missing";
        let app_dir = root.path().join(app_id);
        ensure_release_dirs(&app_dir.join("releases"))
            .await
            .expect("ensure release dirs");

        let error = service
            .abort_release(app_id, "no-such-release", None)
            .await
            .expect_err("abort must fail for missing release");

        assert!(
            matches!(error, AppOperationError::NotFound(_)),
            "expected NotFound, got {error:?}"
        );
    }
}
