use crate::error::{AppError, AppResult};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read as _,
    path::{Component, Path, PathBuf},
};

pub(super) const LOCK: &str = ".rcoder-version-restore.lock";
pub(super) const ACTIVE: &str = ".rcoder-version-restore-active.json";
pub(super) const PREFIX: &str = ".rcoder-version-restore-";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Identity {
    Unix { device: u64, inode: u64 },
    Windows { volume: u64, index: u64 },
}

pub(super) fn file_id(file: &File) -> AppResult<Identity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let m = file.metadata()?;
        Ok(Identity::Unix {
            device: m.dev(),
            inode: m.ino(),
        })
    }
    #[cfg(windows)]
    {
        let info = winapi_util::file::information(file)
            .map_err(|e| AppError::system(format!("read restore file identity: {e}")))?;
        Ok(Identity::Windows {
            volume: info.volume_serial_number(),
            index: info.file_index(),
        })
    }
    #[cfg(not(any(unix, windows)))]
    Err(AppError::system(
        "version restore identities are unsupported on this platform",
    ))
}

pub(super) fn identity(path: &Path) -> AppResult<Option<Identity>> {
    let m = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(AppError::system(format!(
                "inspect restore entry {}: {e}",
                path.display()
            )));
        }
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Ok(Some(Identity::Unix {
            device: m.dev(),
            inode: m.ino(),
        }))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        let _ = m;
        const FILE_READ_ATTRIBUTES: u32 = 0x80;
        const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        let file = File::options()
            .access_mode(FILE_READ_ATTRIBUTES)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .open(path)?;
        file_id(&file).map(Some)
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = m;
        Err(AppError::system(
            "version restore identities are unsupported on this platform",
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Active {
    pub directory: String,
    pub identity: Identity,
    pub root: Identity,
    pub cleanup: Option<String>,
    pub plan_identity: Identity,
    pub plan_hash: String,
    pub cursor: usize,
    pub state: State,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Applying,
    RollingBack,
    Committed,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Action {
    pub source: PathBuf,
    pub target: PathBuf,
    pub identity: Identity,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Receipt {
    pub schema: u8,
    pub root: Identity,
    pub directory: Identity,
    pub actions: Vec<Action>,
    pub cursor: usize,
    pub state: State,
}

pub(super) fn normal_relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|p| matches!(p, Component::Normal(_)))
}

pub(super) fn valid_name(name: &str) -> bool {
    let Some(id) = name.strip_prefix(PREFIX) else {
        return false;
    };
    normal_relative(Path::new(name))
        && Path::new(name).components().count() == 1
        && uuid::Uuid::parse_str(id).is_ok()
}

pub(super) fn reserved(path: &Path) -> bool {
    path.components()
        .next()
        .is_some_and(|p| p.as_os_str() == LOCK || p.as_os_str() == ACTIVE)
}

pub(super) fn ensure_root(root: &Path, expected: &Identity) -> AppResult<()> {
    if identity(root)?.as_ref() != Some(expected) {
        return Err(AppError::system(
            "restore workspace identity changed; recovery evidence retained",
        ));
    }
    Ok(())
}

pub(super) fn read_json<T: serde::de::DeserializeOwned>(
    root: &Path,
    relative: &Path,
    max: u64,
) -> AppResult<Option<T>> {
    let Some((bytes, _)) = read_bytes(root, relative, max)? else {
        return Ok(None);
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        AppError::system(format!(
            "parse restore receipt {}: {e}; evidence retained",
            relative.display()
        ))
    })
}

fn read_bytes(root: &Path, relative: &Path, max: u64) -> AppResult<Option<(Vec<u8>, Identity)>> {
    #[cfg(unix)]
    let file = {
        let Some(parent) = crate::path_safety::ScopedParent::capture(root, relative, false)? else {
            return Ok(None);
        };
        parent.open_read()
    };
    #[cfg(not(unix))]
    let file = File::open(crate::path_safety::ensure_within_path(root, relative)?);
    let file = match file {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(AppError::system(format!(
                "read restore receipt {}: {e}",
                relative.display()
            )));
        }
    };
    let id = file_id(&file)?;
    let mut bytes = Vec::new();
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(AppError::system(
            "restore receipt exceeds size limit; evidence retained",
        ));
    }
    Ok(Some((bytes, id)))
}

