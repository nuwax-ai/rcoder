use super::*;

/// Explicit unlock also releases an inherited/duplicated file description. Relying
/// on close alone can leave the lock alive in a concurrently spawned child.
pub(super) struct PreparationLease(pub(super) std::fs::File);

impl Drop for PreparationLease {
    fn drop(&mut self) {
        if let Err(error) = self.0.unlock() {
            tracing::error!(%error, "Failed to release artifact preparation lease");
        }
    }
}

/// Prepared files are owned by this operation. Drop cleans failures/cancellation.
/// Extraction moves the owner into its blocking worker, so cancellation cannot race cleanup.
pub(crate) struct PreparedDeploy {
    pub(super) staging: tempfile::TempDir,
    _lease: PreparationLease,
    state: DeployState,
}

impl PreparedDeploy {
    /// Identity from the already validated, operation-owned staging directory.
    pub(crate) fn artifact_release_id(&self) -> Result<String> {
        Ok(crate::manifest::read_release_lock(self.staging.path())?.release_id)
    }
}

pub(crate) async fn deploy(
    workspace: &Path,
    url: &str,
    release_id: &str,
    expected_sha: Option<&str>,
) -> Result<()> {
    if let Some(prepared) = prepare(workspace, url, release_id, expected_sha, None).await? {
        activate(workspace, prepared).await?;
    }
    Ok(())
}

pub(crate) async fn prepare(
    workspace: &Path,
    url: &str,
    release_id: &str,
    expected_sha: Option<&str>,
    progress: Option<ProgressCallback>,
) -> Result<Option<PreparedDeploy>> {
    prepare_with_local(workspace, url, None, release_id, expected_sha, progress).await
}

/// [`prepare`] 的可参数化核心（R03）：`local_source` = 共享卷上的登记制品
/// 路径（Some 时跳过下载，仍走完整校验/解压/激活准备链）。
pub(crate) async fn prepare_with_local(
    workspace: &Path,
    url: &str,
    local_source: Option<&Path>,
    release_id: &str,
    expected_sha: Option<&str>,
    progress: Option<ProgressCallback>,
) -> Result<Option<PreparedDeploy>> {
    validate_release_id_fs_safe(release_id)?;
    let root = workspace.parent().context("workspace has no volume root")?;
    if let Some(state) = read_state(root).await
        && state.release_id == release_id
        && expected_sha.is_some_and(|sha| sha.eq_ignore_ascii_case(&state.sha256))
        && tokio::fs::try_exists(workspace.join("release.lock.toml")).await?
    {
        crate::manifest::preflight_startup(workspace, false).await?;
        return Ok(None);
    }
    tokio::fs::create_dir_all(root)
        .await
        .context("create volume root")?;
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".deploy-prepare.lock"))
        .context("open preparation lease")?;
    lease
        .try_lock()
        .context("another artifact preparation owns this volume")?;
    let lease = PreparationLease(lease);
    let incoming = incoming_dir(root);
    let staging_root = root.join(STAGING_DIR);
    tokio::fs::create_dir_all(&incoming)
        .await
        .context("create incoming directory")?;
    tokio::fs::create_dir_all(&staging_root)
        .await
        .context("create staging directory")?;
    // The OS lease outlives blocking extraction, including cancellation. Only a
    // holder can reclaim leftovers from a terminated operation.
    clean_owned_temporary(&incoming).await?;
    clean_owned_temporary(&staging_root).await?;
    let part = tempfile::Builder::new()
        .prefix("deploy-")
        .suffix(".part")
        .tempfile_in(&incoming)?;
    if let Some(ref cb) = progress {
        cb(AppDeploymentProgress {
            epoch: release_id.to_owned(),
            step: "downloading".into(),
            detail: Some(format!("fetching {url}")),
            updated_at: Some(chrono::Utc::now().timestamp_millis()),
            ..Default::default()
        });
    }
    // R03：本地登记制品（共享卷 builds/）不经网络下载；仍计算 sha256
    // （expected_sha 提供时校验一致）并走同一 zip 魔数/解压/校验链。
    let actual_hex = match &local_source {
        Some(source) => {
            anyhow::ensure!(
                tokio::fs::try_exists(source).await?,
                "registered local artifact is missing on the shared volume: {}",
                source.display()
            );
            let bytes = tokio::fs::read(source)
                .await
                .with_context(|| format!("read local artifact {}", source.display()))?;
            tokio::fs::write(part.path(), &bytes)
                .await
                .context("stage local artifact into incoming")?;
            to_hex(&sha2::Sha256::digest(&bytes))
        }
        None => to_hex(&download_to_file(url, part.path()).await?),
    };
    if let Some(expected) = expected_sha
        && !expected.eq_ignore_ascii_case(&actual_hex)
    {
        bail!("artifact sha256 mismatch: expected {expected}, downloaded {actual_hex}");
    }
    verify_zip_magic(part.path()).await?;
    let staging = tempfile::Builder::new()
        .prefix("deploy-")
        .tempdir_in(staging_root)?;
    if let Some(ref cb) = progress {
        cb(AppDeploymentProgress {
            epoch: release_id.to_owned(),
            step: "extracting".into(),
            detail: Some("extracting artifact".into()),
            updated_at: Some(chrono::Utc::now().timestamp_millis()),
            ..Default::default()
        });
    }
    let (staging, lease) =
        tokio::task::spawn_blocking(move || -> Result<(tempfile::TempDir, PreparationLease)> {
            extract_zip_sync(part.path(), staging.path())?;
            crate::manifest::read_release_lock(staging.path())
                .context("staged package has no parsable release.lock.toml")?;
            Ok((staging, lease))
        })
        .await
        .context("join artifact preparation")??;
    crate::manifest::preflight_startup(staging.path(), false)
        .await
        .context("validate staged startup contract before activation")?;
    Ok(Some(PreparedDeploy {
        staging,
        _lease: lease,
        state: DeployState {
            release_id: release_id.to_owned(),
            sha256: actual_hex,
            deployed_at: chrono::Utc::now().to_rfc3339(),
        },
    }))
}

