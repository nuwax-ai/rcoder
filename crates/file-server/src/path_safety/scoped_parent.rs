//! Captured parent-directory handles for workspace leaf I/O.
//!
//! Directory links may resolve inside the selected root. Their targets are
//! translated into root-relative components and reopened from the captured root
//! handle; mutations never follow the original directory link. Once captured,
//! replacing the path of a parent cannot redirect leaf I/O to its replacement.

use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{self, AtFlags, FileType, Mode, OFlags};

use crate::error::{AppError, AppResult};

pub struct ScopedParent {
    directory: File,
    leaf: OsString,
}

impl ScopedParent {
    /// Capture the leaf's parent without following directory links during I/O.
    /// `false` performs no mutations and returns `None` for missing parents.
    /// `true` creates missing parents beneath the captured root, respecting umask.
    pub fn capture(root: &Path, relative: &Path, create_missing: bool) -> AppResult<Option<Self>> {
        let canonical = std::fs::canonicalize(root).map_err(|e| {
            AppError::system(format!("resolve workspace root {}: {e}", root.display()))
        })?;
        let target = super::ensure_within_path(&canonical, relative)?;
        let relative = target
            .strip_prefix(&canonical)
            .map_err(|e| AppError::validation(format!("resolve workspace-relative leaf: {e}")))?;
        let leaf = relative
            .file_name()
            .ok_or_else(|| AppError::validation("workspace path must name a leaf"))?
            .to_os_string();
        let root_fd = File::from(
            fs::open(root, directory_flags() & !OFlags::NOFOLLOW, Mode::empty()).map_err(|e| {
                AppError::system(format!("open workspace root {}: {e}", root.display()))
            })?,
        );
        let captured = root_fd
            .metadata()
            .map_err(|e| AppError::system(format!("inspect workspace handle: {e}")))?;
        let observed = std::fs::metadata(&canonical)
            .map_err(|e| AppError::system(format!("inspect workspace root: {e}")))?;
        if captured.dev() != observed.dev() || captured.ino() != observed.ino() {
            return Err(AppError::validation(
                "workspace root changed while capturing directory",
            ));
        }
        let mut pending = components(relative.parent().unwrap_or(Path::new("")))?;
        let mut resolved = PathBuf::new();
        let mut directory = root_fd
            .try_clone()
            .map_err(|e| AppError::system(format!("clone workspace directory handle: {e}")))?;
        let mut links = 0_u32;
        while let Some(name) = pending.pop_front() {
            match fs::openat(&directory, &name, directory_flags(), Mode::empty()) {
                Ok(fd) => {
                    directory = File::from(fd);
                    resolved.push(name);
                }
                Err(rustix::io::Errno::NOENT) if !create_missing => return Ok(None),
                Err(rustix::io::Errno::NOENT) => {
                    fs::mkdirat(&directory, &name, Mode::from_raw_mode(0o777))
                        .or_else(|e| {
                            if e == rustix::io::Errno::EXIST {
                                Ok(())
                            } else {
                                Err(e)
                            }
                        })
                        .map_err(|e| {
                            AppError::system(format!(
                                "create workspace directory {}: {e}",
                                name.to_string_lossy()
                            ))
                        })?;
                    // Reprocess with NOFOLLOW: a raced mkdir/symlink is checked
                    // by the same resolver instead of trusted as a directory.
                    pending.push_front(name);
                }
                Err(error)
                    if error == rustix::io::Errno::LOOP || error == rustix::io::Errno::NOTDIR =>
                {
                    let link = fs::readlinkat(&directory, &name, Vec::new()).map_err(|e| {
                        AppError::validation(format!(
                            "workspace path crosses a non-directory or changed link {}: {e}",
                            name.to_string_lossy()
                        ))
                    })?;
                    links += 1;
                    if links > 40 {
                        return Err(AppError::validation("too many workspace directory links"));
                    }
                    let target = root_relative_link(
                        &canonical,
                        &resolved,
                        OsStr::from_bytes(link.as_bytes()),
                    )?;
                    let mut expanded = components(&target)?;
                    expanded.append(&mut pending);
                    pending = expanded;
                    directory = root_fd.try_clone().map_err(|e| {
                        AppError::system(format!("clone workspace directory handle: {e}"))
                    })?;
                    resolved.clear();
                }
                Err(e) => {
                    return Err(AppError::system(format!(
                        "open workspace directory {}: {e}",
                        name.to_string_lossy()
                    )));
                }
            }
        }
        Ok(Some(Self { directory, leaf }))
    }

