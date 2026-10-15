//! Identity-bound directory clearing. Callers select an authorized root and hold
//! its application lease. Unix traversal stays relative to open directory handles.
use serde::{Deserialize, Serialize};
use std::{
    io,
    path::{Path, PathBuf},
};

#[cfg(unix)]
mod unix;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StorageDirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Persist before effects. Absence is also a captured state: a subsequently
/// created root must not be treated as the original empty directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapturedStorageDirectory {
    pub path: PathBuf,
    pub identity: Option<StorageDirectoryIdentity>,
}

/// Live handle ownership is separate from durable evidence. Deserializing a
/// receipt never grants permission to reopen and delete a replacement root.
#[derive(Clone)]
pub struct StorageDirectoryLease {
    pub receipt: CapturedStorageDirectory,
    #[cfg(unix)]
    handle: Option<std::sync::Arc<rustix::fd::OwnedFd>>,
}

impl StorageDirectoryLease {
    pub async fn capture(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Storage root must be absolute",
            ));
        }
        let path = path.to_owned();
        #[cfg(unix)]
        return tokio::task::spawn_blocking(move || unix::capture(path))
            .await
            .map_err(|error| {
                io::Error::other(format!("Storage capture worker interrupted: {error}"))
            })?;
        #[cfg(not(unix))]
        {
            drop(path);
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Identity-bound storage clearing requires Unix directory handles",
            ))
        }
    }

    pub async fn clear(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let target = self.clone();
            tokio::task::spawn_blocking(move || unix::clear(&target))
                .await
                .map_err(|error| {
                    io::Error::other(format!("Storage clear worker interrupted: {error}"))
                })?
        }
        #[cfg(not(unix))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Identity-bound storage clearing requires Unix directory handles",
        ))
    }

    pub async fn validate_current(&self) -> io::Result<()> {
        #[cfg(unix)]
        {
            let target = self.clone();
            tokio::task::spawn_blocking(move || unix::validate_current(&target))
                .await
                .map_err(|error| {
                    io::Error::other(format!("Storage validation worker interrupted: {error}"))
                })?
        }
        #[cfg(not(unix))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Identity-bound storage clearing requires Unix directory handles",
        ))
    }
}

/// Remove children while retaining the captured root. Only captured absence is
/// idempotent; replacements fail. Symlinks are unlinked, never traversed.
pub async fn clear_directory_contents(root: &Path) -> io::Result<()> {
    StorageDirectoryLease::capture(root).await?.clear().await
}

/// Retire exactly a captured directory before removal. The detached blocking
/// task holds a separate flock until filesystem work ends; cancellation cannot
/// let a late recursive removal target a newly installed workspace pathname.
pub async fn remove_captured_directory(
    directory: &CapturedStorageDirectory,
    operation_id: &str,
) -> io::Result<()> {
    crate::validate_identifier(operation_id, "directory_deletion_operation")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let directory = directory.clone();
    let operation_id = operation_id.to_owned();
    tokio::task::spawn_blocking(move || {
        remove_captured_directory_blocking(&directory, &operation_id)
    })
    .await
    .map_err(|error| {
        io::Error::other(format!("Directory retirement worker interrupted: {error}"))
    })?
}

/// Persist a non-destructive cancellation witness under the same directory
/// retirement lock. Delayed blocking tasks check this witness before moving or
/// removing the path. A currently running removal keeps the lock and must finish
/// before this method can succeed.
pub async fn retire_captured_directory_deletion(
    directory: &CapturedStorageDirectory,
    operation_id: &str,
) -> io::Result<()> {
    crate::validate_identifier(operation_id, "directory_deletion_operation")
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    let directory = directory.clone();
    let operation_id = operation_id.to_owned();
    tokio::task::spawn_blocking(move || {
        retire_directory_deletion_blocking(&directory, &operation_id)
    })
    .await
    .map_err(|error| {
        io::Error::other(format!(
            "Directory deletion inspection worker interrupted: {error}"
        ))
    })?
}

#[cfg(unix)]
fn directory_retirement_lock(
    directory: &CapturedStorageDirectory,
) -> io::Result<(std::fs::File, PathBuf)> {
    use sha2::Digest as _;
    if !directory.path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Captured directory must be absolute",
        ));
    }
    let parent = directory.path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Captured directory has no parent",
        )
    })?;
    std::fs::create_dir_all(parent)?;
    let digest = sha2::Sha256::digest(directory.path.as_os_str().as_encoded_bytes());
    let hex = digest
        .iter()
        .flat_map(|byte| {
            const DIGITS: &[u8; 16] = b"0123456789abcdef";
            [
                char::from(DIGITS[(byte >> 4) as usize]),
                char::from(DIGITS[(byte & 15) as usize]),
            ]
        })
        .collect::<String>();
    let stem = parent.join(format!(".rcoder-directory-retire-{hex}"));
    let lock = open_retirement_file(&stem.with_extension("lock"), false)?;
    lock.try_lock().map_err(|error| {
        io::Error::other(format!("Directory retirement remains active: {error}"))
    })?;
    Ok((lock, stem))
}

