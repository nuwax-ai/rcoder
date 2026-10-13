//! Historical application metadata never replaces workspace/execution checks.
use super::{initialize, reconcile_prior_until};
use crate::domain::PhysicalDomain;
use process_utils::command_authority::{Gate, OwnerLease, ResidentIdentity, ResidentScope};
use std::{fs::File, path::PathBuf, time::Duration};

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    previous: PathBuf,
    next: ResidentIdentity,
    owner: Option<OwnerLease>,
    guardians: Vec<PathBuf>,
    commands: Vec<PathBuf>,
    original: Vec<(PathBuf, Vec<u8>)>,
}

impl Fixture {
    fn new(native: bool, historical_application: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let binding = serde_json::json!({"component":"app-cli","resource":root});
        let domain = PhysicalDomain {
            authority: "k8s:cluster-one:namespace-one".into(),
            instance_source_env: Some("RCODER_PHYSICAL_POD_UID".into()),
            instance: "old-pod".into(),
            volume: "retained-workspace-pvc".into(),
        };
        let owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let old = ResidentIdentity {
            application_id: historical_application.into(),
            binding: binding.clone(),
            owner_instance: uuid::Uuid::new_v4().to_string(),
            physical_domain: (!native).then(|| serde_json::to_value(&domain).unwrap()),
            process_epoch: Some("historical-process-space".into()),
        };
        Self::discovery(&root, &old);
        let scope = ResidentScope::initialize(&owner, old.clone()).unwrap();
        let previous = scope.root().to_path_buf();
        let mut guardians = Vec::new();
        let mut commands = Vec::new();
        for _ in 0..2 {
            let record = process_utils::command_context::CommandRecord::prepare_in(
                previous.join("commands"),
            )
            .unwrap();
            let mut command = std::process::Command::new("sh")
                .args(["-c", "exit 17"])
                .spawn()
                .unwrap();
            record.running(Some(command.id())).unwrap();
            assert_eq!(command.wait().unwrap().code(), Some(17));
            record.quiescent().unwrap();
            let path = record.path().unwrap().to_path_buf();
            let guardian = previous
                .join("guardians")
                .join(uuid::Uuid::new_v4().to_string());
            crate::record::save(
                &guardian.join("receipt.json"),
                &serde_json::json!({
                    "version":2, "id":guardian.file_name().unwrap().to_str().unwrap(),
                    "instance_id":old.owner_instance,"phase":"Quiescent",
                    "command_record":path,"command_digest":"protocol-fixture",
                    "root_status":null,"diagnostic_pid":command.id(),"resident_identity":old,
                }),
            )
            .unwrap();
            guardians.push(guardian);
            commands.push(path);
        }
        // A crash may leave the scope's informational phase Running even
        // though every original command/guardian has already settled.
        let header: serde_json::Value =
            crate::record::read(&previous.join("resident-scope.json")).unwrap();
        assert_eq!(header["phase"], "Running");
        drop(scope);
        drop(owner);
        let owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let next = ResidentIdentity {
            application_id: "81".into(),
            binding,
            owner_instance: uuid::Uuid::new_v4().to_string(),
            physical_domain: (!native).then(|| {
                serde_json::to_value(PhysicalDomain {
                    instance: "current-pod".into(),
                    ..domain
                })
                .unwrap()
            }),
            process_epoch: crate::epoch::current(),
        };
        Self::discovery(&root, &next);
        let original = std::iter::once(previous.join("resident-scope.json"))
            .chain(guardians.iter().map(|path| path.join("receipt.json")))
            .chain(commands.iter().cloned())
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect();
        Self {
            _temporary: temporary,
            root,
            previous,
            next,
            owner: Some(owner),
            guardians,
            commands,
            original,
        }
    }

