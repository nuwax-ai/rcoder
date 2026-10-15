//! Cleanup only captured preparation objects. Child links are unlinked, never
//! traversed; replacing a captured name produces an error instead of deleting
//! its replacement.
use super::receipt::{self, Identity};
use crate::error::{AppError, AppResult};
#[cfg(unix)]
use std::fs::File;
use std::path::Path;

pub(super) fn remove_preparation(
    root: &Path,
    relative: &Path,
    expected: &Identity,
) -> AppResult<()> {
    #[cfg(unix)]
    {
        let parent =
            crate::path_safety::ScopedParent::capture(root, relative, false)?.ok_or_else(|| {
                AppError::system("restore cleanup parent disappeared; evidence retained")
            })?;
        let directory = parent.open_directory()?;
        if receipt::file_id(&directory)? != *expected {
            return Err(AppError::system(
                "captured restore cleanup identity changed; evidence retained",
            ));
        }
        remove_children(&directory)?;
        let actual = parent
            .leaf_identity()?
            .map(|(device, inode)| Identity::Unix { device, inode });
        if actual.as_ref() != Some(expected) {
            return Err(AppError::system(
                "restore cleanup directory was replaced; evidence retained",
            ));
        }
        parent.remove_directory()?;
        parent.sync_parent()?;
    }
    #[cfg(not(unix))]
    {
        if receipt::identity(&root.join(relative))?.as_ref() != Some(expected) {
            return Err(AppError::system(
                "restore cleanup identity changed; evidence retained",
            ));
        }
        std::fs::remove_dir_all(root.join(relative))?;
    }
    Ok(())
}

#[cfg(unix)]
fn remove_children(directory: &File) -> std::io::Result<()> {
    remove_children_with_hook(directory, &mut |_| Ok(()))
}

#[cfg(unix)]
fn remove_children_with_hook(
    directory: &File,
    hook: &mut impl FnMut(&File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use rustix::fs::{self, AtFlags, FileType, Mode, OFlags, RenameFlags};
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    let mut children = Vec::new();
    for entry in fs::Dir::read_from(directory)? {
        let entry = entry?;
        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if name == "." || name == ".." {
            continue;
        }
        let stat = fs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
        children.push((
            name.to_os_string(),
            stat.st_dev,
            stat.st_ino,
            FileType::from_raw_mode(stat.st_mode),
        ));
    }
    hook(directory)?;
    for (name, device, inode, kind) in children {
        let current = fs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)?;
        if current.st_dev != device || current.st_ino != inode {
            return Err(std::io::Error::other(
                "restore cleanup child identity changed; evidence retained",
            ));
        }
        let private = format!(".cleanup-{}", uuid::Uuid::now_v7().simple());
        fs::renameat_with(
            directory,
            &name,
            directory,
            &private,
            RenameFlags::NOREPLACE,
        )?;
        let moved = fs::statat(directory, &private, AtFlags::SYMLINK_NOFOLLOW)?;
        if moved.st_dev != device || moved.st_ino != inode {
            let restored = fs::renameat_with(
                directory,
                &private,
                directory,
                &name,
                RenameFlags::NOREPLACE,
            );
            return Err(std::io::Error::other(format!(
                "restore cleanup child changed during capture; retained at {private}; restoring its name: {restored:?}"
            )));
        }
        if kind == FileType::Directory {
            let child = File::from(fs::openat(
                directory,
                &private,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            let observed = child.metadata()?;
            use std::os::unix::fs::MetadataExt;
            #[cfg(any(target_os = "macos", target_os = "ios"))]
            let expected_device = device as u64;
            #[cfg(not(any(target_os = "macos", target_os = "ios")))]
            let expected_device = device;
            if observed.dev() != expected_device || observed.ino() != inode {
                return Err(std::io::Error::other(
                    "restore cleanup child handle changed; evidence retained",
                ));
            }
            remove_children_with_hook(&child, hook)?;
            let current = fs::statat(directory, &private, AtFlags::SYMLINK_NOFOLLOW)?;
            if current.st_dev != device || current.st_ino != inode {
                return Err(std::io::Error::other(
                    "restore cleanup child replaced before unlink; evidence retained",
                ));
            }
            fs::unlinkat(directory, &private, AtFlags::REMOVEDIR)?;
        } else {
            let current = fs::statat(directory, &private, AtFlags::SYMLINK_NOFOLLOW)?;
            if current.st_dev != device || current.st_ino != inode {
                return Err(std::io::Error::other(
                    "restore cleanup leaf replaced before unlink; evidence retained",
                ));
            }
            fs::unlinkat(directory, &private, AtFlags::empty())?;
        }
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    #[test]
    fn replaced_cleanup_child_is_reported_without_deleting_the_replacement() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("child"), b"ORIGINAL").unwrap();
        let directory = File::open(root.path()).unwrap();
        let mut swapped = false;
        let result = remove_children_with_hook(&directory, &mut |_| {
            if !swapped {
                std::fs::rename(root.path().join("child"), root.path().join("original"))?;
                std::fs::write(root.path().join("child"), b"REPLACEMENT")?;
                swapped = true;
            }
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(
            std::fs::read(root.path().join("child")).unwrap(),
            b"REPLACEMENT"
        );
        assert_eq!(
            std::fs::read(root.path().join("original")).unwrap(),
            b"ORIGINAL"
        );
    }
}
