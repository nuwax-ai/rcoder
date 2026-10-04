use std::{
    collections::HashMap,
    fs::File,
    path::{Path, PathBuf},
    sync::{Arc, LazyLock, Mutex, Weak},
};

use serde::{Deserialize, Serialize};
use tokio::sync::OwnedMutexGuard;

use crate::error::{AppError, AppResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum DirectoryIdentity {
    Unix { device: u64, inode: u64 },
    Windows { volume: u64, index: u64 },
}

pub(super) fn identity(path: &Path, directory: bool) -> AppResult<DirectoryIdentity> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| AppError::system(format!("inspect {}: {error}", path.display())))?;
    if metadata.is_symlink()
        || (directory && !metadata.is_dir())
        || (!directory && !metadata.is_file())
    {
        return Err(AppError::system(format!(
            "preservation entry {} has an unexpected file type",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(DirectoryIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        let handle = winapi_util::Handle::from_path_any(path).map_err(|error| {
            AppError::system(format!("open identity {}: {error}", path.display()))
        })?;
        let info = winapi_util::file::information(&handle).map_err(|error| {
            AppError::system(format!("read identity {}: {error}", path.display()))
        })?;
        Ok(DirectoryIdentity::Windows {
            volume: info.volume_serial_number(),
            index: info.file_index(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    Err(AppError::system(
        "skill preservation directory identity is unsupported on this platform",
    ))
}

pub(super) fn file_identity(file: &File) -> AppResult<DirectoryIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().map_err(AppError::from)?;
        Ok(DirectoryIdentity::Unix {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(windows)]
    {
        let info = winapi_util::file::information(file).map_err(|error| {
            AppError::system(format!("read preservation lock identity: {error}"))
        })?;
        Ok(DirectoryIdentity::Windows {
            volume: info.volume_serial_number(),
            index: info.file_index(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    Err(AppError::system(
        "preservation file identity is unsupported on this platform",
    ))
}

pub(super) fn checked_directory(root: &Path, path: &Path) -> AppResult<PathBuf> {
    let root = std::fs::canonicalize(root).map_err(|error| {
        AppError::system(format!("resolve workspace {}: {error}", root.display()))
    })?;
    let resolved = std::fs::canonicalize(path)
        .map_err(|error| AppError::system(format!("resolve {}: {error}", path.display())))?;
    if !resolved.starts_with(&root) {
        return Err(AppError::system(format!(
            "skill preservation directory {} resolves outside the workspace",
            path.display()
        )));
    }
    identity(&resolved, true)?;
    Ok(resolved)
}

struct PhysicalLock {
    file: File,
    root: PathBuf,
    root_id: DirectoryIdentity,
    agents: PathBuf,
    agents_id: DirectoryIdentity,
    path: PathBuf,
    file_id: DirectoryIdentity,
}

fn try_physical_lock(root: &Path) -> AppResult<Option<PhysicalLock>> {
    let root_id = identity(root, true)?;
    let agents_path = root.join(".agents");
    std::fs::create_dir_all(&agents_path)
        .map_err(|error| AppError::system(format!("create {}: {error}", agents_path.display())))?;
    let agents = checked_directory(root, &agents_path)?;
    let agents_id = identity(&agents, true)?;
    let path = agents.join(".skill-preservation.lock");
    #[cfg(unix)]
    let file = {
        use rustix::fs::{Mode, OFlags};
        let parent = rustix::fs::open(
            &agents,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|error| AppError::system(format!("open preservation lock parent: {error}")))?;
        let fd = rustix::fs::openat(
            &parent,
            ".skill-preservation.lock",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )
        .map_err(|error| AppError::system(format!("open {}: {error}", path.display())))?;
        File::from(fd)
    };
    #[cfg(not(unix))]
    let file = {
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.is_symlink()) {
            return Err(AppError::system(
                "skill preservation lock is a symbolic link",
            ));
        }
        File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| AppError::system(format!("open {}: {error}", path.display())))?
    };
    if !file
        .metadata()
        .map_err(|error| AppError::system(format!("inspect preservation lock: {error}")))?
        .is_file()
    {
        return Err(AppError::system(
            "skill preservation lock is not a regular file",
        ));
    }
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => return Ok(None),
        Err(error) => {
            return Err(AppError::system(format!(
                "acquire {}: {error}",
                path.display()
            )));
        }
    }
    let file_id = file_identity(&file)?;
    let held = PhysicalLock {
        file,
        root: root.to_owned(),
        root_id,
        agents,
        agents_id,
        file_id,
        path,
    };
    held.validate()?;
    Ok(Some(held))
}

impl PhysicalLock {
    fn validate(&self) -> AppResult<()> {
        if identity(&self.root, true)? != self.root_id
            || checked_directory(&self.root, &self.root.join(".agents"))? != self.agents
            || identity(&self.agents, true)? != self.agents_id
            || identity(&self.path, false)? != self.file_id
            || file_identity(&self.file)? != self.file_id
            || !self.file.metadata().map_err(AppError::from)?.is_file()
        {
            return Err(AppError::system(
                "workspace or preservation lock identity changed; recovery data retained",
            ));
        }
        Ok(())
    }
}

pub(in crate::service::computer_ws) struct WorkspaceGuard {
    physical: PhysicalLock,
    _local: OwnedMutexGuard<()>,
}

impl WorkspaceGuard {
    pub(in crate::service::computer_ws) fn root(&self) -> &Path {
        &self.physical.root
    }

    pub(in crate::service::computer_ws) fn validate(&self) -> AppResult<()> {
        self.physical.validate()
    }
}

pub(in crate::service::computer_ws) async fn acquire(
    workspace: &Path,
) -> AppResult<WorkspaceGuard> {
    tokio::fs::create_dir_all(workspace).await?;
    let root = tokio::fs::canonicalize(workspace).await?;
    static LOCKS: LazyLock<Mutex<HashMap<PathBuf, Weak<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    let mutex = {
        let mut locks = LOCKS
            .lock()
            .map_err(|_| AppError::system("workspace preservation lock registry is poisoned"))?;
        locks.retain(|_, mutex| mutex.strong_count() > 0);
        match locks.get(&root).and_then(Weak::upgrade) {
            Some(mutex) => mutex,
            None => {
                let mutex = Arc::new(tokio::sync::Mutex::new(()));
                locks.insert(root.clone(), Arc::downgrade(&mutex));
                mutex
            }
        }
    };
    let local = mutex.lock_owned().await;
    loop {
        let path = root.clone();
        let physical = tokio::task::spawn_blocking(move || try_physical_lock(&path))
            .await
            .map_err(|error| {
                AppError::system(format!("preservation lock worker interrupted: {error}"))
            })??;
        if let Some(physical) = physical {
            return Ok(WorkspaceGuard {
                physical,
                _local: local,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
}
