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

#[cfg(all(test, unix))]
mod tests {
    use super::*;

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
