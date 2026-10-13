use super::*;
use crate::command_authority::{Gate, OwnerLease, ResidentIdentity};

struct Fixture {
    _temporary: tempfile::TempDir,
    _owner: OwnerLease,
    work: PathBuf,
    identity: ResidentIdentity,
    guardians: Vec<PathBuf>,
    commands: Vec<PathBuf>,
}

impl Fixture {
    fn new(header: &str) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let owner = OwnerLease::try_acquire(&root).unwrap().unwrap();
        let work = root.join("resident").join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&work).unwrap();
        let identity = ResidentIdentity {
            application_id: "unknown-app".into(),
            binding: serde_json::json!({"component":"app-cli","resource":"/home/user/81/code"}),
            owner_instance: work.file_name().unwrap().to_str().unwrap().into(),
            physical_domain: Some(
                serde_json::json!({"authority":"owned-platform","volume":"retained-pvc","instance":"old-pod"}),
            ),
            process_epoch: Some("old-process-space".into()),
        };
        let mut value =
            serde_json::json!({"version":1,"identity":identity,"phase":"Running","error":null});
        match header {
            "version" => value["version"] = 999.into(),
            "phase" => value["phase"] = "FutureHistorical".into(),
            "extra" => value["historical_note"] = serde_json::json!({"keep":true}),
            _ => {}
        }
        std::fs::write(
            work.join("resident-scope.json"),
            serde_json::to_vec_pretty(&value).unwrap(),
        )
        .unwrap();
        Gate::try_acquire(&work).unwrap().close().unwrap();
        let mut guardians = Vec::new();
        let mut commands = Vec::new();
        for _ in 0..2 {
            let command =
                crate::command_context::CommandRecord::prepare_in(work.join("commands")).unwrap();
            let mut child = std::process::Command::new("sh")
                .args(["-c", "exit 17"])
                .spawn()
                .unwrap();
            command.running(Some(child.id())).unwrap();
            assert_eq!(child.wait().unwrap().code(), Some(17));
            command.quiescent().unwrap();
            let command_path = command.path().unwrap().to_owned();
            let guardian = work
                .join("guardians")
                .join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(&guardian).unwrap();
            save(
                &guardian,
                &Receipt {
                    version: 2,
                    id: guardian.file_name().unwrap().to_str().unwrap().into(),
                    instance_id: identity.owner_instance.clone(),
                    phase: "Quiescent".into(),
                    command_record: Some(command_path.clone()),
                    command_digest: "protocol-fixture".into(),
                    root_status: None,
                    diagnostic_pid: Some(child.id()),
                    resident_identity: Some(identity.clone()),
                },
            )
            .unwrap();
            guardians.push(guardian);
            commands.push(command_path);
        }
        Self {
            _temporary: temporary,
            _owner: owner,
            work,
            identity,
            guardians,
            commands,
        }
    }

    fn bytes(&self) -> Vec<(PathBuf, Vec<u8>)> {
        std::iter::once(self.work.join("resident-scope.json"))
            .chain(self.guardians.iter().map(|root| root.join("receipt.json")))
            .chain(self.commands.iter().cloned())
            .map(|path| {
                let bytes = std::fs::read(&path).unwrap();
                (path, bytes)
            })
            .collect()
    }
}

#[test]
fn historical_header_changes_observe_two_actual_settled_commands_without_rewriting_bytes() {
    for header in ["version", "phase", "extra"] {
        let fixture = Fixture::new(header);
        let original = fixture.bytes();
        assert!(
            is_scope_settled(&fixture.work).is_err(),
            "old strict admission counterexample: {header}"
        );
        for _ in 0..2 {
            assert!(
                is_historical_resident_scope_settled(&fixture.work, &fixture.identity).unwrap()
            );
            assert_eq!(fixture.bytes(), original);
            assert!(
                OwnerLease::try_acquire(fixture._owner.root())
                    .unwrap()
                    .is_none()
            );
        }
    }
}