/// Startup reclamation is exclusive with every live preparation, including
/// extraction that continues after its async caller has been cancelled.
pub(crate) async fn cleanup_startup(workspace: &Path) -> Result<()> {
    let root = workspace.parent().context("workspace has no volume root")?;
    tokio::fs::create_dir_all(root).await?;
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".deploy-prepare.lock"))?;
    lease
        .try_lock()
        .context("active preparation prevents startup cleanup")?;
    let _lease = PreparationLease(lease);
    for directory in [root.join(INCOMING_DIR), root.join(STAGING_DIR)] {
        if tokio::fs::try_exists(&directory).await? {
            clean_owned_temporary(&directory).await?;
        }
    }
    Ok(())
}

async fn clean_owned_temporary(directory: &Path) -> Result<()> {
    let mut entries = tokio::fs::read_dir(directory).await?;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_name().to_string_lossy().starts_with("deploy-") {
            continue;
        }
        if entry.file_type().await?.is_dir() {
            tokio::fs::remove_dir_all(entry.path()).await?;
        } else {
            tokio::fs::remove_file(entry.path()).await?;
        }
    }
    Ok(())
}

/// Restore the confirmed old directory after a interrupted promotion. The caller
/// must hold owner authority and have confirmed previous process quiescence.
pub(crate) fn restore_previous_generation(workspace: &Path, expected: &str) -> Result<()> {
    let root = workspace.parent().context("workspace has no volume root")?;
    let lease = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(root.join(".deploy-prepare.lock"))?;
    lease
        .try_lock()
        .context("preparation is active during directory recovery")?;
    let _lease = PreparationLease(lease);
    match std::fs::symlink_metadata(workspace) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Ok(_) => bail!("recovery destination already exists"),
        Err(error) => return Err(error).context("inspect recovery destination"),
    }
    let previous = root.join(PREVIOUS_DIR);
    let metadata = std::fs::symlink_metadata(&previous)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "previous generation must be a real directory"
    );
    anyhow::ensure!(
        crate::manifest::read_release_lock(&previous)?.release_id == expected,
        "previous generation artifact mismatch"
    );
    crate::migration_journal::require_confirmed_migrations(workspace)?;
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        &previous,
        rustix::fs::CWD,
        workspace,
        rustix::fs::RenameFlags::NOREPLACE,
    )
    .context("restore previous generation without replacement")?;
    // Windows directory rename fails if the destination already exists.
    #[cfg(windows)]
    std::fs::rename(&previous, workspace).context("restore previous generation")?;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    bail!("exclusive directory recovery is unsupported on this platform");
    #[cfg(unix)]
    std::fs::File::open(root)?.sync_all()?;
    anyhow::ensure!(
        crate::manifest::read_release_lock(workspace)?.release_id == expected,
        "restored generation artifact mismatch"
    );
    Ok(())
}

