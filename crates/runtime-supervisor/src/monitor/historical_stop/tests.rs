use super::*;
use std::os::unix::process::ExitStatusExt;

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    work: PathBuf,
    guardian: PathBuf,
    command: PathBuf,
    owner: Option<Owner>,
    attempt: StopWorkAttempt,
    discovery: Discovery,
    original: Vec<u8>,
    retained: Vec<(PathBuf, Vec<u8>)>,
}

impl Fixture {
    fn new(malformed: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let owner = Owner::try_acquire(&root).unwrap().unwrap();
        let binding = control::Binding {
            component: "app-cli".into(),
            resource: root.clone(),
        };
        let supervisor = uuid::Uuid::new_v4().to_string();
        let generation = uuid::Uuid::new_v4().to_string();
        let work = record::work_root(&root, &generation).unwrap();
        let ledger = Generation {
            version: 1,
            id: generation.clone(),
            supervisor: supervisor.clone(),
            token: "original-generation-token".into(),
            intent: Intent::Stopped,
            phase: GenerationPhase::Quiescent,
            worker_pid: None,
            exit_code: Some(17),
            error: None,
            physical_domain: None,
            process_epoch: crate::epoch::current(),
        };
        record::save(&work.join("generation.json"), &ledger).unwrap();
        let command_record =
            process_utils::command_context::CommandRecord::prepare_in(work.join("commands"))
                .unwrap();
        let mut child = std::process::Command::new("sh")
            .args(["-c", "exit 17"])
            .spawn()
            .unwrap();
        command_record.running(Some(child.id())).unwrap();
        let observed = child.wait().unwrap();
        assert_eq!(observed.code(), Some(17));
        command_record.quiescent().unwrap();
        let command = command_record.path().unwrap().to_path_buf();
        let guardian = work
            .join("guardians")
            .join(uuid::Uuid::new_v4().to_string());
        record::save(
            &guardian.join("receipt.json"),
            &serde_json::json!({
                "version":2,"id":guardian.file_name().unwrap().to_str().unwrap(),
                "instance_id":generation,"phase":"Quiescent","command_record":command,
                "command_digest":"fixture","root_status":observed.into_raw(),
                "diagnostic_pid":child.id(),"resident_identity":null,
            }),
        )
        .unwrap();
        let mut discovery = inactive_discovery(binding.clone());
        discovery.version = 999;
        discovery.instance = supervisor.clone();
        discovery.snapshot.supervisor_id = supervisor.clone();
        discovery.snapshot.generation = Some(generation.clone());
        discovery.snapshot.intent = Intent::Run;
        discovery.snapshot.phase = Phase::Ready;
        let original = if malformed {
            b"{damaged original locator".to_vec()
        } else {
            serde_json::to_vec(&discovery).unwrap()
        };
        std::fs::write(root.join("supervisor.json"), &original).unwrap();
        let mut attempt = StopWorkAttempt::allocate(&root, &binding);
        attempt.mode = StopMode::OfflineChosen {
            supervisor_id: supervisor,
        };
        attempt.captured_generation = Some(generation.clone());
        attempt.request.expected_generation = Some(generation);
        let retained = [
            work.join("generation.json"),
            guardian.join("receipt.json"),
            command.clone(),
        ]
        .into_iter()
        .map(|path| {
            let bytes = std::fs::read(&path).unwrap();
            (path, bytes)
        })
        .collect();
        Self {
            _temporary: temporary,
            root,
            work,
            guardian,
            command,
            owner: Some(owner),
            attempt,
            discovery,
            original,
            retained,
        }
    }

    fn preserve_execution(&self) {
        for (path, bytes) in &self.retained {
            assert_eq!(std::fs::read(path).unwrap(), *bytes, "{}", path.display());
        }
    }
    fn preserve_locator(&self) {
        assert_eq!(
            std::fs::read(self.root.join("supervisor.json")).unwrap(),
            self.original
        );
    }
}

