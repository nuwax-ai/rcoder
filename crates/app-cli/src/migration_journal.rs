//! A command may commit SQL before its process or coordinator disappears.
//! Persist intent before dispatch; only confirmed success permits automatic reuse.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    identity: String,
    completed: bool,
}

pub(crate) struct MigrationJournal {
    root: PathBuf,
    identity: String,
    _lease: File,
}

fn state_root(workspace: &Path) -> Result<PathBuf> {
    Ok(
        match std::env::var_os("APP_CLI_STATE_ROOT").filter(|value| !value.is_empty()) {
            Some(root) => PathBuf::from(root),
            None => workspace
                .parent()
                .context("workspace has no state root")?
                .to_path_buf(),
        }
        .join("migration-receipts"),
    )
}

fn inspect_confirmed(root: &Path) -> Result<bool> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(true),
        Err(error) => return Err(error).context("read migration receipts"),
    };
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let receipt: Receipt = serde_json::from_slice(&std::fs::read(&path)?)
                .context("decode migration receipt")?;
            if !receipt.completed {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn require_confirmed(root: &Path) -> Result<()> {
    ensure!(
        inspect_confirmed(root)?,
        "Migration outcome is unconfirmed; explicit database reconciliation is required"
    );
    Ok(())
}

/// Observation is not execution permission; begin() rechecks under its lease.
pub(crate) fn inspect_migrations(workspace: &Path) -> Result<bool> {
    inspect_confirmed(&state_root(workspace)?)
}

pub(crate) fn require_confirmed_migrations(workspace: &Path) -> Result<()> {
    require_confirmed(&state_root(workspace)?)
}

impl MigrationJournal {
    /// `identity` is a digest of the immutable release and service, never credentials.
    pub fn begin(workspace: &Path, identity: String) -> Result<Option<Self>> {
        Self::begin_at(state_root(workspace)?, identity)
    }

    fn begin_at(root: PathBuf, identity: String) -> Result<Option<Self>> {
        std::fs::create_dir_all(&root)?;
        let lease = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("execution.lock"))?;
        lease
            .try_lock()
            .context("another database migration is executing")?;
        require_confirmed(&root)?;
        let journal = Self {
            root,
            identity,
            _lease: lease,
        };
        match std::fs::read(journal.path()) {
            Ok(bytes) => {
                let receipt: Receipt = serde_json::from_slice(&bytes)?;
                ensure!(
                    receipt.identity == journal.identity && receipt.completed,
                    "Migration receipt identity or completion mismatch"
                );
                Ok(None)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                journal.write(false)?;
                Ok(Some(journal))
            }
            Err(error) => Err(error).context("read migration receipt"),
        }
    }

    fn path(&self) -> PathBuf {
        self.root.join(format!("{}.json", self.identity))
    }

    fn write(&self, completed: bool) -> Result<()> {
        let receipt = Receipt {
            identity: self.identity.clone(),
            completed,
        };
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)?;
        temporary.write_all(&serde_json::to_vec(&receipt)?)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(self.path())
            .map_err(|error| error.error)?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        let readback: Receipt = serde_json::from_slice(&std::fs::read(self.path())?)?;
        ensure!(
            readback.identity == self.identity && readback.completed == completed,
            "Migration receipt readback mismatch"
        );
        Ok(())
    }

    pub fn complete(self) -> Result<()> {
        self.write(true)
    }
}

pub(crate) fn identity(release: &crate::manifest::ReleaseLock, service_id: &str) -> Result<String> {
    let bytes = shared_types::encode_userapp_intent(&(release, service_id))?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn confirmed_completion_survives_reopen_and_skips_execution() {
        let root = tempfile::tempdir().unwrap();
        let receipt = MigrationJournal::begin_at(root.path().into(), "releaseone".into())
            .unwrap()
            .unwrap();
        receipt.complete().unwrap();
        assert!(
            MigrationJournal::begin_at(root.path().into(), "releaseone".into())
                .unwrap()
                .is_none()
        );
        assert!(
            MigrationJournal::begin_at(root.path().into(), "releasetwo".into())
                .unwrap()
                .is_some()
        );
    }
    #[test]
    fn interruption_fences_same_and_new_release_without_rewriting_receipt() {
        let root = tempfile::tempdir().unwrap();
        let receipt = MigrationJournal::begin_at(root.path().into(), "releaseone".into())
            .unwrap()
            .unwrap();
        assert!(MigrationJournal::begin_at(root.path().into(), "releasetwo".into()).is_err());
        drop(receipt);
        let before = std::fs::read(root.path().join("releaseone.json")).unwrap();
        for identity in ["releaseone", "releasetwo"] {
            assert!(MigrationJournal::begin_at(root.path().into(), identity.into()).is_err());
        }
        assert_eq!(
            std::fs::read(root.path().join("releaseone.json")).unwrap(),
            before
        );
    }

    #[test]
    fn corrupted_receipt_is_not_treated_as_no_migration() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("old.json"), "broken").unwrap();
        assert!(MigrationJournal::begin_at(root.path().into(), "newrelease".into()).is_err());
    }
}
