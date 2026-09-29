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
    roots: Vec<PathBuf>,
    identity: String,
    _leases: Vec<File>,
}

fn state_roots(workspace: &Path) -> Result<Vec<PathBuf>> {
    receipt_roots(
        workspace,
        std::env::var_os("APP_CLI_MIGRATION_RECEIPTS_DIR")
            .as_deref()
            .map(Path::new),
        std::env::var_os("APP_CLI_STATE_ROOT")
            .as_deref()
            .map(Path::new),
    )
}

fn receipt_roots(
    workspace: &Path,
    bound: Option<&Path>,
    explicit: Option<&Path>,
) -> Result<Vec<PathBuf>> {
    let mut roots = Vec::new();
    if let Some(dir) = bound.filter(|path| !path.as_os_str().is_empty()) {
        roots.push(dir.to_path_buf());
    }
    if let Some(root) = explicit.filter(|path| !path.as_os_str().is_empty()) {
        roots.push(root.join("migration-receipts"));
    }
    roots.push(
        workspace
            .parent()
            .context("workspace has no state root")?
            .join("migration-receipts"),
    );
    // Artifact runs use .run; their source-mode history used the stable
    // project's parent. Observe both without copying unrelated identities.
    let origin = runtime_state_layout::resolve_project_origin(workspace)?;
    roots.push(
        origin
            .parent()
            .context("project origin has no parent")?
            .join("migration-receipts"),
    );
    roots.sort();
    roots.dedup();
    Ok(roots)
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
    for root in state_roots(workspace)? {
        if !inspect_confirmed(&root)? {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn require_confirmed_migrations(workspace: &Path) -> Result<()> {
    for root in state_roots(workspace)? {
        require_confirmed(&root)?;
    }
    Ok(())
}

impl MigrationJournal {
    /// `identity` is a digest of the immutable release and service, never credentials.
    pub fn begin(workspace: &Path, identity: String) -> Result<Option<Self>> {
        Self::begin_at_roots(state_roots(workspace)?, identity)
    }

    #[cfg(test)]
    fn begin_at(root: PathBuf, identity: String) -> Result<Option<Self>> {
        Self::begin_at_roots(vec![root], identity)
    }

    fn begin_at_roots(roots: Vec<PathBuf>, identity: String) -> Result<Option<Self>> {
        let mut canonical = Vec::new();
        for root in roots {
            std::fs::create_dir_all(&root)
                .with_context(|| format!("create migration receipts {}", root.display()))?;
            canonical.push(std::fs::canonicalize(root)?);
        }
        canonical.sort();
        canonical.dedup();
        let mut leases = Vec::new();
        for root in &canonical {
            let lease = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(root.join("execution.lock"))?;
            lease.try_lock().with_context(|| {
                format!(
                    "another database migration is executing at {}",
                    root.display()
                )
            })?;
            leases.push(lease);
        }
        for root in &canonical {
            require_confirmed(root)?;
        }
        let journal = Self {
            roots: canonical,
            identity,
            _leases: leases,
        };
        let mut completed = false;
        for root in &journal.roots {
            match std::fs::read(journal.path(root)) {
                Ok(bytes) => {
                    let receipt: Receipt = serde_json::from_slice(&bytes)?;
                    ensure!(
                        receipt.identity == journal.identity && receipt.completed,
                        "Migration receipt identity or completion mismatch"
                    );
                    completed = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("read migration receipt"),
            }
        }
        // Write only this immutable release/service identity, under every
        // location's lock. A partial write leaves pending evidence and cannot
        // authorize a rerun. Other applications' receipts are never copied.
        journal.write(completed)?;
        Ok(if completed { None } else { Some(journal) })
    }

    fn path(&self, root: &Path) -> PathBuf {
        root.join(format!("{}.json", self.identity))
    }

    fn write(&self, completed: bool) -> Result<()> {
        let receipt = Receipt {
            identity: self.identity.clone(),
            completed,
        };
        for root in &self.roots {
            let mut temporary = tempfile::NamedTempFile::new_in(root)?;
            temporary.write_all(&serde_json::to_vec(&receipt)?)?;
            temporary.as_file().sync_all()?;
            temporary
                .persist(self.path(root))
                .map_err(|error| error.error)?;
            #[cfg(unix)]
            File::open(root)?.sync_all()?;
            let readback: Receipt = serde_json::from_slice(&std::fs::read(self.path(root))?)?;
            ensure!(
                readback.identity == self.identity && readback.completed == completed,
                "Migration receipt readback mismatch"
            );
        }
        Ok(())
    }

    pub fn complete(self) -> Result<()> {
        self.write(true)
    }
}

pub(crate) fn identity(release: &crate::manifest::ReleaseLock, service_id: &str) -> Result<String> {
    let bytes = shared_types::encode_userapp_intent(&(release, service_id))?;
    Ok(hex::encode(Sha256::digest(bytes)))
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

    #[test]
    fn split_completed_history_is_reused_without_copying_unrelated_receipts() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        for (root, id) in [(&old, "releasea"), (&new, "releaseb"), (&old, "otherapp")] {
            MigrationJournal::begin_at(root.clone(), id.into())
                .unwrap()
                .unwrap()
                .complete()
                .unwrap();
        }
        for id in ["releasea", "releaseb"] {
            assert!(
                MigrationJournal::begin_at_roots(vec![new.clone(), old.clone()], id.into())
                    .unwrap()
                    .is_none()
            );
            for root in [&old, &new] {
                let receipt: Receipt = serde_json::from_slice(
                    &std::fs::read(root.join(format!("{id}.json"))).unwrap(),
                )
                .unwrap();
                assert_eq!(receipt.identity, id);
                assert!(receipt.completed);
            }
        }
        assert!(!new.join("otherapp.json").exists());
    }

    #[test]
    fn every_history_location_is_locked_and_pending_before_execution() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        std::fs::create_dir(&old).unwrap();
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(old.join("execution.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        assert!(
            MigrationJournal::begin_at_roots(vec![new.clone(), old.clone()], "release".into())
                .is_err()
        );
        assert!(!new.join("release.json").exists());
        drop(lock);
        let journal = MigrationJournal::begin_at_roots(
            vec![new.clone(), old.clone(), old.clone()],
            "release".into(),
        )
        .unwrap()
        .unwrap();
        for root in [&old, &new] {
            assert!(!inspect_confirmed(root).unwrap());
            assert!(MigrationJournal::begin_at(root.clone(), "another".into()).is_err());
        }
        journal.complete().unwrap();
        assert!(inspect_confirmed(&old).unwrap());
        assert!(inspect_confirmed(&new).unwrap());
        // A partial completion or a conflicting old receipt must never be
        // overwritten with success merely because another location is complete.
        let pending = serde_json::to_vec(&Receipt {
            identity: "release".into(),
            completed: false,
        })
        .unwrap();
        std::fs::write(old.join("release.json"), &pending).unwrap();
        assert!(
            MigrationJournal::begin_at_roots(vec![new, old.clone()], "release".into()).is_err()
        );
        assert_eq!(std::fs::read(old.join("release.json")).unwrap(), pending);
    }

    #[test]
    fn artifact_receipts_include_source_history_and_bound_location() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let artifact = project.join(".run");
        std::fs::create_dir_all(&artifact).unwrap();
        let bound = dir.path().join("bound");
        let explicit = dir.path().join("explicit");
        let roots = receipt_roots(&artifact, Some(&bound), Some(&explicit)).unwrap();
        let canonical_base = std::fs::canonicalize(dir.path()).unwrap();
        assert!(roots.contains(&bound));
        assert!(roots.contains(&explicit.join("migration-receipts")));
        assert!(roots.contains(&project.join("migration-receipts")));
        assert!(roots.contains(&canonical_base.join("migration-receipts")));
    }
}