/// Activate only after the caller has stopped the previous generation.
pub(crate) async fn activate(workspace: &Path, prepared: PreparedDeploy) -> Result<()> {
    let root = workspace.parent().context("workspace has no volume root")?;
    let previous = root.join(PREVIOUS_DIR);
    if tokio::fs::try_exists(&previous).await? {
        tokio::fs::remove_dir_all(&previous)
            .await
            .context("clean previous generation")?;
    }
    let had_previous = tokio::fs::try_exists(workspace).await?;
    if had_previous {
        tokio::fs::rename(workspace, &previous)
            .await
            .context("preserve previous generation")?;
    }
    if let Err(error) = tokio::fs::rename(prepared.staging.path(), workspace).await {
        if had_previous {
            tokio::fs::rename(&previous, workspace)
                .await
                .context("restore previous after failed promotion")?;
        }
        return Err(error).context("promote prepared generation");
    }
    let marker = tempfile::NamedTempFile::new_in(root)?;
    tokio::fs::write(marker.path(), toml::to_string_pretty(&prepared.state)?).await?;
    tokio::fs::rename(marker.path(), root.join(DEPLOY_STATE_FILE))
        .await
        .context("commit deployment marker")?;
    Ok(())
}

/// 下载中转目录路径（卷根下）。
pub(super) fn incoming_dir(volume_root: &Path) -> std::path::PathBuf {
    volume_root.join(INCOMING_DIR)
}

/// 清 `.incoming/` 下全部 `*.part` 残片（deploy stage 入口调用）。
///
/// 跨重启遗留：成功路径自清、sha mismatch 清、其余失败路径由 staged 块兜底清，
/// 但历史版本残片 / 进程被 SIGKILL 的窗口仍可能留档——下载前统一扫掉，
/// 对齐 file-server 侧 hygiene sweep 的模型。失败仅 warn（清理是卫生动作，
/// 不阻断部署主流程）。
#[cfg(test)]
pub(super) async fn sweep_incoming_parts(dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return; // 目录不存在 = 无残片
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "part")
            && let Err(e) = tokio::fs::remove_file(&path).await
        {
            warn!("sweep stale part {} failed: {e}", path.display());
        }
    }
}

pub(super) async fn read_state(volume_root: &Path) -> Option<DeployState> {
    let path = volume_root.join(DEPLOY_STATE_FILE);
    let content = tokio::fs::read_to_string(&path).await.ok()?;
    match toml::from_str(&content) {
        Ok(state) => Some(state),
        Err(e) => {
            warn!(
                "parse {} failed ({e}); treating as no marker",
                path.display()
            );
            None
        }
    }
}

/// release_id 进 fs 路径（.incoming/.staging）与 env，白名单收紧。
pub(super) fn validate_release_id_fs_safe(release_id: &str) -> Result<()> {
    let ok = !release_id.is_empty()
        && !release_id.starts_with('.')
        && release_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if !ok {
        bail!("APP_RELEASE_ID must be [A-Za-z0-9._-]+ (no leading dot), got '{release_id}'");
    }
    Ok(())
}

// volume_root 辅助（测试共用）：workspace = {vol}/code。
#[cfg(test)]
pub(super) fn volume_root_of(workspace: &Path) -> std::path::PathBuf {
    workspace.parent().expect("workspace parent").to_path_buf()
}
