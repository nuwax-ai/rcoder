use super::*;
use shared_types::{
    ComputeLeaseInspection, PreparedComputeLease, UserAppExecutionContext,
    UserAppOperationLeaseReceipt, UserAppOperationScope,
};

fn lease_name(
    context: &UserAppExecutionContext,
    scope: UserAppOperationScope,
) -> Result<(ServiceType, String)> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    let family = shared_types::compute_lease_family(scope).map_err(Error::ConfigurationError)?;
    let prefix = if family == ServiceType::UserappBuilder {
        "builder"
    } else {
        "prod"
    };
    Ok((family, format!("{prefix}-{}.lock", context.app_id)))
}

impl DockerRuntime {
    pub(crate) async fn prepare_compute_file_lease(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
    ) -> Result<Box<dyn PreparedComputeLease>> {
        let (family, name) = lease_name(context, scope)?;
        let root = ops::application_lease_root()
            .await?
            .join(".app-operation-locks");
        // The attempt identity already exists durably before acquiring the flock.
        let marker = shared_types::AppFileMutationMarker::for_operation(&context.executor_id)
            .map_err(|error| Error::ConfigurationError(error.to_string()))?;
        let mut lease =
            tokio::task::spawn_blocking(move || prepare_builder_file(&root, &name, marker))
                .await
                .map_err(|error| {
                    Error::DockerError(format!("Prepare compute lease worker: {error}"))
                })??;
        lease.service_type = family;
        Ok(Box::new(lease))
    }

    pub(crate) async fn inspect_compute_file_lease(
        &self,
        context: &UserAppExecutionContext,
        scope: UserAppOperationScope,
        receipt: Option<&UserAppOperationLeaseReceipt>,
    ) -> Result<ComputeLeaseInspection> {
        let (family, name) = lease_name(context, scope)?;
        if let Some(receipt) = receipt {
            receipt.validate().map_err(Error::ConfigurationError)?;
            if receipt.service_type() != &family {
                return Err(Error::ConfigurationError(
                    "Compute lease scope differs".into(),
                ));
            }
        }
        let path = ops::application_lease_root()
            .await?
            .join(".app-operation-locks")
            .join(name);
        let receipt = receipt.cloned();
        let token = context.executor_id.clone();
        tokio::task::spawn_blocking(move || inspect(&path, family, &token, receipt.as_ref()))
            .await
            .map_err(|error| Error::DockerError(format!("Inspect compute lease worker: {error}")))?
    }
}

#[cfg(unix)]
pub(super) fn inspect(
    path: &std::path::Path,
    family: ServiceType,
    attempt: &str,
    receipt: Option<&UserAppOperationLeaseReceipt>,
) -> Result<ComputeLeaseInspection> {
    use ComputeLeaseInspection::*;
    if receipt.is_some_and(|value| !matches!(value, UserAppOperationLeaseReceipt::Docker { .. })) {
        return Ok(IdentityChanged("Compute lease runtime differs".into()));
    }
    use std::{
        io::Read as _,
        os::unix::fs::{MetadataExt as _, OpenOptionsExt as _},
    };
    let before = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Absent),
        Err(error) => {
            return Err(Error::DockerError(format!(
                "Inspect compute lease: {error}"
            )));
        }
    };
    if !before.is_file() {
        return Ok(IdentityChanged(
            "Compute lease is not a regular file".into(),
        ));
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| Error::DockerError(format!("Open compute lease: {error}")))?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(Held),
        Err(error) => {
            return Err(Error::DockerError(format!(
                "Inspect compute flock: {error}"
            )));
        }
    }
    let meta = file
        .metadata()
        .map_err(|error| Error::DockerError(format!("Read compute lease identity: {error}")))?;
    let current = std::fs::symlink_metadata(path)
        .map_err(|error| Error::DockerError(format!("Recheck compute lease identity: {error}")))?;
    if !current.is_file()
        || before.dev() != meta.dev()
        || before.ino() != meta.ino()
        || current.dev() != meta.dev()
        || current.ino() != meta.ino()
    {
        return Ok(IdentityChanged(
            "Compute lease path changed during inspection".into(),
        ));
    }
    let mut owner = String::new();
    (&file)
        .take(4096)
        .read_to_string(&mut owner)
        .map_err(|error| Error::DockerError(format!("Read compute lease marker: {error}")))?;
    if let Some(receipt) = receipt {
        let UserAppOperationLeaseReceipt::Docker {
            device,
            inode,
            token,
            ..
        } = receipt
        else {
            return Ok(IdentityChanged("Compute lease runtime differs".into()));
        };
        if *device != meta.dev() || *inode != meta.ino() || (!owner.is_empty() && owner != *token) {
            return Ok(IdentityChanged(
                "Captured compute lease identity changed".into(),
            ));
        }
        return Ok(Releasable(receipt.clone()));
    }
    if owner.is_empty() {
        return Ok(Absent);
    }
    if owner != attempt {
        return Ok(IdentityChanged(
            "Unregistered compute lease lacks this executor's attempt identity".into(),
        ));
    }
    Ok(Releasable(UserAppOperationLeaseReceipt::Docker {
        service_type: family,
        device: meta.dev(),
        inode: meta.ino(),
        token: owner,
    }))
}

#[cfg(not(unix))]
pub(super) fn inspect(
    _: &std::path::Path,
    _: ServiceType,
    _: &str,
    _: Option<&UserAppOperationLeaseReceipt>,
) -> Result<ComputeLeaseInspection> {
    Err(Error::ConfigurationError(
        "Physical operation lease recovery requires Unix".into(),
    ))
}
