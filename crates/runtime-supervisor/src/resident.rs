//! Resident ranges belong to the locked owner, independently of work/<UUID>.
use anyhow::{Context, Result, ensure};
use process_utils::command_authority::{ResidentIdentity, ResidentScope};
use std::{path::Path, time::Duration};

#[cfg(test)]
pub(crate) async fn initialize(
    root: &Path,
    lease: &process_utils::command_authority::OwnerLease,
    identity: ResidentIdentity,
) -> Result<ResidentScope> {
    reconcile_prior(root, &identity, None).await?;
    ResidentScope::initialize(lease, identity)
}

pub(crate) async fn initialize_recovered(
    root: &Path,
    lease: &process_utils::command_authority::OwnerLease,
    identity: ResidentIdentity,
    settled_previous: Option<&crate::Binding>,
) -> Result<ResidentScope> {
    reconcile_prior(root, &identity, settled_previous).await?;
    ResidentScope::initialize(lease, identity)
}

/// The adapter-authorized rebind calls this while the original binding is still
/// published. A previous binding may be accepted later only after this exact
/// original range has reached guardian quiescence under the held owner lease.
pub(crate) async fn reconcile_prior(
    root: &Path,
    identity: &ResidentIdentity,
    settled_previous: Option<&crate::Binding>,
) -> Result<()> {
    reconcile_prior_until(
        root,
        identity,
        settled_previous,
        tokio::time::Instant::now() + Duration::from_secs(15),
    )
    .await
}

pub(crate) async fn reconcile_prior_until(
    root: &Path,
    identity: &ResidentIdentity,
    settled_previous: Option<&crate::Binding>,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let directory = root.join("resident");
    if directory.try_exists()? {
        for entry in std::fs::read_dir(&directory)? {
            let previous = entry?.path();
            ensure!(
                std::fs::canonicalize(&previous)? == previous,
                "resident range resolves outside owner root"
            );
            let old = process_utils::command_authority::read_resident_identity(&previous)?;
            ensure!(
                old.application_id == identity.application_id,
                "resident belongs to another application"
            );
            if old.binding != identity.binding {
                let permitted = settled_previous.map(serde_json::to_value).transpose()?;
                ensure!(
                    permitted.as_ref() == Some(&old.binding),
                    "resident belongs to another application or workspace"
                );
                process_utils::command_authority::Gate::try_acquire(&previous)?.close()?;
                recover_previous(&previous, false, deadline)
                    .await
                    .context("authorized previous resident cleanup is no longer quiescent")?;
                continue;
            }
            let same_domain = old.physical_domain == identity.physical_domain;
            let current = identity
                .physical_domain
                .as_ref()
                .map(|value| serde_json::from_value::<crate::domain::PhysicalDomain>(value.clone()))
                .transpose()?;
            let platform_exit = !same_domain
                && crate::domain::has_confirmed_resident_exit(root, &old, current.as_ref())?;
            ensure!(
                same_domain || platform_exit,
                "resident belongs to another physical domain; matching original owner/workspace platform exit evidence required"
            );
            process_utils::command_authority::Gate::try_acquire(&previous)?.close()?;
            let replacement = old
                .process_epoch
                .as_deref()
                .zip(identity.process_epoch.as_deref())
                .is_some_and(|(before, now)| crate::epoch::proves_replacement(before, now));
            recover_previous(&previous, platform_exit || replacement, deadline).await?;
        }
    }
    Ok(())
}

async fn recover_previous(
    previous: &Path,
    physical_exit: bool,
    deadline: tokio::time::Instant,
) -> Result<()> {
    loop {
        let result = if physical_exit {
            process_utils::guardian::confirm_physical_domain_exit(previous)
                .map(|()| process_utils::guardian::RecoveryOutcome::Settled)
        } else {
            process_utils::guardian::recover_with_scope_check(previous)
        };
        match result {
            Ok(process_utils::guardian::RecoveryOutcome::Settled) => return Ok(()),
            Ok(process_utils::guardian::RecoveryOutcome::Stopping { detail }) => {
                ensure!(
                    tokio::time::Instant::now() < deadline,
                    "prior resident still draining: {detail}"
                );
            }
            Err(error)
                if error
                    .downcast_ref::<std::fs::TryLockError>()
                    .is_some_and(|e| matches!(e, std::fs::TryLockError::WouldBlock))
                    && tokio::time::Instant::now() < deadline => {}
            Err(error) => {
                return Err(error).context("prior resident guardian cleanup remains unconfirmed");
            }
        }
        tokio::time::sleep_until(std::cmp::min(
            deadline,
            tokio::time::Instant::now() + Duration::from_millis(100),
        ))
        .await;
    }
}

