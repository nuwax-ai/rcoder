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

/// 回执根显式覆盖（单测/部署注入可写根）。
const RECEIPT_ROOT_ENV: &str = "RCODER_OPERATION_RECEIPT_ROOT";

/// 回执根解析，优先序对齐 `app_manager::config::default_operation_lock_root`
/// 推导链：env 显式覆盖 > deploy-host 宿主机形态经 host_map 解析的宿主真实
/// 根 > 容器形态常量。容器常量 `/app/...` 只在容器内可写；宿主机进程
/// （macOS 只读根 `/`）直接落盘会 EROFS——宿主机形态与单测必须解析可写根。
pub(super) async fn receipts_base() -> Result<PathBuf> {
    if let Ok(explicit) = std::env::var(RECEIPT_ROOT_ENV)
        && !explicit.trim().is_empty()
    {
        return Ok(PathBuf::from(explicit));
    }
    #[cfg(feature = "deploy-host")]
    if shared_types::is_deploy_host() {
        return crate::path::resolve_container_path_to_host(std::path::Path::new(
            shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
        ))
        .await
        .map_err(|error| {
            Error::DockerError(format!("deploy-host receipt root resolve failed: {error}"))
        });
    }
    Ok(PathBuf::from(
        shared_types::paths::RCODER_USERAPP_WORKSPACE_ROOT,
    ))
}

async fn path(context: &UserAppExecutionContext, family: &str, action: &str) -> Result<PathBuf> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    let root = receipts_base()
        .await?
        .join(".app-operation-receipts")
        .join(&context.app_id);
    Ok(root.join(format!("{family}-{action}-{}.json", context.operation_id)))
}

pub(super) async fn save_stop(target: &BuilderControlTarget) -> Result<()> {
    target.validate().map_err(Error::ConfigurationError)?;
    save(path(&target.context, "builder", "stop").await?, target).await
}

pub(super) async fn save_app_stop(target: &UserAppMutationTarget) -> Result<()> {
    save(path(&target.context, "prod", "stop").await?, target).await
}

pub(super) async fn save_start(target: &BuilderControlTarget) -> Result<()> {
    target.validate().map_err(Error::ConfigurationError)?;
    save(path(&target.context, "builder", "start").await?, target).await
}
pub(super) async fn matches_start(target: &BuilderControlTarget) -> Result<bool> {
    target.validate().map_err(Error::ConfigurationError)?;
    matches(
        path(&target.context, "builder", "start").await?,
        target.clone(),
    )
    .await
}
pub(super) async fn save_app_start(target: &UserAppMutationTarget) -> Result<()> {
    save(path(&target.context, "prod", "start").await?, target).await
}
pub(super) async fn matches_app_start(target: &UserAppMutationTarget) -> Result<bool> {
    matches(
        path(&target.context, "prod", "start").await?,
        target.clone(),
    )
    .await
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
        if let Err(error) = std::fs::remove_file(&temporary)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(%error, "Could not remove Docker receipt temporary file");
        }
        result
    })
    .await
    .map_err(|error| Error::DockerError(format!("Docker compute receipt worker: {error}")))?
    .map_err(|error| Error::DockerError(format!("Persist Docker compute receipt: {error}")))
}

pub(super) async fn matches_stop(target: &BuilderControlTarget) -> Result<bool> {
    target.validate().map_err(Error::ConfigurationError)?;
    matches(
        path(&target.context, "builder", "stop").await?,
        target.clone(),
    )
    .await
}

pub(super) async fn matches_app_stop(target: &UserAppMutationTarget) -> Result<bool> {
    matches(path(&target.context, "prod", "stop").await?, target.clone()).await
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
    save(
        path(&receipt.target.context, "builder", "create").await?,
        receipt,
    )
    .await
}
pub(super) async fn save_cancellation(receipt: &BuilderCancellationReceipt) -> Result<()> {
    receipt.validate()?;
    save(path(&receipt.context, "builder", "cancel").await?, receipt).await
}
pub(super) async fn read_cancellation(
    context: &UserAppExecutionContext,
) -> Result<Option<BuilderCancellationReceipt>> {
    let receipt =
        read::<BuilderCancellationReceipt>(path(context, "builder", "cancel").await?).await?;
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
    let receipt = read::<BuilderCreationReceipt>(path(context, "builder", "create").await?).await?;
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

/// Delete a terminal compute operation's stop/start receipt files. Content is
/// re-read first; a file whose payload belongs to another execution is never
/// removed.
pub(super) async fn cleanup_compute_receipt_files(context: &UserAppExecutionContext) -> Result<()> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    let paths = [
        path(context, "builder", "stop").await?,
        path(context, "builder", "start").await?,
        path(context, "prod", "stop").await?,
        path(context, "prod", "start").await?,
    ];
    let context = context.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for path in paths {
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Error::DockerError(format!(
                        "Read receipt for cleanup: {error}"
                    )));
                }
            };
            if bytes.len() > 65536 {
                return Err(Error::Conflict("Compute receipt exceeds limit".into()));
            }
            let belongs = if let Ok(target) = serde_json::from_slice::<BuilderControlTarget>(&bytes)
            {
                target.context == context
            } else if let Ok(target) = serde_json::from_slice::<UserAppMutationTarget>(&bytes) {
                target.context == context
            } else {
                false
            };
            if !belongs {
                return Err(Error::Conflict(
                    "Compute receipt belongs to another execution".into(),
                ));
            }
            std::fs::remove_file(&path)
                .map_err(|error| Error::DockerError(format!("Remove receipt file: {error}")))?;
        }
        Ok(())
    })
    .await
    .map_err(|error| Error::DockerError(format!("Compute receipt cleanup worker: {error}")))?
}

