use super::{CapturedStorageDirectory, StorageDirectoryIdentity, StorageDirectoryLease};
use rustix::{
    fd::OwnedFd,
    fs::{self, AtFlags, Dir, FileType, Mode, OFlags},
    io::Errno,
};
use std::sync::Arc;
use std::{
    ffi::CString,
    io,
    path::{Path, PathBuf},
};

fn changed() -> io::Error {
    io::Error::other("Storage directory identity changed after capture")
}

#[allow(
    clippy::unnecessary_cast,
    reason = "libc device and inode widths vary across Unix targets"
)]
fn identity(stat: &fs::Stat) -> StorageDirectoryIdentity {
    StorageDirectoryIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    }
}

fn open_root(path: &Path) -> io::Result<Option<OwnedFd>> {
    match fs::openat(
        fs::CWD,
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Ok(Some(fd)),
        Err(Errno::NOENT) => Ok(None),
        Err(Errno::LOOP | Errno::NOTDIR) => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Storage root must be a directory, not a symbolic link",
        )),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn capture(path: PathBuf) -> io::Result<StorageDirectoryLease> {
    let handle = open_root(&path)?.map(Arc::new);
    let identity = handle
        .as_ref()
        .map(|fd| fs::fstat(fd).map(|stat| identity(&stat)))
        .transpose()?;
    Ok(StorageDirectoryLease {
        receipt: CapturedStorageDirectory { path, identity },
        handle,
    })
}

pub(super) fn validate_current(lease: &StorageDirectoryLease) -> io::Result<()> {
    let target = &lease.receipt;
    let current = capture(target.path.clone())?;
    if current.receipt.identity != target.identity {
        return Err(changed());
    }
    let Some(fd) = &lease.handle else {
        return if target.identity.is_none() {
            Ok(())
        } else {
            Err(changed())
        };
    };
    if Some(identity(&fs::fstat(fd)?)) != target.identity {
        return Err(changed());
    }
    Ok(())
}

pub(super) fn clear(lease: &StorageDirectoryLease) -> io::Result<()> {
    validate_current(lease)?;
    let target = &lease.receipt;
    let Some(fd) = &lease.handle else {
        return Ok(());
    };
    clear_handle(Arc::clone(fd))?;
    // A concurrent rename never redirects traversal. Still report the changed
    // path instead of claiming that the replacement workspace was cleared.
    if capture(target.path.clone())?.receipt.identity != target.identity {
        return Err(changed());
    }
    Ok(())
}

struct Frame {
    fd: Arc<OwnedFd>,
    entries: Dir,
    parent_entry: Option<(CString, StorageDirectoryIdentity)>,
}

fn clear_handle(root: Arc<OwnedFd>) -> io::Result<()> {
    let mut stack = vec![Frame {
        entries: Dir::read_from(&root)?,
        fd: root,
        parent_entry: None,
    }];
    while let Some(frame) = stack.last_mut() {
        if let Some(entry) = frame.entries.next() {
            let entry = entry?;
            let name = entry.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            let stat = fs::statat(&frame.fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
                let fd = Arc::new(fs::openat(
                    &frame.fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?);
                let expected = identity(&stat);
                if identity(&fs::fstat(&fd)?) != expected {
                    return Err(changed());
                }
                let next = Frame {
                    entries: Dir::read_from(&fd)?,
                    fd,
                    parent_entry: Some((name.to_owned(), expected)),
                };
                stack.push(next);
            } else {
                fs::unlinkat(&frame.fd, name, AtFlags::empty())?;
            }
        } else {
            let completed = stack.pop().ok_or_else(changed)?;
            if let Some((name, expected)) = completed.parent_entry {
                let parent = stack.last().ok_or_else(changed)?;
                if identity(&fs::statat(&parent.fd, &name, AtFlags::SYMLINK_NOFOLLOW)?) != expected
                {
                    return Err(changed());
                }
                fs::unlinkat(&parent.fd, &name, AtFlags::REMOVEDIR)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_root_replacement_and_creation_after_absence_are_rejected() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("workspace");
        let missing = capture(root.clone()).expect("capture absent root");
        std::fs::create_dir(&root).expect("create root");
        std::fs::write(root.join("original"), b"original").expect("content");
        assert!(
            clear(&missing).is_err(),
            "captured absence cannot authorize a later directory"
        );
        let original = capture(root.clone()).expect("capture original");
        let moved = fixture.path().join("moved");
        std::fs::rename(&root, &moved).expect("move root");
        std::fs::create_dir(&root).expect("replacement");
        std::fs::write(root.join("replacement"), b"replacement").expect("replacement content");
        assert!(
            clear(&original).is_err(),
            "old receipt cannot clear replacement"
        );
        assert_eq!(
            std::fs::read(moved.join("original")).expect("old content"),
            b"original"
        );
        assert_eq!(
            std::fs::read(root.join("replacement")).expect("new content"),
            b"replacement"
        );
    }

    #[test]
    fn traversal_remains_on_open_directory_after_path_is_replaced() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("workspace");
        std::fs::create_dir_all(root.join("nested")).expect("root");
        std::fs::write(root.join("nested/old"), b"old").expect("old content");
        let original = capture(root.clone()).expect("capture");
        validate_current(&original).expect("preflight");
        // Exact window: after path validation, before the handle traversal.
        let moved = fixture.path().join("moved");
        std::fs::rename(&root, &moved).expect("move");
        std::fs::create_dir(&root).expect("replacement");
        std::fs::write(root.join("keep"), b"replacement").expect("new content");
        clear_handle(Arc::clone(
            original.handle.as_ref().expect("captured handle"),
        ))
        .expect("clear original handle");
        assert!(
            std::fs::read_dir(&moved)
                .expect("original root")
                .next()
                .is_none()
        );
        assert_eq!(
            std::fs::read(root.join("keep")).expect("replacement retained"),
            b"replacement"
        );
        assert!(
            validate_current(&original).is_err(),
            "replacement cannot be reported as cleared"
        );
    }
}
