use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    #[default]
    Run,
    /// Recreate the management plane, with automatic business startup suppressed.
    Stopped,
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Generation {
    pub version: u32,
    pub id: String,
    pub supervisor: String,
    pub token: String,
    pub intent: Intent,
    pub phase: GenerationPhase,
    /// Launch handshake and diagnostics only. Signals use the retained Child.
    #[serde(default)]
    pub worker_pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub physical_domain: Option<crate::domain::PhysicalDomain>,
    /// Identity of the OS boot / PID namespace that ran this generation.
    /// A changed epoch is positive local proof the generation's processes
    /// cannot run again; records without it stay conservative.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_epoch: Option<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum GenerationPhase {
    Pending,
    Running,
    Draining,
    Quiescent,
    Revoked,
}

pub(crate) fn save<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let parent = path.parent().context("receipt parent missing")?;
    process_utils::command_context::create_durable_directory(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut file, value)?;
    file.flush()?;
    file.as_file().sync_all()?;
    process_utils::atomic_file::persist(file, path)
        .with_context(|| format!("publish receipt {}", path.display()))?;
    #[cfg(unix)]
    File::open(parent)?.sync_all()?;
    Ok(())
}
pub(crate) fn read<T: DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(
        &std::fs::read(path).with_context(|| format!("read receipt {}", path.display()))?,
    )
    .with_context(|| format!("decode receipt {}", path.display()))
}
pub(crate) fn lock(path: &Path) -> Result<File> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    file.try_lock().context("execution scope is still owned")?;
    Ok(file)
}
pub(crate) fn is_locked(path: &Path) -> Result<()> {
    let file = File::options().read(true).write(true).open(path)?;
    match file.try_lock() {
        Err(std::fs::TryLockError::WouldBlock) => Ok(()),
        Err(error) => Err(error.into()),
        Ok(()) => anyhow::bail!("execution owner has exited"),
    }
}
pub(crate) fn generation(root: &Path) -> Result<Generation> {
    let value: Generation = read(&root.join("generation.json"))?;
    ensure!(
        value.version == 1 && root.file_name().and_then(|s| s.to_str()) == Some(&value.id),
        "generation identity/version mismatch"
    );
    Ok(value)
}
pub(crate) fn work_root(scope: &Path, id: &str) -> Result<PathBuf> {
    ensure!(
        uuid::Uuid::parse_str(id).is_ok(),
        "invalid execution generation"
    );
    Ok(scope.join("work").join(id))
}

/// An identity-bound local cleanup receipt, not business success evidence.
#[derive(Clone, Debug)]
pub struct Quiescence {
    pub generation: String,
    pub supervisor_id: String,
}

pub fn verify_quiescent(scope: &Path, id: &str) -> Result<Quiescence> {
    let root = work_root(scope, id)?;
    let _lock = lock(&root.join("generation.lock"))?;
    let record = generation(&root)?;
    ensure!(
        matches!(
            record.phase,
            GenerationPhase::Quiescent | GenerationPhase::Revoked
        ),
        "execution generation has no cleanup receipt"
    );
    if record.phase == GenerationPhase::Revoked {
        process_utils::guardian::recover(&root)?;
        process_utils::command_context::require_quiescent(&root.join("commands"))?;
    }
    // Quiescent is the aggregate receipt written only after the worker,
    // command trees and external engine finished cleanup. Historical command
    // diagnostics cannot invalidate that completed cleanup later.
    Ok(Quiescence {
        generation: record.id,
        supervisor_id: record.supervisor,
    })
}

/// Confirm cleanup in this process domain. `None` means the record belongs to
/// a previous container on the same managed workspace, not that it exited.
/// The platform owns cross-container retirement; a local Stop cannot stop or
/// wait for processes in that other container.
pub fn verify_local_quiescent(scope: &Path, id: &str) -> Result<Option<Quiescence>> {
    if belongs_to_previous_container(scope, id)? {
        return Ok(None);
    }
    verify_quiescent(scope, id).map(Some)
}