pub(super) fn write_json<T: Serialize>(root: &Path, relative: &Path, value: &T) -> AppResult<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|e| AppError::system(format!("serialize restore receipt: {e}")))?;
    #[cfg(unix)]
    {
        let parent = crate::path_safety::ScopedParent::capture(root, relative, true)?
            .ok_or_else(|| AppError::system("restore receipt parent remains missing"))?;
        parent.atomic_write(&bytes)?;
    }
    #[cfg(not(unix))]
    {
        use std::io::Write as _;
        let path = crate::path_safety::ensure_within_path(root, relative)?;
        let parent = path
            .parent()
            .ok_or_else(|| AppError::system("restore receipt has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let mut file = tempfile::NamedTempFile::new_in(parent)?;
        file.write_all(&bytes)?;
        file.as_file().sync_all()?;
        file.persist(&path)
            .map_err(|e| AppError::system(format!("publish restore receipt: {}", e.error)))?;
    }
    Ok(())
}

pub(super) fn validate(root: &Path, active: &Active, receipt: &Receipt) -> AppResult<()> {
    if !valid_name(&active.directory)
        || active.cleanup.as_deref().is_some_and(|p| !valid_name(p))
        || receipt.schema != 1
        || receipt.root != active.root
        || receipt.directory != active.identity
        || receipt.cursor > receipt.actions.len()
        || receipt.actions.len() > 200_000
        || (receipt.state == State::Committed && receipt.cursor != receipt.actions.len())
    {
        return Err(AppError::system(
            "invalid restore receipt binding; evidence retained",
        ));
    }
    ensure_root(root, &active.root)?;
    let incoming = Path::new(&active.directory).join("incoming");
    let backup = Path::new(&active.directory).join("backup");
    for action in &receipt.actions {
        if !normal_relative(&action.source) || !normal_relative(&action.target) {
            return Err(AppError::system(
                "restore receipt path escapes workspace; evidence retained",
            ));
        }
        let install = action.source.starts_with(&incoming)
            && !action.target.starts_with(&active.directory)
            && !reserved(&action.target);
        let save = !action.source.starts_with(&active.directory)
            && !reserved(&action.source)
            && action.target.starts_with(&backup);
        if !install && !save {
            return Err(AppError::system(
                "invalid restore action scope; evidence retained",
            ));
        }
    }
    Ok(())
}

pub(super) fn plan_path(active: &Active) -> PathBuf {
    PathBuf::from(format!("{}.plan.json", active.directory))
}

pub(super) fn save_progress(root: &Path, active: &mut Active, receipt: &Receipt) -> AppResult<()> {
    active.cursor = receipt.cursor;
    active.state = receipt.state;
    write_json(root, Path::new(ACTIVE), active)
}

pub(super) fn load_plan(root: &Path, active: &Active) -> AppResult<Receipt> {
    use sha2::Digest as _;
    if !valid_name(&active.directory) || active.cleanup.as_deref().is_some_and(|p| !valid_name(p)) {
        return Err(AppError::system(
            "invalid restore directory binding; evidence retained",
        ));
    }
    let path = plan_path(active);
    let (bytes, id) = read_bytes(root, &path, 128 * 1024 * 1024)?
        .ok_or_else(|| AppError::system("restore plan missing; evidence retained"))?;
    if id != active.plan_identity {
        return Err(AppError::system(
            "restore plan identity changed; evidence retained",
        ));
    }
    if bytes.len() > 128 * 1024 * 1024
        || hex::encode(sha2::Sha256::digest(&bytes)) != active.plan_hash
    {
        return Err(AppError::system(
            "restore plan content changed; evidence retained",
        ));
    }
    let mut receipt: Receipt = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::system(format!("parse restore plan: {e}")))?;
    receipt.cursor = active.cursor;
    receipt.state = active.state;
    validate(root, active, &receipt)?;
    Ok(receipt)
}
