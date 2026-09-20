//! Durable deployment receipts outside the directory exchanged during activation.
use crate::server::DeployRequest;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use shared_types::AppDeploymentOperation;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Boundary {
    Preparing,
    Switching,
    Activated,
    Active,
    /// Artifact and shutdown confirmed; migrations confirmed, only startup failed.
    StartupFailed,
    Failed,
}

/// A confirmed serving artifact can predate URL deployment. Such an existing
/// workspace has content identity but deliberately has no invented download URL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ActiveVersion {
    pub artifact_release_id: String,
    pub request: Option<DeployRequest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Receipt {
    pub generation: String,
    pub operation: AppDeploymentOperation,
    pub request: DeployRequest,
    pub boundary: Boundary,
    pub active: Option<ActiveVersion>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_handoff: Option<shared_types::RuntimeGenerationPrepared>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum OwnerState {
    #[default]
    Active,
    Quiescent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CoordinatorOwner {
    #[serde(default)]
    state: OwnerState,
    process_scope: Option<String>,
}

pub(crate) struct Journal {
    root: PathBuf,
    lease: Option<File>,
    pub receipt: Option<Receipt>,
    pub process_scope: Option<String>,
    previous_owner: Option<CoordinatorOwner>,
    // Retain the old lock domain for the entire owner lifetime: an old binary
    // must not start against now-empty legacy record paths during migration.
    legacy_leases: Vec<File>,
    legacy_root: Option<PathBuf>,
}
impl Drop for Journal {
    fn drop(&mut self) {
        for lease in &self.legacy_leases {
            if let Err(error) = lease.unlock() {
                tracing::error!(%error, "Failed to release legacy journal lease");
            }
        }
        if let Some(lease) = &self.lease
            && let Err(error) = lease.unlock()
        {
            tracing::error!(%error, "Failed to release deployment journal lease");
        }
    }
}
impl Journal {
    #[cfg(test)]
    pub fn open(workspace: &Path) -> Result<Self> {
        // B04：平台显式状态根权威（与 runtime_kernel 同 env）——source/.run
        // 别名经同一目录竞争同一把锁。缺省沿用卷根推导（历史布局兼容）。
        let root = match std::env::var_os("APP_CLI_STATE_ROOT").filter(|value| !value.is_empty()) {
            Some(explicit) => PathBuf::from(explicit),
            None => workspace
                .parent()
                .context("workspace has no volume root")?
                .to_path_buf(),
        };
        Self::open_root(root)
    }

    /// Caller already holds the common OwnerGuard. Merely opening never moves
    /// records: migration waits until the management listener has bound.
    pub fn open_with_root(workspace: &Path, root: PathBuf) -> Result<Self> {
        let mut journal = Self::open_root(root)?;
        let project = runtime_state_layout::canonical_project_root(workspace);
        let mut candidates = vec![project.clone()];
        if let Some(parent) = project.parent() {
            candidates.push(parent.to_path_buf());
        }
        let root_identity = std::fs::canonicalize(&journal.root)?;
        for legacy in candidates {
            if std::fs::canonicalize(&legacy).is_ok_and(|path| path == root_identity) {
                continue;
            }
            let has_records = legacy.join(".deploy-operation.json").try_exists()?
                || legacy.join(".deploy-coordinator.json").try_exists()?;
            if !has_records {
                continue;
            }
            anyhow::ensure!(
                journal.receipt.is_none()
                    && journal.previous_owner.is_none()
                    && journal.legacy_root.is_none(),
                "deployment journal exists in multiple authority domains; explicit recovery required"
            );
            let mut old = Self::open_root(legacy.clone())?;
            journal.receipt = old.receipt.take();
            journal.previous_owner = old.previous_owner.take();
            let lease = old
                .lease
                .take()
                .context("legacy deployment lease missing")?;
            journal.legacy_leases.push(lease);
            journal.legacy_root = Some(legacy);
        }
        Ok(journal)
    }

    pub fn migrate_after_bind(&mut self) -> Result<()> {
        let Some(legacy) = self.legacy_root.as_ref() else {
            return Ok(());
        };
        for name in [".deploy-operation.json", ".deploy-coordinator.json"] {
            let source = legacy.join(name);
            if source.try_exists()? {
                anyhow::ensure!(
                    !self.root.join(name).try_exists()?,
                    "migration destination already exists"
                );
                std::fs::rename(&source, self.root.join(name))
                    .with_context(|| format!("migrate deployment journal {name}"))?;
            }
        }
        #[cfg(unix)]
        {
            File::open(&self.root)?.sync_all()?;
            File::open(legacy)?.sync_all()?;
        }
        self.legacy_root = None;
        Ok(())
    }

    fn open_root(root: PathBuf) -> Result<Self> {
        std::fs::create_dir_all(&root)?;
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".deploy-operation.lock"))?;
        lease
            .try_lock()
            .context("another deployment coordinator owns this volume")?;
        let receipt = match std::fs::read(root.join(".deploy-operation.json")) {
            Ok(bytes) => {
                Some(serde_json::from_slice(&bytes).context("invalid deployment journal")?)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("read deployment journal"),
        };
        let previous_owner = match std::fs::read(root.join(".deploy-coordinator.json")) {
            Ok(bytes) => Some(
                serde_json::from_slice(&bytes).context("invalid deployment coordinator owner")?,
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error).context("read deployment coordinator owner"),
        };
        Ok(Self {
            root,
            lease: Some(lease),
            receipt,
            process_scope: process_scope(),
            previous_owner,
            legacy_leases: Vec::new(),
            legacy_root: None,
        })
    }
    pub(crate) fn source_seal(&self) -> Result<Option<shared_types::RuntimeGenerationSourceSeal>> {
        match std::fs::read(self.root.join(".generation-source-seal.json")) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("invalid generation source seal")?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).context("read generation source seal"),
        }
    }

    pub(crate) fn write_source_seal(
        &self,
        seal: &shared_types::RuntimeGenerationSourceSeal,
    ) -> Result<()> {
        self.write_verified(".generation-source-seal.json", seal)?;
        Ok(())
    }

    fn write_verified<T: Serialize + serde::de::DeserializeOwned>(
        &self,
        name: &str,
        value: &T,
    ) -> Result<T> {
        let mut temp = tempfile::NamedTempFile::new_in(&self.root)?;
        temp.write_all(&serde_json::to_vec(value)?)?;
        temp.as_file().sync_all()?;
        temp.persist(self.root.join(name))
            .map_err(|e| e.error)
            .with_context(|| format!("commit deployment record {name}"))?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        let readback: T = serde_json::from_slice(&std::fs::read(self.root.join(name))?)?;
        anyhow::ensure!(
            serde_json::to_value(&readback)? == serde_json::to_value(value)?,
            "deployment record readback mismatch: {name}"
        );
        Ok(readback)
    }
    pub fn write(&mut self, receipt: Receipt) -> Result<()> {
        self.receipt = Some(self.write_verified(".deploy-operation.json", &receipt)?);
        Ok(())
    }
    pub fn require_fresh_process_scope(&self) -> Result<()> {
        if self
            .previous_owner
            .as_ref()
            .is_some_and(|owner| owner.state == OwnerState::Quiescent)
        {
            return Ok(());
        }
        if self.previous_owner.is_none() && self.receipt.is_none() {
            return Ok(());
        }
        let previous = self
            .previous_owner
            .as_ref()
            .and_then(|owner| owner.process_scope.as_ref());
        anyhow::ensure!(
            self.process_scope.is_some()
                && previous.is_some()
                && self.process_scope.as_ref() != previous,
            "cannot confirm previous builtin process groups stopped; restart the container before deployment"
        );
        Ok(())
    }
    /// Call only after the previous coordinator's processes are confirmed stopped.
    /// Opening the journal never overwrites the evidence needed for that decision.
    pub fn commit_coordinator(&mut self) -> Result<()> {
        let owner = CoordinatorOwner {
            state: OwnerState::Active,
            process_scope: self.process_scope.clone(),
        };
        self.previous_owner = Some(self.write_verified(".deploy-coordinator.json", &owner)?);
        Ok(())
    }
    pub fn commit_quiescent(&mut self) -> Result<()> {
        let previous = self
            .previous_owner
            .as_ref()
            .context("coordinator ownership was never claimed")?;
        anyhow::ensure!(
            previous.state == OwnerState::Active && previous.process_scope == self.process_scope,
            "coordinator ownership changed before shutdown"
        );
        let owner = CoordinatorOwner {
            state: OwnerState::Quiescent,
            process_scope: self.process_scope.clone(),
        };
        self.previous_owner = Some(self.write_verified(".deploy-coordinator.json", &owner)?);
        Ok(())
    }
    pub fn clear(&mut self) -> Result<()> {
        match std::fs::remove_file(self.root.join(".deploy-operation.json")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        self.receipt = None;
        Ok(())
    }
    pub fn resume(&self, generation: &str) -> Result<Option<Receipt>> {
        let Some(receipt) = self.receipt.as_ref().filter(|r| r.generation == generation) else {
            return Ok(None);
        };
        match receipt.boundary {
            Boundary::Switching | Boundary::Activated | Boundary::Failed => {
                bail!("deployment interrupted after switch; explicit redeployment required")
            }
            Boundary::Preparing if receipt.active.is_none() => bail!(
                "deployment preparation has no confirmed active version; explicit redeployment required"
            ),
            _ => Ok(Some(receipt.clone())),
        }
    }
}