#[tokio::test]
async fn captured_stop_repairs_historical_locator_and_finishes_the_original_request() {
    for malformed in [false, true] {
        let mut fixture = Fixture::new(malformed);
        let before = fixture.attempt.clone();
        let owner = fixture.owner.take().unwrap();
        owner
            .validate_captured_stop_target(&fixture.attempt)
            .unwrap();
        owner
            .repair_captured_stop_discovery(&fixture.attempt)
            .unwrap();
        let projection: Discovery = record::read(&fixture.root.join("supervisor.json")).unwrap();
        assert_eq!(projection.instance, fixture.discovery.instance);
        assert_eq!(
            projection.snapshot.generation,
            fixture.attempt.captured_generation
        );
        assert_eq!(projection.snapshot.phase, Phase::Reconciling);
        assert!(
            projection.snapshot.operation_id.is_none(),
            "repair is not a fake Stop receipt"
        );
        assert!(
            Owner::try_acquire(&fixture.root).unwrap().is_none(),
            "original owner lease remains held"
        );
        drop(owner);
        let completed = crate::continue_stop_work(&mut fixture.attempt, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(completed.phase, Phase::Stopped);
        assert_eq!(completed.intent, Intent::Stopped);
        assert_eq!(completed.supervisor_id, fixture.discovery.instance);
        assert_eq!(completed.generation, before.captured_generation);
        assert_eq!(
            completed.operation_id.as_deref(),
            Some(before.request.request_id.as_str())
        );
        assert_eq!(fixture.attempt, before);
        fixture.preserve_execution();
        assert!(
            std::fs::read_dir(&fixture.root).unwrap().any(|entry| {
                let path = entry.unwrap().path();
                path.is_file()
                    && std::fs::read(path).ok().as_deref() == Some(fixture.original.as_slice())
            }),
            "damaged locator original bytes are retained"
        );
        let repeated = crate::continue_stop_work(&mut fixture.attempt, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(repeated.operation_id, completed.operation_id);
        assert_eq!(fixture.attempt, before);
        fixture.preserve_execution();
    }
}

#[tokio::test]
async fn captured_offline_stop_recovers_already_settled_locator_without_caller_retargeting() {
    let mut fixture = Fixture::new(true);
    let before = fixture.attempt.clone();
    drop(fixture.owner.take());
    let completed = crate::continue_stop_work(&mut fixture.attempt, Duration::from_secs(2))
        .await
        .unwrap();
    assert_eq!(completed.phase, Phase::Stopped);
    assert_eq!(
        completed.operation_id.as_deref(),
        Some(before.request.request_id.as_str())
    );
    assert_eq!(fixture.attempt, before);
    fixture.preserve_execution();
}

#[test]
fn captured_stop_never_repairs_a_changed_target_or_foreign_binding() {
    for changed in [
        "owner",
        "generation",
        "binding",
        "ledger-owner",
        "missing-ledger",
        "request",
    ] {
        let mut fixture = Fixture::new(false);
        match changed {
            "owner" => {
                fixture.discovery.instance = uuid::Uuid::new_v4().to_string();
                fixture.discovery.snapshot.supervisor_id = fixture.discovery.instance.clone();
            }
            "generation" => {
                fixture.discovery.snapshot.generation = Some(uuid::Uuid::new_v4().to_string())
            }
            "binding" => fixture.discovery.snapshot.binding.resource = fixture.root.join("foreign"),
            "ledger-owner" => {
                let mut value = record::generation(&fixture.work).unwrap();
                value.supervisor = uuid::Uuid::new_v4().to_string();
                record::save(&fixture.work.join("generation.json"), &value).unwrap();
            }
            "missing-ledger" => std::fs::remove_file(fixture.work.join("generation.json")).unwrap(),
            "request" => {
                fixture.attempt.request.expected_generation = Some(uuid::Uuid::new_v4().to_string())
            }
            _ => unreachable!(),
        }
        fixture.original = serde_json::to_vec(&fixture.discovery).unwrap();
        std::fs::write(fixture.root.join("supervisor.json"), &fixture.original).unwrap();
        let owner = fixture.owner.as_ref().unwrap();
        assert!(
            owner
                .validate_captured_stop_target(&fixture.attempt)
                .is_err(),
            "{changed}"
        );
        assert!(
            owner
                .repair_captured_stop_discovery(&fixture.attempt)
                .is_err(),
            "{changed}"
        );
        fixture.preserve_locator();
    }
}

#[test]
fn captured_stop_keeps_real_locks_and_unknown_command_outcomes_protected() {
    for protected in [
        "generation-lock",
        "cleanup-lock",
        "guardian-lock",
        "pending-command",
        "unknown-guardian",
        "different-active-generation",
    ] {
        let fixture = Fixture::new(true);
        let retained_lock = match protected {
            "generation-lock" => Some(record::lock(&fixture.work.join("generation.lock")).unwrap()),
            "cleanup-lock" => {
                Some(record::lock(&fixture.work.join("external-cleanup.lock")).unwrap())
            }
            "guardian-lock" => Some(record::lock(&fixture.guardian.join("guardian.lock")).unwrap()),
            "pending-command" => {
                let mut value: serde_json::Value = record::read(&fixture.command).unwrap();
                value["phase"] = "SpawnPending".into();
                record::save(&fixture.command, &value).unwrap();
                None
            }
            "unknown-guardian" => {
                let mut value: serde_json::Value =
                    record::read(&fixture.guardian.join("receipt.json")).unwrap();
                value["version"] = 999.into();
                record::save(&fixture.guardian.join("receipt.json"), &value).unwrap();
                None
            }
            "different-active-generation" => {
                let mut value = record::generation(&fixture.work).unwrap();
                value.id = uuid::Uuid::new_v4().to_string();
                value.supervisor = uuid::Uuid::new_v4().to_string();
                value.phase = GenerationPhase::Running;
                record::save(
                    &record::work_root(&fixture.root, &value.id)
                        .unwrap()
                        .join("generation.json"),
                    &value,
                )
                .unwrap();
                None
            }
            _ => unreachable!(),
        };
        assert!(
            fixture
                .owner
                .as_ref()
                .unwrap()
                .repair_captured_stop_discovery(&fixture.attempt)
                .is_err(),
            "{protected}"
        );
        if protected == "different-active-generation" {
            assert!(
                fixture
                    .owner
                    .as_ref()
                    .unwrap()
                    .validate_captured_stop_target(&fixture.attempt)
                    .is_err()
            );
        }
        fixture.preserve_locator();
        drop(retained_lock);
    }
}
