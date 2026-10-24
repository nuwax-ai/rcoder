//! Retain the real mutex before changing a Running coordinator's authority.
use super::*;
use container_runtime_api::ComputeHandoff;
use shared_types::{
    ComputeLeaseInspection, UserAppExecutionContext, UserAppOperationLeaseReceipt,
    UserAppOperationScope,
};

struct FileHandoff {
    _file: std::fs::File,
}
impl container_runtime_api::ComputeHandoffGuard for FileHandoff {}

impl DockerRuntime {
    pub(crate) async fn prepare_compute_file_handoff(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        receipt: Option<&UserAppOperationLeaseReceipt>,
    ) -> Result<ComputeHandoff> {
        let (family, name) = compute_lease::lease_name(context, scope)?;
        let root = ops::application_lease_root()
            .await?
            .join(".app-operation-locks");
        let context = context.clone();
        let receipt = receipt.cloned();
        tokio::task::spawn_blocking(move || {
            retain(&root, &name, family, &context, receipt.as_ref())
        })
        .await
        .map_err(|error| Error::DockerError(format!("Prepare compute handoff worker: {error}")))?
    }
}

#[cfg(unix)]
fn retain(
    root: &std::path::Path,
    name: &str,
    family: ServiceType,
    context: &UserAppExecutionContext,
    receipt: Option<&UserAppOperationLeaseReceipt>,
) -> Result<ComputeHandoff> {
    use std::{
        io::Read as _,
        os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    };
    let failed = |error| Error::DockerError(format!("Observe compute handoff: {error}"));
    if let Some(receipt) = receipt {
        receipt.validate().map_err(Error::ConfigurationError)?;
        let captured_family = match receipt {
            UserAppOperationLeaseReceipt::Docker { service_type, .. } => *service_type,
            UserAppOperationLeaseReceipt::Kubernetes { .. } => {
                return Ok(ComputeHandoff::deferred(
                    ComputeLeaseInspection::IdentityChanged(
                        "Compute handoff runtime differs".into(),
                    ),
                ));
            }
        };
        if captured_family != family {
            return Ok(ComputeHandoff::deferred(
                ComputeLeaseInspection::IdentityChanged(
                    "Compute handoff receipt scope differs".into(),
                ),
            ));
        }
    }
    std::fs::create_dir_all(root).map_err(failed)?;
    let path = root.join(name);
    let (file, created) = match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&path)
    {
        Ok(file) => (file, true),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)
                .map_err(failed)?,
            false,
        ),
        Err(error) => return Err(failed(error)),
    };
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(ComputeHandoff::Held),
        Err(error) => {
            return Err(Error::DockerError(format!(
                "Observe compute handoff flock: {error}"
            )));
        }
    }
    let opened = file.metadata().map_err(failed)?;
    let current = std::fs::symlink_metadata(&path).map_err(failed)?;
    if !current.is_file() || current.dev() != opened.dev() || current.ino() != opened.ino() {
        return Ok(ComputeHandoff::deferred(
            ComputeLeaseInspection::IdentityChanged("Compute handoff path changed".into()),
        ));
    }
    if let Some(UserAppOperationLeaseReceipt::Docker { device, inode, .. }) = receipt
        && !created
        && (*device != opened.dev() || *inode != opened.ino())
    {
        return Ok(ComputeHandoff::deferred(
            ComputeLeaseInspection::IdentityChanged(
                "Captured compute handoff inode changed".into(),
            ),
        ));
    }
    let mut marker = String::new();
    (&file)
        .take(4096)
        .read_to_string(&mut marker)
        .map_err(failed)?;
    // Historical builder creation used an ordinary operation marker. Accept
    // it only with its exact observed inode/token receipt; the retained FD
    // excludes any writer until the durable settlement CAS has completed.
    let bound_ordinary = marker == context.operation_id
        && matches!(receipt, Some(UserAppOperationLeaseReceipt::Docker { token, .. }) if token == &marker);
    if !marker.is_empty() && marker != context.executor_id && !bound_ordinary {
        shared_types::validate_identifier(&marker, "operation_id")
            .map_err(Error::ConfigurationError)?;
        return Ok(ComputeHandoff::deferred(
            ComputeLeaseInspection::PreviousOperation {
                receipt: UserAppOperationLeaseReceipt::Docker {
                    service_type: family,
                    device: opened.dev(),
                    inode: opened.ino(),
                    token: marker.clone(),
                },
                operation_id: marker,
            },
        ));
    }
    if let Some(UserAppOperationLeaseReceipt::Docker { token, .. }) = receipt
        && !marker.is_empty()
        && marker != *token
    {
        return Ok(ComputeHandoff::deferred(
            ComputeLeaseInspection::IdentityChanged(
                "Captured compute handoff marker changed".into(),
            ),
        ));
    }
    Ok(ComputeHandoff::Acquired {
        guard: Box::new(FileHandoff { _file: file }),
        original_absent: created,
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{io::Write as _, os::unix::fs::MetadataExt as _};

    fn context() -> UserAppExecutionContext {
        UserAppExecutionContext {
            app_id: "handoff".into(),
            lifecycle_id: "life".into(),
            operation_id: "original".into(),
            executor_id: "executor".into(),
            request_fingerprint: "a".repeat(64),
        }
    }

    #[test]
    fn stale_ordinary_builder_marker_is_retained_then_cleaned_before_next_acquire() {
        let directory = tempfile::tempdir().unwrap();
        let name = "builder-handoff.lock";
        let path = directory.path().join(name);
        let context = context();
        let original = lock_builder_file_with_marker(
            directory.path(),
            name,
            shared_types::AppFileMutationMarker::for_operation(&context.operation_id).unwrap(),
        )
        .unwrap();
        use shared_types::AppOperationLease as _;
        let receipt = original.receipt().unwrap();
        drop(original); // Process exit leaves a dirty marker, not a held flock.
        assert!(
            lock_builder_file(directory.path(), name).is_err(),
            "slot release alone does not recover Docker creation"
        );
        let ComputeHandoff::Acquired { guard, .. } = retain(
            directory.path(),
            name,
            ServiceType::UserappBuilder,
            &context,
            Some(&receipt),
        )
        .unwrap() else {
            panic!("the exact old ordinary marker must be retainable");
        };
        assert!(
            open_inactive_file_receipt(&path, &receipt).is_err(),
            "physical exclusion must remain held through the storage CAS"
        );
        drop(guard);
        release_file_receipt(&path, &receipt).unwrap();
        let next = lock_builder_file(directory.path(), name).unwrap();
        let successor_receipt = next.receipt().unwrap();
        drop(next);
        // An old cleanup retry must not clear a successor's marker.
        assert!(release_file_receipt(&path, &receipt).is_err());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            match successor_receipt {
                UserAppOperationLeaseReceipt::Docker { token, .. } => token,
                _ => unreachable!(),
            }
        );
    }

    #[test]
    fn handoff_live_fd_and_guard_drop_preserve_the_original_marker() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("builder-handoff.lock");
        let mut original = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .unwrap();
        original.write_all(b"executor").unwrap();
        original.sync_all().unwrap();
        original.try_lock().unwrap();
        let meta = original.metadata().unwrap();
        let receipt = UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::UserappBuilder,
            device: meta.dev(),
            inode: meta.ino(),
            token: "executor".into(),
        };
        assert!(matches!(
            retain(
                directory.path(),
                "builder-handoff.lock",
                ServiceType::UserappBuilder,
                &context(),
                Some(&receipt)
            )
            .unwrap(),
            ComputeHandoff::Held
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"executor");
        original.unlock().unwrap();
        let ComputeHandoff::Acquired {
            guard,
            original_absent,
        } = retain(
            directory.path(),
            "builder-handoff.lock",
            ServiceType::UserappBuilder,
            &context(),
            Some(&receipt),
        )
        .unwrap()
        else {
            panic!("released exact FD must be retainable");
        };
        assert!(!original_absent);
        let observer = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        assert!(matches!(
            observer.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        // CAS loss/cancellation simply drops this handle, without completing a
        // marker or pretending that any Stop RPC returned.
        drop(guard);
        observer.try_lock().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"executor");
    }

    #[test]
    fn handoff_unknown_marker_and_changed_inode_never_gain_cleanup_authority() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("builder-handoff.lock");
        std::fs::write(&path, b"another-operation").unwrap();
        assert!(matches!(
            retain(
                directory.path(),
                "builder-handoff.lock",
                ServiceType::UserappBuilder,
                &context(),
                None
            )
            .unwrap(),
            ComputeHandoff::Deferred(observed) if matches!(*observed, ComputeLeaseInspection::PreviousOperation { .. })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"another-operation");
        let meta = std::fs::metadata(&path).unwrap();
        let foreign = UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::UserappBuilder,
            device: meta.dev(),
            inode: meta.ino() + 1,
            token: "executor".into(),
        };
        assert!(matches!(
            retain(
                directory.path(),
                "builder-handoff.lock",
                ServiceType::UserappBuilder,
                &context(),
                Some(&foreign)
            )
            .unwrap(),
            ComputeHandoff::Deferred(observed) if matches!(*observed, ComputeLeaseInspection::IdentityChanged(_))
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"another-operation");
    }

    #[test]
    fn handoff_actual_missing_path_is_distinct_from_a_changed_old_inode() {
        let directory = tempfile::tempdir().unwrap();
        let old = UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::UserappBuilder,
            device: 1,
            inode: 1,
            token: "executor".into(),
        };
        let ComputeHandoff::Acquired {
            guard,
            original_absent,
        } = retain(
            directory.path(),
            "builder-handoff.lock",
            ServiceType::UserappBuilder,
            &context(),
            Some(&old),
        )
        .unwrap()
        else {
            panic!("actual absence can converge");
        };
        assert!(original_absent);
        drop(guard);
        assert_eq!(
            std::fs::read(directory.path().join("builder-handoff.lock")).unwrap(),
            b""
        );
    }
}

#[cfg(not(unix))]
fn retain(
    _: &std::path::Path,
    _: &str,
    _: ServiceType,
    _: &UserAppExecutionContext,
    _: Option<&UserAppOperationLeaseReceipt>,
) -> Result<ComputeHandoff> {
    Err(Error::ConfigurationError(
        "Physical compute handoff requires Unix".into(),
    ))
}