/// A new Linux PID namespace proves that processes from the previous container
/// cannot still own this workspace. A process restart in the same namespace does not.
fn process_scope() -> Option<String> {
    let namespace = std::fs::read_link("/proc/1/ns/pid").ok()?;
    let stat = std::fs::read_to_string("/proc/1/stat").ok()?;
    let (_, fields) = stat.rsplit_once(") ")?;
    let start = fields.split_whitespace().nth(19)?;
    let boot = std::fs::read_to_string("/proc/sys/kernel/random/boot_id").ok()?;
    Some(format!("{}:{}:{start}", boot.trim(), namespace.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{AppCliDeployPhase, app_cli_deploy::AppDeploymentStage};
    #[test]
    fn source_and_run_alias_use_one_resolved_journal_without_environment() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("project");
        let run = source.join(".run");
        std::fs::create_dir_all(&run).unwrap();
        let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
        let alias_root = runtime_state_layout::ensure_state_root(&run, None, None).unwrap();
        assert_eq!(root, alias_root);
        let mut first = Journal::open_with_root(&source, root.clone()).unwrap();
        first.commit_coordinator().unwrap();
        first.write(receipt(Boundary::Active)).unwrap();
        first.commit_quiescent().unwrap();
        assert!(Journal::open_with_root(&run, alias_root.clone()).is_err());
        drop(first);
        let second = Journal::open_with_root(&run, alias_root).unwrap();
        second.require_fresh_process_scope().unwrap();
        assert_eq!(
            second.receipt.as_ref().unwrap().operation.operation_id,
            "hot-b"
        );
        assert!(!source.join(".deploy-operation.json").exists());
        assert!(!dir.path().join(".deploy-operation.json").exists());
        assert!(root.join(".deploy-operation.json").exists());
    }

    #[test]
    fn legacy_journal_migrates_only_after_bind_and_retains_old_lock() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("project");
        std::fs::create_dir_all(&source).unwrap();
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        legacy.commit_quiescent().unwrap();
        drop(legacy);
        let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
        let mut migrated = Journal::open_with_root(&source, root.clone()).unwrap();
        assert!(dir.path().join(".deploy-operation.json").exists());
        assert!(!root.join(".deploy-operation.json").exists());
        migrated.migrate_after_bind().unwrap();
        assert!(!dir.path().join(".deploy-operation.json").exists());
        assert!(root.join(".deploy-operation.json").exists());
        assert!(Journal::open_root(dir.path().to_path_buf()).is_err());
        assert_eq!(
            migrated.receipt.as_ref().unwrap().operation.operation_id,
            "hot-b"
        );
    }

    #[test]
    fn dual_journal_authority_is_rejected_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("project");
        std::fs::create_dir_all(&source).unwrap();
        let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
        for path in [dir.path(), root.as_path()] {
            let mut journal = Journal::open_root(path.to_path_buf()).unwrap();
            journal.write(receipt(Boundary::Active)).unwrap();
        }
        let before = std::fs::read(root.join(".deploy-operation.json")).unwrap();
        assert!(Journal::open_with_root(&source, root.clone()).is_err());
        assert_eq!(
            before,
            std::fs::read(root.join(".deploy-operation.json")).unwrap()
        );
        assert!(dir.path().join(".deploy-operation.json").exists());
    }

    fn receipt(boundary: Boundary) -> Receipt {
        let request = DeployRequest {
            runtime_operation_id: None,
            url: "http://artifact/b".into(),
            release_id: "b".into(),
            sha256: None,
            local_path: None,
            execution_target: None,
            requires_configuration_activation: false,
            run_pg: None,
        };
        Receipt {
            generation: "generation-a".into(),
            generation_handoff: None,
            operation: AppDeploymentOperation {
                operation_id: "hot-b".into(),
                deployment_generation_id: "generation-a".into(),
                request_release_id: "b".into(),
                artifact_release_id: Some("manifest-b".into()),
                persisted: true,
                deploy_stage: AppDeploymentStage::Succeeded,
                phase: AppCliDeployPhase::Running,
                error: None,
                recovery: None,
            },
            request: request.clone(),
            boundary,
            active: Some(ActiveVersion {
                request: Some(request),
                artifact_release_id: "manifest-b".into(),
            }),
        }
    }
    #[test]
    fn confirmed_startup_failure_keeps_artifact_but_legacy_failure_stays_protected() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&dir.path().join("code")).unwrap();
        journal.write(receipt(Boundary::StartupFailed)).unwrap();
        let saved = journal.resume("generation-a").unwrap().unwrap();
        assert_eq!(saved.boundary, Boundary::StartupFailed);
        assert_eq!(saved.active.unwrap().artifact_release_id, "manifest-b");
        journal.write(receipt(Boundary::Failed)).unwrap();
        assert!(journal.resume("generation-a").is_err());
    }

    #[test]
    fn durable_hot_receipt_overrides_same_generation_stale_seed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let mut journal = Journal::open(&workspace).unwrap();
        journal.write(receipt(Boundary::Active)).unwrap();
        assert!(Journal::open(&workspace).is_err());
        drop(journal);
        let journal = Journal::open(&workspace).unwrap();
        let resumed = journal.resume("generation-a").unwrap().unwrap();
        assert_eq!(resumed.active.unwrap().request.unwrap().release_id, "b");
        assert_eq!(resumed.operation.operation_id, "hot-b");
        assert!(journal.resume("generation-new").unwrap().is_none());
    }
    #[test]
    fn interrupted_switch_fails_closed_but_prepare_preserves_active() {
        let dir = tempfile::tempdir().unwrap();
        let mut journal = Journal::open(&dir.path().join("code")).unwrap();
        journal.write(receipt(Boundary::Preparing)).unwrap();
        assert!(journal.resume("generation-a").unwrap().is_some());
        journal.write(receipt(Boundary::Switching)).unwrap();
        assert!(journal.resume("generation-a").is_err());
        assert!(journal.resume("generation-new").unwrap().is_none());
    }
    #[test]
    fn coordinator_scope_survives_two_opens_without_a_deployment_operation() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let mut first = Journal::open(&workspace).unwrap();
        first.process_scope = Some("container-a".into());
        first.require_fresh_process_scope().unwrap();
        first.commit_coordinator().unwrap();
        assert!(first.receipt.is_none());
        drop(first);
        let before = std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap();
        let mut second = Journal::open(&workspace).unwrap();
        second.process_scope = Some("container-a".into());
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap(),
            before
        );
        assert!(second.require_fresh_process_scope().is_err());
        second.process_scope = Some("container-b".into());
        second.require_fresh_process_scope().unwrap();
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap(),
            before
        );
        second.commit_coordinator().unwrap();
        drop(second);
        let mut third = Journal::open(&workspace).unwrap();
        third.process_scope = Some("container-b".into());
        assert!(third.require_fresh_process_scope().is_err());
    }

    #[test]
    fn clean_owner_allows_unknown_and_same_scope_restarts_but_claim_is_active() {
        for scope in [None, Some("native-linux".to_owned())] {
            let dir = tempfile::tempdir().unwrap();
            let workspace = dir.path().join("code");
            let mut first = Journal::open(&workspace).unwrap();
            first.process_scope = scope.clone();
            first.commit_coordinator().unwrap();
            first.commit_quiescent().unwrap();
            drop(first);
            let mut second = Journal::open(&workspace).unwrap();
            second.process_scope = scope;
            second.require_fresh_process_scope().unwrap();
            second.commit_coordinator().unwrap();
            assert!(second.require_fresh_process_scope().is_err());
        }
    }
    #[test]
    fn legacy_owner_without_state_defaults_active() {
        let owner: CoordinatorOwner = serde_json::from_str(r#"{"process_scope":null}"#).unwrap();
        assert_eq!(owner.state, OwnerState::Active);
    }

    #[test]
    fn unreadable_or_unknown_coordinator_scope_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let owner = dir.path().join(".deploy-coordinator.json");
        std::fs::create_dir(&owner).unwrap();
        assert!(Journal::open(&workspace).is_err());
        std::fs::remove_dir(&owner).unwrap();
        std::fs::write(&owner, br#"{"process_scope":null}"#).unwrap();
        let mut journal = Journal::open(&workspace).unwrap();
        journal.process_scope = Some("container-b".into());
        assert!(journal.require_fresh_process_scope().is_err());
        drop(journal);
        std::fs::write(&owner, b"invalid-json").unwrap();
        assert!(Journal::open(&workspace).is_err());
    }

    #[test]
    fn corrupted_receipt_is_not_treated_as_missing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".deploy-operation.json"), b"invalid").unwrap();
        assert!(Journal::open(&dir.path().join("code")).is_err());
    }
}
