//! Explicit owner resources: a retained owner lock and an independent admission
//! range. Neither a business task-local nor an arbitrary path authorizes spawn.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Created only by successfully acquiring this exact owner's OS lock. Keeping
/// construction here prevents a caller from substituting an arbitrary File.
#[derive(Clone)]
pub struct OwnerLease {
    root: PathBuf,
    lock: Arc<File>,
}

impl OwnerLease {
    pub fn try_acquire(root: &Path) -> Result<Option<Self>> {
        crate::command_context::create_durable_directory(root)?;
        let root = std::fs::canonicalize(root)?;
        let file = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(root.join("owner.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self {
                root,
                lock: Arc::new(file),
            })),
            Err(std::fs::TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn retained_file(&self) -> Arc<File> {
        self.lock.clone()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResidentIdentity {
    pub application_id: String,
    pub binding: serde_json::Value,
    pub owner_instance: String,
    pub physical_domain: Option<serde_json::Value>,
    pub process_epoch: Option<String>,
}

#[derive(Clone)]
pub struct ResidentScope {
    root: PathBuf,
    identity: ResidentIdentity,
    // Cleanup uncertainty retains the real owner lease, preventing replacement.
    owner_lock: Arc<std::sync::Mutex<Option<Arc<File>>>>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    identity: ResidentIdentity,
    phase: String,
    error: Option<String>,
}

impl ResidentScope {
    /// The runtime owner supplies its held lock, not a PID or receipt-derived
    /// signal authority. Published discovery must bind this exact owner/range.
    pub fn initialize(owner: &OwnerLease, identity: ResidentIdentity) -> Result<Self> {
        ensure!(
            uuid::Uuid::parse_str(&identity.owner_instance).is_ok(),
            "invalid resident owner identity"
        );
        ensure!(
            !identity.application_id.trim().is_empty(),
            "resident application identity missing"
        );
        let owner_root = owner.root.clone();
        require_owner(&owner_root, &identity)?;
        // A free lock is not authority. The supplied lease must continue to
        // retain the lock for the scope's entire lifetime.
        let root = owner_root.join("resident").join(&identity.owner_instance);
        crate::command_context::create_durable_directory(&root)?;
        ensure!(
            std::fs::canonicalize(&root)? == root,
            "resident scope resolves outside owner range"
        );
        ensure!(
            !root.join("resident-scope.json").try_exists()?,
            "resident scope already initialized"
        );
        let scope = Self {
            root,
            identity,
            owner_lock: Arc::new(std::sync::Mutex::new(Some(owner.lock.clone()))),
        };
        scope.write("Running", None)?;
        super::Gate::try_acquire(&scope.root)?.initialize()?;
        Ok(scope)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn identity(&self) -> &ResidentIdentity {
        &self.identity
    }

    pub(crate) fn require_open(&self) -> Result<()> {
        self.require_identity()?;
        super::Gate::try_acquire(&self.root)?.require_open()
    }

    fn require_identity(&self) -> Result<()> {
        ensure!(
            self.owner_lock
                .lock()
                .map_err(|_| anyhow::anyhow!("resident owner lease lock poisoned"))?
                .is_some(),
            "resident owner capability is retired"
        );
        let identity = read_identity(&self.root)?;
        ensure!(identity == self.identity, "resident scope identity changed");
        validate_resident_spawn(&self.root)?;
        Ok(())
    }

    pub(crate) async fn require_open_until(
        &self,
        deadline: tokio::time::Instant,
    ) -> Result<super::Gate> {
        self.require_identity()?;
        let gate = super::Gate::observe_open_until(&self.root, deadline).await?;
        self.require_identity()?;
        Ok(gate)
    }

    /// Cleanup may retry after admission closed. This proves the captured owner
    /// identity/lock without reopening the range or granting another spawn.
    pub fn verify_owner_for_cleanup(&self) -> Result<()> {
        ensure!(
            read_identity(&self.root)? == self.identity,
            "resident cleanup identity changed"
        );
        validate_resident_spawn(&self.root)
    }

    pub fn close(&self) -> Result<()> {
        let old = read_identity(&self.root)?;
        ensure!(old == self.identity, "resident shutdown identity changed");
        super::Gate::try_acquire(&self.root)?.close()?;
        self.write("Draining", None)
    }

    pub fn record_unknown(&self, error: &str) -> Result<()> {
        self.write("Unknown", Some(error.into()))
    }
    pub fn record_running(&self) -> Result<()> {
        self.require_open()?;
        self.write("Running", None)
    }

    pub async fn record_running_until(&self, deadline: tokio::time::Instant) -> Result<()> {
        let _gate = self.require_open_until(deadline).await?;
        self.write("Running", None)
    }

    pub fn record_publication(&self, publication: serde_json::Value) -> Result<()> {
        self.require_open()?;
        self.write_publication(publication)
    }

    pub async fn record_publication_until(
        &self,
        publication: serde_json::Value,
        deadline: tokio::time::Instant,
    ) -> Result<()> {
        let _gate = self.require_open_until(deadline).await?;
        self.write_publication(publication)
    }

    fn write_publication(&self, publication: serde_json::Value) -> Result<()> {
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1, "identity": self.identity, "publication": publication,
        }))?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.as_file().sync_all()?;
        crate::atomic_file::persist(file, &self.root.join("confirmed-publication.json"))?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn record_quiescent(&self) -> Result<()> {
        crate::guardian::recover(&self.root)?;
        self.write("Quiescent", None)?;
        self.owner_lock
            .lock()
            .map_err(|_| anyhow::anyhow!("resident owner lease lock poisoned"))?
            .take();
        Ok(())
    }

    fn write(&self, phase: &str, error: Option<String>) -> Result<()> {
        let bytes = serde_json::to_vec(&Receipt {
            version: 1,
            identity: self.identity.clone(),
            phase: phase.into(),
            error,
        })?;
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.as_file().sync_all()?;
        crate::atomic_file::persist(file, &self.root.join("resident-scope.json"))?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        Ok(())
    }
}

pub fn read_identity(root: &Path) -> Result<ResidentIdentity> {
    let receipt: Receipt =
        serde_json::from_slice(&std::fs::read(root.join("resident-scope.json"))?)?;
    ensure!(
        receipt.version == 1
            && matches!(
                receipt.phase.as_str(),
                "Running" | "Draining" | "Unknown" | "Quiescent"
            ),
        "invalid resident receipt"
    );
    ensure!(
        root.file_name().and_then(|v| v.to_str()) == Some(receipt.identity.owner_instance.as_str()),
        "resident instance differs from scope"
    );
    Ok(receipt.identity)
}

pub(crate) fn validate_resident_spawn(root: &Path) -> Result<()> {
    if !root.join("resident-scope.json").try_exists()? {
        return Ok(());
    }
    let identity = read_identity(root)?;
    let resident = root.parent().context("resident parent missing")?;
    ensure!(
        resident.file_name().is_some_and(|v| v == "resident"),
        "resident range is not an owner resource"
    );
    let owner = resident.parent().context("resident owner root missing")?;
    require_owner(owner, &identity)
}

fn require_owner(root: &Path, identity: &ResidentIdentity) -> Result<()> {
    let discovery: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("supervisor.json"))?)?;
    ensure!(
        discovery["instance"] == identity.owner_instance
            && discovery["snapshot"]["supervisor_id"] == identity.owner_instance
            && discovery["snapshot"]["binding"] == identity.binding,
        "resident belongs to another application, workspace or owner"
    );
    let probe = File::options()
        .read(true)
        .write(true)
        .open(root.join("owner.lock"))?;
    match probe.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Ok(()) => anyhow::bail!("resident owner has exited"),
        Err(error) => Err(error).context("verify resident owner lock"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn scope(root: &Path) -> (OwnerLease, ResidentScope) {
        let lease = OwnerLease::try_acquire(root).unwrap().unwrap();
        let identity = ResidentIdentity {
            application_id: "admission-fixture".into(),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            binding: serde_json::json!({"component":"app-cli","resource":lease.root()}),
            physical_domain: None,
            process_epoch: None,
        };
        std::fs::write(lease.root().join("supervisor.json"), serde_json::to_vec(&serde_json::json!({
            "instance":identity.owner_instance,"snapshot":{"supervisor_id":identity.owner_instance,"binding":identity.binding}
        })).unwrap()).unwrap();
        let scope = ResidentScope::initialize(&lease, identity).unwrap();
        (lease, scope)
    }

    #[tokio::test]
    async fn resident_receipt_commit_waits_for_exact_short_admission_contention() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, scope) = scope(root.path());
        let consumed = super::super::Gate::try_acquire(scope.root()).unwrap();
        let observed = scope.clone();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        let commit = tokio::spawn(async move {
            observed.record_running_until(deadline).await?;
            observed
                .record_publication_until(serde_json::json!({"publication_id":"fixture"}), deadline)
                .await
        });
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            !commit.is_finished(),
            "short admission contention was treated as permanent failure"
        );
        drop(consumed);
        commit.await.unwrap().unwrap();
        let value: serde_json::Value = serde_json::from_slice(
            &std::fs::read(scope.root().join("confirmed-publication.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            value["identity"]["owner_instance"],
            scope.identity().owner_instance
        );
        assert_eq!(value["publication"]["publication_id"], "fixture");
    }

    #[tokio::test]
    async fn resident_wait_never_reopens_closed_admission() {
        let root = tempfile::tempdir().unwrap();
        let (_lease, scope) = scope(root.path());
        let consumed = super::super::Gate::try_acquire(scope.root()).unwrap();
        let observed = scope.clone();
        let commit = tokio::spawn(async move {
            observed
                .record_running_until(tokio::time::Instant::now() + Duration::from_secs(1))
                .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        consumed.close().unwrap();
        drop(consumed);
        assert!(commit.await.unwrap().is_err());
        assert!(
            super::super::Gate::try_acquire(scope.root())
                .unwrap()
                .require_open()
                .is_err()
        );
    }
}
