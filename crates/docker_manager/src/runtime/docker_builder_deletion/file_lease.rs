use super::*;

#[cfg(test)]
pub(super) fn lock_builder_file(root: &std::path::Path, name: &str) -> Result<BuilderFileLease> {
    lock_builder_file_with_marker(root, name, shared_types::AppFileMutationMarker::new())
}

pub(super) fn lock_builder_file_with_marker(
    root: &std::path::Path,
    name: &str,
    marker: shared_types::AppFileMutationMarker,
) -> Result<BuilderFileLease> {
    std::fs::create_dir_all(root)
        .map_err(|e| Error::DockerError(format!("builder operation lock directory: {e}")))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(name))
        .map_err(|e| Error::DockerError(format!("builder operation lock file: {e}")))?;
    file.try_lock()
        .map_err(|e| Error::Conflict(format!("builder operation lease unavailable: {e}")))?;
    shared_types::AppFileMutationMarker::check_clean(&file)
        .map_err(|e| Error::Conflict(format!("builder operation requires recovery: {e}")))?;
    marker
        .begin(&file)
        .map_err(|e| Error::DockerError(format!("persist builder operation marker: {e}")))?;
    Ok(BuilderFileLease {
        file,
        marker,
        unlocked: false,
        service_type: ServiceType::UserappBuilder,
    })
}

#[cfg(unix)]
fn open_inactive_file_receipt(
    path: &std::path::Path,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt as _;
    let shared_types::UserAppOperationLeaseReceipt::Docker { device, inode, .. } = receipt else {
        return Err(Error::Conflict("Operation lease runtime mismatch".into()));
    };
    let before = std::fs::symlink_metadata(path)
        .map_err(|error| Error::Conflict(format!("Operation lease file unavailable: {error}")))?;
    if !before.is_file() || before.dev() != *device || before.ino() != *inode {
        return Err(Error::Conflict(
            "Operation lease file identity changed".into(),
        ));
    }
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| Error::DockerError(format!("Open operation lease receipt: {error}")))?;
    file.try_lock()
        .map_err(|error| Error::Conflict(format!("Operation lease remains active: {error}")))?;
    let opened = file
        .metadata()
        .map_err(|error| Error::DockerError(format!("Read operation lease identity: {error}")))?;
    let current = std::fs::symlink_metadata(path)
        .map_err(|error| Error::Conflict(format!("Recheck operation lease identity: {error}")))?;
    if opened.dev() != *device
        || opened.ino() != *inode
        || !current.is_file()
        || current.dev() != *device
        || current.ino() != *inode
    {
        return Err(Error::Conflict(
            "Operation lease file identity changed".into(),
        ));
    }
    Ok(file)
}

/// Lock-file presence shared by validate/release: `Absent` is a released
/// state (nothing to observe, complete, or unlock — cleanup chains must not
/// retry forever on it); `Foreign` means the path hosts a non-regular file
/// (identity changed); `Present` proceeds to the inactive-open path. A stat
/// transport error defers to `open_inactive_file_receipt`'s error mapping.
#[cfg(unix)]
enum LockFilePresence {
    Present,
    Absent,
    Foreign,
}

#[cfg(unix)]
fn lock_file_presence(path: &std::path::Path) -> LockFilePresence {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_file() => LockFilePresence::Present,
        Ok(_) => LockFilePresence::Foreign,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => LockFilePresence::Absent,
        Err(_) => LockFilePresence::Present,
    }
}