    fn discovery(root: &std::path::Path, identity: &ResidentIdentity) {
        crate::record::save(
            &root.join("supervisor.json"),
            &crate::control::Discovery {
                version: crate::control::CONTROL_VERSION,
                instance: identity.owner_instance.clone(),
                address: "127.0.0.1:1".into(),
                token: "protocol-fixture".into(),
                requests: Vec::new(),
                snapshot: crate::control::Snapshot {
                    version: 1,
                    binding: serde_json::from_value(identity.binding.clone()).unwrap(),
                    supervisor_id: identity.owner_instance.clone(),
                    generation: None,
                    phase: crate::Phase::Reconciling,
                    intent: crate::Intent::Stopped,
                    operation_id: None,
                    error: None,
                    problem: None,
                },
            },
        )
        .unwrap();
    }

    fn preserve(&self) {
        for (path, bytes) in &self.original {
            assert_eq!(std::fs::read(path).unwrap(), *bytes, "{}", path.display());
        }
    }

    async fn reconcile(&self) -> anyhow::Result<()> {
        reconcile_prior_until(
            &self.root,
            &self.next,
            None,
            tokio::time::Instant::now() + Duration::from_millis(30),
        )
        .await
    }
}

#[tokio::test]
async fn historical_application_metadata_allows_repeated_successors_preserving_history() {
    for historical_application in ["unknown-app", "previous-named-app"] {
        let mut fixture = Fixture::new(false, historical_application);
        for _ in 0..2 {
            let successor = initialize(
                &fixture.root,
                fixture.owner.as_ref().unwrap(),
                fixture.next.clone(),
            )
            .await
            .unwrap_or_else(|error| {
                panic!("same retained volume with settled legacy history must recover: {error:#}")
            });
            fixture.preserve();
            assert!(
                Gate::try_acquire(&fixture.previous)
                    .unwrap()
                    .require_open()
                    .is_err()
            );
            assert!(OwnerLease::try_acquire(&fixture.root).unwrap().is_none());
            successor.close().unwrap();
            successor.record_quiescent().unwrap();
            drop(successor);
            fixture.next.owner_instance = uuid::Uuid::new_v4().to_string();
            Fixture::discovery(&fixture.root, &fixture.next);
        }
        assert!(
            !fixture.root.join("work").exists(),
            "settled history must not manufacture physical exit records"
        );
    }
}

#[tokio::test]
async fn historical_native_metadata_preserves_bytes_under_the_real_owner_lease() {
    let fixture = Fixture::new(true, "previous-native-app");
    let successor = initialize(
        &fixture.root,
        fixture.owner.as_ref().unwrap(),
        fixture.next.clone(),
    )
    .await
    .unwrap();
    fixture.preserve();
    assert!(OwnerLease::try_acquire(&fixture.root).unwrap().is_none());
    successor.close().unwrap();
    successor.record_quiescent().unwrap();
}

