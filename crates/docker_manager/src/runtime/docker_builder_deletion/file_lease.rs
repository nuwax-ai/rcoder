use super::*;

#[cfg(test)]
pub(super) fn lock_builder_file(root: &std::path::Path, name: &str) -> Result<BuilderFileLease> {
    lock_builder_file_with_marker(root, name, shared_types::AppFileMutationMarker::new())
}

/// 归档旁文件追加一条记录（best-effort：I/O 失败只影响 validate fallback
/// 的精度，不影响持锁/释放本身）。
#[cfg(unix)]
fn append_lock_archive(lock_path: &std::path::Path, suffix: &str) {
    let archive = lock_path.with_extension("lock.orphaned");
    let entry = format!(
        "[{}] {}\n",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        suffix,
    );
    if let Some(parent) = archive.parent() {
        let _unused = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&archive)
    {
        use std::io::Write as _;
        let _unused = file.write_all(entry.as_bytes());
    }
}

pub(super) fn lock_builder_file_with_marker(
    root: &std::path::Path,
    name: &str,
    marker: shared_types::AppFileMutationMarker,
) -> Result<BuilderFileLease> {
    let lease = prepare_builder_file(root, name, marker)?;
    lease
        .marker
        .begin(&lease.file)
        .map_err(|e| Error::DockerError(format!("persist builder operation marker: {e}")))?;
    // 获取即落归档授权记录（codex 复审 2026-11-20）：正常持锁不写归档会让
    // 后继完成的 done 无法撤销更早的裸权威（行序不能替代锁代次）。
    #[cfg(unix)]
    append_lock_archive(&lease.path, lease.marker.operation_id());
    Ok(lease)
}

pub(super) fn prepare_builder_file(
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
    // The exclusive flock is held here: a nonempty marker can only be a former
    // holder's remnant (kernel-released flock proves the holder stopped), so
    // reclaim it instead of fencing the application forever. Record-backed
    // fencing (compute control records, operation leases) is unaffected — it
    // reads durable storage, not this physical remnant.
    let lock_path = root.join(name);
    match shared_types::AppFileMutationMarker::reclaim_orphaned(
        &file,
        Some(&lock_path.with_extension("lock.orphaned")),
    ) {
        Ok(None) => {}
        Ok(Some(orphan_operation_id)) => {
            tracing::warn!(
                lock_path = %lock_path.display(),
                archive = %lock_path.with_extension("lock.orphaned").display(),
                orphan_operation_id,
                "Reclaimed orphaned builder operation marker under held flock; previous holder is proven stopped; operation id archived for reconciliation"
            );
        }
        Err(error) => {
            return Err(Error::Conflict(format!(
                "builder operation lease marker at {}: {error}",
                lock_path.display()
            )));
        }
    }
    Ok(BuilderFileLease {
        file,
        path: root.join(name),
        marker,
        unlocked: false,
        service_type: ServiceType::UserappBuilder,
    })
}

