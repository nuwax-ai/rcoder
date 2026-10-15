#[cfg(not(unix))]
use std::fs::File;
use std::{
    io::Read as _,
    path::{Path, PathBuf},
};

use serde::Serialize;

use crate::error::{AppError, AppResult};

#[cfg(not(unix))]
use super::lock::DirectoryIdentity;
use super::receipt::{self, StoredReceipt};

pub(super) struct Captured {
    root: PathBuf,
    path: PathBuf,
    #[cfg(unix)]
    parent: crate::path_safety::ScopedParent,
    #[cfg(not(unix))]
    parent_id: DirectoryIdentity,
}

impl Captured {
    pub(super) fn capture(root: &Path, path: &Path) -> AppResult<Option<Self>> {
        let relative = path
            .strip_prefix(root)
            .map_err(|error| AppError::system(format!("preserve receipt scope: {error}")))?;
        #[cfg(unix)]
        {
            let Some(parent) = crate::path_safety::ScopedParent::capture(root, relative, false)?
            else {
                return Ok(None);
            };
            Ok(Some(Self {
                root: root.to_owned(),
                path: path.to_owned(),
                parent,
            }))
        }
        #[cfg(not(unix))]
        {
            crate::path_safety::ensure_within_path(root, relative)?;
            let parent = path
                .parent()
                .ok_or_else(|| AppError::system("receipt has no parent"))?;
            if !parent.exists() {
                return Ok(None);
            }
            let resolved = super::lock::checked_directory(root, parent)?;
            let parent_id = super::lock::identity(&resolved, true)?;
            Ok(Some(Self {
                root: root.to_owned(),
                path: path.to_owned(),
                parent_id,
            }))
        }
    }

    pub(super) fn check_binding(&self) -> AppResult<()> {
        #[cfg(unix)]
        {
            let relative = self
                .path
                .strip_prefix(&self.root)
                .map_err(|error| AppError::system(format!("receipt scope changed: {error}")))?;
            let current = crate::path_safety::ScopedParent::capture(&self.root, relative, false)?
                .ok_or_else(|| AppError::system("preserve receipt parent disappeared"))?;
            if !self.parent.same_parent(&current)? {
                return Err(AppError::system(
                    "preserve receipt parent binding changed; data retained",
                ));
            }
        }
        #[cfg(not(unix))]
        {
            let parent = self
                .path
                .parent()
                .ok_or_else(|| AppError::system("receipt has no parent"))?;
            let current = super::lock::checked_directory(&self.root, parent)?;
            if super::lock::identity(&current, true)? != self.parent_id {
                return Err(AppError::system(
                    "preserve receipt parent binding changed; data retained",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn bytes(&self) -> AppResult<Option<Vec<u8>>> {
        #[cfg(unix)]
        let opened = self.parent.open_read();
        #[cfg(not(unix))]
        let opened = {
            self.check_binding()?;
            match std::fs::symlink_metadata(&self.path) {
                Ok(meta) if !meta.is_file() || meta.is_symlink() => {
                    return Err(AppError::system("preserve receipt is not a regular file"));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(AppError::from(error)),
                Ok(_) => {}
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::OpenOptionsExt;
                const OPEN_REPARSE_POINT: u32 = 0x0020_0000;
                File::options()
                    .read(true)
                    .custom_flags(OPEN_REPARSE_POINT)
                    .open(&self.path)
            }
            #[cfg(not(windows))]
            File::open(&self.path)
        };
        let mut file = match opened {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(AppError::system(format!(
                    "read {}: {error}",
                    self.path.display()
                )));
            }
        };
        if !file.metadata()?.is_file() {
            return Err(AppError::system(
                "opened preserve receipt is not a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        self.check_binding()?;
        Ok(Some(bytes))
    }

    pub(super) fn read(&self) -> AppResult<Option<StoredReceipt>> {
        match self.bytes()? {
            Some(bytes) => receipt::decode(&self.path, bytes),
            None => Ok(None),
        }
    }

    pub(super) fn write_bytes(&self, bytes: &[u8]) -> AppResult<()> {
        self.check_binding()?;
        #[cfg(test)]
        super::test_gate::after_receipt_check(&self.root)?;
        #[cfg(unix)]
        self.parent.atomic_write(bytes)?;
        #[cfg(not(unix))]
        {
            use std::io::Write as _;
            receipt::publish(&self.path, |file| file.write_all(bytes))?;
        }
        self.check_binding()
    }

    pub(super) fn write_json<T: Serialize>(&self, value: &T) -> AppResult<()> {
        let bytes = serde_json::to_vec(value)
            .map_err(|error| AppError::system(format!("serialize preserve receipt: {error}")))?;
        self.write_bytes(&bytes)
    }

    pub(super) fn remove(&self) -> AppResult<()> {
        self.check_binding()?;
        #[cfg(unix)]
        {
            self.parent.remove_file()?;
            self.parent.sync_parent()?;
        }
        #[cfg(not(unix))]
        std::fs::remove_file(&self.path)?;
        self.check_binding()
    }
}