#[tokio::test]
async fn historical_resident_header_diagnostics_do_not_block_current_workspace_recovery() {
    let fixture = Fixture::new(false, "previous-named-app");
    let path = fixture.previous.join("resident-scope.json");
    let mut history: serde_json::Value = crate::record::read(&path).unwrap();
    history["version"] = 999.into();
    history["phase"] = "historical-diagnostic-phase".into();
    history["diagnostic"] = serde_json::json!({"older_release":"fixture"});
    let bytes = serde_json::to_vec(&history).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    let guardian_bytes: Vec<_> = fixture
        .guardians
        .iter()
        .map(|root| {
            (
                root.join("receipt.json"),
                std::fs::read(root.join("receipt.json")).unwrap(),
            )
        })
        .collect();
    let command_bytes: Vec<_> = fixture
        .commands
        .iter()
        .map(|path| (path.clone(), std::fs::read(path).unwrap()))
        .collect();
    let successor = initialize(
        &fixture.root,
        fixture.owner.as_ref().unwrap(),
        fixture.next.clone(),
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    for (path, before) in guardian_bytes.into_iter().chain(command_bytes) {
        assert_eq!(std::fs::read(path).unwrap(), before);
    }
    assert!(
        Gate::try_acquire(&fixture.previous)
            .unwrap()
            .require_open()
            .is_err()
    );
    successor.close().unwrap();
    successor.record_quiescent().unwrap();
}

#[tokio::test]
async fn historical_application_metadata_keeps_real_guardian_lock_and_pending_outcomes_protected() {
    for scenario in [
        "held",
        "guardian-pending",
        "guardian-running",
        "command-pending",
        "command-running",
        "guardian-schema",
        "command-schema",
    ] {
        let fixture = Fixture::new(false, "previous-named-app");
        let file = fixture.guardians[0].join("receipt.json");
        let mut held = None;
        match scenario {
            "held" => {
                let lock = File::options()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(fixture.guardians[0].join("guardian.lock"))
                    .unwrap();
                lock.try_lock().unwrap();
                held = Some(lock);
            }
            "guardian-pending" | "guardian-running" | "guardian-schema" => {
                let mut value: serde_json::Value = crate::record::read(&file).unwrap();
                value["phase"] = if scenario == "guardian-pending" {
                    "Pending"
                } else if scenario == "guardian-running" {
                    "Running"
                } else {
                    "Unknown"
                }
                .into();
                std::fs::write(&file, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => {
                let path = &fixture.commands[0];
                let mut value: serde_json::Value = crate::record::read(path).unwrap();
                value["phase"] = if scenario == "command-pending" {
                    "SpawnPending"
                } else if scenario == "command-running" {
                    "Running"
                } else {
                    "Unknown"
                }
                .into();
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        let before = std::fs::read(&file).unwrap();
        let command_before = std::fs::read(&fixture.commands[0]).unwrap();
        let error = fixture.reconcile().await.unwrap_err();
        assert!(
            !format!("{error:#}").contains("resident belongs to another application"),
            "{scenario}: history metadata cannot hide the real blocker: {error:#}"
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
        assert_eq!(std::fs::read(&fixture.commands[0]).unwrap(), command_before);
        assert!(
            Gate::try_acquire(&fixture.previous)
                .unwrap()
                .require_open()
                .is_err()
        );
        if let Some(lock) = held {
            drop(lock);
            fixture.reconcile().await.unwrap();
            fixture.preserve();
        }
    }
}

#[tokio::test]
async fn historical_application_metadata_cannot_adopt_foreign_binding_or_physical_domain() {
    for scenario in [
        "binding",
        "authority",
        "volume",
        "one-none",
        "domain-schema",
        "resident-instance",
    ] {
        let mut fixture = Fixture::new(false, "previous-named-app");
        match scenario {
            "resident-instance" => {
                let path = fixture.previous.join("resident-scope.json");
                let mut value: serde_json::Value = crate::record::read(&path).unwrap();
                value["identity"]["owner_instance"] = uuid::Uuid::new_v4().to_string().into();
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "binding" => fixture.next.binding["resource"] = "/other/workspace".into(),
            "authority" => {
                fixture.next.physical_domain.as_mut().unwrap()["authority"] =
                    "other-cluster:namespace".into()
            }
            "volume" => {
                fixture.next.physical_domain.as_mut().unwrap()["volume"] = "other-pvc".into()
            }
            "one-none" => fixture.next.physical_domain = None,
            "domain-schema" => {
                fixture.next.physical_domain.as_mut().unwrap()["extra"] = "unknown".into()
            }
            _ => unreachable!(),
        }
        Fixture::discovery(&fixture.root, &fixture.next);
        let path = fixture.previous.join("resident-scope.json");
        let before = std::fs::read(&path).unwrap();
        assert!(
            fixture.reconcile().await.is_err(),
            "foreign {scenario} must remain protected"
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
        Gate::try_acquire(&fixture.previous)
            .unwrap()
            .require_open()
            .unwrap();
    }
}
