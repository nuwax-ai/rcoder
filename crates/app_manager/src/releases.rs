//! Workspace release package persistence and atomic code switching.

use std::sync::Arc;

use chrono::Utc;
use download_utils::{DownloadConfig, Downloader};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, instrument, warn};

use crate::models::{
    AppOperationError, AppResult, PrepareReleaseRequest, ReleaseInfo, ReleaseListResponse,
    ReleaseStatus,
};
use crate::release_store::{
    acquire_lock, ensure_release_dirs, map_download_error, plan_retention, read_code_release_id,
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
        // 与 activate/rollback/list/delete 一致:读后归一化,append 后的 write_index
        // 才不会把遗留 PendingStart 原样持久化回去。
        index.normalize_legacy_pending();
        if let Some(existing) = index
            .releases
            .iter()
            .find(|release| release.release_id == request.release_id)
        {
            // 幂等键归一化：存储侧恒 lowercase（append 时 to_ascii_lowercase），大写
            // hex 重试同样幂等命中，而非误报"digest 不一致"409。
            if existing.sha256 == request.sha256.to_ascii_lowercase()
                && existing.size_bytes == request.size_bytes
            {
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

    /// 激活发布（单接口：切流 → ensure 运行容器 → 等就绪 → 提交/失败）。
    ///
    /// - **成功**（就绪）：旧 Active 让位 Prepared、本 release 置 Active、清 `.rollback`、retention。
    /// - **就绪失败/启动失败**：置 Failed 并 **保留现场**（code=失败版、`.rollback`=上一版、
    ///   制品包保留）——自动回滚会销毁排查线索，恢复动作显式化（`rollback_release`）。
    ///   就绪失败返回 `Ok(ReleaseInfo{status:Failed})`（发布结果，非系统错误，不触发调用方
    ///   对 5xx 的重试）；操作性错误（制品缺失/IO）仍返回 `Err`。
    /// - 幂等：重复 activate 已 Active 的 release 直接返回。
    /// - `readiness_timeout` None=默认 300s（范围 clamp 兜底；handler 层已校验 5..=1800 → 400）。
    /// - 崩溃窗口（切流中途中断）：index 停留在激活前状态，code 可能已是新版——恢复用
    ///   `rollback_release`（`.rollback` 快照在）或重新 activate 同一 release。
    #[instrument(skip(self))]
    pub async fn activate_release(
        &self,
        app_id: &str,
        release_id: &str,
        readiness_timeout: Option<u64>,
    ) -> AppResult<ReleaseInfo> {
        use crate::release_runtime::{
            DEFAULT_READY_TIMEOUT_SECS, MAX_READY_TIMEOUT_SECS, MIN_READY_TIMEOUT_SECS,
        };
        validate_app_id(app_id)?;
        validate_release_id(release_id)?;
        let timeout_secs = readiness_timeout
            .unwrap_or(DEFAULT_READY_TIMEOUT_SECS)
            .clamp(MIN_READY_TIMEOUT_SECS, MAX_READY_TIMEOUT_SECS);
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _process_lock = self.acquire_process_release_lock(app_id).await;
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        index.normalize_legacy_pending();
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

        // 激活序列。**index 在序列内不写**（崩溃窗口停留激活前状态）；code 切换完成后的
        // 一切失败走"保留现场"出口（置 Failed + write），切换前的失败恢复激活前状态。
        let code = app_dir.join("code");
        let rollback = releases_dir.join(".rollback").join("code");
        // 快照覆盖判定：`.rollback` 的语义是"最近一次成功版本的快照"，只有当前 code
        // 就是在线成功版本时才允许把 code 挪成新快照。code 是失败现场（上次激活失败
        // 保留）时快照不动、失败版直接给 staging 让位——否则连续两次失败会把快照覆盖
        // 成失败代码，rollback 将恢复失败版却报告旧 Active。lock 读不出（异常损坏）时
        // 保守视为失败现场（保护快照优先）。
        let code_is_active = read_code_release_id(&app_dir)
            .await
            .is_some_and(|code_id| index.active_release_id.as_deref() == Some(code_id.as_str()));
        if code_is_active {
            remove_dir_if_exists(&rollback).await?;
        }
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
        if app_exists && let Err(error) = self.stop_app(app_id).await {
            remove_dir_if_exists(&staging).await?;
            return Err(error);
        }
        if code.exists() {
            if code_is_active {
                if let Err(error) = tokio::fs::rename(&code, &rollback).await {
                    remove_dir_if_exists(&staging).await?;
                    if app_exists && let Err(restart_error) = self.start_app(app_id).await {
                        error!(app_id, %restart_error, "failed to restart app after code move failure");
                    }
                    return Err(map_io_error("move active code to rollback", error, true));
                }
            } else {
                // 失败现场让位（重新 activate 即放弃旧失败现场，与旧的 rename-走语义
                // 等价）；`.rollback` 里的成功快照保持不动。
                remove_dir_if_exists(&code).await?;
            }
        }
        if let Err(error) = tokio::fs::rename(&staging, &code).await {
            // 切换未完成：仅当 code_is_active（快照=刚挪出的成功版）才做消费性恢复；
            // 失败现场路径下 code 已弃、成功快照保留给 rollback——重试 activate 或
            // rollback 均可自愈。
            let mut restore_failed = false;
            if code_is_active
                && rollback.exists()
                && let Err(rollback_error) = tokio::fs::rename(&rollback, &code).await
            {
                restore_failed = true;
                error!(app_id, %rollback_error, "rollback rename failed after activation error");
            }
            if app_exists
                && code_is_active
                && !restore_failed
                && let Err(restart_error) = self.start_app(app_id).await
            {
                error!(app_id, %restart_error, "failed to restart restored app after activation error");
            }
            return Err(map_io_error("activate staged release", error, true));
        }
        // code 已切到新版。此后的一切失败=保留现场（rollback 快照与制品包不动）。
        if app_exists && let Err(error) = self.start_app(app_id).await {
            self.fail_activation(
                &releases_dir,
                &mut index,
                release_position,
                &error.to_string(),
            )
            .await?;
            return Err(error);
        }
        if let Err(error) = self.ensure_app_runtime(app_id, app_id, _process_lock).await {
            self.fail_activation(
                &releases_dir,
                &mut index,
                release_position,
                &error.to_string(),
            )
            .await?;
            return Err(error);
        }
        if let Err(error) = self.wait_app_ready(app_id, timeout_secs).await {
            // 就绪失败是"发布结果"而非系统错误：置 Failed 保留现场后返回 Ok(Failed)。
            warn!(
                "[app_manager] release {release_id} for app {app_id} failed readiness: {error} \
                 (failure scene preserved: code/rollback/package intact)"
            );
            return self
                .fail_activation(
                    &releases_dir,
                    &mut index,
                    release_position,
                    &error.to_string(),
                )
                .await;
        }
        // 就绪 → 提交。
        let release = self
            .commit_activation(&releases_dir, &mut index, release_position)
            .await?;
        info!(app_id, release_id, "release activated and ready");
        Ok(release)
    }

    /// 激活失败的落账出口：置 Failed + failure_message + write_index。
    /// 现场三不动（code/`.rollback`/制品包）；返回 Failed release（调用方决定 Ok/Err 语义）。
    async fn fail_activation(
        &self,
        releases_dir: &std::path::Path,
        index: &mut crate::models::ReleaseIndex,
        position: usize,
        message: &str,
    ) -> AppResult<ReleaseInfo> {
        index.releases[position].status = ReleaseStatus::Failed;
        index.releases[position].failure_message = Some(message.to_string());
        let release = index.releases[position].clone();
        write_index(releases_dir, index).await?;
        Ok(release)
    }

    /// 激活成功的提交（原 confirm healthy=true 分支）：旧 Active 让位 Prepared、本 release
    /// 置 Active、清 `.rollback` 快照、retention 清理。
    async fn commit_activation(
        &self,
        releases_dir: &std::path::Path,
        index: &mut crate::models::ReleaseIndex,
        position: usize,
    ) -> AppResult<ReleaseInfo> {
        let release_id = index.releases[position].release_id.clone();
        for release in &mut index.releases {
            if release.status == ReleaseStatus::Active {
                release.status = ReleaseStatus::Prepared;
            }
        }
        index.releases[position].status = ReleaseStatus::Active;
        index.releases[position].failure_message = None;
        index.releases[position].activated_at = Some(Utc::now().to_rfc3339());
        index.active_release_id = Some(release_id);
        index.pending_release_id = None;
        let release = index.releases[position].clone();
        // 先持久化 Active 权威状态。后续保留清理均可重试，不能让清理失败破坏提交。
        write_index(releases_dir, index).await?;
        let removals = plan_retention(index);
        if !removals.is_empty() {
            write_index(releases_dir, index).await?;
            remove_release_packages(releases_dir, &removals).await;
        }
        if let Err(error) = remove_dir_if_exists(&releases_dir.join(".rollback").join("code")).await
        {
            warn!(%error, "failed to remove rollback directory after commit");
        }
        Ok(release)
    }

    /// 回滚到最近一次成功版本（`.rollback` 快照 rename 恢复，秒级）。
    ///
    /// - **有快照**（最近一次 activate 失败、现场还在）：stop → 清失败版 code → 快照恢复
    ///   → start；失败版 release 保持 Failed（message 记入 failure_message），
    ///   `active_release_id` 不变（指向恢复的旧版）。
    /// - **无快照**（最近一次部署是成功的）：幂等返回当前 Active（无事可做）。
    /// - **首次发布失败**（无旧版本可回滚）：409 ERR_INVALID_STATE。
    pub async fn rollback_release(
        &self,
        app_id: &str,
        message: Option<String>,
    ) -> AppResult<ReleaseInfo> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let releases_dir = app_dir.join("releases");
        let _process_lock = self.acquire_process_release_lock(app_id).await;
        let _lock = acquire_lock(releases_dir.join(".operation.lock")).await?;
        let mut index = read_index(&releases_dir, release_retention(None)?).await?;
        index.normalize_legacy_pending();
        let rollback = releases_dir.join(".rollback").join("code");
        // 前置判定一：从未成功过（无 active）——快照存在属异常残留（历史缺陷现场），
        // 清掉再 409，不做任何恢复副作用。
        let Some(active_id) = index.active_release_id.clone() else {
            if rollback.exists()
                && let Err(cleanup_error) = remove_dir_if_exists(&rollback).await
            {
                warn!(%cleanup_error, "failed to clear orphan snapshot");
            }
            return Err(AppOperationError::InvalidState(format!(
                "no previous release to rollback for app {app_id} (no successful release ever activated)"
            )));
        };
        let active_release = index
            .releases
            .iter()
            .find(|release| release.release_id == active_id)
            .cloned()
            .ok_or_else(|| {
                AppOperationError::InvalidState(format!(
                    "active release {active_id} missing from index for app {app_id}"
                ))
            })?;
        if !rollback.exists() {
            // 幂等分支（最近一次部署成功）：自愈"上次 rollback 恢复了代码但启动失败"
            // 的中间态——应用存在但停止时拉起，而不是直接返回成功让服务继续 down。
            match self.runtime.get_deployment_status(app_id).await {
                Ok(Some(status)) if status.replicas == 0 => {
                    if let Err(error) = self.start_app(app_id).await {
                        return Err(AppOperationError::Backend(format!(
                            "rollback found stopped app but restart failed for {app_id}: {error}"
                        )));
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(app_id, %error, "rollback: status check failed in idempotent branch");
                }
            }
            info!(
                app_id,
                "rollback: no pending snapshot, returning active release"
            );
            return Ok(active_release);
        }
        // 前置判定二：残留快照——code 已是在线成功版本（最近一次激活其实成功、快照
        // 清理失败留下的垃圾）。恢复它会静默把运行中的应用降级回旧代码、却报告当前
        // Active；按垃圾清理，幂等返回。
        let code_is_active = read_code_release_id(&app_dir)
            .await
            .is_some_and(|code_id| code_id == active_id);
        if code_is_active {
            warn!(
                app_id,
                "rollback: clearing stale snapshot (code already at active release {active_id})"
            );
            if let Err(cleanup_error) = remove_dir_if_exists(&rollback).await {
                warn!(%cleanup_error, "failed to clear stale snapshot");
            }
            return Ok(active_release);
        }
        // 失败版记回滚原因（最新 Failed;index.releases 为 prepare 时序,末尾最新）
        let reason = message.unwrap_or_else(|| "rolled back by user".to_string());
        if let Some(position) = index
            .releases
            .iter()
            .rposition(|release| release.status == ReleaseStatus::Failed)
        {
            index.releases[position].failure_message = Some(reason.clone());
        }
        let code = app_dir.join("code");
        match self.stop_app(app_id).await {
            Ok(_) | Err(AppOperationError::NotFound(_)) => {}
            Err(error) => {
                // 回滚原因先落盘（否则用户填写的 message 被吞），再上抛。
                if let Err(write_error) = write_index(&releases_dir, &index).await {
                    warn!(%write_error, "failed to persist rollback reason before stop failure");
                }
                return Err(AppOperationError::Backend(format!(
                    "failed to stop app before rollback: {error}"
                )));
            }
        }
        remove_dir_if_exists(&code).await?;
        tokio::fs::rename(&rollback, &code)
            .await
            .map_err(|error| map_io_error("restore previous release from snapshot", error, true))?;
        write_index(&releases_dir, &index).await?;
        if let Err(error) = self.start_app(app_id).await {
            // 代码已回滚、重启失败：如实上抛（代码状态已恢复,应用可 restart 自救）
            return Err(AppOperationError::Backend(format!(
                "rollback restored previous code but restart failed for {app_id}: {error}"
            )));
        }
        info!(app_id, active_release_id = %active_release.release_id, "release rolled back to previous active");
        Ok(active_release)
    }

    pub async fn list_releases(&self, app_id: &str) -> AppResult<ReleaseListResponse> {
        validate_app_id(app_id)?;
        let app_dir = self.get_container_app_dir(app_id).await?;
        let mut index = read_index(&app_dir.join("releases"), release_retention(None)?).await?;
        index.normalize_legacy_pending();
        // 最近一次激活失败（releases 为 prepare 时序,末尾最新）;恢复走 rollback 或重新 activate
        let last_failed_release_id = index
            .releases
            .iter()
            .rev()
            .find(|release| release.status == ReleaseStatus::Failed)
            .map(|release| release.release_id.clone());
        Ok(ReleaseListResponse {
            active_release_id: index.active_release_id,
            last_failed_release_id,
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
        index.normalize_legacy_pending();
        if index.active_release_id.as_deref() == Some(release_id) {
            return Err(AppOperationError::InvalidState(format!(
                "active release cannot be deleted: {release_id}"
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
    use std::path::Path;

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

    async fn seed_index(root: &Path, app_id: &str, index: ReleaseIndex) {
        let releases_dir = root.join(app_id).join("releases");
        ensure_release_dirs(&releases_dir)
            .await
            .expect("ensure release dirs");
        write_index(&releases_dir, &index)
            .await
            .expect("write index");
    }

    /// activate 幂等：目标 release 已 Active → 直接返回（不要求制品包存在）。
    #[tokio::test]
    async fn activate_is_idempotent_for_active_release() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);
        let mut index = ReleaseIndex {
            active_release_id: Some("release-1".into()),
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Active, "2026-08-01"));
        seed_index(root.path(), "app-idem", index).await;

        let info = service
            .activate_release("app-idem", "release-1", None)
            .await
            .expect("idempotent activate");
        assert_eq!(info.status, ReleaseStatus::Active);
    }

    /// activate 制品缺失 → 404 FileNotFound（Prepared 状态但包被清理）。
    #[tokio::test]
    async fn activate_missing_package_returns_not_found() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);
        let mut index = ReleaseIndex {
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Prepared, "2026-08-01"));
        seed_index(root.path(), "app-nopkg", index).await;

        let error = service
            .activate_release("app-nopkg", "release-1", None)
            .await
            .expect_err("missing package must fail");
        assert!(
            error.to_string().contains("release package missing"),
            "got: {error}"
        );
    }

    /// rollback 有快照（最近一次激活失败）：恢复 `.rollback` → code，失败版记
    /// message，返回上一 Active，应用被拉起。
    #[tokio::test]
    async fn rollback_restores_previous_active_from_snapshot() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "app-rb".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-rb".into(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Error".into(),
                ..Default::default()
            },
        );
        let service = test_service(root.path(), runtime.clone());
        let app_dir = root.path().join("app-rb");
        let releases_dir = app_dir.join("releases");
        let mut index = ReleaseIndex {
            active_release_id: Some("release-1".into()),
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Active, "2026-08-01"));
        index
            .releases
            .push(release("release-2", ReleaseStatus::Failed, "2026-08-02"));
        seed_index(root.path(), "app-rb", index).await;

        // 失败现场：code=失败版标记、.rollback=上一版内容
        let rollback_code = releases_dir.join(".rollback").join("code");
        tokio::fs::create_dir_all(&rollback_code)
            .await
            .expect("mkdir rollback");
        tokio::fs::write(rollback_code.join("marker.txt"), "previous")
            .await
            .expect("seed rollback content");

        let restored = service
            .rollback_release("app-rb", Some("排查后放弃 v2".into()))
            .await
            .expect("rollback");
        assert_eq!(restored.release_id, "release-1");
        assert_eq!(restored.status, ReleaseStatus::Active);
        // code 恢复为上一版内容、快照清空、应用被拉起
        assert_eq!(
            tokio::fs::read_to_string(app_dir.join("code").join("marker.txt"))
                .await
                .expect("restored code"),
            "previous"
        );
        assert!(!rollback_code.exists(), "snapshot consumed");
        assert_eq!(
            runtime.deployments.get("app-rb").unwrap().phase,
            "Running",
            "app restarted after rollback"
        );
        // 失败版 message 已记
        let listed = service.list_releases("app-rb").await.expect("list");
        let failed = listed
            .releases
            .iter()
            .find(|r| r.release_id == "release-2")
            .expect("failed release kept");
        assert_eq!(failed.failure_message.as_deref(), Some("排查后放弃 v2"));
        assert_eq!(
            listed.last_failed_release_id.as_deref(),
            Some("release-2"),
            "last failed pointer exposed"
        );
    }

    /// rollback 无快照（最近一次部署成功）：幂等返回当前 Active。
    #[tokio::test]
    async fn rollback_without_snapshot_returns_active_idempotently() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);
        let mut index = ReleaseIndex {
            active_release_id: Some("release-1".into()),
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Active, "2026-08-01"));
        seed_index(root.path(), "app-ok", index).await;

        let info = service
            .rollback_release("app-ok", None)
            .await
            .expect("idempotent rollback");
        assert_eq!(info.release_id, "release-1");
    }

    /// rollback 首次发布失败（无旧版本、无快照）→ 409 InvalidState。
    #[tokio::test]
    async fn rollback_first_release_failure_returns_conflict() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);
        let mut index = ReleaseIndex {
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Failed, "2026-08-01"));
        seed_index(root.path(), "app-first", index).await;

        let error = service
            .rollback_release("app-first", None)
            .await
            .expect_err("first release failure has nothing to rollback");
        assert!(
            matches!(error, AppOperationError::InvalidState(_)),
            "got: {error}"
        );
    }

    /// 旧 index 兼容：PendingStart 行读时归一化为 Failed、pending 指针清空。
    #[test]
    fn normalize_legacy_pending_maps_to_failed() {
        let mut index = ReleaseIndex {
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
            .push(release("release-2", ReleaseStatus::Active, "2026-08-02"));
        index.normalize_legacy_pending();
        assert_eq!(index.pending_release_id, None);
        assert_eq!(index.releases[0].status, ReleaseStatus::Failed);
        assert!(index.releases[0].failure_message.is_some());
        assert_eq!(index.releases[1].status, ReleaseStatus::Active);
    }
    /// 回归测试：首次发布 activate（app 不存在 + 制品有效）不得死锁——
    /// 修复前 activate 持 process lock → ensure_app_runtime → create_app
    /// 再次取同一把 tokio Mutex → 永久挂起（P0）。
    /// 用 tokio::time::timeout 包裹：挂起 = 超时失败，而非测试永卡。
    #[tokio::test]
    async fn activate_first_publish_does_not_deadlock() {
        let root = tempfile::tempdir().expect("tempdir");
        // MockRuntime 预置 deployment（模拟 app 已有运行时），绕开实际 K8s 调用；
        // 关键是让 ensure_app_runtime 的 get_app 返回 NotFound → 走 create 分支
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);

        let mut index = ReleaseIndex {
            active_release_id: None,
            retention: 15,
            ..ReleaseIndex::default()
        };
        index.releases.push(release(
            "release-first",
            ReleaseStatus::Prepared,
            "2026-08-19",
        ));
        seed_index(root.path(), "app-deadlock-test", index).await;
        // 制品包占位（activate 会尝试切换 code 目录）
        let releases_dir = root.path().join("app-deadlock-test").join("releases");
        let pkg = releases_dir.join("release-first");
        tokio::fs::create_dir_all(&pkg)
            .await
            .expect("create package dir");
        let code_dir = pkg.join("code");
        tokio::fs::create_dir_all(&code_dir)
            .await
            .expect("create code dir");

        // 关键断言：5 秒超时——修复前此处永久挂起
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            service.activate_release("app-deadlock-test", "release-first", Some(1)),
        )
        .await;

        // 超时 = 死锁（测试失败）；正常返回（无论 Ok/Err，取决于 MockRuntime 行为）
        // 都说明锁链没有死锁
        assert!(
            result.is_ok(),
            "activate_release hung >5s — process lock re-entry deadlock"
        );
    }

    // ---- .rollback 快照保护与 rollback 判定增强（本轮修复）----

    use std::io::Write as _;

    /// 构造合法制品 zip（workspace.manifest.toml + release.lock.toml，lock 的
    /// release_id 参数化），activate 的 stage 校验可解包通过。
    fn write_release_package(releases_dir: &Path, release_id: &str) {
        use zip::ZipWriter;
        use zip::write::SimpleFileOptions;
        std::fs::create_dir_all(releases_dir.join("packages")).expect("mkdir packages");
        let path = releases_dir
            .join("packages")
            .join(format!("{release_id}.zip"));
        let file = std::fs::File::create(&path).expect("create package");
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default();
        zip.start_file("workspace.manifest.toml", options)
            .expect("manifest entry");
        zip.write_all(b"schema_version = 1\n")
            .expect("manifest body");
        zip.start_file("release.lock.toml", options)
            .expect("lock entry");
        let lock = crate::test_support::release_lock().replace("release-1", release_id);
        zip.write_all(lock.as_bytes()).expect("lock body");
        zip.finish().expect("finish zip");
    }

    /// 构造 code 目录（带 release.lock.toml，release_id 参数化）——快照覆盖/残留
    /// 判定的"code 自证版本"数据源。
    async fn seed_code_with_lock(app_dir: &Path, release_id: &str) {
        let code = app_dir.join("code");
        tokio::fs::create_dir_all(&code).await.expect("mkdir code");
        let lock = crate::test_support::release_lock().replace("release-1", release_id);
        tokio::fs::write(code.join("release.lock.toml"), lock)
            .await
            .expect("seed code lock");
    }

    /// 读目录内 release.lock.toml 的 release_id（断言用）。
    async fn lock_release_id(dir: &Path) -> String {
        let content = tokio::fs::read_to_string(dir.join("release.lock.toml"))
            .await
            .expect("lock file");
        workspace_manifest::load_release_lock(&content)
            .expect("valid lock")
            .release_id
    }

    /// 连续失败保护：r1 Active → activate r2 失败 → activate r3 再失败，
    /// `.rollback` 快照必须仍是最近成功版 r1（不被失败版覆盖），code=r3 失败版。
    #[tokio::test]
    async fn activate_repeated_failure_keeps_success_snapshot() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        // app 存在且运行中（v1 在跑）
        runtime.deployments.insert(
            "app-snap".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-snap".into(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Running".into(),
                ..Default::default()
            },
        );
        // 启动即崩：activate 的 start_app 后 phase=Error → wait_app_ready 首个轮询失败
        runtime
            .crash_on_start
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let service = test_service(root.path(), runtime);
        let app_dir = root.path().join("app-snap");
        let releases_dir = app_dir.join("releases");

        seed_code_with_lock(&app_dir, "release-1").await;
        let mut index = ReleaseIndex {
            active_release_id: Some("release-1".into()),
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Active, "2026-08-01"));
        index
            .releases
            .push(release("release-2", ReleaseStatus::Prepared, "2026-08-02"));
        index
            .releases
            .push(release("release-3", ReleaseStatus::Prepared, "2026-08-03"));
        seed_index(root.path(), "app-snap", index).await;
        write_release_package(&releases_dir, "release-2");
        write_release_package(&releases_dir, "release-3");

        let first = service
            .activate_release("app-snap", "release-2", Some(5))
            .await
            .expect("first activation outcome (Failed)");
        assert_eq!(first.status, ReleaseStatus::Failed);
        assert_eq!(
            lock_release_id(&releases_dir.join(".rollback").join("code")).await,
            "release-1",
            "snapshot = last successful version after first failure"
        );

        let second = service
            .activate_release("app-snap", "release-3", Some(5))
            .await
            .expect("second activation outcome (Failed)");
        assert_eq!(second.status, ReleaseStatus::Failed);
        // 核心断言：第二次失败不覆盖快照——仍是 r1，而非失败版 r2。
        assert_eq!(
            lock_release_id(&releases_dir.join(".rollback").join("code")).await,
            "release-1",
            "snapshot must keep last SUCCESSFUL version across repeated failures"
        );
        // code = 最新失败版（staging 进来）
        assert_eq!(
            lock_release_id(&app_dir.join("code")).await,
            "release-3",
            "code switched to the latest failing release"
        );
    }

    /// 残留快照：激活其实成功（code=active 版）但快照清理失败留下的垃圾——
    /// rollback 清垃圾幂等返回，不静默降级运行代码。
    #[tokio::test]
    async fn rollback_clears_stale_snapshot_when_code_is_active() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "app-stale".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-stale".into(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Running".into(),
                ..Default::default()
            },
        );
        let service = test_service(root.path(), runtime.clone());
        let app_dir = root.path().join("app-stale");
        let rollback = app_dir.join("releases").join(".rollback").join("code");

        seed_code_with_lock(&app_dir, "release-1").await;
        let mut index = ReleaseIndex {
            active_release_id: Some("release-1".into()),
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Active, "2026-08-01"));
        seed_index(root.path(), "app-stale", index).await;
        tokio::fs::create_dir_all(&rollback)
            .await
            .expect("mkdir stale snapshot");

        let info = service
            .rollback_release("app-stale", None)
            .await
            .expect("idempotent rollback on stale snapshot");
        assert_eq!(info.release_id, "release-1");
        assert!(!rollback.exists(), "stale snapshot cleared as garbage");
        // code 未被动（仍是 active 版）、应用未被重启
        assert_eq!(lock_release_id(&app_dir.join("code")).await, "release-1");
        assert_eq!(
            runtime.deployments.get("app-stale").unwrap().phase,
            "Running",
            "app untouched by stale-snapshot rollback"
        );
    }

    /// 无成功版本 + 异常残留快照：先清残留再 409，不做任何恢复副作用。
    #[tokio::test]
    async fn rollback_without_active_clears_orphan_snapshot_then_conflicts() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        let service = test_service(root.path(), runtime);
        let app_dir = root.path().join("app-orphan");
        let rollback = app_dir.join("releases").join(".rollback").join("code");
        let mut index = ReleaseIndex {
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Failed, "2026-08-01"));
        seed_index(root.path(), "app-orphan", index).await;
        tokio::fs::create_dir_all(&rollback)
            .await
            .expect("mkdir orphan snapshot");

        let error = service
            .rollback_release("app-orphan", None)
            .await
            .expect_err("no successful release ever activated");
        assert!(
            matches!(error, AppOperationError::InvalidState(_)),
            "got: {error}"
        );
        assert!(!rollback.exists(), "orphan snapshot cleared before 409");
    }

    /// 幂等分支自愈：上次 rollback 恢复了代码但启动失败 → 重试 rollback 拉起停止的
    /// 应用（而非直接返回成功让服务继续 down）。
    #[tokio::test]
    async fn rollback_without_snapshot_restarts_stopped_app() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "app-down".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-down".into(),
                replicas: 0,
                ready_replicas: 0,
                phase: "Stopped".into(),
                ..Default::default()
            },
        );
        let service = test_service(root.path(), runtime.clone());
        let mut index = ReleaseIndex {
            active_release_id: Some("release-1".into()),
            retention: 15,
            ..ReleaseIndex::default()
        };
        index
            .releases
            .push(release("release-1", ReleaseStatus::Active, "2026-08-01"));
        seed_index(root.path(), "app-down", index).await;

        let info = service
            .rollback_release("app-down", None)
            .await
            .expect("rollback restarts stopped app");
        assert_eq!(info.release_id, "release-1");
        let status = runtime.deployments.get("app-down").unwrap();
        assert_eq!(status.replicas, 1, "app restarted");
        assert_eq!(status.phase, "Running");
    }

    /// wait_app_ready 容错：后端瞬时错误在预算内重试（不判死）；应用消失（发布期间
    /// 被用户删除）以明确的"deleted while waiting"语义失败。
    #[tokio::test]
    async fn wait_app_ready_tolerates_transient_errors_and_names_deletion() {
        let root = tempfile::tempdir().expect("tempdir");
        let runtime = Arc::new(MockRuntime::default());
        runtime.deployments.insert(
            "app-ready".into(),
            container_runtime_api::DeploymentStatus {
                app_id: "app-ready".into(),
                replicas: 1,
                ready_replicas: 1,
                phase: "Running".into(),
                ..Default::default()
            },
        );
        let service = test_service(root.path(), runtime.clone());

        // 两次瞬时后端错误（API 抖动）→ 预算内继续轮询（3s 间隔吃掉 6s 预算）→ 最终就绪成功
        runtime
            .status_fails
            .store(2, std::sync::atomic::Ordering::SeqCst);
        service
            .wait_app_ready("app-ready", 30)
            .await
            .expect("ready despite transient backend errors");

        // 应用被删除（NotFound）→ 明确语义失败，而非笼统就绪失败
        runtime.deployments.remove("app-ready");
        let error = service
            .wait_app_ready("app-ready", 5)
            .await
            .expect_err("app deleted while waiting");
        assert!(
            error.to_string().contains("deleted while waiting"),
            "got: {error}"
        );
    }
}
