//! Refuse opening an old Turso development file with a downgraded engine. The
//! marker is written before first engine access, under the directory lease.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct FormatMarker {
    format: String,
    filename: String,
    ready: bool,
}
const FORMAT: &str = "rcoder-toasty-0.10-turso-0.7-baseline-v1";
fn marker_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".rcoder-format");
    PathBuf::from(name)
}
fn filename(path: &Path) -> Result<String> {
    Ok(path
        .file_name()
        .context("missing database filename")?
        .as_encoded_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
fn require_regular(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "database format marker must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        ensure!(
            metadata.nlink() == 1,
            "database format marker must not be hard-linked"
        );
    }
    Ok(())
}
fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        File::open(path.parent().context("missing database directory")?)?.sync_all()?;
    }
    Ok(())
}
fn write_new(path: &Path, marker: &FormatMarker) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(&serde_json::to_vec(marker)?)?;
    file.sync_all()?;
    sync_directory(path)
}

pub(super) fn prepare(path: &Path) -> Result<()> {
    let target = marker_path(path);
    match std::fs::symlink_metadata(&target) {
        Ok(_) => {
            require_regular(&target)?;
            let marker: FormatMarker = serde_json::from_slice(&std::fs::read(&target)?)
                .context("invalid database format marker")?;
            ensure!(
                marker.format == FORMAT && marker.filename == filename(path)?,
                "database belongs to a different storage baseline"
            );
            ensure!(
                !marker.ready || path.exists(),
                "initialized database file is missing; refusing to create a new lifecycle authority"
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                !path.exists(),
                "existing database has no Toasty baseline format marker; refusing to open or downgrade an old development database; use a fresh data directory"
            );
            write_new(
                &target,
                &FormatMarker {
                    format: FORMAT.into(),
                    filename: filename(path)?,
                    ready: false,
                },
            )?;
        }
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

pub(super) fn mark_ready(path: &Path) -> Result<()> {
    let target = marker_path(path);
    require_regular(&target)?;
    let marker: FormatMarker = serde_json::from_slice(&std::fs::read(&target)?)?;
    ensure!(
        marker.format == FORMAT && marker.filename == filename(path)?,
        "database format marker changed during initialization"
    );
    if marker.ready {
        return Ok(());
    }
    let temporary = target.with_extension(format!("ready-{}", uuid::Uuid::new_v4().simple()));
    write_new(
        &temporary,
        &FormatMarker {
            ready: true,
            ..marker
        },
    )?;
    std::fs::rename(&temporary, &target)?;
    sync_directory(&target)
}

pub(super) fn require_ready(path: &Path) -> Result<()> {
    let target = marker_path(path);
    require_regular(&target)?;
    let marker: FormatMarker = serde_json::from_slice(&std::fs::read(target)?)?;
    ensure!(
        marker.ready
            && marker.format == FORMAT
            && marker.filename == filename(path)?
            && path.is_file(),
        "Offline observer requires an existing initialized database with the current format"
    );
    Ok(())
}