#[test]
fn historical_observation_retains_actual_locks_and_unknown_or_pending_outcomes() {
    for scenario in [
        "held",
        "guardian-pending",
        "guardian-running",
        "guardian-schema",
        "guardian-version",
        "guardian-extra",
        "command-pending",
        "command-running",
        "command-schema",
        "command-version",
        "command-malformed",
    ] {
        let fixture = Fixture::new("version");
        let mut held = None;
        let receipt_path = fixture.guardians[0].join("receipt.json");
        match scenario {
            "held" => held = Some(lock(&fixture.guardians[0]).unwrap()),
            "guardian-pending" | "guardian-running" | "guardian-schema" | "guardian-version"
            | "guardian-extra" => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&receipt_path).unwrap()).unwrap();
                match scenario {
                    "guardian-version" => value["version"] = 999.into(),
                    "guardian-extra" => value["unknown_outcome_schema"] = true.into(),
                    _ => {
                        value["phase"] = match scenario {
                            "guardian-pending" => "Pending",
                            "guardian-running" => "Running",
                            _ => "Unknown",
                        }
                        .into()
                    }
                }
                std::fs::write(&receipt_path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            "command-malformed" => {
                std::fs::write(&fixture.commands[0], b"{incomplete actual command receipt")
                    .unwrap();
            }
            _ => {
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&fixture.commands[0]).unwrap()).unwrap();
                if scenario == "command-version" {
                    value["version"] = 999.into();
                } else {
                    value["phase"] = match scenario {
                        "command-pending" => "SpawnPending",
                        "command-running" => "Running",
                        _ => "Unknown",
                    }
                    .into();
                }
                std::fs::write(&fixture.commands[0], serde_json::to_vec(&value).unwrap()).unwrap();
            }
        }
        let original = fixture.bytes();
        assert!(
            !is_historical_resident_scope_settled(&fixture.work, &fixture.identity)
                .unwrap_or(false),
            "{scenario}"
        );
        assert_eq!(fixture.bytes(), original);
        drop(held);
        if scenario == "held" {
            assert!(
                is_historical_resident_scope_settled(&fixture.work, &fixture.identity).unwrap()
            );
        }
    }
}

#[test]
fn historical_observation_requires_original_full_identity_and_instance() {
    for scenario in [
        "captured-binding",
        "captured-domain",
        "captured-owner",
        "guardian-binding",
        "guardian-domain",
        "guardian-instance",
        "header-missing-binding",
    ] {
        let mut fixture = Fixture::new("phase");
        match scenario {
            "captured-binding" => fixture.identity.binding["resource"] = "/foreign/source".into(),
            "captured-domain" => {
                fixture.identity.physical_domain.as_mut().unwrap()["volume"] = "foreign-pvc".into()
            }
            "captured-owner" => fixture.identity.owner_instance = uuid::Uuid::new_v4().to_string(),
            "header-missing-binding" => {
                let path = fixture.work.join("resident-scope.json");
                let mut value: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                value["identity"].as_object_mut().unwrap().remove("binding");
                std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
            }
            _ => {
                let mut receipt = read(&fixture.guardians[0]).unwrap();
                match scenario {
                    "guardian-binding" => {
                        receipt.resident_identity.as_mut().unwrap().binding["resource"] =
                            "/foreign/source".into()
                    }
                    "guardian-domain" => {
                        receipt
                            .resident_identity
                            .as_mut()
                            .unwrap()
                            .physical_domain
                            .as_mut()
                            .unwrap()["authority"] = "foreign-platform".into()
                    }
                    _ => receipt.instance_id = uuid::Uuid::new_v4().to_string(),
                }
                save(&fixture.guardians[0], &receipt).unwrap();
            }
        }
        let original = fixture.bytes();
        assert!(
            is_historical_resident_scope_settled(&fixture.work, &fixture.identity).is_err(),
            "{scenario}"
        );
        assert_eq!(fixture.bytes(), original);
    }
}
