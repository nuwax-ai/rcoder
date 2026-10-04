//! Repair the historical native-session/deployment-generation mix-up only
//! when the original platform declaration and the retired owner prove it.
use super::super::deploy_replay;
use super::*;

const OPERATION_RECORD: &str = ".deploy-operation.json";

#[derive(Serialize, Deserialize)]
struct LegacyOwnerEvidence {
    version: u8,
    generation: String,
    operation_id: String,
    receipt_digest: String,
    coordinator_bytes: Vec<u8>,
}

fn receipt_digest(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    hex::encode(sha2::Sha256::digest(bytes))
}

impl Journal {
    fn legacy_owner_evidence_path(&self, generation: &str, digest: &str) -> Result<PathBuf> {
        let generation = uuid::Uuid::parse_str(generation)?;
        Ok(self.root.join(format!(
            ".deploy-generation-owner-{generation}-{digest}.json"
        )))
    }

    pub(super) fn preserve_legacy_generation_owner(&self) -> Result<()> {
        let (Some(receipt), Some(owner)) = (&self.receipt, &self.previous_owner) else {
            return Ok(());
        };
        if receipt.operation.deployment_generation_id != receipt.generation
            || owner.worker_generation.as_deref() != Some(receipt.generation.as_str())
            || uuid::Uuid::parse_str(&receipt.generation).is_err()
        {
            return Ok(());
        }
        let receipt_bytes = std::fs::read(self.root.join(OPERATION_RECORD))?;
        let stored: StoredReceipt = serde_json::from_slice(&receipt_bytes)?;
        anyhow::ensure!(
            serde_json::to_value(&stored.receipt)? == serde_json::to_value(receipt)?,
            "legacy receipt changed before preserving its native owner"
        );
        let digest = receipt_digest(&receipt_bytes);
        let path = self.legacy_owner_evidence_path(&receipt.generation, &digest)?;
        if let Some(saved) = read_record::<LegacyOwnerEvidence>(&path, false)? {
            let archived: CoordinatorOwner = serde_json::from_slice(&saved.coordinator_bytes)?;
            anyhow::ensure!(
                saved.version == 1
                    && saved.generation == receipt.generation
                    && saved.operation_id == receipt.operation.operation_id
                    && saved.receipt_digest == digest
                    && archived.worker_generation.as_deref() == Some(receipt.generation.as_str()),
                "existing legacy owner evidence does not match this receipt"
            );
            return Ok(());
        }
        let coordinator_bytes = std::fs::read(self.root.join(".deploy-coordinator.json"))?;
        let recorded: CoordinatorOwner = serde_json::from_slice(&coordinator_bytes)?;
        anyhow::ensure!(
            serde_json::to_value(&recorded)? == serde_json::to_value(owner)?,
            "legacy native owner changed before preserving compatibility evidence"
        );
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("legacy owner evidence has no valid file name")?;
        write_record_verified(
            &self.root,
            name,
            &LegacyOwnerEvidence {
                version: 1,
                generation: receipt.generation.clone(),
                operation_id: receipt.operation.operation_id.clone(),
                receipt_digest: digest,
                coordinator_bytes,
            },
        )?;
        Ok(())
    }

    fn legacy_generation_owner(&self, receipt: &Receipt) -> Result<Option<CoordinatorOwner>> {
        if let Some(owner) = &self.previous_owner
            && owner.worker_generation.as_deref() == Some(receipt.generation.as_str())
        {
            return Ok(Some(owner.clone()));
        }
        let bytes = std::fs::read(self.root.join(OPERATION_RECORD))?;
        let digest = receipt_digest(&bytes);
        let path = self.legacy_owner_evidence_path(&receipt.generation, &digest)?;
        let Some(evidence) = read_record::<LegacyOwnerEvidence>(&path, false)? else {
            return Ok(None);
        };
        anyhow::ensure!(
            evidence.version == 1
                && evidence.generation == receipt.generation
                && evidence.operation_id == receipt.operation.operation_id
                && evidence.receipt_digest == digest,
            "legacy deployment owner evidence does not match this receipt"
        );
        let owner: CoordinatorOwner = serde_json::from_slice(&evidence.coordinator_bytes)?;
        anyhow::ensure!(
            owner.worker_generation.as_deref() == Some(receipt.generation.as_str()),
            "legacy deployment owner evidence names another native generation"
        );
        Ok(Some(owner))
    }

