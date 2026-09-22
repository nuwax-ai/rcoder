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
    /// The previous artifact is serving again; the failed attempt stays failed.
    RestoredActive,
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
            if journal.receipt.is_none()
                && journal.previous_owner.is_none()
                && journal.legacy_root.is_none()
            {
                let mut old = Self::open_root(legacy.clone())?;
                journal.receipt = old.receipt.take();
                journal.previous_owner = old.previous_owner.take();
                let lease = old
                    .lease
                    .take()
                    .context("legacy deployment lease missing")?;
                journal.legacy_leases.push(lease);
                journal.legacy_root = Some(legacy);
                continue;
            }
            // 双权威域（2026-09-22 app-105 死锁修复）：权威根已有记录且 legacy
            // 也残留记录时，旧守卫一律拒绝——但"权威侧是一笔 failed 待重部署
            // + legacy 是被取代的陈旧记录"恰恰是升级窗口被中断部署的必然产
            // 物，此时 serve 起不来则重部署（journal 自己要求的恢复方式）永
            // 远无法发生：守卫死锁自己的补救路径。改为状态感知收束：
            // - 双侧均无在途交接（coordinator quiescent ∧ boundary 终态）且
            //   权威记录不旧于 legacy → 陈旧侧改名归档（不删除、可回滚），
            //   serve 正常启动，重部署路径恢复；
            // - 任一侧在途、或 legacy 反而更新（旧运行时在新布局之后又跑过，
            //   降级混跑）→ 仍显式拒绝，文案给出两侧路径。
            let mut old = Self::open_root(legacy.clone())?;
            let legacy_settled = domain_settled(
                old.receipt.as_ref().map(|receipt| &receipt.boundary),
                old.previous_owner.as_ref(),
            );
            let authority_settled = domain_settled(
                journal.receipt.as_ref().map(|receipt| &receipt.boundary),
                journal.previous_owner.as_ref(),
            );
            anyhow::ensure!(
                authority_settled && legacy_settled,
                "deployment journal conflict: in-flight records in authority {} or legacy {} \
                 (settle the interrupted deployment, then restart)",
                journal.root.display(),
                legacy.display()
            );
            anyhow::ensure!(
                authority_covers_legacy(&journal.root, &legacy),
                "legacy deployment records at {} are newer than authority {} \
                 (old runtime ran after the state-root migration); explicit recovery required",
                legacy.display(),
                journal.root.display()
            );
            archive_superseded_records(&legacy)?;
            tracing::warn!(
                legacy = %legacy.display(),
                authority = %journal.root.display(),
                "archived superseded legacy deployment journal (both domains settled, \
                 authority records newer); startup continues"
            );
            // 保留 legacy 锁到 owner 生命周期结束（与迁移路径同理）：归档后旧
            // 二进制不得在 legacy 路径上以全新记录启动。
            if let Some(lease) = old.lease.take() {
                journal.legacy_leases.push(lease);
            }
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

    /// Caller verified the target directory against the persisted switch intent
    /// after old writers stopped. No directory mutation is performed here.
    pub(crate) fn confirm_switched_artifact(
        &mut self,
        expected: &Receipt,
        artifact: &str,
    ) -> Result<Receipt> {
        let current = self.receipt.as_ref().context("switch journal missing")?;
        anyhow::ensure!(
            serde_json::to_value(current)? == serde_json::to_value(expected)?,
            "switch journal changed during reconciliation"
        );
        anyhow::ensure!(
            current.boundary == Boundary::Switching
                && current.operation.artifact_release_id.as_deref() == Some(artifact)
                && !artifact.is_empty()
                && current.operation.recovery.is_none(),
            "switch intent does not confirm the observed artifact"
        );
        let mut resolved = current.clone();
        resolved.boundary = Boundary::Activated;
        resolved.operation.deploy_stage =
            shared_types::app_cli_deploy::AppDeploymentStage::Succeeded;
        resolved.operation.persisted = true;
        resolved.operation.phase = shared_types::AppCliDeployPhase::Orchestrating;
        resolved.active = Some(ActiveVersion {
            artifact_release_id: artifact.into(),
            request: Some(resolved.request.clone()),
        });
        self.write(resolved)?;
        self.receipt
            .clone()
            .context("reconciled switch journal missing")
    }

    /// The old artifact remains at the original execution path after quiescence.
    /// Preserve it as active; the attempted deployment remains a failed attempt.
    pub(crate) fn confirm_preserved_active(
        &mut self,
        expected: &Receipt,
        artifact: &str,
    ) -> Result<Receipt> {
        let current = self.receipt.as_ref().context("switch journal missing")?;
        anyhow::ensure!(
            serde_json::to_value(current)? == serde_json::to_value(expected)?,
            "switch journal changed during reconciliation"
        );
        let active = current
            .active
            .as_ref()
            .context("previous active artifact missing")?;
        anyhow::ensure!(
            current.boundary == Boundary::Switching
                && current.operation.recovery.is_none()
                && active.artifact_release_id == artifact
                && active.request.as_ref().is_some_and(
                    |request| request.execution_target == current.request.execution_target
                ),
            "previous artifact execution binding does not match"
        );
        let mut resolved = current.clone();
        resolved.boundary = Boundary::Preparing;
        resolved.operation.deploy_stage = shared_types::app_cli_deploy::AppDeploymentStage::Failed;
        resolved.operation.phase = shared_types::AppCliDeployPhase::Failed;
        resolved.operation.error =
            Some("Activation interrupted; the confirmed previous artifact remains in place".into());
        self.write(resolved)?;
        self.receipt
            .clone()
            .context("preserved active journal missing")
    }

    /// Caller proved process quiescence, current artifact identity and no
    /// unconfirmed migrations. This records startup interruption, not rollback.
    pub(crate) fn confirm_interrupted_activation(&mut self, expected: &Receipt) -> Result<Receipt> {
        let current = self
            .receipt
            .as_ref()
            .context("activation journal missing")?;
        anyhow::ensure!(
            serde_json::to_value(current)? == serde_json::to_value(expected)?,
            "activation journal changed during reconciliation"
        );
        anyhow::ensure!(
            current.boundary == Boundary::Activated
                && current.operation.persisted
                && current.operation.deploy_stage
                    == shared_types::app_cli_deploy::AppDeploymentStage::Succeeded
                && current.operation.recovery.is_none()
                && current.active.is_some(),
            "activation is not eligible for confirmed startup interruption"
        );
        let mut resolved = current.clone();
        resolved.boundary = Boundary::StartupFailed;
        resolved.operation.phase = shared_types::AppCliDeployPhase::Failed;
        resolved.operation.error =
            Some("Owner restarted after activation before startup completed".into());
        self.write(resolved)?;
        self.receipt
            .clone()
            .context("reconciled activation journal missing")
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

/// A record domain is settled when no deployment handoff is mid-flight:
/// the coordinator is quiescent (or absent) and the receipt sits at a
/// terminal boundary (or is absent). Preparing/Switching/Activated mean an
/// interrupted handoff the owner may still reconcile — such records stay
/// untouchable by automatic supersession.
fn domain_settled(boundary: Option<&Boundary>, owner: Option<&CoordinatorOwner>) -> bool {
    owner.is_none_or(|owner| owner.state == OwnerState::Quiescent)
        && boundary.is_none_or(|boundary| {
            !matches!(
                boundary,
                Boundary::Preparing | Boundary::Switching | Boundary::Activated
            )
        })
}

/// Authority records must be at least as new as the legacy ones before the
/// legacy set may be treated as superseded. Any unprovable mtime refuses.
fn authority_covers_legacy(authority: &Path, legacy: &Path) -> bool {
    let newest = |root: &Path| {
        [".deploy-operation.json", ".deploy-coordinator.json"]
            .into_iter()
            .filter_map(|name| {
                std::fs::metadata(root.join(name))
                    .ok()
                    .and_then(|meta| meta.modified().ok())
            })
            .max()
    };
    match (newest(authority), newest(legacy)) {
        (Some(authority_time), Some(legacy_time)) => authority_time >= legacy_time,
        _ => false,
    }
}

/// Archive (rename, never delete) the superseded legacy record set. The
/// `.deploy-operation.lock` stays in place and stays held by the caller so an
/// old binary cannot start fresh against the archived legacy paths.
fn archive_superseded_records(legacy: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default();
    for name in [
        ".deploy-operation.json",
        ".deploy-coordinator.json",
        ".deploy-state.toml",
    ] {
        let source = legacy.join(name);
        let archived = legacy.join(format!("{name}.superseded-{stamp}"));
        match std::fs::metadata(&source) {
            Ok(_) => std::fs::rename(&source, &archived)
                .with_context(|| format!("archive legacy record {name}"))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("stat legacy record"),
        }
    }
    #[cfg(unix)]
    File::open(legacy)?.sync_all()?;
    Ok(())
}

/// `app-cli journal adopt`：双权威域冲突的可审计显式裁决。
///
/// - 无 legacy 记录 → no-op 成功（可作升级前批量预检）；
/// - 权威根为空 → 拒绝归档（legacy 是下次 serve 自动迁移的来源，归档即
///   掐断迁移），报告引导直接启动；
/// - 有冲突（默认）：执行与 serve 启动同款的判定——双侧 settled 且权威
///   记录不旧于 legacy → 归档陈旧侧；不满足则非零退出并给出具体原因；
/// - `--force`：跳过 settled/新旧判定（操作者断言权威根为真相），仅保留
///   两侧活锁拒绝这一道防线。归档改名可回滚，永不删除。
pub fn adopt_superseded_legacy(workspace: &Path, force: bool) -> Result<String> {
    let root = match std::env::var_os("APP_CLI_STATE_ROOT").filter(|value| !value.is_empty()) {
        Some(explicit) => PathBuf::from(explicit),
        None => workspace
            .parent()
            .context("workspace has no volume root")?
            .to_path_buf(),
    };
    adopt_superseded_legacy_with_root(workspace, root, force)
}

/// [`adopt_superseded_legacy`] 的显式 root 形态（serve 的 open_with_root 同款
/// 布局：authority root 独立于 legacy 候选路径）。
pub fn adopt_superseded_legacy_with_root(
    workspace: &Path,
    root: PathBuf,
    force: bool,
) -> Result<String> {
    // 活 owner 在跑 → open_root 的 try_lock 直接拒绝（与 serve 同因同文案）。
    let authority = Journal::open_root(root.clone())?;
    let project = runtime_state_layout::canonical_project_root(workspace);
    let mut candidates = vec![project.clone()];
    if let Some(parent) = project.parent() {
        candidates.push(parent.to_path_buf());
    }
    let root_identity = std::fs::canonicalize(&root)?;
    let conflicts: Vec<PathBuf> = candidates
        .into_iter()
        .filter(|legacy| {
            std::fs::canonicalize(legacy).is_ok_and(|path| path != root_identity)
                && (legacy
                    .join(".deploy-operation.json")
                    .try_exists()
                    .unwrap_or(false)
                    || legacy
                        .join(".deploy-coordinator.json")
                        .try_exists()
                        .unwrap_or(false))
        })
        .collect();
    if conflicts.is_empty() {
        return Ok(format!(
            "authority root: {}\nno conflicting legacy deployment records found",
            root.display()
        ));
    }
    anyhow::ensure!(
        authority.receipt.is_some() || authority.previous_owner.is_some(),
        "authority root {} has no deployment records; legacy records \
         migrate automatically on the next serve — nothing to adopt",
        root.display()
    );
    let mut report = format!("authority root: {}\n", root.display());
    for legacy in conflicts {
        let old = Journal::open_root(legacy.clone())?;
        if !force {
            let legacy_settled = domain_settled(
                old.receipt.as_ref().map(|receipt| &receipt.boundary),
                old.previous_owner.as_ref(),
            );
            let authority_settled = domain_settled(
                authority.receipt.as_ref().map(|receipt| &receipt.boundary),
                authority.previous_owner.as_ref(),
            );
            anyhow::ensure!(
                authority_settled && legacy_settled,
                "legacy {} or authority {} has an in-flight deployment handoff; \
                 settle it or rerun with --force after verifying the authority root",
                legacy.display(),
                root.display()
            );
            anyhow::ensure!(
                authority_covers_legacy(&root, &legacy),
                "legacy records at {} are newer than authority {}; verify for \
                 downgrade mixing, then rerun with --force",
                legacy.display(),
                root.display()
            );
        }
        archive_superseded_records(&legacy)?;
        report.push_str(&format!(
            "archived superseded legacy records at {}\n",
            legacy.display()
        ));
    }
    Ok(report)
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

    /// 双权威域·放行（2026-09-22 app-105 死锁修复的正例）：权威根记录更新且
    /// 双侧均无在途交接（coordinator quiescent ∧ boundary 终态）→ 陈旧 legacy
    /// 记录改名归档（不删除）、权威记录原样保留、serve 可正常启动。修复前该
    /// 形态恒拒绝，"failed 待重部署"的自我补救（重新部署）永远无法发生。
    #[test]
    fn superseded_legacy_records_are_archived_when_authority_newer_and_settled() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("project");
        std::fs::create_dir_all(&source).unwrap();
        let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
        // legacy 先落（settled：quiescent + 终态 boundary + 遗留 state.toml）
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        legacy.commit_quiescent().unwrap();
        std::fs::write(dir.path().join(".deploy-state.toml"), "legacy").unwrap();
        drop(legacy);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // 权威根后落（app-105 形态：一笔 failed 待重部署 + quiescent）
        let mut authority = Journal::open_root(root.clone()).unwrap();
        authority.commit_coordinator().unwrap();
        authority.write(receipt(Boundary::Failed)).unwrap();
        authority.commit_quiescent().unwrap();
        let authority_bytes = std::fs::read(root.join(".deploy-operation.json")).unwrap();
        drop(authority);
        std::thread::sleep(std::time::Duration::from_millis(20));

        let journal = Journal::open_with_root(&source, root.clone()).unwrap();
        // 权威记录原样保留；journal 直接可用（重部署路径恢复）
        assert_eq!(
            std::fs::read(root.join(".deploy-operation.json")).unwrap(),
            authority_bytes
        );
        assert_eq!(
            journal.receipt.as_ref().unwrap().operation.operation_id,
            "hot-b"
        );
        // legacy 三类记录全部改名归档；锁文件原样保留（旧二进制不得在 legacy
        // 路径以全新记录启动）
        assert!(!dir.path().join(".deploy-operation.json").exists());
        assert!(!dir.path().join(".deploy-coordinator.json").exists());
        assert!(!dir.path().join(".deploy-state.toml").exists());
        assert!(dir.path().join(".deploy-operation.lock").exists());
        let archived = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.contains(".superseded-"))
            })
            .count();
        assert!(archived >= 3, "archived records: {archived}");
    }

    /// 双权威域·拒绝（任一侧在途）：legacy 侧 coordinator 从未 quiescent
    /// （可能在跑/被中断未收束）→ 归档不生效，记录原样保留。
    #[test]
    fn dual_domain_with_in_flight_legacy_records_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("project");
        std::fs::create_dir_all(&source).unwrap();
        let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        // 不 commit_quiescent：owner 仍 Active
        drop(legacy);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut authority = Journal::open_root(root.clone()).unwrap();
        authority.commit_coordinator().unwrap();
        authority.write(receipt(Boundary::Failed)).unwrap();
        authority.commit_quiescent().unwrap();
        drop(authority);
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(Journal::open_with_root(&source, root.clone()).is_err());
        assert!(dir.path().join(".deploy-operation.json").exists());
        assert!(root.join(".deploy-operation.json").exists());
    }

    /// 双权威域·拒绝（权威侧在途）：权威 boundary 处于交接中
    /// （Switching）→ 不允许自动归档 legacy。
    #[test]
    fn dual_domain_with_in_flight_authority_records_still_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("project");
        std::fs::create_dir_all(&source).unwrap();
        let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        legacy.commit_quiescent().unwrap();
        drop(legacy);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut authority = Journal::open_root(root.clone()).unwrap();
        authority.commit_coordinator().unwrap();
        authority.write(receipt(Boundary::Switching)).unwrap();
        authority.commit_quiescent().unwrap();
        drop(authority);
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(Journal::open_with_root(&source, root.clone()).is_err());
        assert!(dir.path().join(".deploy-operation.json").exists());
    }

    /// 双权威域·拒绝（legacy 反而更新）：旧运行时在新布局迁移之后又写过
    /// legacy 记录（降级混跑）——真相有争议，必须显式恢复。
    #[test]
    fn legacy_records_newer_than_authority_rejected_as_downgrade_mixing() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("project");
        std::fs::create_dir_all(&source).unwrap();
        let root = runtime_state_layout::ensure_state_root(&source, None, None).unwrap();
        // 权威先落（settled）
        let mut authority = Journal::open_root(root.clone()).unwrap();
        authority.commit_coordinator().unwrap();
        authority.write(receipt(Boundary::Failed)).unwrap();
        authority.commit_quiescent().unwrap();
        drop(authority);
        std::thread::sleep(std::time::Duration::from_millis(20));
        // legacy 后落（settled 但更新）
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        legacy.commit_quiescent().unwrap();
        drop(legacy);
        std::thread::sleep(std::time::Duration::from_millis(20));

        assert!(Journal::open_with_root(&source, root.clone()).is_err());
        assert!(dir.path().join(".deploy-operation.json").exists());
        assert!(root.join(".deploy-operation.json").exists());
    }

    // ── `app-cli journal adopt` 子命令（可审计显式裁决）────────────────────

    /// 无冲突 → no-op 成功（升级前批量预检语义）。
    #[test]
    fn adopt_without_conflict_is_noop_success() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).unwrap();
        let root = runtime_state_layout::ensure_state_root(&workspace, None, None).unwrap();
        let report = adopt_superseded_legacy_with_root(&workspace, root, false).unwrap();
        assert!(report.contains("no conflicting legacy deployment records found"));
    }

    /// 权威根为空 → 拒绝归档：legacy 是下次 serve 自动迁移的来源，
    /// 归档即掐断迁移（journal 历史丢失）。
    #[test]
    fn adopt_never_archives_when_authority_root_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).unwrap();
        let root = runtime_state_layout::ensure_state_root(&workspace, None, None).unwrap();
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        legacy.commit_quiescent().unwrap();
        drop(legacy);

        let error = adopt_superseded_legacy_with_root(&workspace, root, false).unwrap_err();
        assert!(error.to_string().contains("nothing to adopt"));
        assert!(dir.path().join(".deploy-operation.json").exists());
    }

    /// 默认判定与 serve 自动收束同款：settled+权威新 → 归档；
    /// legacy 在途 → 拒绝且记录原样；--force → 归档放行。
    #[test]
    fn adopt_follows_settled_checks_and_force_overrides() {
        // 形态一：settled + 权威新（可自动收束的冲突）→ adopt 默认即归档
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).unwrap();
        let root = runtime_state_layout::ensure_state_root(&workspace, None, None).unwrap();
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        legacy.commit_quiescent().unwrap();
        drop(legacy);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut authority = Journal::open_root(root.clone()).unwrap();
        authority.commit_coordinator().unwrap();
        authority.write(receipt(Boundary::Failed)).unwrap();
        authority.commit_quiescent().unwrap();
        drop(authority);
        std::thread::sleep(std::time::Duration::from_millis(20));

        let report = adopt_superseded_legacy_with_root(&workspace, root.clone(), false).unwrap();
        assert!(report.contains("archived superseded legacy records"));
        assert!(!dir.path().join(".deploy-operation.json").exists());
        assert!(root.join(".deploy-operation.json").exists());

        // 形态二：legacy 在途（coordinator 未 quiescent）→ 默认拒绝带原因，
        // --force 归档放行
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir_all(&workspace).unwrap();
        let root = runtime_state_layout::ensure_state_root(&workspace, None, None).unwrap();
        let mut legacy = Journal::open_root(dir.path().to_path_buf()).unwrap();
        legacy.commit_coordinator().unwrap();
        legacy.write(receipt(Boundary::Active)).unwrap();
        // 不 quiescent：在途
        drop(legacy);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let mut authority = Journal::open_root(root.clone()).unwrap();
        authority.commit_coordinator().unwrap();
        authority.write(receipt(Boundary::Failed)).unwrap();
        authority.commit_quiescent().unwrap();
        drop(authority);
        std::thread::sleep(std::time::Duration::from_millis(20));

        let error = adopt_superseded_legacy_with_root(&workspace, root.clone(), false).unwrap_err();
        assert!(error.to_string().contains("in-flight"));
        assert!(dir.path().join(".deploy-operation.json").exists());
        let report = adopt_superseded_legacy_with_root(&workspace, root, true).unwrap();
        assert!(report.contains("archived superseded legacy records"));
        assert!(!dir.path().join(".deploy-operation.json").exists());
    }
    fn receipt(boundary: Boundary) -> Receipt {
        let request = DeployRequest {
            runtime_operation_id: None,
            url: "http://artifact/b".into(),
            release_id: "b".into(),
            sha256: None,
            local_path: None,
            execution_target: None,
            run_pg: None,
        };
        Receipt {
            generation: "generation-a".into(),

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
    fn retired_configuration_fields_do_not_change_deployment_identity_or_boundary() {
        let mut legacy = serde_json::to_value(receipt(Boundary::RestoredActive)).unwrap();
        legacy["generation_handoff"] = serde_json::json!({"retired": true});
        legacy["request"]["requires_configuration_activation"] = true.into();
        legacy["active"]["request"]["requires_configuration_activation"] = true.into();
        let restored: Receipt = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.generation, "generation-a");
        assert_eq!(restored.boundary, Boundary::RestoredActive);
        assert_eq!(restored.operation.operation_id, "hot-b");
        assert_eq!(
            restored.active.as_ref().unwrap().artifact_release_id,
            "manifest-b"
        );
        let saved = serde_json::to_value(restored).unwrap();
        assert!(saved.get("generation_handoff").is_none());
        assert!(
            saved["request"]
                .get("requires_configuration_activation")
                .is_none()
        );
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