#[cfg(unix)]
pub(super) fn validate_file_receipt(
    path: &std::path::Path,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<bool> {
    use std::io::Read as _;
    // A deleted lock file holds no marker: no continuation authority.
    match lock_file_presence(path) {
        LockFilePresence::Absent => return Ok(false),
        LockFilePresence::Foreign => {
            return Err(Error::Conflict(
                "Operation lease file identity changed".into(),
            ));
        }
        LockFilePresence::Present => {}
    }
    let file = open_inactive_file_receipt(path, receipt)?;
    let shared_types::UserAppOperationLeaseReceipt::Docker { token, .. } = receipt else {
        return Err(Error::Conflict("Operation lease runtime mismatch".into()));
    };
    let mut owner = Vec::new();
    (&file)
        .take(token.len() as u64 + 1)
        .read_to_end(&mut owner)
        .map_err(|error| Error::DockerError(format!("Read operation lease owner: {error}")))?;
    // Empty markers are already released, not authority to resume this operation.
    // Dropping this temporary fd unlocks without clearing or rewriting the marker.
    Ok(owner == token.as_bytes())
}

#[cfg(not(unix))]
pub(super) fn validate_file_receipt(
    _: &std::path::Path,
    _: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<bool> {
    Ok(false)
}

/// Docker 侧持有者死亡证明：flock 是活性真源，marker 只是 authority 残留。
///
/// `Ok(true)` = 旧持有者确定已死或已被取代：锁文件缺席（同 K8s Lease 对象
/// 不存在的极性）、身份被替换（superseded，同被接管租约的极性）、或 flock
/// 可被获取——内核在进程死亡时释放 flock，孤儿 marker 不能证明持有者存活。
/// `Ok(false)` = flock 被持有（holder 可能仍在变更中）。探测自身 I/O 失败
/// 返回 Err，保持围栏（fail-safe）。极性与 [`validate_file_receipt`] 不同，
/// 不得由 validate 推导。
#[cfg(unix)]
pub(super) fn file_receipt_holder_dead(
    path: &std::path::Path,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;
    let shared_types::UserAppOperationLeaseReceipt::Docker { device, inode, .. } = receipt else {
        return Err(Error::Conflict("Operation lease runtime mismatch".into()));
    };
    let before = match std::fs::symlink_metadata(path) {
        Ok(before) => before,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => {
            return Err(Error::DockerError(format!(
                "Probe operation lease file: {error}"
            )));
        }
    };
    if !before.is_file() || before.dev() != *device || before.ino() != *inode {
        return Ok(true);
    }
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| {
            Error::DockerError(format!("Open operation lease for liveness: {error}"))
        })?;
    let opened = file
        .metadata()
        .map_err(|error| Error::DockerError(format!("Read operation lease identity: {error}")))?;
    if opened.dev() != *device || opened.ino() != *inode {
        return Ok(true);
    }
    match file.try_lock() {
        // Acquiring the flock proves the kernel released it; dropping the fd
        // releases it again without touching the marker.
        Ok(()) => Ok(true),
        Err(std::fs::TryLockError::WouldBlock) => Ok(false),
        Err(std::fs::TryLockError::Error(error)) => Err(Error::DockerError(format!(
            "Probe operation lease liveness: {error}"
        ))),
    }
}

#[cfg(not(unix))]
pub(super) fn file_receipt_holder_dead(
    _: &std::path::Path,
    _: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<bool> {
    Err(Error::ConfigurationError(
        "Physical operation lease liveness probing requires Unix".into(),
    ))
}

#[cfg(unix)]
pub(super) fn release_file_receipt(
    path: &std::path::Path,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<()> {
    match lock_file_presence(path) {
        // A deleted lock file is already released: nothing to complete or
        // unlock. Reporting it as an error made terminal-lease discovery
        // retry the same binding forever.
        LockFilePresence::Absent => return Ok(()),
        LockFilePresence::Foreign => {
            return Err(Error::Conflict(
                "Operation lease file identity changed".into(),
            ));
        }
        LockFilePresence::Present => {}
    }
    let file = open_inactive_file_receipt(path, receipt)?;
    let shared_types::UserAppOperationLeaseReceipt::Docker { token, .. } = receipt else {
        return Err(Error::Conflict("Operation lease runtime mismatch".into()));
    };
    let marker = shared_types::AppFileMutationMarker::for_operation(token)
        .map_err(|error| Error::ConfigurationError(error.to_string()))?;
    marker
        .complete(&file)
        .map_err(|error| Error::Conflict(format!("Operation lease ownership changed: {error}")))?;
    file.unlock()
        .map_err(|error| Error::DockerError(format!("Unlock completed operation lease: {error}")))
}

#[cfg(not(unix))]
pub(super) fn release_file_receipt(
    _: &std::path::Path,
    _: &shared_types::UserAppOperationLeaseReceipt,
) -> Result<()> {
    Err(Error::ConfigurationError(
        "Physical operation lease recovery requires Unix".into(),
    ))
}

// Cancellation before the blocking lock-acquisition result is delivered cannot
// have admitted a resource mutation. Reclaim that marker instead of stranding it.
pub(super) struct UnclaimedBuilderLease(pub(super) Option<BuilderFileLease>);
impl Drop for UnclaimedBuilderLease {
    fn drop(&mut self) {
        if let Some(lease) = self.0.take()
            && let Err(error) = lease.marker.complete(&lease.file)
        {
            tracing::error!(%error, "Failed to release unclaimed builder lease");
        }
    }
}

pub(super) struct BuilderFileLease {
    pub(super) service_type: ServiceType,
    pub(super) file: std::fs::File,
    marker: shared_types::AppFileMutationMarker,
    unlocked: bool,
}
impl Drop for BuilderFileLease {
    fn drop(&mut self) {
        if !self.unlocked
            && let Err(error) = self.file.unlock()
        {
            tracing::error!(%error, "unlock builder operation file failed");
        }
    }
}
#[async_trait::async_trait]
impl shared_types::AppOperationLease for BuilderFileLease {
    fn receipt(&self) -> Option<shared_types::UserAppOperationLeaseReceipt> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt as _;
            let metadata = match self.file.metadata() {
                Ok(metadata) => metadata,
                Err(error) => {
                    tracing::error!(%error, "Read builder lease identity failed");
                    return None;
                }
            };
            Some(shared_types::UserAppOperationLeaseReceipt::Docker {
                service_type: self.service_type,
                device: metadata.dev(),
                inode: metadata.ino(),
                token: self.marker.operation_id().to_owned(),
            })
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    async fn release(mut self: Box<Self>) -> std::result::Result<(), String> {
        self.marker
            .complete(&self.file)
            .map_err(|error| format!("complete builder operation marker: {error}"))?;
        self.file
            .unlock()
            .map_err(|error| format!("release builder operation lease: {error}"))?;
        self.unlocked = true;
        Ok(())
    }
}