async fn receipts_root() -> Result<PathBuf> {
    Ok(receipts_base().await?.join(".app-operation-receipts"))
}

/// List operation contexts that still have builder creation or cancellation
/// receipt files. Damaged entries surface as errors; they are never skipped
/// silently and never deleted by the sweep.
pub(super) async fn list_builder_creation_receipt_contexts() -> Result<Vec<UserAppExecutionContext>>
{
    let root = receipts_root().await?;
    tokio::task::spawn_blocking(move || -> Result<Vec<UserAppExecutionContext>> {
        let mut contexts = Vec::new();
        let entries = match std::fs::read_dir(root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(contexts),
            Err(error) => return Err(Error::DockerError(format!("Open receipts root: {error}"))),
        };
        for app_dir in entries.flatten() {
            let files = match std::fs::read_dir(app_dir.path()) {
                Ok(files) => files,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Error::DockerError(format!(
                        "Open receipts app directory: {error}"
                    )));
                }
            };
            for file in files.flatten() {
                let file_name = file.file_name();
                let Some(file_name) = file_name.to_str() else {
                    continue;
                };
                let kind = if file_name.starts_with("builder-create-") {
                    "create"
                } else if file_name.starts_with("builder-cancel-") {
                    "cancel"
                } else {
                    continue;
                };
                let _ = kind;
                let bytes = match std::fs::read(file.path()) {
                    Ok(bytes) => bytes,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                    Err(error) => {
                        return Err(Error::DockerError(format!("Read receipt file: {error}")));
                    }
                };
                if bytes.len() > 65536 {
                    return Err(Error::Conflict("Creation receipt exceeds limit".into()));
                }
                // The payload type is untagged between create/cancel shapes;
                // decode by which one validates against its own identity.
                let context = if let Ok(receipt) =
                    serde_json::from_slice::<BuilderCreationReceipt>(&bytes)
                    && receipt.validate().is_ok()
                {
                    receipt.target.context
                } else if let Ok(receipt) =
                    serde_json::from_slice::<BuilderCancellationReceipt>(&bytes)
                    && receipt.validate().is_ok()
                {
                    receipt.context
                } else {
                    return Err(Error::Conflict(format!(
                        "Undecodable builder receipt file: {file_name}"
                    )));
                };
                if !contexts.contains(&context) {
                    contexts.push(context);
                }
            }
        }
        Ok(contexts)
    })
    .await
    .map_err(|error| Error::DockerError(format!("Receipt list worker: {error}")))?
}

/// Delete one operation's creation and cancellation receipt files. Content is
/// re-read first; a file whose payload belongs to another execution is never
/// removed.
pub(super) async fn cleanup_builder_creation_receipt_files(
    context: &UserAppExecutionContext,
) -> Result<()> {
    context
        .validate_identity(&context.app_id)
        .map_err(Error::ConfigurationError)?;
    let create_path = path(context, "builder", "create").await?;
    let cancel_path = path(context, "builder", "cancel").await?;
    let context = context.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        for (path, expected_create) in [(create_path, true), (cancel_path, false)] {
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(Error::DockerError(format!(
                        "Read receipt for cleanup: {error}"
                    )));
                }
            };
            if bytes.len() > 65536 {
                return Err(Error::Conflict("Creation receipt exceeds limit".into()));
            }
            let belongs = if expected_create {
                serde_json::from_slice::<BuilderCreationReceipt>(&bytes)
                    .map(|receipt| receipt.validate().is_ok() && receipt.target.context == context)
            } else {
                serde_json::from_slice::<BuilderCancellationReceipt>(&bytes)
                    .map(|receipt| receipt.validate().is_ok() && receipt.context == context)
            }
            .map_err(|error| Error::Conflict(format!("Decode receipt for cleanup: {error}")))?;
            if !belongs {
                return Err(Error::Conflict(
                    "Creation receipt belongs to another execution".into(),
                ));
            }
            std::fs::remove_file(&path)
                .map_err(|error| Error::DockerError(format!("Remove receipt file: {error}")))?;
        }
        Ok(())
    })
    .await
    .map_err(|error| Error::DockerError(format!("Receipt cleanup worker: {error}")))?
}