    /// The caller holds the common OwnerGuard and this journal's lease, has
    /// confirmed old execution retirement, and has not committed a new owner.
    /// `workspace` is the confirmed active artifact's actual execution directory.
    /// A false result preserves records that do not establish this exact legacy
    /// defect; an error retains validation/I/O failures without authorizing reuse.
    pub(crate) fn normalize_legacy_deployment_generation(
        &mut self,
        expected_generation: &str,
        seed_operation_id: &str,
        seed_request: &DeployRequest,
        workspace: &Path,
    ) -> Result<bool> {
        anyhow::ensure!(self.lease.is_some(), "deployment journal lease missing");
        let Some(receipt) = self.receipt.as_ref() else {
            return Ok(false);
        };
        let old_generation = &receipt.generation;
        if old_generation == expected_generation
            || receipt.operation.deployment_generation_id != *old_generation
            || uuid::Uuid::parse_str(old_generation).is_err()
        {
            return Ok(false);
        }
        anyhow::ensure!(
            !expected_generation.trim().is_empty() && expected_generation == seed_operation_id,
            "legacy deployment normalization requires the original cold operation generation"
        );
        let Some(legacy_owner) = self.legacy_generation_owner(receipt)? else {
            return Ok(false);
        };
        // This repairs identity, never replays credentials. The current cold
        // operation carries its original non-sensitive deployment fields; a
        // redacted or later changed PG password must not invalidate that proof.
        let seed_matches = if receipt.operation.operation_id == seed_operation_id {
            receipt.operation.request_release_id == seed_request.release_id
                && receipt.request.url == seed_request.url
                && receipt.request.release_id == seed_request.release_id
                && receipt.request.sha256 == seed_request.sha256
                && receipt.request.local_path == seed_request.local_path
                && receipt.request.execution_target == seed_request.execution_target
        } else {
            // A hot receipt has different deployment input. Only the saved
            // fingerprint of the original cold operation establishes its map.
            let seed_fingerprint = deploy_replay::fingerprint(seed_request)
                .context("fingerprint original cold deployment declaration")?;
            self.deploy_replays
                .get(seed_operation_id)
                .is_some_and(|anchor| {
                    anchor.fingerprint.as_deref() == Some(seed_fingerprint.as_str())
                        && anchor.operation.operation_id == seed_operation_id
                        && anchor.operation.deployment_generation_id == *old_generation
                })
        };
        if !seed_matches {
            return Ok(false);
        }
        // Identity repair does not settle an interrupted directory exchange or
        // invent an outcome for a failed/unfinished deployment or SQL command.
        match receipt.boundary {
            Boundary::Active | Boundary::RestoredActive | Boundary::StartupFailed => {}
            Boundary::Preparing | Boundary::Switching | Boundary::Activated | Boundary::Failed => {
                return Ok(false);
            }
        }
        self.require_fresh_process_scope()
            .context("confirm retired owner before deployment generation normalization")?;
        if legacy_owner.state != OwnerState::Quiescent {
            runtime_supervisor::verify_local_quiescent(&self.root, old_generation)
                .context("legacy native execution remains unconfirmed")?;
        }
        let active = receipt
            .active
            .as_ref()
            .context("legacy confirmed boundary has no active artifact")?;
        let release = crate::manifest::read_release_lock(workspace)
            .context("verify active artifact for deployment generation normalization")?;
        anyhow::ensure!(
            !active.artifact_release_id.is_empty()
                && active.artifact_release_id == release.release_id,
            "legacy active artifact does not match the execution directory"
        );
        crate::migration_journal::require_confirmed_migrations(workspace)
            .context("verify SQL outcomes before deployment generation normalization")?;

        let path = self.root.join(OPERATION_RECORD);
        let original = std::fs::read(&path).context("read original legacy deployment receipt")?;
        let mut value: serde_json::Value = serde_json::from_slice(&original)
            .context("decode original legacy deployment receipt")?;
        let stored: StoredReceipt = serde_json::from_value(value.clone())
            .context("validate original legacy deployment receipt")?;
        anyhow::ensure!(
            serde_json::to_value(&stored.receipt)? == serde_json::to_value(receipt)?
                && serde_json::to_value(&stored.deploy_replays)?
                    == serde_json::to_value(&self.deploy_replays)?,
            "deployment journal changed before generation normalization"
        );
        if let Some(replay) = stored.deploy_replays.get(&receipt.operation.operation_id) {
            anyhow::ensure!(
                serde_json::to_value(&replay.operation)?
                    == serde_json::to_value(&receipt.operation)?,
                "current deployment replay disagrees with its receipt"
            );
        }
        *value
            .pointer_mut("/generation")
            .context("legacy receipt generation missing")? = expected_generation.into();
        *value
            .pointer_mut("/operation/deployment_generation_id")
            .context("legacy operation generation missing")? = expected_generation.into();
        if let Some(replays) = value.get_mut("deploy_replays")
            && let Some(replay) = replays.get_mut(&receipt.operation.operation_id)
        {
            *replay
                .pointer_mut("/operation/deployment_generation_id")
                .context("legacy current replay generation missing")? = expected_generation.into();
        }
        // Validate before publishing. Value retains unknown fields and every
        // unrelated replay; serializing StoredReceipt would discard them.
        let _: StoredReceipt = serde_json::from_value(value.clone())
            .context("validate normalized deployment receipt")?;
        let backup = self.root.join(format!(
            "{OPERATION_RECORD}.generation-compat-{}.backup",
            uuid::Uuid::new_v4().simple()
        ));
        let mut temporary = tempfile::NamedTempFile::new_in(&self.root)
            .context("create original deployment receipt backup")?;
        temporary.write_all(&original)?;
        temporary.as_file().sync_all()?;
        temporary
            .persist_noclobber(&backup)
            .map_err(|error| error.error)
            .context("preserve original deployment receipt before generation normalization")?;
        #[cfg(unix)]
        File::open(&self.root)?.sync_all()?;
        anyhow::ensure!(
            std::fs::read(&backup)? == original,
            "legacy deployment receipt backup readback mismatch"
        );
        let readback: serde_json::Value =
            write_record_verified(&self.root, OPERATION_RECORD, &value)
                .context("commit normalized deployment generation")?;
        let normalized: StoredReceipt = serde_json::from_value(readback)
            .context("decode normalized deployment receipt readback")?;
        tracing::warn!(
            old_generation = %old_generation,
            deployment_generation = expected_generation,
            operation_id = %receipt.operation.operation_id,
            seed_operation_id,
            backup = %backup.display(),
            "normalized proven legacy native-session deployment generation"
        );
        self.receipt = Some(normalized.receipt);
        self.deploy_replays = normalized.deploy_replays;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shared_types::{AppCliDeployPhase, app_cli_deploy::AppDeploymentStage};

    struct Fixture {
        _directory: tempfile::TempDir,
        journal: Journal,
        workspace: PathBuf,
        seed: DeployRequest,
        native_generation: String,
        migration_path: PathBuf,
        migration_identity: String,
        original: Vec<u8>,
    }

    impl Fixture {
        fn new(current_is_seed: bool) -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path();
            let workspace = root.join("code");
            std::fs::create_dir(&workspace).unwrap();
            std::fs::write(
                workspace.join("release.lock.toml"),
                r#"
schema_version = 1
release_id = "manifest-b"
workspace_name = "demo"
minimum_app_cli_version = "0.1.3"
runtime_image_digest = "registry.example/app-runtime:0.1.140"
[pingap]
mode = "managed"
version = "0.14.3"
commit = "abc123"
[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200
[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30
[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"
[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"
[services.env]
"#,
            )
            .unwrap();
            let native_generation = uuid::Uuid::new_v4().to_string();
            let native_root = root.join("work").join(&native_generation);
            std::fs::create_dir_all(&native_root).unwrap();
            std::fs::write(
                native_root.join("generation.json"),
                serde_json::to_vec(&serde_json::json!({
                    "version": 1, "id": native_generation,
                    "supervisor": "retired-owner", "token": "fixture",
                    "intent": "run", "phase": "Quiescent",
                    "worker_pid": null, "exit_code": 0, "error": null
                }))
                .unwrap(),
            )
            .unwrap();
            let seed = DeployRequest {
                runtime_operation_id: None,
                url: "http://artifact/cold".into(),
                release_id: "cold-release".into(),
                sha256: Some("a".repeat(64)),
                local_path: None,
                execution_target: None,
                run_pg: None,
            };
            let mut request = seed.clone();
            if !current_is_seed {
                request.url = "http://artifact/hot".into();
                request.release_id = "hot-release".into();
            }
            let current_id = if current_is_seed { "cold-op" } else { "hot-op" };
            let operation = AppDeploymentOperation {
                operation_id: current_id.into(),
                deployment_generation_id: native_generation.clone(),
                request_release_id: request.release_id.clone(),
                artifact_release_id: Some("manifest-b".into()),
                persisted: true,
                deploy_stage: AppDeploymentStage::Succeeded,
                phase: AppCliDeployPhase::Failed,
                error: Some("confirmed startup failure".into()),
                recovery: None,
            };
            let receipt = Receipt {
                generation: native_generation.clone(),
                operation: operation.clone(),
                request: request.clone(),
                boundary: Boundary::StartupFailed,
                active: Some(ActiveVersion {
                    artifact_release_id: "manifest-b".into(),
                    request: Some(request.clone()),
                }),
            };
            let mut history = deploy_replay::History::new();
            let mut anchor = operation.clone();
            anchor.operation_id = "cold-op".into();
            anchor.request_release_id = seed.release_id.clone();
            history.insert(
                "cold-op".into(),
                deploy_replay::Replay {
                    fingerprint: Some(deploy_replay::fingerprint(&seed).unwrap()),
                    operation: anchor,
                },
            );
            history.insert(
                current_id.into(),
                deploy_replay::Replay {
                    fingerprint: Some(deploy_replay::fingerprint(&request).unwrap()),
                    operation: operation.clone(),
                },
            );
            let mut foreign = operation;
            foreign.operation_id = "foreign-op".into();
            foreign.deployment_generation_id = "foreign-generation".into();
            history.insert(
                "foreign-op".into(),
                deploy_replay::Replay {
                    fingerprint: Some("unchanged-fingerprint".into()),
                    operation: foreign,
                },
            );
            let mut value = serde_json::to_value(StoredReceipt {
                receipt,
                deploy_replays: history,
            })
            .unwrap();
            value["unknown_receipt"] = serde_json::json!({"keep": [1, 2, 3]});
            value["operation"]["unknown_result"] = "unchanged".into();
            value["deploy_replays"]["foreign-op"]["unknown_history"] = true.into();
            let original = serde_json::to_vec_pretty(&value).unwrap();
            std::fs::write(root.join(OPERATION_RECORD), &original).unwrap();
            write_record_verified(
                root,
                ".deploy-coordinator.json",
                &CoordinatorOwner {
                    state: OwnerState::Active,
                    process_scope: Some("old-container".into()),
                    worker_generation: Some(native_generation.clone()),
                },
            )
            .unwrap();
            let release = crate::manifest::read_release_lock(&workspace).unwrap();
            let migration_identity = crate::migration_journal::identity(&release, "web").unwrap();
            let migrations = root.join("migration-receipts");
            std::fs::create_dir(&migrations).unwrap();
            let migration_path = migrations.join(format!("{migration_identity}.json"));
            std::fs::write(
                &migration_path,
                serde_json::to_vec(&serde_json::json!({
                    "identity": migration_identity, "completed": true
                }))
                .unwrap(),
            )
            .unwrap();
            let journal = Journal::open_root(root.to_path_buf()).unwrap();
            Self {
                _directory: directory,
                journal,
                workspace,
                seed,
                native_generation,
                migration_path,
                migration_identity,
                original,
            }
        }

        fn normalize(&mut self) -> Result<bool> {
            self.journal.normalize_legacy_deployment_generation(
                "cold-op",
                "cold-op",
                &self.seed,
                &self.workspace,
            )
        }

        fn bytes(&self) -> Vec<u8> {
            std::fs::read(self.journal.root.join(OPERATION_RECORD)).unwrap()
        }

        fn backups(&self) -> Vec<PathBuf> {
            std::fs::read_dir(&self.journal.root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(".generation-compat-")
                })
                .collect()
        }
    }

    #[test]
    fn fresh_journal_accepts_distinct_cold_operation_and_generation_without_writes() {
        let directory = tempfile::tempdir().unwrap();
        let mut journal = Journal::open_root(directory.path().to_path_buf()).unwrap();
        let seed = DeployRequest {
            runtime_operation_id: None,
            url: "http://artifact/cold".into(),
            release_id: "cold-release".into(),
            sha256: Some("a".repeat(64)),
            local_path: None,
            execution_target: None,
            run_pg: None,
        };
        let before: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            !journal
                .normalize_legacy_deployment_generation(
                    "platform-generation",
                    "cold-operation",
                    &seed,
                    &directory.path().join("code"),
                )
                .unwrap()
        );
        assert!(journal.receipt.is_none());
        assert!(journal.deploy_replays.is_empty());
        let after: Vec<_> = std::fs::read_dir(directory.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(after, before, "no receipt, replay or backup may be created");
    }

    #[test]
    fn correct_receipt_accepts_distinct_cold_operation_and_generation_without_changes() {
        let mut fixture = Fixture::new(true);
        let expected = uuid::Uuid::new_v4().to_string();
        let mut value: serde_json::Value = serde_json::from_slice(&fixture.original).unwrap();
        value["generation"] = expected.clone().into();
        value["operation"]["deployment_generation_id"] = expected.clone().into();
        value["deploy_replays"]["cold-op"]["operation"]["deployment_generation_id"] =
            expected.clone().into();
        let stored: StoredReceipt = serde_json::from_value(value.clone()).unwrap();
        fixture.journal.receipt = Some(stored.receipt);
        fixture.journal.deploy_replays = stored.deploy_replays;
        fixture.original = serde_json::to_vec(&value).unwrap();
        std::fs::write(
            fixture.journal.root.join(OPERATION_RECORD),
            &fixture.original,
        )
        .unwrap();
        let history_before = serde_json::to_value(&fixture.journal.deploy_replays).unwrap();
        let migration_before = std::fs::read(&fixture.migration_path).unwrap();
        assert!(
            !fixture
                .journal
                .normalize_legacy_deployment_generation(
                    &expected,
                    "cold-op",
                    &fixture.seed,
                    &fixture.workspace,
                )
                .unwrap()
        );
        assert_eq!(fixture.bytes(), fixture.original);
        assert_eq!(
            serde_json::to_value(&fixture.journal.deploy_replays).unwrap(),
            history_before
        );
        assert_eq!(
            std::fs::read(&fixture.migration_path).unwrap(),
            migration_before
        );
        assert!(fixture.backups().is_empty());
    }

    #[test]
    fn missing_lease_and_legacy_seed_mismatch_remain_rejected_without_changes() {
        let mut fixture = Fixture::new(true);
        let lease = fixture.journal.lease.take().unwrap();
        assert!(format!("{:#}", fixture.normalize().unwrap_err()).contains("lease missing"));
        fixture.journal.lease = Some(lease);
        for current_is_seed in [true, false] {
            let mut fixture = Fixture::new(current_is_seed);
            for expected in ["", "another-cold-generation"] {
                let error = fixture
                    .journal
                    .normalize_legacy_deployment_generation(
                        expected,
                        "cold-op",
                        &fixture.seed,
                        &fixture.workspace,
                    )
                    .unwrap_err();
                assert!(format!("{error:#}").contains("original cold operation generation"));
                assert_eq!(fixture.bytes(), fixture.original);
                assert!(fixture.backups().is_empty());
            }
        }
    }

    #[test]
    fn proven_cold_and_hot_receipts_resume_without_losing_history_or_repeating_sql() {
        for current_is_seed in [true, false] {
            let mut fixture = Fixture::new(current_is_seed);
            let native_before = std::fs::read(
                fixture
                    .journal
                    .root
                    .join("work")
                    .join(&fixture.native_generation)
                    .join("generation.json"),
            )
            .unwrap();
            let migration_before = std::fs::read(&fixture.migration_path).unwrap();
            assert!(fixture.normalize().unwrap());
            let resumed = fixture.journal.resume("cold-op").unwrap().unwrap();
            assert_eq!(resumed.generation, "cold-op");
            assert_eq!(resumed.operation.deployment_generation_id, "cold-op");
            assert_eq!(resumed.boundary, Boundary::StartupFailed);
            assert_eq!(
                resumed.operation.error.as_deref(),
                Some("confirmed startup failure")
            );
            assert_eq!(resumed.operation.phase, AppCliDeployPhase::Failed);
            let mut expected: serde_json::Value =
                serde_json::from_slice(&fixture.original).unwrap();
            expected["generation"] = "cold-op".into();
            expected["operation"]["deployment_generation_id"] = "cold-op".into();
            expected["deploy_replays"][&resumed.operation.operation_id]["operation"]["deployment_generation_id"] =
                "cold-op".into();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&fixture.bytes()).unwrap(),
                expected
            );
            let backups = fixture.backups();
            assert_eq!(backups.len(), 1);
            assert_eq!(std::fs::read(&backups[0]).unwrap(), fixture.original);
            assert_eq!(
                std::fs::read(&fixture.migration_path).unwrap(),
                migration_before
            );
            assert_eq!(
                std::fs::read(
                    fixture
                        .journal
                        .root
                        .join("work")
                        .join(&fixture.native_generation)
                        .join("generation.json")
                )
                .unwrap(),
                native_before
            );
            let committed = fixture.bytes();
            assert!(!fixture.normalize().unwrap());
            assert_eq!(fixture.bytes(), committed);
            assert_eq!(fixture.backups(), backups);
            fixture.journal.write(resumed).unwrap();
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&fixture.bytes()).unwrap(),
                expected,
                "later boundary writes must retain unrelated extensions"
            );
            assert!(
                crate::migration_journal::MigrationJournal::begin(
                    &fixture.workspace,
                    fixture.migration_identity.clone()
                )
                .unwrap()
                .is_none()
            );
        }
    }

    #[test]
    fn compatibility_evidence_survives_a_failed_attempt_and_new_coordinator() {
        let mut fixture = Fixture::new(true);
        let pending = serde_json::json!({
            "identity":fixture.migration_identity, "completed":false
        });
        std::fs::write(
            &fixture.migration_path,
            serde_json::to_vec(&pending).unwrap(),
        )
        .unwrap();
        assert!(fixture.normalize().is_err());
        fixture
            .journal
            .attach_worker_generation(uuid::Uuid::new_v4().to_string());
        fixture.journal.commit_coordinator().unwrap();
        fixture.journal.commit_quiescent().unwrap();
        assert_ne!(
            fixture
                .journal
                .previous_owner
                .as_ref()
                .unwrap()
                .worker_generation
                .as_deref(),
            Some(fixture.native_generation.as_str())
        );
        assert_eq!(fixture.bytes(), fixture.original);
        std::fs::write(
            &fixture.migration_path,
            serde_json::to_vec(&serde_json::json!({
                "identity":fixture.migration_identity, "completed":true
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(
            fixture.normalize().unwrap(),
            "the bound original owner proof survives retry"
        );
        assert_eq!(
            fixture
                .journal
                .resume("cold-op")
                .unwrap()
                .unwrap()
                .generation,
            "cold-op"
        );
    }

    #[test]
    fn foreign_or_unproven_owner_and_seed_keep_original_bytes() {
        for scenario in 0..3 {
            let mut fixture = Fixture::new(false);
            match scenario {
                0 => fixture.journal.previous_owner = None,
                1 => {
                    fixture
                        .journal
                        .previous_owner
                        .as_mut()
                        .unwrap()
                        .worker_generation = Some(uuid::Uuid::new_v4().to_string())
                }
                _ => fixture.seed.url = "http://artifact/another-cold".into(),
            }
            assert!(!fixture.normalize().unwrap());
            assert_eq!(fixture.bytes(), fixture.original);
            assert!(fixture.backups().is_empty());
        }
    }

    #[test]
    fn pending_migration_rejects_normalization_without_writing_or_backing_up() {
        let mut fixture = Fixture::new(false);
        let pending = serde_json::to_vec(&serde_json::json!({
            "identity": fixture.migration_identity, "completed": false
        }))
        .unwrap();
        std::fs::write(&fixture.migration_path, &pending).unwrap();
        assert!(format!("{:#}", fixture.normalize().unwrap_err()).contains("unconfirmed"));
        assert_eq!(fixture.bytes(), fixture.original);
        assert_eq!(std::fs::read(&fixture.migration_path).unwrap(), pending);
        assert!(fixture.backups().is_empty());
    }

    #[test]
    fn hot_receipt_with_redacted_pg_requires_its_original_cold_fingerprint() {
        for saved_fingerprint in [true, false] {
            let mut fixture = Fixture::new(false);
            fixture.seed.run_pg = Some(shared_types::StartPgCredential {
                username: "fixture-user".into(),
                password: "original-fixture-credential".into(),
            });
            let mut value: serde_json::Value = serde_json::from_slice(&fixture.original).unwrap();
            let redacted = serde_json::json!({"username": "fixture-user", "password": ""});
            value["request"]["run_pg"] = redacted.clone();
            value["active"]["request"]["run_pg"] = redacted;
            value["deploy_replays"]["cold-op"]["fingerprint"] = if saved_fingerprint {
                deploy_replay::fingerprint(&fixture.seed).unwrap().into()
            } else {
                serde_json::Value::Null
            };
            let stored: StoredReceipt = serde_json::from_value(value.clone()).unwrap();
            fixture.journal.receipt = Some(stored.receipt);
            fixture.journal.deploy_replays = stored.deploy_replays;
            fixture.original = serde_json::to_vec(&value).unwrap();
            std::fs::write(
                fixture.journal.root.join(OPERATION_RECORD),
                &fixture.original,
            )
            .unwrap();
            assert_eq!(fixture.normalize().unwrap(), saved_fingerprint);
            if saved_fingerprint {
                assert_eq!(fixture.backups().len(), 1);
                assert_eq!(
                    std::fs::read(&fixture.backups()[0]).unwrap(),
                    fixture.original
                );
            } else {
                assert_eq!(fixture.bytes(), fixture.original);
                assert!(fixture.backups().is_empty());
            }
        }
    }

    #[test]
    fn changed_pg_password_does_not_replay_or_rewrite_the_current_cold_deployment() {
        let mut fixture = Fixture::new(true);
        let mut original_request = fixture.seed.clone();
        original_request.run_pg = Some(shared_types::StartPgCredential {
            username: "fixture-user".into(),
            password: "original-fixture-credential".into(),
        });
        let original_fingerprint = deploy_replay::fingerprint(&original_request).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&fixture.original).unwrap();
        let redacted = serde_json::json!({"username":"fixture-user", "password":""});
        value["request"]["run_pg"] = redacted.clone();
        value["active"]["request"]["run_pg"] = redacted;
        value["deploy_replays"]["cold-op"]["fingerprint"] = original_fingerprint.clone().into();
        let stored: StoredReceipt = serde_json::from_value(value.clone()).unwrap();
        fixture.journal.receipt = Some(stored.receipt);
        fixture.journal.deploy_replays = stored.deploy_replays;
        fixture.original = serde_json::to_vec(&value).unwrap();
        std::fs::write(
            fixture.journal.root.join(OPERATION_RECORD),
            &fixture.original,
        )
        .unwrap();
        fixture.seed.run_pg = Some(shared_types::StartPgCredential {
            username: "fixture-user".into(),
            password: "changed-fixture-credential".into(),
        });
        assert_ne!(
            deploy_replay::fingerprint(&fixture.seed).unwrap(),
            original_fingerprint
        );
        let migrations_before = std::fs::read(&fixture.migration_path).unwrap();
        assert!(fixture.normalize().unwrap());
        value["generation"] = "cold-op".into();
        value["operation"]["deployment_generation_id"] = "cold-op".into();
        value["deploy_replays"]["cold-op"]["operation"]["deployment_generation_id"] =
            "cold-op".into();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&fixture.bytes()).unwrap(),
            value
        );
        assert_eq!(
            std::fs::read(&fixture.backups()[0]).unwrap(),
            fixture.original
        );
        assert_eq!(
            std::fs::read(&fixture.migration_path).unwrap(),
            migrations_before
        );
        assert!(
            crate::migration_journal::MigrationJournal::begin(
                &fixture.workspace,
                fixture.migration_identity.clone()
            )
            .unwrap()
            .is_none(),
            "confirmed SQL must remain skipped"
        );
    }

    #[test]
    fn current_cold_identity_mismatch_is_not_rescued_by_its_saved_fingerprint() {
        for field in 0..5 {
            let mut fixture = Fixture::new(true);
            let receipt = &mut fixture.journal.receipt.as_mut().unwrap().request;
            match field {
                0 => receipt.url = "http://different-artifact".into(),
                1 => receipt.release_id = "different-release".into(),
                2 => receipt.sha256 = Some("b".repeat(64)),
                3 => receipt.local_path = Some(PathBuf::from("different.zip")),
                _ => {
                    receipt.execution_target =
                        Some(super::super::super::state::ExecutionTarget::Source)
                }
            }
            assert!(!fixture.normalize().unwrap());
            assert_eq!(fixture.bytes(), fixture.original);
            assert!(fixture.backups().is_empty());
        }
    }

    #[test]
    fn active_artifact_mismatch_or_unretired_native_execution_keeps_original_bytes() {
        for unretired_execution in [true, false] {
            let mut fixture = Fixture::new(false);
            if unretired_execution {
                let path = fixture
                    .journal
                    .root
                    .join("work")
                    .join(&fixture.native_generation)
                    .join("generation.json");
                let mut native: serde_json::Value =
                    serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
                native["phase"] = "Running".into();
                std::fs::write(path, serde_json::to_vec(&native).unwrap()).unwrap();
            } else {
                let path = fixture.workspace.join("release.lock.toml");
                let manifest = std::fs::read_to_string(&path).unwrap();
                std::fs::write(path, manifest.replace("manifest-b", "foreign-artifact")).unwrap();
            }
            assert!(fixture.normalize().is_err());
            assert_eq!(fixture.bytes(), fixture.original);
            assert!(fixture.backups().is_empty());
        }
    }

    #[test]
    fn unfinished_boundaries_do_not_become_confirmed_by_identity_repair() {
        for boundary in [
            Boundary::Preparing,
            Boundary::Switching,
            Boundary::Activated,
            Boundary::Failed,
        ] {
            let mut fixture = Fixture::new(false);
            let mut value: serde_json::Value = serde_json::from_slice(&fixture.original).unwrap();
            value["boundary"] = serde_json::to_value(&boundary).unwrap();
            fixture.original = serde_json::to_vec(&value).unwrap();
            std::fs::write(
                fixture.journal.root.join(OPERATION_RECORD),
                &fixture.original,
            )
            .unwrap();
            fixture.journal.receipt.as_mut().unwrap().boundary = boundary;
            assert!(!fixture.normalize().unwrap());
            assert_eq!(fixture.bytes(), fixture.original);
            assert!(fixture.backups().is_empty());
        }
    }
}
