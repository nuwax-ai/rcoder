#[cfg(any(test, not(unix)))]
use std::fs::File;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::{AppError, AppResult};

use super::lock::DirectoryIdentity;

pub(super) const VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct LegacyReceipt {
    pub version: u32,
    pub operation_id: String,
    pub workspace_root: String,
    pub skills: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Preserving,
    Preserved,
    Rebuilding,
    Restoring,
    Restored,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SkillPhase {
    Planned,
    Preserved,
    RestoreAccepted,
    Restored,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum SourceLayout {
    Operation,
    Legacy,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Skill {
    pub name: String,
    pub identity: DirectoryIdentity,
    pub phase: SkillPhase,
    pub source_layout: SourceLayout,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub version: u32,
    pub operation_id: String,
    pub workspace_root: String,
    pub workspace_identity: DirectoryIdentity,
    pub data_dir: String,
    pub data_identity: DirectoryIdentity,
    pub revision: u64,
    pub phase: Phase,
    pub skills: Vec<Skill>,
}

pub(super) enum StoredReceipt {
    Legacy(LegacyReceipt, Vec<u8>),
    Current(Receipt),
}

pub(super) fn validate_name(name: &str) -> AppResult<()> {
    let mut components = Path::new(name).components();
    let valid = matches!(
        components.next(),
        Some(std::path::Component::Normal(component))
            if component == std::ffi::OsStr::new(name)
    ) && components.next().is_none()
        && !name.contains('\0');
    if valid {
        Ok(())
    } else {
        Err(AppError::system(format!(
            "invalid skill name {name:?} in preserve receipt; recovery data and receipt left unchanged"
        )))
    }
}

pub(super) fn validate_names<'a>(names: impl Iterator<Item = &'a str>) -> AppResult<()> {
    let mut seen = std::collections::HashSet::new();
    for name in names {
        validate_name(name)?;
        if !seen.insert(name) {
            return Err(AppError::system(format!(
                "duplicate skill name {name:?} in preserve receipt; recovery data retained"
            )));
        }
    }
    Ok(())
}

pub(super) use super::receipt_io::Captured;

pub(super) fn read(root: &Path, path: &Path) -> AppResult<Option<StoredReceipt>> {
    match Captured::capture(root, path)? {
        Some(parent) => parent.read(),
        None => Ok(None),
    }
}

pub(super) fn read_bytes(root: &Path, path: &Path) -> AppResult<Option<Vec<u8>>> {
    match Captured::capture(root, path)? {
        Some(parent) => parent.bytes(),
        None => Ok(None),
    }
}

pub(super) fn decode(path: &Path, bytes: Vec<u8>) -> AppResult<Option<StoredReceipt>> {
    #[derive(Deserialize)]
    struct Version {
        version: u32,
    }
    let version: Version = serde_json::from_slice(&bytes).map_err(|error| {
        AppError::system(format!(
            "invalid preserve receipt {}: {error}; receipt and copies retained",
            path.display()
        ))
    })?;
    let invalid = |error: serde_json::Error| {
        AppError::system(format!(
            "invalid preserve receipt {}: {error}; receipt and copies retained",
            path.display()
        ))
    };
    match version.version {
        1 => {
            let receipt: LegacyReceipt = serde_json::from_slice(&bytes).map_err(invalid)?;
            validate_names(receipt.skills.iter().map(String::as_str))?;
            if receipt.operation_id.is_empty() || receipt.skills.is_empty() {
                return Err(AppError::system(
                    "invalid empty legacy preserve operation; original retained",
                ));
            }
            Ok(Some(StoredReceipt::Legacy(receipt, bytes)))
        }
        VERSION => {
            let receipt: Receipt = serde_json::from_slice(&bytes).map_err(invalid)?;
            validate_names(receipt.skills.iter().map(|skill| skill.name.as_str()))?;
            validate_name(&receipt.data_dir)?;
            if receipt.operation_id.is_empty() || receipt.skills.is_empty() {
                return Err(AppError::system(
                    "invalid empty preserve operation; receipt retained",
                ));
            }
            Ok(Some(StoredReceipt::Current(receipt)))
        }
        other => Err(AppError::system(format!(
            "unsupported preserve receipt version {other} at {}; receipt retained",
            path.display()
        ))),
    }
}

#[cfg(any(test, not(unix)))]
pub(super) fn publish<F>(path: &Path, write: F) -> AppResult<()>
where
    F: FnOnce(&mut File) -> std::io::Result<()>,
{
    let parent = path
        .parent()
        .ok_or_else(|| AppError::system("preserve receipt has no parent directory"))?;
    let mut file = tempfile::Builder::new()
        .prefix(".receipt-")
        .suffix(".tmp")
        .tempfile_in(parent)?;
    write(file.as_file_mut())
        .map_err(|error| AppError::system(format!("write {}: {error}", path.display())))?;
    file.as_file().sync_all()?;
    process_utils::atomic_file::persist(file, path)
        .map_err(|error| AppError::system(format!("publish {}: {error}", path.display())))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}

pub(super) fn write_json<T: Serialize>(root: &Path, path: &Path, value: &T) -> AppResult<()> {
    Captured::capture(root, path)?
        .ok_or_else(|| AppError::system("preserve receipt parent is unavailable"))?
        .write_json(value)
}

pub(super) fn write_bytes(root: &Path, path: &Path, bytes: &[u8]) -> AppResult<()> {
    Captured::capture(root, path)?
        .ok_or_else(|| AppError::system("preserve receipt parent is unavailable"))?
        .write_bytes(bytes)
}
