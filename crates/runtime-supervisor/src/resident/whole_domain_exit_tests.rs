//! A terminated container covers every retained owner in its exact domain.
use super::{initialize, reconcile_prior_until};
use crate::{
    Binding, Intent, Phase,
    control::{Discovery, Snapshot},
    domain::{PhysicalDomain, Retirement},
    record::{self, Generation, GenerationPhase},
};
use process_utils::command_authority::{OwnerLease, ResidentIdentity, ResidentScope};
use std::{path::PathBuf, time::Duration};

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    binding: Binding,
    old_domain: PhysicalDomain,
    current_domain: PhysicalDomain,
    originals: Vec<ResidentIdentity>,
    guardians: Vec<PathBuf>,
    resident_bytes: Vec<Vec<u8>>,
    work: PathBuf,
    generation_bytes: Vec<u8>,
    proof: Retirement,
    lease: OwnerLease,
    next: ResidentIdentity,
}

impl Fixture {
    fn discovery(&self, identity: &ResidentIdentity) {
        record::save(
            &self.root.join("supervisor.json"),
            &Discovery {
                version: crate::control::CONTROL_VERSION,
                instance: identity.owner_instance.clone(),
                address: "127.0.0.1:1".into(),
                token: "protocol-fixture".into(),
                requests: vec![],
                snapshot: Snapshot {
                    version: 1,
                    binding: serde_json::from_value(identity.binding.clone()).unwrap(),
                    supervisor_id: identity.owner_instance.clone(),
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
    }

    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let binding = Binding {
            component: "app-cli".into(),
            resource: root.clone(),
        };
        let old_domain = PhysicalDomain {
            authority: "docker:actual-daemon-fixture".into(),
            instance_source_env: None,
            instance: "physically-exited-container".into(),
            volume: "retained-workspace-volume".into(),
        };
        let current_domain = PhysicalDomain {
            instance: "replacement-container".into(),
            ..old_domain.clone()
        };
        let mut originals = Vec::new();
        let mut guardians = Vec::new();
        let mut resident_bytes = Vec::new();
        // Two successive OS owner leases used the same now-terminated domain.
        // Their retained owner/guardian records remain honestly Running.
        for _ in 0..2 {
            let lease = OwnerLease::try_acquire(&root).unwrap().unwrap();
            let identity = ResidentIdentity {
                application_id: "app".into(),
                binding: serde_json::to_value(&binding).unwrap(),
                owner_instance: uuid::Uuid::new_v4().to_string(),
                physical_domain: Some(serde_json::to_value(&old_domain).unwrap()),
                process_epoch: Some("retired-process-space".into()),
            };
            record::save(
                &root.join("supervisor.json"),
                &serde_json::json!({
                    "instance": identity.owner_instance,
                    "snapshot": {"supervisor_id": identity.owner_instance, "binding": identity.binding}
                }),
            )
            .unwrap();
            let resident = ResidentScope::initialize(&lease, identity.clone()).unwrap();
            let guardian_id = uuid::Uuid::new_v4().to_string();
            let guardian = resident.root().join("guardians").join(&guardian_id);
            record::save(
                &guardian.join("receipt.json"),
                &serde_json::json!({
                    "version": 2, "id": guardian_id,
                    "instance_id": identity.owner_instance, "phase": "Running",
                    "command_record": null, "command_digest": "protocol-fixture",
                    "root_status": null, "diagnostic_pid": null,
                    "resident_identity": identity
                }),
            )
            .unwrap();
            resident_bytes
                .push(std::fs::read(resident.root().join("resident-scope.json")).unwrap());
            guardians.push(guardian);
            originals.push(identity);
            drop(resident);
            drop(lease);
        }
        let latest = originals.last().unwrap();
        let generation_id = uuid::Uuid::new_v4().to_string();
        let work = root.join("work").join(&generation_id);
        record::save(
            &work.join("generation.json"),
            &Generation {
                version: 1,
                id: generation_id.clone(),
                supervisor: latest.owner_instance.clone(),
                token: "protocol-fixture".into(),
                intent: Intent::Run,
                phase: GenerationPhase::Running,
                worker_pid: None,
                exit_code: None,
                error: None,
                physical_domain: Some(old_domain.clone()),
                process_epoch: Some("retired-process-space".into()),
            },
        )
        .unwrap();
        let proof = Retirement {
            binding: binding.clone(),
            generation: generation_id,
            supervisor_id: latest.owner_instance.clone(),
            domain: old_domain.clone(),
        };
        record::save(&work.join("physical-exit.json"), &proof).unwrap();
        let generation_bytes = std::fs::read(work.join("generation.json")).unwrap();
        let lease = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let next = ResidentIdentity {
            application_id: "app".into(),
            binding: serde_json::to_value(&binding).unwrap(),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            physical_domain: Some(serde_json::to_value(&current_domain).unwrap()),
            process_epoch: Some("replacement-process-space".into()),
        };
        let fixture = Self {
            _directory: directory,
            root,
            binding,
            old_domain,
            current_domain,
            originals,
            guardians,
            resident_bytes,
            work,
            generation_bytes,
            proof,
            lease,
            next,
        };
        fixture.discovery(&fixture.next);
        fixture
    }

    fn assert_guardians_running(&self) {
        for guardian in &self.guardians {
            let value: serde_json::Value = record::read(&guardian.join("receipt.json")).unwrap();
            assert_eq!(value["phase"], "Running");
            assert!(value["root_status"].is_null());
        }
    }

    fn assert_original_identity_bytes_preserved(&self) {
        for (identity, before) in self.originals.iter().zip(&self.resident_bytes) {
            let file = self
                .root
                .join("resident")
                .join(&identity.owner_instance)
                .join("resident-scope.json");
            assert_eq!(std::fs::read(file).unwrap(), *before);
        }
        assert_eq!(
            std::fs::read(self.work.join("generation.json")).unwrap(),
            self.generation_bytes
        );
    }
}

#[tokio::test]
async fn whole_physical_exit_covers_older_resident_owned_by_previous_supervisor() {
    let fixture = Fixture::new();
    assert_ne!(
        fixture.originals[0].owner_instance,
        fixture.proof.supervisor_id
    );
    let proof_bytes = std::fs::read(fixture.work.join("physical-exit.json")).unwrap();
    let result = initialize(&fixture.root, &fixture.lease, fixture.next.clone()).await;
    assert!(
        result.is_ok(),
        "latest valid physical exit must also retire earlier same-domain owner: {:#}",
        result.err().unwrap()
    );
    let successor = result.unwrap();
    assert_eq!(successor.identity(), &fixture.next);
    for guardian in &fixture.guardians {
        let receipt: serde_json::Value = record::read(&guardian.join("receipt.json")).unwrap();
        assert_eq!(receipt["phase"], "Quiescent");
        assert!(
            receipt["root_status"].is_null(),
            "physical retirement must not invent business success"
        );
    }
    fixture.assert_original_identity_bytes_preserved();
    assert_eq!(
        std::fs::read(fixture.work.join("physical-exit.json")).unwrap(),
        proof_bytes
    );
    assert!(OwnerLease::try_acquire(&fixture.root).unwrap().is_none());
    successor.close().unwrap();
    successor.record_quiescent().unwrap();
}

#[tokio::test]
async fn whole_domain_retirement_allows_application_metadata_and_protects_workspace() {
    for foreign_application in [true, false] {
        let fixture = Fixture::new();
        let mut next = fixture.next.clone();
        if foreign_application {
            next.application_id = "another-app".into();
        } else {
            next.binding = serde_json::to_value(Binding {
                resource: fixture.root.join("another-workspace"),
                ..fixture.binding.clone()
            })
            .unwrap();
        }
        fixture.discovery(&next);
        let result = initialize(&fixture.root, &fixture.lease, next.clone()).await;
        if foreign_application {
            let successor = result.unwrap();
            assert_eq!(successor.identity(), &next);
            for guardian in &fixture.guardians {
                let receipt: serde_json::Value =
                    record::read(&guardian.join("receipt.json")).unwrap();
                assert_eq!(receipt["phase"], "Quiescent");
                assert!(
                    receipt["root_status"].is_null(),
                    "retirement cannot invent business success"
                );
            }
            assert!(OwnerLease::try_acquire(&fixture.root).unwrap().is_none());
            successor.close().unwrap();
            successor.record_quiescent().unwrap();
        } else {
            assert!(result.is_err());
            fixture.assert_guardians_running();
        }
        fixture.assert_original_identity_bytes_preserved();
    }
}

#[test]
fn whole_domain_proof_cannot_authorize_foreign_authority_volume_or_same_instance() {
    let fixture = Fixture::new();
    for current in [
        PhysicalDomain {
            authority: "another-cluster".into(),
            ..fixture.current_domain.clone()
        },
        PhysicalDomain {
            volume: "another-workspace-volume".into(),
            ..fixture.current_domain.clone()
        },
        fixture.old_domain.clone(),
    ] {
        assert!(
            !crate::domain::has_confirmed_resident_exit(
                &fixture.root,
                &fixture.originals[0],
                Some(&current)
            )
            .unwrap()
        );
    }
    assert!(
        !crate::domain::has_confirmed_resident_exit(&fixture.root, &fixture.originals[0], None)
            .unwrap()
    );
    fixture.assert_guardians_running();
}

#[test]
fn whole_domain_proof_still_validates_its_own_generation_supervisor_binding_and_domain() {
    for changed in ["generation", "supervisor", "binding", "domain", "malformed"] {
        let fixture = Fixture::new();
        let mut proof = fixture.proof.clone();
        match changed {
            "generation" => proof.generation = uuid::Uuid::new_v4().to_string(),
            "supervisor" => proof.supervisor_id = uuid::Uuid::new_v4().to_string(),
            "binding" => proof.binding.resource = fixture.root.join("another-workspace"),
            "domain" => proof.domain.instance = "another-exited-instance".into(),
            _ => {}
        }
        if changed == "malformed" {
            std::fs::write(fixture.work.join("physical-exit.json"), b"{malformed").unwrap();
        } else {
            record::save(&fixture.work.join("physical-exit.json"), &proof).unwrap();
        }
        assert!(
            crate::domain::has_confirmed_resident_exit(
                &fixture.root,
                &fixture.originals[0],
                Some(&fixture.current_domain)
            )
            .is_err(),
            "invalid proof variant {changed} was ignored or accepted"
        );
        fixture.assert_guardians_running();
        fixture.assert_original_identity_bytes_preserved();
    }
}

#[tokio::test]
async fn whole_domain_receipt_does_not_override_an_actual_guardian_lock() {
    let fixture = Fixture::new();
    let lock = std::fs::File::options()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(fixture.guardians[0].join("guardian.lock"))
        .unwrap();
    lock.try_lock().unwrap();
    let result = reconcile_prior_until(
        &fixture.root,
        &fixture.next,
        None,
        tokio::time::Instant::now() + Duration::from_millis(50),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(
        record::read::<serde_json::Value>(&fixture.guardians[0].join("receipt.json")).unwrap()["phase"],
        "Running"
    );
    assert!(OwnerLease::try_acquire(&fixture.root).unwrap().is_none());
}
