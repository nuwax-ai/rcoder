//! Application migration receipts are diagnostic history, never admission gates.
//! Keep execution serialized and skip only a consistently confirmed identity.
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
    // Source and .run must observe the same legacy receipt locations in both
    // directions, even when the previous artifact directory no longer exists.
    let origin = runtime_state_layout::resolve_project_origin(workspace)?;
    roots.push(origin.join("migration-receipts"));
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
    let mut confirmed = true;
    for entry in entries {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let receipt: Receipt = serde_json::from_slice(&std::fs::read(&path)?)
                .context("decode migration receipt")?;
            if !receipt.completed {
                confirmed = false;
            }
        }
    }
    Ok(confirmed)
}

/// Observation only: unconfirmed or unreadable history does not deny execution.
pub(crate) fn inspect_migrations(workspace: &Path) -> Result<bool> {
    let mut confirmed = true;
    for root in state_roots(workspace)? {
        if !inspect_confirmed(&root)? {
            confirmed = false;
        }
    }
    Ok(confirmed)
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
        let journal = Self {
            roots: canonical,
            identity,
            _leases: leases,
        };
        let mut known_completion = false;
        let mut unconfirmed = false;
        for root in &journal.roots {
            let path = journal.path(root);
            match std::fs::read(&path) {
                Ok(bytes) => match serde_json::from_slice::<Receipt>(&bytes) {
                    Ok(receipt) if receipt.identity == journal.identity && receipt.completed => {
                        known_completion = true;
                    }
                    Ok(_) => {
                        unconfirmed = true;
                        tracing::warn!(path = %path.display(), "application migration receipt is unconfirmed; execution may retry");
                    }
                    Err(error) => {
                        unconfirmed = true;
                        tracing::error!(path = %path.display(), %error, "application migration receipt is unreadable; execution may retry");
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    unconfirmed = true;
                    tracing::error!(path = %path.display(), %error, "read application migration receipt failed; execution may retry");
                }
            }
        }
        let completed = known_completion && !unconfirmed;
        // Write only this immutable release/service identity under every lease.
        // Mixed completion evidence requires real execution, not promotion of
        // pending history. Unrelated receipt bytes remain untouched. Persistence
        // failures stay visible, while the execution leases remain held.
        if let Err(error) = journal.write(completed) {
            tracing::error!(%error, "persist application migration diagnostic receipt failed");
        }
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
    fn advisory_pending_and_corrupt_receipts_allow_a_new_execution() {
        for (identity, previous) in [
            (
                "same",
                br#"{"identity":"same","completed":false}"#.as_slice(),
            ),
            (
                "new",
                br#"{"identity":"same","completed":false}"#.as_slice(),
            ),
            ("same", b"invalid-receipt".as_slice()),
            ("new", b"invalid-receipt".as_slice()),
        ] {
            let root = tempfile::tempdir().unwrap();
            std::fs::write(root.path().join("same.json"), previous).unwrap();
            let journal = MigrationJournal::begin_at(root.path().into(), identity.into())
                .expect("application migration history is advisory")
                .expect("unconfirmed history must permit execution");
            let current: Receipt = serde_json::from_slice(
                &std::fs::read(root.path().join(format!("{identity}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(current.identity, identity);
            assert!(
                !current.completed,
                "begin must never invent migration success"
            );
            if identity == "new" {
                assert_eq!(
                    std::fs::read(root.path().join("same.json")).unwrap(),
                    previous
                );
            }
            assert!(MigrationJournal::begin_at(root.path().into(), "contender".into()).is_err());
            drop(journal);
        }
    }

    #[test]
    fn advisory_partial_completion_reexecutes_instead_of_promoting_pending_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let old = directory.path().join("old");
        let new = directory.path().join("new");
        for (root, completed) in [(&old, false), (&new, true)] {
            std::fs::create_dir(root).unwrap();
            std::fs::write(
                root.join("release.json"),
                serde_json::to_vec(&Receipt {
                    identity: "release".into(),
                    completed,
                })
                .unwrap(),
            )
            .unwrap();
        }
        let journal =
            MigrationJournal::begin_at_roots(vec![new.clone(), old.clone()], "release".into())
                .expect("partial history is advisory")
                .expect("pending history must be retried");
        for root in [&old, &new] {
            assert!(
                !serde_json::from_slice::<Receipt>(
                    &std::fs::read(root.join("release.json")).unwrap()
                )
                .unwrap()
                .completed
            );
            assert!(MigrationJournal::begin_at(root.clone(), "contender".into()).is_err());
        }
        drop(journal);
    }

    #[test]
    fn advisory_receipt_io_failure_keeps_execution_lease_without_inventing_completion() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("release.json")).unwrap();
        let journal = MigrationJournal::begin_at(root.path().into(), "release".into())
            .expect("diagnostic receipt read/write cannot prevent execution")
            .expect("no confirmed completion exists");
        assert!(MigrationJournal::begin_at(root.path().into(), "contender".into()).is_err());
        assert!(
            journal.complete().is_err(),
            "failed receipt persistence remains diagnostic failure"
        );
        assert!(root.path().join("release.json").is_dir());
    }

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
    fn interruption_allows_same_and_new_release_after_execution_lease_is_released() {
        let root = tempfile::tempdir().unwrap();
        let receipt = MigrationJournal::begin_at(root.path().into(), "releaseone".into())
            .unwrap()
            .unwrap();
        assert!(MigrationJournal::begin_at(root.path().into(), "releasetwo".into()).is_err());
        drop(receipt);
        let before = std::fs::read(root.path().join("releaseone.json")).unwrap();
        for identity in ["releaseone", "releasetwo"] {
            let journal = MigrationJournal::begin_at(root.path().into(), identity.into())
                .unwrap()
                .expect("interrupted application migration can retry");
            let receipt: Receipt = serde_json::from_slice(
                &std::fs::read(root.path().join(format!("{identity}.json"))).unwrap(),
            )
            .unwrap();
            assert_eq!(receipt.identity, identity);
            assert!(!receipt.completed);
            assert!(MigrationJournal::begin_at(root.path().into(), "contender".into()).is_err());
            drop(journal);
        }
        assert_eq!(
            std::fs::read(root.path().join("releaseone.json")).unwrap(),
            before
        );
    }

    #[test]
    fn corrupted_unrelated_receipt_stays_diagnostic_during_a_new_execution() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("old.json"), "broken").unwrap();
        let journal = MigrationJournal::begin_at(root.path().into(), "newrelease".into())
            .unwrap()
            .expect("unreadable application migration history is advisory");
        assert_eq!(
            std::fs::read(root.path().join("old.json")).unwrap(),
            b"broken"
        );
        assert!(inspect_confirmed(root.path()).is_err());
        assert!(MigrationJournal::begin_at(root.path().into(), "contender".into()).is_err());
        drop(journal);
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
        // Partial completion may retry, but begin cannot invent success from
        // the other location's completed receipt.
        let pending = serde_json::to_vec(&Receipt {
            identity: "release".into(),
            completed: false,
        })
        .unwrap();
        std::fs::write(old.join("release.json"), &pending).unwrap();
        let journal =
            MigrationJournal::begin_at_roots(vec![new.clone(), old.clone()], "release".into())
                .unwrap()
                .expect("pending diagnostic history permits a new execution");
        assert_eq!(std::fs::read(old.join("release.json")).unwrap(), pending);
        assert!(!inspect_confirmed(&new).unwrap());
        for root in [&old, &new] {
            assert!(MigrationJournal::begin_at(root.clone(), "contender".into()).is_err());
        }
        journal.complete().unwrap();
        assert!(inspect_confirmed(&old).unwrap());
        assert!(inspect_confirmed(&new).unwrap());
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