#[cfg(unix)]
fn open_retirement_file(path: &Path, truncate: bool) -> io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};
    let mut flags = OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    if truncate {
        flags |= OFlags::TRUNC;
    }
    Ok(std::fs::File::from(rustix::fs::openat(
        rustix::fs::CWD,
        path,
        flags,
        Mode::RUSR | Mode::WUSR,
    )?))
}

#[cfg(unix)]
fn retire_directory_deletion_blocking(
    directory: &CapturedStorageDirectory,
    operation_id: &str,
) -> io::Result<()> {
    use std::io::Write as _;
    let (_lock, stem) = directory_retirement_lock(directory)?;
    let marker = stem.with_extension(format!("{operation_id}.retired"));
    let bytes = serde_json::to_vec(directory).map_err(io::Error::other)?;
    match std::fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.is_file() => {
            if std::fs::read(&marker)? != bytes {
                return Err(io::Error::other(
                    "Directory retirement witness differs from captured target",
                ));
            }
            return Ok(());
        }
        Ok(_) => {
            return Err(io::Error::other(
                "Directory retirement witness is not a regular file",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // A crash during persistence leaves only a replaceable temporary file, not
    // an incomplete authoritative witness that would permanently block retry.
    let temporary = marker.with_extension("retired.tmp");
    let mut file = open_retirement_file(&temporary, true)?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    std::fs::rename(&temporary, &marker)?;
    if let Some(parent) = marker.parent() {
        std::fs::File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn retire_directory_deletion_blocking(_: &CapturedStorageDirectory, _: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Captured directory retirement requires Unix",
    ))
}

#[cfg(unix)]
fn remove_captured_directory_blocking(
    directory: &CapturedStorageDirectory,
    operation_id: &str,
) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if !directory.path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Captured directory must be absolute",
        ));
    }
    let parent = directory.path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "Captured directory has no parent",
        )
    })?;
    let (_lock, stem) = directory_retirement_lock(directory)?;
    match std::fs::symlink_metadata(stem.with_extension(format!("{operation_id}.retired"))) {
        Ok(_) => {
            return Err(io::Error::other(
                "Captured directory deletion was retired by recovery",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let identity_at = |path: &Path| -> io::Result<Option<StorageDirectoryIdentity>> {
        match std::fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => Ok(Some(StorageDirectoryIdentity {
                device: meta.dev(),
                inode: meta.ino(),
            })),
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Captured directory is not a real directory",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    };
    let Some(identity) = &directory.identity else {
        return if identity_at(&directory.path)?.is_none() {
            Ok(())
        } else {
            Err(io::Error::other("Directory appeared after absent capture"))
        };
    };
    let retired = parent.join(format!(
        ".rcoder-delete-{operation_id}-{}-{}",
        identity.device, identity.inode
    ));
    let current = identity_at(&directory.path)?;
    if current.is_some() && current.as_ref() != Some(identity) {
        return Err(io::Error::other("Captured directory identity changed"));
    }
    match (current, identity_at(&retired)?) {
        (None, None) => return Ok(()),
        (None, Some(actual)) if actual == *identity => {}
        (Some(_), None) => {
            std::fs::rename(&directory.path, &retired)?;
            if identity_at(&retired)?.as_ref() != Some(identity) {
                if identity_at(&directory.path)?.is_none() {
                    std::fs::rename(&retired, &directory.path)?;
                }
                return Err(io::Error::other(
                    "Directory changed during retirement; contents preserved",
                ));
            }
        }
        _ => return Err(io::Error::other("Retired directory identity is ambiguous")),
    }
    std::fs::remove_dir_all(&retired)
}

#[cfg(not(unix))]
fn remove_captured_directory_blocking(_: &CapturedStorageDirectory, _: &str) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Captured directory retirement requires Unix",
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn retired_deletion_cannot_remove_reused_path_and_new_operation_can_proceed() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("workspace");
        tokio::fs::create_dir(&root).await.expect("original root");
        tokio::fs::write(root.join("keep"), b"original")
            .await
            .expect("original data");
        let captured = StorageDirectoryLease::capture(&root)
            .await
            .expect("capture")
            .receipt;
        retire_captured_directory_deletion(&captured, "old-operation")
            .await
            .expect("retire");
        retire_captured_directory_deletion(&captured, "old-operation")
            .await
            .expect("idempotent retirement");
        assert!(
            remove_captured_directory(&captured, "old-operation")
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read(root.join("keep"))
                .await
                .expect("original retained"),
            b"original"
        );
        tokio::fs::rename(&root, fixture.path().join("retained-original"))
            .await
            .expect("replace path");
        tokio::fs::create_dir(&root)
            .await
            .expect("replacement root");
        tokio::fs::write(root.join("keep"), b"replacement")
            .await
            .expect("replacement data");
        assert!(
            remove_captured_directory(&captured, "old-operation")
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read(root.join("keep"))
                .await
                .expect("replacement retained"),
            b"replacement"
        );
        let fresh = StorageDirectoryLease::capture(&root)
            .await
            .expect("fresh capture")
            .receipt;
        remove_captured_directory(&fresh, "new-operation")
            .await
            .expect("new request may delete own target");
        assert!(!root.exists());
        assert_eq!(
            tokio::fs::read(fixture.path().join("retained-original/keep"))
                .await
                .expect("unrelated original preserved"),
            b"original"
        );
    }

    #[tokio::test]
    async fn captured_directory_replacement_and_absence_never_delete_new_contents() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("workspace");
        let absent = StorageDirectoryLease::capture(&root)
            .await
            .expect("capture absence")
            .receipt;
        tokio::fs::create_dir(&root).await.expect("root");
        tokio::fs::write(root.join("keep"), b"new")
            .await
            .expect("new data");
        assert!(
            remove_captured_directory(&absent, "absent-operation")
                .await
                .is_err()
        );
        let captured = StorageDirectoryLease::capture(&root)
            .await
            .expect("capture root")
            .receipt;
        tokio::fs::rename(&root, fixture.path().join("old-root"))
            .await
            .expect("move original");
        tokio::fs::create_dir(&root).await.expect("new root");
        tokio::fs::write(root.join("keep"), b"replacement")
            .await
            .expect("replacement data");
        assert!(
            remove_captured_directory(&captured, "old-operation")
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read(root.join("keep"))
                .await
                .expect("replacement retained"),
            b"replacement"
        );
    }

    #[tokio::test]
    async fn clear_retains_root_and_distinguishes_missing_from_invalid_root() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("workspace");
        clear_directory_contents(&root).await.expect("missing root");
        tokio::fs::create_dir_all(root.join("nested"))
            .await
            .expect("directories");
        tokio::fs::write(root.join("nested/data"), b"content")
            .await
            .expect("file");
        clear_directory_contents(&root).await.expect("clear");
        assert!(root.is_dir());
        assert!(
            tokio::fs::read_dir(&root)
                .await
                .expect("root")
                .next_entry()
                .await
                .expect("entry")
                .is_none()
        );
        let file = fixture.path().join("file");
        tokio::fs::write(&file, b"retain").await.expect("file");
        assert_eq!(
            clear_directory_contents(&file)
                .await
                .expect_err("invalid root")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            tokio::fs::read(file).await.expect("retained file"),
            b"retain"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clear_unlinks_child_links_and_rejects_a_link_as_root() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("workspace");
        let outside = fixture.path().join("outside");
        tokio::fs::create_dir_all(&root).await.expect("root");
        tokio::fs::create_dir_all(&outside).await.expect("outside");
        tokio::fs::write(outside.join("keep"), b"external")
            .await
            .expect("outside data");
        std::os::unix::fs::symlink(&outside, root.join("directory-link")).expect("link");
        std::os::unix::fs::symlink(fixture.path().join("missing"), root.join("dangling-link"))
            .expect("dangling link");
        let linked_root = fixture.path().join("linked-root");
        std::os::unix::fs::symlink(&outside, &linked_root).expect("root link");
        assert_eq!(
            clear_directory_contents(&linked_root)
                .await
                .expect_err("root link rejected")
                .kind(),
            io::ErrorKind::InvalidInput
        );
        clear_directory_contents(&root)
            .await
            .expect("clear linked children");
        assert_eq!(
            tokio::fs::read(outside.join("keep"))
                .await
                .expect("external data"),
            b"external"
        );
        assert!(
            tokio::fs::read_dir(root)
                .await
                .expect("root")
                .next_entry()
                .await
                .expect("entry")
                .is_none()
        );
    }
}