#[cfg(test)]
#[path = "resident/whole_domain_exit_tests.rs"]
mod whole_domain_exit_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use process_utils::command_authority::OwnerLease;

    fn identity(
        application: &str,
        instance: &str,
        binding: &serde_json::Value,
    ) -> ResidentIdentity {
        ResidentIdentity {
            application_id: application.into(),
            binding: binding.clone(),
            owner_instance: instance.into(),
            physical_domain: None,
            process_epoch: crate::epoch::current(),
        }
    }

    fn discovery(root: &Path, identity: &ResidentIdentity) {
        std::fs::write(
            root.join("supervisor.json"),
            serde_json::to_vec(&serde_json::json!({
                "instance": identity.owner_instance,
                "snapshot": {"supervisor_id": identity.owner_instance, "binding": identity.binding}
            }))
            .unwrap(),
        )
        .unwrap();
    }

    #[cfg(unix)]
    struct ProcessCleanup(std::process::Child);

    #[cfg(unix)]
    impl Drop for ProcessCleanup {
        fn drop(&mut self) {
            let _result = self.0.kill();
            let _result = self.0.wait();
        }
    }

    #[cfg(unix)]
    fn running_guardian(scope: &ResidentScope, command_pid: u32) -> std::path::PathBuf {
        let root = scope
            .root()
            .join("guardians")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("receipt.json"),
            serde_json::to_vec(&serde_json::json!({
                "version":2, "id":root.file_name().unwrap().to_str().unwrap(),
                "instance_id":scope.identity().owner_instance, "phase":"Running",
                "command_record":null, "command_digest":"0".repeat(64),
                "root_status":null, "diagnostic_pid":command_pid,
                "resident_identity":scope.identity()
            }))
            .unwrap(),
        )
        .unwrap();
        root
    }

    /// The actual owner lock and command process are gone, but the crash left
    /// an ordinary Running receipt. A new owner must converge without deleting
    /// that receipt or inventing a successful command result.
    #[cfg(unix)]
    #[tokio::test]
    async fn dead_resident_command_is_settled_before_successor_initialization() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let binding = serde_json::json!({"component":"app-cli", "resource":root});
        let owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let original = identity("app", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &original);
        let scope = ResidentScope::initialize(&owner, original).unwrap();
        let mut command = ProcessCleanup(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let command_pid = command.0.id();
        let guardian = running_guardian(&scope, command_pid);
        scope.close().unwrap();
        command.0.kill().unwrap();
        command.0.wait().unwrap();
        assert!(!process_utils::process_exists(command_pid).unwrap());
        assert!(!process_utils::process_group_exists(command_pid).unwrap());
        drop(scope);
        drop(owner);
        let successor = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let next = identity("app", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &next);
        let replacement = initialize(&root, &successor, next.clone()).await.unwrap();
        assert_eq!(replacement.identity(), &next);
        let recovered: serde_json::Value =
            serde_json::from_slice(&std::fs::read(guardian.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(recovered["phase"], "Quiescent");
        assert!(recovered["root_status"].is_null());
        assert!(root.join("owner.lock").is_file());
        assert!(OwnerLease::try_acquire(&root).unwrap().is_none());
        replacement.close().unwrap();
        replacement.record_quiescent().unwrap();
    }

    /// A released owner lock alone is insufficient while its actual command
    /// remains alive. Recovery waits, preserves the receipt, and continues as
    /// soon as the real command exits within the original recovery budget.
    #[cfg(unix)]
    #[tokio::test]
    async fn resident_recovery_waits_for_live_command_then_converges() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let binding = serde_json::json!({"component":"app-cli", "resource":root});
        let owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let original = identity("app", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &original);
        let scope = ResidentScope::initialize(&owner, original).unwrap();
        let mut command = ProcessCleanup(
            std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        );
        let guardian = running_guardian(&scope, command.0.id());
        let before = std::fs::read(guardian.join("receipt.json")).unwrap();
        scope.close().unwrap();
        drop(scope);
        drop(owner);
        let successor = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let next = identity("app", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &next);
        let recovery = initialize(&root, &successor, next.clone());
        tokio::pin!(recovery);
        tokio::select! {
            result = &mut recovery => panic!("live original command must defer successor (succeeded={})", result.is_ok()),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert_eq!(
            std::fs::read(guardian.join("receipt.json")).unwrap(),
            before
        );
        assert!(process_utils::process_exists(command.0.id()).unwrap());
        command.0.kill().unwrap();
        command.0.wait().unwrap();
        let replacement = tokio::time::timeout(Duration::from_secs(2), recovery)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replacement.identity(), &next);
        replacement.close().unwrap();
        replacement.record_quiescent().unwrap();
    }

    /// The guardian's own real lock remains authoritative even when its
    /// command PID already exited; a delayed guardian may still commit writes.
    #[cfg(unix)]
    #[tokio::test]
    async fn resident_recovery_waits_for_real_guardian_lock_release() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let binding = serde_json::json!({"component":"app-cli", "resource":root});
        let owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let original = identity("app", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &original);
        let scope = ResidentScope::initialize(&owner, original).unwrap();
        let mut command = std::process::Command::new("true").spawn().unwrap();
        let command_pid = command.id();
        command.wait().unwrap();
        let guardian = running_guardian(&scope, command_pid);
        let lock = std::fs::File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(guardian.join("guardian.lock"))
            .unwrap();
        lock.try_lock().unwrap();
        let before = std::fs::read(guardian.join("receipt.json")).unwrap();
        scope.close().unwrap();
        drop(scope);
        drop(owner);
        let successor = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let next = identity("app", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &next);
        let recovery = initialize(&root, &successor, next.clone());
        tokio::pin!(recovery);
        tokio::select! {
            result = &mut recovery => panic!("held guardian lock must defer successor (succeeded={})", result.is_ok()),
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
        }
        assert_eq!(
            std::fs::read(guardian.join("receipt.json")).unwrap(),
            before
        );
        drop(lock);
        let replacement = tokio::time::timeout(Duration::from_secs(2), recovery)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replacement.identity(), &next);
        replacement.close().unwrap();
        replacement.record_quiescent().unwrap();
    }

    #[tokio::test]
    async fn resident_recovery_rejects_cross_application_domain_and_unknown_prior_tree() {
        let temp = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(temp.path()).unwrap();
        let binding = serde_json::json!({"component": "app-cli", "resource": root});
        let old_owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let old_identity = identity("app-a", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &old_identity);
        let old_scope = ResidentScope::initialize(&old_owner, old_identity.clone()).unwrap();
        old_scope.close().unwrap();
        old_scope.record_quiescent().unwrap();
        let old_root = old_scope.root().to_path_buf();
        let receipt_before = std::fs::read(old_root.join("resident-scope.json")).unwrap();
        drop(old_scope);
        drop(old_owner);

        let successor = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let foreign_app = identity("app-b", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &foreign_app);
        let error = initialize(&root, &successor, foreign_app)
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("another application"));
        assert_eq!(
            std::fs::read(old_root.join("resident-scope.json")).unwrap(),
            receipt_before
        );

        let mut foreign_domain = identity("app-a", &uuid::Uuid::new_v4().to_string(), &binding);
        foreign_domain.physical_domain = Some(
            serde_json::json!({"authority":"container","instance":"other","volume":"workspace"}),
        );
        discovery(&root, &foreign_domain);
        let error = initialize(&root, &successor, foreign_domain)
            .await
            .err()
            .unwrap();
        assert!(format!("{error:#}").contains("another physical domain"));

        let current = identity("app-a", &uuid::Uuid::new_v4().to_string(), &binding);
        discovery(&root, &current);
        let guardian_id = uuid::Uuid::new_v4().to_string();
        let guardian_root = old_root.join("guardians").join(&guardian_id);
        std::fs::create_dir_all(&guardian_root).unwrap();
        std::fs::write(
            guardian_root.join("receipt.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 2, "id": guardian_id, "instance_id": old_identity.owner_instance,
                "phase": "Running", "command_record": null, "command_digest": "unknown",
                "root_status": null, "diagnostic_pid": null
            }))
            .unwrap(),
        )
        .unwrap();
        let error = initialize(&root, &successor, current).await.err().unwrap();
        assert!(format!("{error:#}").contains("cleanup remains unconfirmed"));
        assert!(
            std::fs::read_to_string(guardian_root.join("receipt.json"))
                .unwrap()
                .contains("Running")
        );
        assert!(
            OwnerLease::try_acquire(&root).unwrap().is_none(),
            "uncertain recovery must retain current owner"
        );
    }
    #[tokio::test]
    async fn cross_domain_resident_requires_the_original_owner_retirement_proof() {
        use crate::{
            Binding, Intent, Phase,
            control::{Discovery, Snapshot},
            domain::{PhysicalDomain, Retirement},
            record::{self, Generation, GenerationPhase},
        };
        let root_dir = tempfile::tempdir().unwrap();
        let root = root_dir.path().canonicalize().unwrap();
        let binding = Binding {
            component: "app-cli".into(),
            resource: root.clone(),
        };
        let original_domain = PhysicalDomain {
            authority: "k8s:fixture".into(),
            instance_source_env: None,
            instance: "old-pod".into(),
            volume: "pvc:workspace".into(),
        };
        let current_domain = PhysicalDomain {
            instance: "new-pod".into(),
            ..original_domain.clone()
        };
        let previous_owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let mut original = identity(
            "app",
            &uuid::Uuid::new_v4().to_string(),
            &serde_json::to_value(&binding).unwrap(),
        );
        original.physical_domain = Some(serde_json::to_value(&original_domain).unwrap());
        discovery(&root, &original);
        let scope = ResidentScope::initialize(&previous_owner, original.clone()).unwrap();
        let old_root = scope.root().to_path_buf();
        let guardian_id = uuid::Uuid::new_v4().to_string();
        let guardian = old_root.join("guardians").join(&guardian_id);
        std::fs::create_dir_all(&guardian).unwrap();
        std::fs::write(guardian.join("receipt.json"), serde_json::to_vec(&serde_json::json!({
            "version":2,"id":guardian_id,"instance_id":original.owner_instance,"phase":"Running",
            "command_record":null,"command_digest":"fixture","root_status":null,"diagnostic_pid":null
        })).unwrap()).unwrap();
        drop(scope);
        drop(previous_owner);
        let successor = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let mut next = identity(
            "app",
            &uuid::Uuid::new_v4().to_string(),
            &serde_json::to_value(&binding).unwrap(),
        );
        next.physical_domain = Some(serde_json::to_value(&current_domain).unwrap());
        record::save(
            &root.join("supervisor.json"),
            &Discovery {
                version: crate::control::CONTROL_VERSION,
                instance: next.owner_instance.clone(),
                address: "127.0.0.1:1".into(),
                token: "fixture".into(),
                requests: vec![],
                snapshot: Snapshot {
                    version: 1,
                    binding: binding.clone(),
                    supervisor_id: next.owner_instance.clone(),
                    generation: None,
                    phase: Phase::Reconciling,
                    intent: Intent::Run,
                    operation_id: None,
                    error: None,
                    problem: None,
                },
            },
        )
        .unwrap();
        let generation_id = uuid::Uuid::new_v4().to_string();
        let work = root.join("work").join(&generation_id);
        std::fs::create_dir_all(&work).unwrap();
        record::save(
            &work.join("generation.json"),
            &Generation {
                version: 1,
                id: generation_id.clone(),
                supervisor: original.owner_instance.clone(),
                token: "fixture".into(),
                intent: Intent::Run,
                phase: GenerationPhase::Running,
                worker_pid: None,
                exit_code: None,
                error: None,
                physical_domain: Some(original_domain.clone()),
                process_epoch: Some("opaque-old-epoch".into()),
            },
        )
        .unwrap();
        let blocked = initialize(&root, &successor, next.clone())
            .await
            .err()
            .unwrap();
        assert!(format!("{blocked:#}").contains("platform exit evidence required"));
        let foreign = Retirement {
            binding: binding.clone(),
            generation: generation_id.clone(),
            supervisor_id: uuid::Uuid::new_v4().to_string(),
            domain: original_domain.clone(),
        };
        record::save(&work.join("physical-exit.json"), &foreign).unwrap();
        assert!(
            initialize(&root, &successor, next.clone()).await.is_err(),
            "foreign owner proof retired resident"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(guardian.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["phase"], "Running");
        let proof = Retirement {
            supervisor_id: original.owner_instance.clone(),
            ..foreign
        };
        record::save(&work.join("physical-exit.json"), &proof).unwrap();
        let new_scope = initialize(&root, &successor, next.clone()).await.unwrap();
        assert_eq!(new_scope.identity(), &next);
        let wrong_volume = PhysicalDomain {
            volume: "pvc:foreign".into(),
            ..current_domain.clone()
        };
        assert!(
            !crate::domain::has_confirmed_resident_exit(&root, &original, Some(&wrong_volume))
                .unwrap(),
            "retirement proof authorized another mounted volume"
        );
        let receipt: serde_json::Value =
            serde_json::from_slice(&std::fs::read(guardian.join("receipt.json")).unwrap()).unwrap();
        assert_eq!(receipt["phase"], "Quiescent");
        assert!(
            receipt["root_status"].is_null(),
            "platform exit fabricated a command success"
        );
        new_scope.close().unwrap();
        new_scope.record_quiescent().unwrap();
    }
}