pub(crate) fn belongs_to_previous_container(scope: &Path, id: &str) -> Result<bool> {
    let value = generation(&work_root(scope, id)?)?;
    Ok(previous_container(
        &value,
        crate::domain::PhysicalDomain::from_env()?.as_ref(),
    ))
}

fn previous_container(value: &Generation, current: Option<&crate::domain::PhysicalDomain>) -> bool {
    matches!((value.physical_domain.as_ref(), current), (Some(old), Some(current))
        if old.authority == current.authority
            && old.volume == current.volume
            && old.instance != current.instance)
}

pub fn verify_live(scope: &Path, supervisor: &str, id: &str) -> Result<()> {
    let root = work_root(scope, id)?;
    let generation = generation(&root)?;
    ensure!(
        generation.supervisor == supervisor && generation.phase == GenerationPhase::Running,
        "execution identity is not active"
    );
    is_locked(&root.join("generation.lock"))?;
    is_locked(&scope.join("owner.lock"))?;
    Ok(())
}

/// Run only with the stable owner lock held. A late root guardian must obtain
/// this same generation lock and cannot consume revoked authorization.
pub(crate) fn reconcile(scope: &Path) -> Result<()> {
    reconcile_local(scope, crate::domain::PhysicalDomain::from_env()?.as_ref())
}

fn reconcile_local(scope: &Path, current: Option<&crate::domain::PhysicalDomain>) -> Result<()> {
    let dir = scope.join("work");
    if !dir.try_exists()? {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let root = entry?.path();
        if !root.join("generation.json").try_exists()? {
            continue;
        } // legacy adapter owns legacy receipts
        let value = generation(&root)?;
        if previous_container(&value, current) {
            // Keep foreign process and command receipts unchanged. A replacement
            // container must not interpret them as live work in its own process
            // namespace, or depend on a deleted Pod to bootstrap management.
            tracing::debug!(generation = %value.id,
                "previous container execution retained as history; recovering local management");
            continue;
        }
        let _lock = lock(&root.join("generation.lock"))?;
        let mut value = generation(&root)?;
        let retirable = matches!(
            value.phase,
            GenerationPhase::Running | GenerationPhase::Draining
        );
        if retirable
            && (crate::domain::has_confirmed_exit(&root, &value)?
                || local_process_space_ended(&value)?)
        {
            // Platform receipt or process-space epoch both prove the physical
            // runtime ended; neither invents an exit code nor touches journals.
            process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
            process_utils::guardian::confirm_physical_domain_exit(&root)?;
            retire_confirmed(&mut value);
            save(&root.join("generation.json"), &value)?;
        }
        if value.phase == GenerationPhase::Pending {
            process_utils::command_authority::Gate::try_acquire(&root)?.close()?;
            value.phase = GenerationPhase::Revoked;
            save(&root.join("generation.json"), &value)?;
        }
        ensure!(
            matches!(
                value.phase,
                GenerationPhase::Quiescent | GenerationPhase::Revoked
            ),
            "generation {} cleanup is unconfirmed: {:?}",
            value.id,
            value.phase
        );
        if value.phase == GenerationPhase::Revoked {
            process_utils::guardian::recover(&root)?;
            process_utils::command_context::require_quiescent(&root.join("commands"))?;
        }
    }
    Ok(())
}

/// Terminal retirement of one generation. No exit code is invented and no
/// business journal is edited; cleanup is process evidence, not success.
fn retire_confirmed(value: &mut Generation) {
    value.phase = GenerationPhase::Quiescent;
    value.error =
        Some("physical runtime confirmed exit; business outcomes remain unchanged".into());
}

/// The generation's process space (OS boot or PID namespace) was replaced
/// after it stopped writing, so none of its processes can still exist. This
/// only speaks for generations that ran in OUR process space: a generation
/// stamped with a different physical domain (another container/pod) must be
/// retired by platform evidence, never by the local epoch.
fn local_process_space_ended(value: &Generation) -> Result<bool> {
    let (Some(recorded), Some(current)) = (value.process_epoch.as_deref(), crate::epoch::current())
    else {
        return Ok(false);
    };
    if !crate::epoch::proves_replacement(recorded, &current) {
        return Ok(false);
    }
    Ok(process_space_ended_with(
        value,
        crate::domain::PhysicalDomain::from_env()?.as_ref(),
    ))
}

