//! Causal evidence written only after Docker acknowledged a compute operation
//! and its captured container was inspected. State alone is insufficient.
use super::builder_creation_receipt::BuilderCancellationReceipt;
use super::builder_creation_receipt::BuilderCreationReceipt;
use container_runtime_api::{ContainerRuntimeError as Error, ContainerRuntimeResult as Result};
use shared_types::{BuilderControlTarget, UserAppExecutionContext, UserAppMutationTarget};
use std::{
    io::{Read as _, Write as _},
    path::PathBuf,
};

fn path(context: &UserAppExecutionContext, family: &str, action: &str) -> Result<PathBuf> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    let root = PathBuf::from(shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT)
        .join(".app-operation-receipts")
        .join(&context.app_id);
    Ok(root.join(format!("{family}-{action}-{}.json", context.operation_id)))
}

pub(super) async fn save_stop(target: &BuilderControlTarget) -> Result<()> {
    target.validate().map_err(Error::ConfigurationError)?;
    save(path(&target.context, "builder", "stop")?, target).await
}

pub(super) async fn save_app_stop(target: &UserAppMutationTarget) -> Result<()> {
    save(path(&target.context, "prod", "stop")?, target).await
}

pub(super) async fn save_start(target: &BuilderControlTarget) -> Result<()> {
    target.validate().map_err(Error::ConfigurationError)?;
    save(path(&target.context, "builder", "start")?, target).await
}
pub(super) async fn matches_start(target: &BuilderControlTarget) -> Result<bool> {
    target.validate().map_err(Error::ConfigurationError)?;
    matches(path(&target.context, "builder", "start")?, target.clone()).await
}
pub(super) async fn save_app_start(target: &UserAppMutationTarget) -> Result<()> {
    save(path(&target.context, "prod", "start")?, target).await
}
pub(super) async fn matches_app_start(target: &UserAppMutationTarget) -> Result<bool> {
    matches(path(&target.context, "prod", "start")?, target.clone()).await
}

async fn save(path: PathBuf, target: &impl serde::Serialize) -> Result<()> {
    let bytes = serde_json::to_vec(target).map_err(|error| {
        Error::ConfigurationError(format!("Encode Docker compute receipt: {error}"))
    })?;
    tokio::task::spawn_blocking(move || -> std::io::Result<()> {
        let parent = path
            .parent()
            .ok_or_else(|| std::io::Error::other("Receipt parent missing"))?;
        std::fs::create_dir_all(parent)?;
        let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&temporary)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
            // Do not overwrite evidence belonging to another target. Publication
            // is atomic; a partially written temporary file is never a receipt.
            match std::fs::hard_link(&temporary, &path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if std::fs::read(&path)? != bytes {
                        return Err(std::io::Error::other(
                            "Docker compute receipt identity changed",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
            #[cfg(unix)]
            std::fs::File::open(parent)?.sync_all()?;
            Ok(())
        })();
        if let Err(error) = std::fs::remove_file(&temporary) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "Could not remove Docker receipt temporary file");
            }
        }
        result
    })
    .await
    .map_err(|error| Error::DockerError(format!("Docker compute receipt worker: {error}")))?
    .map_err(|error| Error::DockerError(format!("Persist Docker compute receipt: {error}")))
}

pub(super) async fn matches_stop(target: &BuilderControlTarget) -> Result<bool> {
    target.validate().map_err(Error::ConfigurationError)?;
    matches(path(&target.context, "builder", "stop")?, target.clone()).await
}

pub(super) async fn matches_app_stop(target: &UserAppMutationTarget) -> Result<bool> {
    matches(path(&target.context, "prod", "stop")?, target.clone()).await
}

async fn matches<T>(path: PathBuf, expected: T) -> Result<bool>
where
    T: serde::de::DeserializeOwned + PartialEq + Send + 'static,
{
    Ok(read::<T>(path)
        .await?
        .is_some_and(|actual| actual == expected))
}

async fn read<T>(path: PathBuf) -> Result<Option<T>>
where
    T: serde::de::DeserializeOwned + Send + 'static,
{
    tokio::task::spawn_blocking(move || -> Result<Option<T>> {
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(Error::DockerError(format!(
                    "Inspect Docker compute receipt: {error}"
                )));
            }
        };
        if !metadata.is_file() || metadata.len() > 65536 {
            return Err(Error::Conflict(
                "Invalid Docker compute receipt file".into(),
            ));
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.custom_flags(libc::O_NOFOLLOW);
        }
        let file = options
            .open(path)
            .map_err(|error| Error::DockerError(format!("Open Docker compute receipt: {error}")))?;
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .map_err(|error| Error::DockerError(format!("Read Docker compute receipt: {error}")))?;
        if bytes.len() > 65536 {
            return Err(Error::Conflict(
                "Docker compute receipt exceeds limit".into(),
            ));
        }
        let actual: T = serde_json::from_slice(&bytes)
            .map_err(|error| Error::Conflict(format!("Decode Docker compute receipt: {error}")))?;
        Ok(Some(actual))
    })
    .await
    .map_err(|error| Error::DockerError(format!("Docker compute receipt reader: {error}")))?
}

pub(super) async fn save_creation(receipt: &BuilderCreationReceipt) -> Result<()> {
    receipt.validate()?;
    save(path(&receipt.target.context, "builder", "create")?, receipt).await
}
pub(super) async fn save_cancellation(receipt: &BuilderCancellationReceipt) -> Result<()> {
    receipt.validate()?;
    save(path(&receipt.context, "builder", "cancel")?, receipt).await
}
pub(super) async fn read_cancellation(
    context: &UserAppExecutionContext,
) -> Result<Option<BuilderCancellationReceipt>> {
    let receipt = read::<BuilderCancellationReceipt>(path(context, "builder", "cancel")?).await?;
    if let Some(receipt) = &receipt {
        receipt.validate()?;
        if receipt.context != *context {
            return Err(Error::Conflict(
                "Builder cancellation executor differs".into(),
            ));
        }
    }
    Ok(receipt)
}
pub(super) async fn read_creation(
    context: &UserAppExecutionContext,
) -> Result<Option<BuilderCreationReceipt>> {
    let receipt = read::<BuilderCreationReceipt>(path(context, "builder", "create")?).await?;
    if let Some(receipt) = &receipt {
        receipt.validate()?;
        if receipt.target.context != *context {
            return Err(Error::Conflict(
                "Builder creation receipt belongs to another executor".into(),
            ));
        }
    }
    Ok(receipt)
}