    pub fn open_read(&self) -> std::io::Result<File> {
        self.open_regular(OFlags::RDONLY)
    }

    pub fn open_write(&self) -> std::io::Result<File> {
        self.open_regular(OFlags::WRONLY | OFlags::CREATE | OFlags::TRUNC)
    }

    pub fn open_directory(&self) -> std::io::Result<File> {
        fs::openat(
            &self.directory,
            &self.leaf,
            directory_flags(),
            Mode::empty(),
        )
        .map(File::from)
        .map_err(Into::into)
    }

    pub fn leaf_exists(&self) -> std::io::Result<bool> {
        match fs::statat(&self.directory, &self.leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn leaf_identity(&self) -> std::io::Result<Option<(u64, u64)>> {
        match fs::statat(&self.directory, &self.leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => {
                #[cfg(any(target_os = "macos", target_os = "ios"))]
                let device = stat.st_dev as u64;
                #[cfg(not(any(target_os = "macos", target_os = "ios")))]
                let device = stat.st_dev;
                Ok(Some((device, stat.st_ino)))
            }
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub fn sync_parent(&self) -> std::io::Result<()> {
        let directory = File::from(fs::openat(
            &self.directory,
            ".",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )?);
        directory.sync_all()
    }

    pub fn same_parent(&self, other: &Self) -> std::io::Result<bool> {
        let own = self.directory.metadata()?;
        let observed = other.directory.metadata()?;
        Ok(own.dev() == observed.dev() && own.ino() == observed.ino())
    }

    fn open_regular(&self, access: OFlags) -> std::io::Result<File> {
        let fd = fs::openat(
            &self.directory,
            &self.leaf,
            access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::from_raw_mode(0o666),
        )?;
        let file = File::from(fd);
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "workspace leaf is not a regular file",
            ));
        }
        Ok(file)
    }

    pub fn is_symlink(&self) -> std::io::Result<bool> {
        match fs::statat(&self.directory, &self.leaf, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(FileType::from_raw_mode(stat.st_mode) == FileType::Symlink),
            Err(rustix::io::Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub fn remove_file(&self) -> std::io::Result<()> {
        fs::unlinkat(&self.directory, &self.leaf, AtFlags::empty()).map_err(Into::into)
    }

    pub fn remove_directory(&self) -> std::io::Result<()> {
        fs::unlinkat(&self.directory, &self.leaf, AtFlags::REMOVEDIR).map_err(Into::into)
    }

    pub fn symlink(&self, target: &OsStr) -> std::io::Result<()> {
        fs::symlinkat(target, &self.directory, &self.leaf).map_err(Into::into)
    }

    /// Move a leaf between captured parents without replacing a raced target.
    pub fn rename_to_noreplace(&self, target: &Self) -> std::io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "ios"))]
        {
            fs::renameat_with(
                &self.directory,
                &self.leaf,
                &target.directory,
                &target.leaf,
                fs::RenameFlags::NOREPLACE,
            )
            .map_err(Into::into)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "ios")))]
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "conditional captured-parent rename is unsupported on this Unix platform",
        ))
    }

    /// Atomically replace a component-owned state file inside this parent.
    pub fn atomic_write(&self, bytes: &[u8]) -> std::io::Result<()> {
        use std::io::Write as _;
        let temporary = format!(".rcoder-write-{}", uuid::Uuid::now_v7().simple());
        let fd = fs::openat(
            &self.directory,
            &temporary,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?;
        let mut file = File::from(fd);
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::renameat(&self.directory, &temporary, &self.directory, &self.leaf)?;
            let directory = File::from(fs::openat(
                &self.directory,
                ".",
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
                Mode::empty(),
            )?);
            directory.sync_all()
        })();
        if result.is_err() {
            // The generated name is never reused. Preserve an unexpected entry
            // rather than deleting it based only on a familiar temporary name.
            if let (Ok(meta), Ok(fd)) = (
                file.metadata(),
                fs::openat(
                    &self.directory,
                    &temporary,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                ),
            ) && let Ok(current) = File::from(fd).metadata()
                && meta.dev() == current.dev()
                && meta.ino() == current.ino()
            {
                let _ = fs::unlinkat(&self.directory, &temporary, AtFlags::empty());
            }
        }
        result
    }
}