/// Epoch-side scoping only: a changed epoch proves the end of the process
/// space that ran this generation, but a stamped generation belongs to one
/// container/pod, so the local proof may only fire when that identity is the
/// current environment's own domain.
fn process_space_ended_with(
    value: &Generation,
    current_domain: Option<&crate::domain::PhysicalDomain>,
) -> bool {
    match value.physical_domain.as_ref() {
        // Only native (un-stamped) scopes may use the bare local proof.
        None => true,
        Some(domain) => current_domain == Some(domain),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EpochGuard;
    impl EpochGuard {
        fn new(value: Option<String>) -> Self {
            crate::epoch::set_epoch_for_tests(value);
            Self
        }
    }
    impl Drop for EpochGuard {
        fn drop(&mut self) {
            crate::epoch::set_epoch_for_tests(None);
        }
    }

    /// Minimal stuck-generation scope: Running, no cleanup receipts, a live
    /// command record and an initialized admission gate.
    fn stuck_scope(
        domain: Option<crate::domain::PhysicalDomain>,
        epoch: Option<String>,
    ) -> (tempfile::TempDir, String) {
        let temp = tempfile::tempdir().unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let root = work_root(temp.path(), &id).unwrap();
        std::fs::create_dir_all(root.join("commands")).unwrap();
        process_utils::command_authority::Gate::try_acquire(&root)
            .unwrap()
            .initialize()
            .unwrap();
        save(
            &root.join("generation.json"),
            &Generation {
                version: 1,
                id: id.clone(),
                supervisor: "gone-supervisor".into(),
                token: "test".into(),
                intent: Intent::Run,
                phase: GenerationPhase::Running,
                worker_pid: Some(4242),
                exit_code: None,
                error: None,
                physical_domain: domain,
                process_epoch: epoch,
            },
        )
        .unwrap();
        save(
            &root.join("commands/command.json"),
            &serde_json::json!({"version":1,"phase":"Running","identity":{"task_id":"original"}}),
        )
        .unwrap();
        (temp, id)
    }

    #[test]
    fn completed_generation_receipt_outlives_command_diagnostics_but_not_a_live_guardian() {
        let (temp, id) = stuck_scope(None, None);
        let root = work_root(temp.path(), &id).unwrap();
        std::fs::write(root.join("commands/command.json"), "{damaged").unwrap();
        assert!(reconcile(temp.path()).is_err(), "Running is not exit proof");
        let mut value = generation(&root).unwrap();
        value.phase = GenerationPhase::Quiescent;
        save(&root.join("generation.json"), &value).unwrap();
        let held = lock(&root.join("generation.lock")).unwrap();
        assert!(verify_quiescent(temp.path(), &id).is_err());
        assert!(reconcile(temp.path()).is_err());
        drop(held);
        verify_quiescent(temp.path(), &id).unwrap();
        reconcile(temp.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("commands/command.json")).unwrap(),
            "{damaged"
        );
    }

    #[test]
    fn process_epoch_change_retires_only_local_generations_and_preserves_outcomes() {
        // One sequential test: the epoch override is process-global.
        // (a) native scope, epoch changed → retired, business record untouched.
        let (_temp, id) = stuck_scope(
            None,
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        let _guard = EpochGuard::new(Some(
            "pid1:11111111-1111-4111-8111-111111111111:9000".into(),
        ));
        reconcile(_temp.path()).unwrap();
        let retired = generation(&work_root(_temp.path(), &id).unwrap()).unwrap();
        assert_eq!(retired.phase, GenerationPhase::Quiescent);
        assert_eq!(retired.exit_code, None, "no exit code is invented");
        assert!(
            retired
                .error
                .unwrap()
                .contains("business outcomes remain unchanged")
        );
        let command: serde_json::Value = read(
            &work_root(_temp.path(), &id)
                .unwrap()
                .join("commands/command.json"),
        )
        .unwrap();
        assert_eq!(command["phase"], "Quiescent");
        assert_eq!(command["termination"], "PhysicalDomainExited");
        assert_eq!(command["identity"]["task_id"], "original");
        drop(_guard);

        // (b) native scope, same epoch → still unconfirmed (hard kill only).
        let (_temp, _id) = stuck_scope(
            None,
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        let _guard = EpochGuard::new(Some("pid1:11111111-1111-4111-8111-111111111111:100".into()));
        let error = reconcile(_temp.path()).unwrap_err().to_string();
        assert!(error.contains("cleanup is unconfirmed"), "{error}");
        drop(_guard);

        // (c) legacy record without epoch → conservative.
        let (_temp, _id) = stuck_scope(None, None);
        let _guard = EpochGuard::new(Some(
            "pid1:11111111-1111-4111-8111-111111111111:9000".into(),
        ));
        assert!(reconcile(_temp.path()).is_err());
        drop(_guard);

        // (d) unreadable epoch → conservative.
        let (_temp, _id) = stuck_scope(
            None,
            Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        );
        let _guard = EpochGuard::new(None);
        assert!(reconcile(_temp.path()).is_err());
        drop(_guard);
    }

    #[test]
    fn replacement_container_preserves_foreign_history_without_blocking_local_recovery() {
        let old = crate::domain::PhysicalDomain {
            authority: "test-runtime".into(),
            instance_source_env: None,
            instance: "old-container".into(),
            volume: "same-workspace-volume".into(),
        };
        let (temp, id) = stuck_scope(Some(old.clone()), None);
        let _owner = crate::Owner::try_acquire(temp.path()).unwrap().unwrap();
        let root = work_root(temp.path(), &id).unwrap();
        let original = std::fs::read(root.join("generation.json")).unwrap();
        let command = std::fs::read(root.join("commands/command.json")).unwrap();
        // Neither another container nor a missing local process is exit proof.
        assert!(reconcile_local(temp.path(), Some(&old)).is_err());
        assert!(reconcile_local(temp.path(), None).is_err());
        let mut current = old.clone();
        current.instance = "replacement-container".into();
        reconcile_local(temp.path(), Some(&current)).unwrap();
        assert_eq!(
            std::fs::read(root.join("generation.json")).unwrap(),
            original
        );
        assert_eq!(
            std::fs::read(root.join("commands/command.json")).unwrap(),
            command
        );
        assert!(
            verify_quiescent(temp.path(), &id).is_err(),
            "no fabricated exit receipt"
        );
        current.volume = "another-volume".into();
        assert!(reconcile_local(temp.path(), Some(&current)).is_err());
        current.volume = old.volume;
        current.authority = "another-runtime".into();
        assert!(reconcile_local(temp.path(), Some(&current)).is_err());
    }

    #[test]
    fn local_epoch_never_retires_another_physical_domain() {
        use crate::domain::PhysicalDomain;
        let other = PhysicalDomain {
            authority: "daemon-a".into(),
            instance_source_env: None,
            volume: "volume-a".into(),
            instance: uuid::Uuid::new_v4().to_string(),
        };
        let current = other.clone();
        let same_domain = Generation {
            version: 1,
            id: uuid::Uuid::new_v4().to_string(),
            supervisor: "s".into(),
            token: "t".into(),
            intent: Intent::Run,
            phase: GenerationPhase::Running,
            worker_pid: None,
            exit_code: None,
            error: None,
            physical_domain: Some(other.clone()),
            process_epoch: Some("pid1:11111111-1111-4111-8111-111111111111:100".into()),
        };
        // Same container identity, new incarnation → local proof applies.
        assert!(process_space_ended_with(&same_domain, Some(&current)));
        // A generation from a different container/pod needs platform evidence.
        let mut foreign = same_domain.clone();
        foreign.physical_domain = Some(PhysicalDomain {
            authority: "daemon-a".into(),
            instance_source_env: None,
            volume: "volume-a".into(),
            instance: uuid::Uuid::new_v4().to_string(),
        });
        assert!(!process_space_ended_with(&foreign, Some(&current)));
        // No current domain (native reader) cannot speak for stamped records.
        assert!(!process_space_ended_with(&foreign, None));
    }
}
