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
    let directory = root.join("resident");
    if directory.try_exists()? {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
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
                process_utils::guardian::recover(&previous)
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
            loop {
                let replacement = old
                    .process_epoch
                    .as_deref()
                    .zip(identity.process_epoch.as_deref())
                    .is_some_and(|(before, now)| crate::epoch::proves_replacement(before, now));
                let result = if platform_exit || replacement {
                    // Same platform domain plus a replaced boot/PID namespace
                    // proves no process of that incarnation can survive.
                    process_utils::guardian::confirm_physical_domain_exit(&previous)
                } else {
                    process_utils::guardian::recover(&previous)
                };
                match result {
                    Ok(()) => break,
                    Err(error)
                        if error
                            .downcast_ref::<std::fs::TryLockError>()
                            .is_some_and(|e| matches!(e, std::fs::TryLockError::WouldBlock))
                            && tokio::time::Instant::now() < deadline =>
                    {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(error) => {
                        return Err(error)
                            .context("prior resident guardian cleanup remains unconfirmed");
                    }
                }
            }
        }
    }
    Ok(())
}

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