fn directory_flags() -> OFlags {
    #[cfg(target_os = "linux")]
    let access = OFlags::PATH;
    #[cfg(not(target_os = "linux"))]
    let access = OFlags::RDONLY;
    access | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn components(path: &Path) -> AppResult<VecDeque<OsString>> {
    path.components()
        .map(|part| match part {
            Component::Normal(name) => Ok(name.to_os_string()),
            _ => Err(AppError::validation(
                "unexpected component in workspace-relative directory",
            )),
        })
        .collect()
}

fn root_relative_link(root: &Path, parent: &Path, target: &OsStr) -> AppResult<PathBuf> {
    // Resolve the actual link spelling before reducing components: collapsing
    // `missing/../inside` would silently turn a broken link into a valid one.
    let mut probe = root.join(parent).join(target);
    let mut missing = Vec::new();
    let resolved = loop {
        match std::fs::canonicalize(&probe) {
            Ok(mut path) => {
                for name in missing.iter().rev() {
                    path.push(name);
                }
                break path;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = probe
                    .file_name()
                    .ok_or_else(|| {
                        AppError::validation("directory link has no accessible ancestor")
                    })?
                    .to_os_string();
                missing.push(name);
                if !probe.pop() {
                    return Err(AppError::validation(
                        "directory link has no accessible ancestor",
                    ));
                }
            }
            Err(error) => {
                return Err(AppError::system(format!(
                    "resolve workspace directory link {}: {error}",
                    probe.display()
                )));
            }
        }
    };
    resolved
        .strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| AppError::validation("directory link resolves outside the workspace"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};

    #[test]
    fn captured_parent_write_and_unlink_ignore_a_replacement_directory_link() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        let outside = fixture.path().join("outside");
        std::fs::create_dir_all(root.join("parent")).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("file"), b"KEEP").unwrap();
        let captured = ScopedParent::capture(&root, Path::new("parent/file"), false)
            .unwrap()
            .unwrap();
        std::fs::rename(root.join("parent"), root.join("original")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("parent")).unwrap();
        captured.open_write().unwrap().write_all(b"scoped").unwrap();
        assert_eq!(
            std::fs::read(root.join("original/file")).unwrap(),
            b"scoped"
        );
        captured.remove_file().unwrap();
        assert!(!root.join("original/file").exists());
        captured.symlink(OsStr::new("target")).unwrap();
        assert_eq!(
            std::fs::read_link(root.join("original/file")).unwrap(),
            Path::new("target")
        );
        assert_eq!(std::fs::read(outside.join("file")).unwrap(), b"KEEP");
    }

    #[test]
    fn capture_preserves_inside_links_and_missing_parent_observation() {
        let fixture = tempfile::tempdir().unwrap();
        let root = fixture.path().join("root");
        std::fs::create_dir_all(root.join("inside")).unwrap();
        std::os::unix::fs::symlink("inside", root.join("alias")).unwrap();
        assert!(
            ScopedParent::capture(&root, Path::new("missing/leaf"), false)
                .unwrap()
                .is_none()
        );
        assert!(!root.join("missing").exists());
        let parent = ScopedParent::capture(&root, Path::new("alias/file"), true)
            .unwrap()
            .unwrap();
        parent.open_write().unwrap().write_all(b"inside").unwrap();
        let mut contents = String::new();
        parent
            .open_read()
            .unwrap()
            .read_to_string(&mut contents)
            .unwrap();
        assert_eq!(contents, "inside");
        assert_eq!(std::fs::read(root.join("inside/file")).unwrap(), b"inside");
        let outside = fixture.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(outside, root.join("foreign")).unwrap();
        assert!(ScopedParent::capture(&root, Path::new("foreign/file"), false).is_err());
    }

    #[test]
    fn scoped_read_refuses_fifo_without_waiting_for_a_writer() {
        let fixture = tempfile::tempdir().unwrap();
        let fifo = fixture.path().join("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        let parent = ScopedParent::capture(fixture.path(), Path::new("fifo"), false)
            .unwrap()
            .unwrap();
        assert_eq!(
            parent.open_read().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
}