#[async_trait::async_trait]
impl shared_types::PreparedComputeLease for BuilderFileLease {
    async fn activate(&mut self) -> std::result::Result<(), String> {
        self.marker
            .begin(&self.file)
            .map_err(|error| format!("Activate compute lease: {error}"))?;
        #[cfg(unix)]
        append_lock_archive(&self.path, self.marker.operation_id());
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn open_inactive_file_receipt(
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
    if owner == token.as_bytes() {
        return Ok(true);
    }
    // 活 marker 非空且与回执不符：当前权威属于其他执行者——**不走归档**
    // （第十一轮 P1：旧回执曾凭归档在同 inode 新执行者写入时误验真）。
    if !owner.is_empty() {
        return Ok(false);
    }
    // 活 marker 为空（被归档回收清零）：查归档。codex 复审（2026-11-20）
    // 两反例定案的代次规则——归档行序不等于锁代次：
    // - 本 token 验真 ⟺ 存在裸授权记录（获取/归档写入），且其后：
    //   · 无本 token 的 done（已结算，幂等——重复 done 不改变结论）
    //   · 无**更晚获取**的其他 token 裸记录（后继代次接管）
    //   · 无"没有任何裸记录"的其他 token done（兼容无获取记录的旧归档/
    //     旧部署：无法证明其代次更早，按后继处置）
    // - 更早获取的其他 token 的迟到 done **不撤销**本 token（裸记录位置
    //   在本 token 之前 = 旧代次，其重复释放不改变在逈权威）。
    let archive = path.with_extension("lock.orphaned");
    if let Ok(content) = std::fs::read_to_string(&archive) {
        struct Record {
            token: String,
            done: bool,
        }
        let records: Vec<(usize, Record)> = content
            .lines()
            .enumerate()
            .filter_map(|(index, line)| {
                let (_, rest) = line.split_once(' ')?;
                let rest = rest.trim();
                let (token, done) = match rest.strip_prefix("done ") {
                    Some(token) => (token.trim().to_string(), true),
                    None => (rest.to_string(), false),
                };
                (!token.is_empty()).then_some((index, Record { token, done }))
            })
            .collect();
        let own_last_bare = records
            .iter()
            .filter(|(_, record)| !record.done && record.token == token.as_str())
            .map(|(index, _)| *index)
            .max();
        let Some(own_last_bare) = own_last_bare else {
            return Ok(false);
        };
        let revoked = records.iter().any(|(index, record)| {
            if *index <= own_last_bare {
                return false;
            }
            if record.token == token.as_str() {
                // 本 token 的 done：结算。
                record.done
            } else if record.done {
                // 其他 token 的 done：仅当其为更晚获取（裸记录在本 token
                // 之后）或完全无裸记录（代次不可证，按后继处置）时撤销。
                let acquired_after = records.iter().any(|(bare_index, bare)| {
                    *bare_index > own_last_bare && !bare.done && bare.token == record.token
                });
                let has_any_bare = records
                    .iter()
                    .any(|(_, candidate)| !candidate.done && candidate.token == record.token);
                acquired_after || !has_any_bare
            } else {
                // 其他 token 更晚获取的裸记录：后继代次接管。
                true
            }
        });
        return Ok(!revoked);
    }
    // Empty markers are already released, not authority to resume this operation.
    // Dropping this temporary fd unlocks without clearing or rewriting the marker.
    Ok(false)
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
    // 释放成功：向归档旁文件追加 done 标记——validate 的归档 fallback 据此
    // 停止把该 token 验真（第十一轮 P1：已释放回执不得凭归档复活）。
    // best-effort（归档不可写只影响 fallback 精度，不影响释放本身）。
    {
        let archive = path.with_extension("lock.orphaned");
        let entry = format!(
            "[{}] done {}\n",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            token
        );
        if let Some(parent) = archive.parent() {
            let _unused = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&archive)
        {
            use std::io::Write as _;
            let _unused = file.write_all(entry.as_bytes());
        }
    }
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
    path: std::path::PathBuf,
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

    async fn validate_retained(&self) -> std::result::Result<bool, String> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::{FileExt as _, MetadataExt as _};
            if self.unlocked {
                return Ok(false);
            }
            let opened = self.file.metadata().map_err(|error| error.to_string())?;
            let current =
                std::fs::symlink_metadata(&self.path).map_err(|error| error.to_string())?;
            if !current.is_file() || current.dev() != opened.dev() || current.ino() != opened.ino()
            {
                return Ok(false);
            }
            // This is the already retained fd, not a second open description.
            // A WouldBlock here is a real failure and never becomes success.
            self.file.try_lock().map_err(|error| error.to_string())?;
            let token = self.marker.operation_id().as_bytes();
            let mut bytes = vec![0; token.len() + 1];
            let count = self
                .file
                .read_at(&mut bytes, 0)
                .map_err(|error| error.to_string())?;
            Ok(&bytes[..count] == token)
        }
        #[cfg(not(unix))]
        {
            Ok(false)
        }
    }

    async fn release(mut self: Box<Self>) -> std::result::Result<(), String> {
        self.marker
            .complete(&self.file)
            .map_err(|error| format!("complete builder operation marker: {error}"))?;
        // 结算即落归档 done 记录：validate fallback 据此停止把本 token 的
        // 裸授权验真（codex 反例：不写则归档裸记录在正常完成后仍复活）。
        #[cfg(unix)]
        append_lock_archive(&self.path, &format!("done {}", self.marker.operation_id()));
        self.file
            .unlock()
            .map_err(|error| format!("release builder operation lease: {error}"))?;
        self.unlocked = true;
        Ok(())
    }
}

#[cfg(unix)]
pub(super) fn resume_original_builder_file(
    path: &std::path::Path,
    receipt: &shared_types::UserAppOperationLeaseReceipt,
    executor_id: &str,
) -> Result<BuilderFileLease> {
    use std::os::unix::fs::FileExt as _;
    let shared_types::UserAppOperationLeaseReceipt::Docker {
        service_type,
        token,
        ..
    } = receipt
    else {
        return Err(Error::Conflict(
            "Original compute receipt is not Docker".into(),
        ));
    };
    if token != executor_id {
        return Err(Error::Conflict(
            "Original compute executor token differs".into(),
        ));
    }
    let file = open_inactive_file_receipt(path, receipt)?;
    let mut owner = vec![0; token.len() + 1];
    let count = file
        .read_at(&mut owner, 0)
        .map_err(|error| Error::DockerError(error.to_string()))?;
    if &owner[..count] != token.as_bytes() {
        return Err(Error::Conflict(
            "Original compute marker is not retained".into(),
        ));
    }
    Ok(BuilderFileLease {
        file,
        path: path.to_owned(),
        marker: shared_types::AppFileMutationMarker::for_operation(executor_id)
            .map_err(|error| Error::ConfigurationError(error.to_string()))?,
        service_type: *service_type,
        unlocked: false,
    })
}

#[cfg(not(unix))]
pub(super) fn resume_original_builder_file(
    _: &std::path::Path,
    _: &shared_types::UserAppOperationLeaseReceipt,
    _: &str,
) -> Result<BuilderFileLease> {
    Err(Error::ConfigurationError(
        "Retained file guards require Unix".into(),
    ))
}
