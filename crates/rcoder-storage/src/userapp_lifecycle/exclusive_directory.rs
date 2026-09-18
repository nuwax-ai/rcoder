//! Startup checks for a private, local, single-instance database directory.
//! Directory aliases resolve to the same lease; database/sidecar aliases are
//! rejected because database sidecar files must share the database's physical directory.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

use super::storage;
use shared_types::UserAppStoreError as Error;

const LOCK_NAME: &str = ".userapp-instance.lock";

pub(super) fn acquire(path: &Path) -> Result<(PathBuf, File), Error> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(Error::InvalidOperation(
            "database path must be an absolute file path".into(),
        ));
    }
    let name = path
        .file_name()
        .ok_or_else(|| Error::InvalidOperation("database path must name a file".into()))?;
    if name == LOCK_NAME {
        return Err(Error::InvalidOperation(
            "database cannot use the instance lock filename".into(),
        ));
    }
    let directory = path
        .parent()
        .ok_or_else(|| Error::InvalidOperation("database path must name a file".into()))?
        .canonicalize()
        .map_err(storage)?;
    require_local_filesystem(&directory)?;
    let lock_path = directory.join(LOCK_NAME);
    require_unaliased_file(&lock_path)?;
    let mut options = OpenOptions::new();
    options.create(true).truncate(false).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let lock = options.open(&lock_path).map_err(storage)?;
    require_single_link(&lock.metadata().map_err(storage)?)?;
    lock.try_lock().map_err(|error| {
        Error::InvalidOperation(format!(
            "userApp database directory is already in use or does not support file locking: {error}"
        ))
    })?;

    let canonical_path = directory.join(name);
    require_unaliased_file(&canonical_path)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let mut sidecar = canonical_path.as_os_str().to_os_string();
        sidecar.push(suffix);
        require_unaliased_file(Path::new(&sidecar))?;
    }
    Ok((canonical_path, lock))
}

fn require_unaliased_file(path: &Path) -> Result<(), Error> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(Error::InvalidOperation(
                    "database, sidecars and instance lock must be regular files without symbolic links".into(),
                ));
            }
            require_single_link(&metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(storage(error)),
    }
}

fn require_single_link(metadata: &std::fs::Metadata) -> Result<(), Error> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        if metadata.nlink() != 1 {
            return Err(Error::InvalidOperation(
                "database files must not have hard-link aliases".into(),
            ));
        }
    }
    Ok(())
}

fn require_local_filesystem(directory: &Path) -> Result<(), Error> {
    let filesystem = nix::sys::statfs::statfs(directory).map_err(storage)?;
    #[cfg(target_os = "linux")]
    let remote = matches!(
        filesystem.filesystem_type().0 as u64,
        0x6969 | 0x517b | 0xff534d42 | 0xfe534d42
    );
    #[cfg(target_os = "macos")]
    let remote = matches!(filesystem.filesystem_type_name(), "nfs" | "smbfs");
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let remote = {
        let _ = filesystem;
        true
    };
    if remote {
        return Err(Error::InvalidOperation(
            "the local userApp database requires a local filesystem; use a local named volume instead of NFS/SMB"
                .into(),
        ));
    }
    Ok(())
}
