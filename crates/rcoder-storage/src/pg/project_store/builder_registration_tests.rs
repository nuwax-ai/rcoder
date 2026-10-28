//! Real PostgreSQL regressions for completion acknowledgements written before
//! lifecycle resource bindings existed. No Kubernetes ownership is inferred
//! from a name or a replica's local registry.
use super::*;
use crate::db::{
    owner::DatabaseOwner,
    schema::{Backend, Component},
};
use crate::userapp_lifecycle::PgUserAppStore;
use shared_types::*;
use std::time::Duration;

#[tokio::test]
async fn completed_builder_registration_retries_a_rolled_back_root_token_race() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let (completed, evidence) = f.completed(f.basic("racepod", "racests"), true).await;
    let original_bytes = f.operation_bytes(&completed.operation_id).await;
    let volumes = f.volumes();

    let (mut competing_writer, connection) = tokio_postgres::connect(&f.dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let connection = tokio::spawn(connection);
    let writer_pid: i32 = competing_writer
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    let competing = competing_writer.transaction().await.unwrap();
    competing
        .query_one(
            "SELECT control_revision FROM userapps WHERE app_id=$1 FOR UPDATE",
            &[&f.app.app_id],
        )
        .await
        .unwrap();

    // The production registration reads the old root token, then its UPDATE
    // waits on this actual PostgreSQL row lock. Observe that wait before the
    // other writer commits a new token; no mocked conflict or sleep ordering.
    let release_competing_writer = async {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let blocked: bool = competing
                    .query_one(
                        "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)))",
                        &[&writer_pid],
                    )
                    .await
                    .unwrap()
                    .get(0);
                if blocked {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("registration must reach the real blocked root-token UPDATE");
        assert_eq!(
            competing
                .execute(
                    "UPDATE userapps SET control_revision=control_revision+1 WHERE app_id=$1",
                    &[&f.app.app_id],
                )
                .await
                .unwrap(),
            1
        );
        competing.commit().await.unwrap();
    };
    let (registration, ()) = tokio::join!(
        f.store
            .register_completed_builder_replacement(&completed, &evidence, &volumes),
        release_competing_writer
    );
    registration.expect("known rolled-back root-token race must reread and confirm the builder");
    assert_eq!(
        f.store.get(&f.app.app_id).unwrap().container_info(),
        Some(evidence.container.clone())
    );
    let peer = f.peer().await;
    assert_eq!(
        peer.get(&f.app.app_id).unwrap().container_info(),
        Some(evidence.container.clone()),
        "another replica must observe the committed registration"
    );
    assert_eq!(
        f.control
            .get_operation(&f.app.app_id, &completed.operation_id)
            .await
            .unwrap()
            .unwrap(),
        completed,
        "registration retry must not rewrite the original succeeded operation"
    );
    assert_eq!(
        f.operation_bytes(&completed.operation_id).await,
        original_bytes
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    f.owner.shutdown().await.unwrap();
    drop(competing_writer);
    connection.await.unwrap().unwrap();
}

struct Fixture {
    dsn: String,
    owner: DatabaseOwner,
    control: PgUserAppStore,
    store: PgStore,
    app: UserAppLifecycleRecord,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let dsn = crate::pg::test_support::test_dsn().await?;
        let config = crate::config::PostgresConfig {
            url: Some(dsn.clone()),
            ..Default::default()
        };
        let owner =
            crate::db::postgres::open(&config, vec![Component::Project, Component::Userapp])
                .await
                .unwrap();
        let control = PgUserAppStore::from_owner(owner.clone(), Backend::Postgres);
        let app = control
            .ensure_identity(&format!("receipt{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let store = PgStore::connect(&config, "receipt-test".into(), "cluster.local".into())
            .await
            .unwrap();
        Some(Self {
            dsn,
            owner,
            control,
            store,
            app,
        })
    }

    fn uid(&self, label: &str) -> String {
        format!("{label}{}", self.app.app_id)
    }

    fn basic(&self, pod: &str, sts: &str) -> ContainerBasicInfo {
        ContainerBasicInfo {
            container_id: self.uid(pod),
            container_name: format!("builder-{}", self.app.app_id),
            container_ip: "10.42.0.11".into(),
            internal_port: 60000,
            external_port: 0,
            project_id: self.app.app_id.clone(),
            status: "running".into(),
            created_at: chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
            service_url: "http://10.42.0.11:60000".into(),
            workload_uid: Some(self.uid(sts)),
        }
    }

    fn volumes(&self) -> Vec<AppResourceIdentity> {
        vec![AppResourceIdentity {
            kind: AppResourceKind::PersistentVolumeClaim,
            name: format!("workspace-{}", self.app.app_id),
            uid: format!("pvc{}", self.app.app_id),
            resource_version: None,
        }]
    }

    async fn completed(
        &self,
        basic: ContainerBasicInfo,
        recorded: bool,
    ) -> (UserAppOperationRecord, BuilderCreationEvidence) {
        self.completed_with_source(basic, recorded, None).await
    }

    async fn completed_with_source(
        &self,
        basic: ContainerBasicInfo,
        recorded: bool,
        predecessor: Option<BuilderCreationPredecessor>,
    ) -> (UserAppOperationRecord, BuilderCreationEvidence) {
        self.completed_for_application(&self.app, basic, recorded, predecessor)
            .await
    }

    async fn completed_for_application(
        &self,
        app: &UserAppLifecycleRecord,
        basic: ContainerBasicInfo,
        recorded: bool,
        predecessor: Option<BuilderCreationPredecessor>,
    ) -> (UserAppOperationRecord, BuilderCreationEvidence) {
        let accepted = match self
            .control
            .admit(&UserAppAdmission {
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: format!("ensure{}", uuid::Uuid::new_v4().simple()),
                request_id: None,
                request_fingerprint: "a".repeat(64),
                kind: UserAppOperationKind::EnsureBuilder,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap()
        {
            UserAppAdmissionOutcome::Accepted(record) => record,
            _ => panic!("fresh operation"),
        };
        let progress = |record: &UserAppOperationRecord, state, step: &str, checkpoint| {
            UserAppOperationProgress {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                operation_id: record.operation_id.clone(),
                expected_revision: record.revision,
                executor_id: "receiptworker".into(),
                state,
                step: step.into(),
                checkpoint,
                error_code: None,
                error_message: None,
            }
        };
        let running = self
            .control
            .advance(&progress(
                &accepted,
                UserAppOperationState::Running,
                "claimed",
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        let mut evidence = BuilderCreationEvidence {
            creation_lease_released: true,
            target: BuilderControlTarget {
                builder_startup_mode: Default::default(),
                context: UserAppExecutionContext {
                    app_id: app.app_id.clone(),
                    lifecycle_id: app.lifecycle_id.clone(),
                    operation_id: running.operation_id.clone(),
                    executor_id: "receiptworker".into(),
                    request_fingerprint: running.request_fingerprint.clone(),
                },
                workload: Some(AppResourceIdentity {
                    kind: AppResourceKind::StatefulSet,
                    name: format!("builder-{}", app.app_id),
                    uid: basic.workload_uid.clone().unwrap(),
                    resource_version: Some("1".into()),
                }),
                pod: Some(BuilderPodIdentity {
                    name: format!("{}-0", basic.container_name),
                    uid: basic.container_id.clone(),
                    resource_version: "1".into(),
                }),
                resource_binding: None,
                restart_image: None,
                restart_runtime_workspace: None,
            },
            container: basic,
            registration_predecessor: predecessor,
        };
        if let Some(source) = &mut evidence.registration_predecessor {
            source.target.context = evidence.target.context.clone();
        }
        let ready = self
            .control
            .advance(&progress(
                &running,
                UserAppOperationState::Running,
                "builder_ready_confirmed",
                serde_json::to_value(&evidence).unwrap(),
            ))
            .await
            .unwrap();
        let mut checkpoint = serde_json::to_value(&evidence.container).unwrap();
        if recorded {
            checkpoint.as_object_mut().unwrap().insert(
                "builder_creation_evidence".into(),
                serde_json::to_value(&evidence).unwrap(),
            );
        }
        let completed = self
            .control
            .advance(&progress(
                &ready,
                UserAppOperationState::Succeeded,
                "creation_result",
                checkpoint,
            ))
            .await
            .unwrap();
        (completed, evidence)
    }

    async fn register_old(&self, basic: ContainerBasicInfo) -> Arc<ProjectAndContainerInfo> {
        let mut project = ProjectAndContainerInfo::new(self.app.app_id.clone());
        project.set_service_type(Some(ServiceType::UserappBuilder));
        project.set_container(Some(basic));
        self.store
            .insert_with_session(
                self.app.app_id.clone(),
                Arc::new(project),
                Some(&format!("session{}", self.app.app_id)),
            )
            .unwrap();
        assert!(self.store.wait_drained(Duration::from_secs(5)).await);
        self.store.get(&self.app.app_id).unwrap()
    }

    async fn peer(&self) -> PgStore {
        PgStore::connect(
            &crate::config::PostgresConfig {
                url: Some(self.dsn.clone()),
                ..Default::default()
            },
            "receipt-peer".into(),
            "cluster.local".into(),
        )
        .await
        .unwrap()
    }

    async fn insert_foreign_binding(&self, uid: &str) -> UserAppResourceBinding {
        // The FK is part of the production contract. The conflicting binding
        // must reference an actually admitted successful operation owned by a
        // different application/lifecycle, rather than invalid fixture strings.
        let foreign = self
            .control
            .ensure_identity(&format!("foreignreceipt{}", uuid::Uuid::new_v4().simple()))
            .await
            .unwrap();
        let mut basic = self.basic("foreignpod", uid);
        basic.project_id = foreign.app_id.clone();
        basic.container_name = format!("builder-{}", foreign.app_id);
        let (operation, _) = self
            .completed_for_application(&foreign, basic, false, None)
            .await;
        let binding = UserAppResourceBinding {
            service_type: ServiceType::UserappBuilder,
            physical_uid: self.uid(uid),
            app_id: foreign.app_id,
            lifecycle_id: foreign.lifecycle_id,
            adopted_by_operation: operation.operation_id,
        };
        let persisted = binding.clone();
        self.owner.execute(move |mut db| async move {
            toasty::sql::statement("INSERT INTO userapp_resource_bindings(service_type,physical_uid,app_id,lifecycle_id,adopted_by_operation,created_at_us) VALUES($1,$2,$3,$4,$5,$6)")
                .bind(persisted.service_type.to_string()).bind(persisted.physical_uid)
                .bind(persisted.app_id).bind(persisted.lifecycle_id).bind(persisted.adopted_by_operation)
                .bind(chrono::Utc::now().timestamp_micros()).exec(&mut db).await?;
            Ok(())
        }).await.unwrap();
        binding
    }

    async fn operation_bytes(&self, id: &str) -> String {
        crate::pg::test_support::optional_text(
            &self.owner,
            "SELECT checkpoint_json FROM userapp_operations WHERE operation_id=$1",
            id,
        )
        .await
        .unwrap()
    }

    async fn delete_and_recreate(&self, app: &UserAppLifecycleRecord) -> UserAppLifecycleRecord {
        let accepted = self
            .control
            .admit(&UserAppAdmission {
                app_id: app.app_id.clone(),
                lifecycle_id: Some(app.lifecycle_id.clone()),
                operation_id: uuid::Uuid::new_v4().to_string(),
                request_id: Some(uuid::Uuid::new_v4().to_string()),
                request_fingerprint: "d".repeat(64),
                kind: UserAppOperationKind::DeleteApplication,
                command: None,
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap();
        let mut operation = match accepted {
            UserAppAdmissionOutcome::Accepted(operation) => operation,
            UserAppAdmissionOutcome::Existing(_) => panic!("fresh deletion required"),
        };
        let progress =
            |operation: &UserAppOperationRecord, state, checkpoint| UserAppOperationProgress {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                operation_id: operation.operation_id.clone(),
                expected_revision: operation.revision,
                executor_id: "deletionworker".into(),
                state,
                step: "deletion_contract".into(),
                checkpoint,
                error_code: None,
                error_message: None,
            };
        operation = self
            .control
            .advance(&progress(
                &operation,
                UserAppOperationState::Running,
                serde_json::Value::Null,
            ))
            .await
            .unwrap();
        // This is a real storage state-machine fixture, not a claimed runtime
        // deletion. The K8s regression separately supplies physical evidence.
        let mut checkpoint = UserAppDeletionCheckpoint {
            schema_version: 1,
            stage: UserAppDeletionStage::Captured,
            kind: UserAppOperationKind::DeleteApplication,
            context: UserAppExecutionContext {
                app_id: app.app_id.clone(),
                lifecycle_id: app.lifecycle_id.clone(),
                operation_id: operation.operation_id.clone(),
                executor_id: "deletionworker".into(),
                request_fingerprint: operation.request_fingerprint.clone(),
            },
            production: AppDeletionSnapshot {
                app_id: app.app_id.clone(),
                operation_id: "captured-production".into(),
                resources: vec![],
                directories: None,
            },
            development: Some(UserappDevDeletionReceipt {
                runtime: BuilderDeletionSnapshot {
                    app_id: app.app_id.clone(),
                    operation_id: "captured-development".into(),
                    resources: vec![],
                    resource_binding: None,
                    docker_bind_cleanup: false,
                },
                registry: None,
                lease: None,
                collaborators: vec![],
                directories: None,
            }),
        };
        for stage in [
            UserAppDeletionStage::Captured,
            UserAppDeletionStage::ComputeRemoved,
            UserAppDeletionStage::ProductionStorageRemoved,
            UserAppDeletionStage::DevelopmentRemoved,
        ] {
            checkpoint.stage = stage;
            operation = self
                .control
                .advance(&progress(
                    &operation,
                    UserAppOperationState::Running,
                    serde_json::to_value(&checkpoint).unwrap(),
                ))
                .await
                .unwrap();
        }
        self.control
            .advance(&progress(
                &operation,
                UserAppOperationState::Succeeded,
                operation.checkpoint.clone(),
            ))
            .await
            .unwrap();
        let deleted = self
            .control
            .get_application(&app.app_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(deleted.state, UserAppLifecycleState::Deleted);
        self.control
            .recreate(
                &app.app_id,
                &app.lifecycle_id,
                &uuid::Uuid::new_v4().to_string(),
            )
            .await
            .unwrap()
    }

    async fn completed_old_compute(&self) -> ComputeControlRecord {
        let id = uuid::Uuid::new_v4().to_string();
        let pending = self
            .control
            .admit_compute_control(&ComputeControlRequest {
                builder_startup_mode: Default::default(),
                app_id: self.app.app_id.clone(),
                lifecycle_id: self.app.lifecycle_id.clone(),
                scope: UserAppOperationScope::Dev,
                operation_id: id.clone(),
                request_id: id,
                request_fingerprint: "c".repeat(64),
                action: ComputeControlAction::Restart,
                restart_image_roll: false,
            })
            .await
            .unwrap();
        let identity = ComputeExecutorIdentity {
            app_id: pending.app_id.clone(),
            lifecycle_id: pending.lifecycle_id.clone(),
            scope: pending.scope,
            operation_id: pending.operation_id.clone(),
            generation: pending.generation,
            executor_id: "computeworker".into(),
        };
        self.control
            .claim_compute_control(&identity, pending.revision)
            .await
            .unwrap();
        let receipt = UserAppOperationLeaseReceipt::Docker {
            service_type: ServiceType::UserappBuilder,
            device: 1,
            inode: 42,
            token: "original-compute-lease".into(),
        };
        let mut control = self
            .control
            .bind_compute_lease(&identity, &receipt)
            .await
            .unwrap();
        for stage in [
            ComputeControlStage::Stopping,
            ComputeControlStage::Stopped,
            ComputeControlStage::Starting,
            ComputeControlStage::Verifying,
            ComputeControlStage::Completed,
        ] {
            control = self
                .control
                .advance_compute_control(&ComputeControlProgress {
                    identity: identity.clone(),
                    expected_revision: control.revision,
                    state: if stage == ComputeControlStage::Completed {
                        ComputeControlState::Succeeded
                    } else {
                        ComputeControlState::Running
                    },
                    stage,
                    checkpoint: serde_json::json!({"original":"compute-history"}),
                    error_code: None,
                    error_message: None,
                })
                .await
                .unwrap();
        }
        self.control
            .forget_compute_lease(&identity, &receipt)
            .await
            .unwrap();
        self.control
            .get_compute_control(&self.app.app_id, &control.operation_id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn recreate_mapping_removed(&self) {
        let app = self.app.app_id.clone();
        self.owner
            .execute(move |mut db| async move {
                let count = toasty::sql::statement(
                    "DELETE FROM userapp_requests WHERE app_id=$1 AND target_kind='recreate'",
                )
                .bind(app)
                .exec(&mut db)
                .await?;
                assert!(
                    count > 0,
                    "fixture removes only already committed recreate mappings"
                );
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn replace_checkpoint(
        &self,
        operation: &UserAppOperationRecord,
        checkpoint: serde_json::Value,
    ) -> UserAppOperationRecord {
        let app_id = operation.app_id.clone();
        let lifecycle_id = operation.lifecycle_id.clone();
        let operation_id = operation.operation_id.clone();
        let bytes = checkpoint.to_string();
        self.owner.execute(move |mut db| async move {
            toasty::sql::statement("UPDATE userapp_operations SET checkpoint_json=$1 WHERE app_id=$2 AND lifecycle_id=$3 AND operation_id=$4 AND state='succeeded'")
                .bind(bytes).bind(app_id).bind(lifecycle_id).bind(operation_id).exec(&mut db).await?;
            Ok(())
        }).await.unwrap();
        self.control
            .get_operation(&self.app.app_id, &operation.operation_id)
            .await
            .unwrap()
            .unwrap()
    }

    async fn assert_unchanged(
        &self,
        before: &ProjectAndContainerInfo,
        operation: &UserAppOperationRecord,
        bytes: &str,
    ) {
        let current = self.store.get(&self.app.app_id).unwrap();
        assert_eq!(current.container_info(), before.container_info());
        assert_eq!(
            current.persistence_identity().generation,
            before.persistence_identity().generation
        );
        assert_eq!(
            current.persistence_identity().revision,
            before.persistence_identity().revision
        );
        assert_eq!(
            current.persistence_identity().container,
            before.persistence_identity().container
        );
        assert_eq!(
            current.persistence_identity().sessions,
            before.persistence_identity().sessions
        );
        assert_eq!(
            self.control
                .get_operation(&self.app.app_id, &operation.operation_id)
                .await
                .unwrap()
                .unwrap(),
            *operation
        );
        assert_eq!(self.operation_bytes(&operation.operation_id).await, bytes);
        let peer = self.peer().await;
        let persisted = peer.get(&self.app.app_id).unwrap();
        assert_eq!(
            persisted.container_info(),
            before.container_info(),
            "rejected registration rolled back database rows"
        );
        assert_eq!(
            persisted.persistence_identity().generation,
            before.persistence_identity().generation
        );
        assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    }
}

#[tokio::test]
async fn null_completion_repairs_unbound_legacy_registry_and_converges_both_replicas() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let old_basic = f.basic("podbefore", "stsbefore");
    let (historical, _) = f.completed(old_basic.clone(), false).await;
    let old = f.register_old(old_basic).await;
    let peer = f.peer().await;
    let stale_peer = f.peer().await;
    let (completed, evidence) = f.completed(f.basic("podafter", "stsafter"), false).await;
    let original_bytes = f.operation_bytes(&completed.operation_id).await;
    assert!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("stsbefore"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("stsafter"))
            .await
            .unwrap()
            .is_none()
    );
    f.store
        .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    let current = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(current.container_info().unwrap(), evidence.container);
    assert_eq!(
        current.persistence_identity().generation,
        old.persistence_identity().generation
    );
    assert_eq!(current.sessions(), old.sessions());
    assert_eq!(
        current
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .predecessor
            .as_deref(),
        Some(
            old.persistence_identity()
                .container
                .as_ref()
                .unwrap()
                .generation
                .as_str()
        )
    );
    assert_eq!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("stsafter"))
            .await
            .unwrap()
            .unwrap()
            .adopted_by_operation,
        completed.operation_id
    );
    assert_eq!(
        f.operation_bytes(&completed.operation_id).await,
        original_bytes
    );
    assert_eq!(
        f.control
            .get_operation(&f.app.app_id, &historical.operation_id)
            .await
            .unwrap()
            .unwrap(),
        historical
    );
    assert_eq!(
        peer.get(&f.app.app_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        f.uid("podbefore")
    );
    // Another replica can reconcile its old private view even after the
    // database already contains the new UID; it must not enqueue an old row.
    stale_peer
        .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    assert_eq!(
        stale_peer
            .get(&f.app.app_id)
            .unwrap()
            .container_info()
            .unwrap(),
        evidence.container
    );
    assert!(
        stale_peer
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    super::super::sync::sync_once(&peer, peer.inner(), &peer.database)
        .await
        .unwrap();
    assert_eq!(
        peer.get(&f.app.app_id).unwrap().container_info().unwrap(),
        evidence.container
    );
    let replay_generation = current
        .persistence_identity()
        .container
        .as_ref()
        .unwrap()
        .generation
        .clone();
    peer.register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    assert_eq!(
        peer.get(&f.app.app_id)
            .unwrap()
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        replay_generation
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

async fn recreated_registration_contract(
    old_binding: bool,
    old_workload_uid: bool,
    old_history: bool,
    mapping: bool,
    recreates: usize,
) {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let old_basic = f.basic("recreateoldpod", "recreateoldsts");
    let (historical, old_evidence) = f.completed(old_basic.clone(), false).await;
    if old_binding && old_workload_uid {
        f.store
            .register_completed_builder_replacement(&historical, &old_evidence, &f.volumes())
            .await
            .unwrap();
    } else {
        if old_binding {
            let (operation, evidence, volumes) =
                (historical.clone(), old_evidence.clone(), f.volumes());
            super::super::database::write_with_conflict_retry(&f.store.database, move |tx| {
                let (operation, evidence, volumes) =
                    (operation.clone(), evidence.clone(), volumes.clone());
                Box::pin(async move {
                    crate::userapp_lifecycle::bind_completed_builder_registration(
                        tx, &operation, &evidence, &volumes,
                    )
                    .await
                })
            })
            .await
            .unwrap();
        }
        let mut legacy = old_basic;
        if !old_workload_uid {
            legacy.workload_uid = None;
        }
        // Seed the actual legacy shape before any replica loads it. Turning a
        // live known UID into NULL would test a forbidden identity downgrade
        // and interfere with concurrent replicas' whole-database sync.
        f.register_old(legacy).await;
    }
    let old_binding_before = f
        .control
        .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("recreateoldsts"))
        .await
        .unwrap();
    let historical = if old_history {
        historical
    } else {
        // Simulate a retained pre-projection checkpoint. Keep its bytes rather
        // than rewriting history to fit the current registration decoder.
        f.replace_checkpoint(&historical, serde_json::json!({"historical":"opaque"}))
            .await
    };
    let history_bytes = f.operation_bytes(&historical.operation_id).await;
    let old_compute = f.completed_old_compute().await;
    let compute_bytes = crate::pg::test_support::optional_text(
        &f.owner,
        "SELECT row_to_json(c)::text FROM userapp_compute_controls c WHERE operation_id=$1",
        &old_compute.operation_id,
    )
    .await
    .unwrap();
    let stale = f.store.get(&f.app.app_id).unwrap();
    // ContainerEntry is shared by cached projects. Freeze the immutable old
    // write before publication updates that shared view to the new physical ID.
    let old_container = stale.container_info().unwrap();
    let stale_container = ContainerSnapshot::from_info(
        &old_container.container_name,
        &old_container,
        &ServiceType::UserappBuilder,
        stale.persistence_identity().container.as_ref().unwrap(),
        old_container.workload_uid.as_deref(),
    )
    .unwrap();
    let stale_project = ProjectSnapshot::from_info(&stale).unwrap();
    let peer = f.peer().await;
    let mut current = f.app.clone();
    for _ in 0..recreates {
        current = f.delete_and_recreate(&current).await;
    }
    assert_ne!(current.lifecycle_id, f.app.lifecycle_id);
    if !mapping {
        f.recreate_mapping_removed().await;
    }
    let (completed, evidence) = f
        .completed_for_application(
            &current,
            f.basic("recreatenewpod", "recreatenewsts"),
            false,
            None,
        )
        .await;
    let current_bytes = f.operation_bytes(&completed.operation_id).await;
    f.store.register_completed_builder_replacement(&completed, &evidence, &f.volumes()).await
        .unwrap_or_else(|error| panic!("current recreated builder must converge (binding={old_binding}, old_uid={old_workload_uid}, history={old_history}, mapping={mapping}, recreates={recreates}): {error:#}"));
    let registered = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(
        registered.container_info(),
        Some(evidence.container.clone())
    );
    let generation = registered
        .persistence_identity()
        .container
        .as_ref()
        .unwrap()
        .generation
        .clone();
    assert_ne!(
        generation,
        stale
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    let new_binding = f
        .control
        .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("recreatenewsts"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(new_binding.lifecycle_id, current.lifecycle_id);
    assert_eq!(new_binding.adopted_by_operation, completed.operation_id);
    // The authoritative transaction committed; another replica can reconcile
    // its old memory view without allocating another registry generation.
    peer.register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    f.store
        .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    super::super::sync::sync_once(&peer, peer.inner(), &peer.database)
        .await
        .unwrap();
    assert_eq!(
        peer.get(&f.app.app_id).unwrap().container_info(),
        Some(evidence.container.clone())
    );
    assert_eq!(
        peer.get(&f.app.app_id)
            .unwrap()
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        generation,
        "same completion is idempotent across replicas"
    );

    f.owner
        .execute(move |mut db| async move {
            assert_eq!(
                repo::upsert_container(&mut db, &stale_container).await?,
                PersistenceOperationOutcome::Superseded,
                "old immutable container write stays retired"
            );
            assert_eq!(
                repo::upsert_project(&mut db, &stale_project).await?,
                PersistenceOperationOutcome::Superseded,
                "old immutable project write cannot restore its route"
            );
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("recreateoldsts"))
            .await
            .unwrap(),
        old_binding_before,
        "old binding is immutable history"
    );
    assert_eq!(
        f.control
            .get_compute_control(&f.app.app_id, &old_compute.operation_id)
            .await
            .unwrap(),
        Some(old_compute.clone())
    );
    assert_eq!(
        crate::pg::test_support::optional_text(
            &f.owner,
            "SELECT row_to_json(c)::text FROM userapp_compute_controls c WHERE operation_id=$1",
            &old_compute.operation_id
        )
        .await
        .unwrap(),
        compute_bytes
    );
    assert_eq!(
        f.control
            .get_operation(&f.app.app_id, &historical.operation_id)
            .await
            .unwrap(),
        Some(historical.clone())
    );
    assert_eq!(
        f.operation_bytes(&historical.operation_id).await,
        history_bytes
    );
    assert_eq!(
        f.operation_bytes(&completed.operation_id).await,
        current_bytes
    );
    assert_eq!(
        f.control
            .get_application(&f.app.app_id)
            .await
            .unwrap()
            .unwrap()
            .lifecycle_id,
        current.lifecycle_id
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    f.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn recreated_current_builder_rebinds_retained_old_lifecycle_registry() {
    recreated_registration_contract(true, true, true, true, 1).await;
}

#[tokio::test]
async fn recreated_current_builder_does_not_require_legacy_registration_history() {
    for binding in [false, true] {
        for uid in [false, true] {
            for history in [false, true] {
                for mapping in [false, true] {
                    recreated_registration_contract(binding, uid, history, mapping, 1).await;
                }
            }
        }
    }
}

#[tokio::test]
async fn recreated_current_builder_skips_unused_intermediate_lifecycle_without_mapping() {
    recreated_registration_contract(true, false, false, false, 2).await;
}

#[tokio::test]
async fn current_completion_repairs_missing_project_family_but_rejects_foreign_family() {
    for foreign in [false, true] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let old = f
            .register_old(f.basic("familyoldpod", "familyoldsts"))
            .await;
        let (completed, evidence) = f
            .completed(f.basic("familynewpod", "familynewsts"), false)
            .await;
        let (operation, confirmation, volumes, app_id) = (
            completed.clone(),
            evidence.clone(),
            f.volumes(),
            f.app.app_id.clone(),
        );
        // The legacy shape and production rebind share a real transaction, so
        // concurrent fixtures' whole-database readers never see a transient
        // invalid global snapshot. The production branch reads the actual row.
        let result =
            super::super::database::write_with_conflict_retry(&f.store.database, move |tx| {
                let (operation, confirmation, volumes, app_id) = (
                    operation.clone(),
                    confirmation.clone(),
                    volumes.clone(),
                    app_id.clone(),
                );
                Box::pin(async move {
                    if foreign {
                        toasty::sql::statement(
                            "UPDATE projects SET service_type=$1 WHERE project_id=$2",
                        )
                        .bind(ServiceType::ComputerAgentRunner.to_string())
                        .bind(app_id)
                        .exec(tx)
                        .await?;
                    } else {
                        toasty::sql::statement(
                            "UPDATE projects SET service_type=NULL WHERE project_id=$1",
                        )
                        .bind(app_id)
                        .exec(tx)
                        .await?;
                    }
                    rebind(tx, &operation, &confirmation, &volumes).await
                })
            })
            .await;
        if foreign {
            assert!(
                result.is_err(),
                "an explicit other family is a real conflict"
            );
            f.assert_unchanged(
                &old,
                &completed,
                &f.operation_bytes(&completed.operation_id).await,
            )
            .await;
        } else {
            let committed =
                result.expect("missing legacy project family must not block a current completion");
            f.store
                .publish_rebound_builder(Some(old), committed)
                .unwrap();
            assert_eq!(
                f.store.get(&f.app.app_id).unwrap().service_type(),
                Some(ServiceType::UserappBuilder)
            );
            assert_eq!(
                f.store.get(&f.app.app_id).unwrap().container_info(),
                Some(evidence.container)
            );
        }
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
        f.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn same_physical_completion_persists_missing_project_family_before_publication() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let (completed, evidence) = f
        .completed(f.basic("familysamepod", "familysamests"), false)
        .await;
    f.store
        .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    let before = f.store.get(&f.app.app_id).unwrap();
    let bytes = f.operation_bytes(&completed.operation_id).await;
    let binding = f
        .control
        .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("familysamests"))
        .await
        .unwrap();
    let (operation, confirmation, volumes, app_id) = (
        completed.clone(),
        evidence.clone(),
        f.volumes(),
        f.app.app_id.clone(),
    );
    let committed =
        super::super::database::write_with_conflict_retry(&f.store.database, move |tx| {
            let (operation, confirmation, volumes, app_id) = (
                operation.clone(),
                confirmation.clone(),
                volumes.clone(),
                app_id.clone(),
            );
            Box::pin(async move {
                toasty::sql::statement("UPDATE projects SET service_type=NULL WHERE project_id=$1")
                    .bind(app_id)
                    .exec(tx)
                    .await?;
                rebind(tx, &operation, &confirmation, &volumes).await
            })
        })
        .await
        .unwrap();
    // Observe durable state before publishing the returned model: a cached
    // Some(UserappBuilder) must never hide a NULL still committed in SQL.
    assert_eq!(
        crate::pg::test_support::optional_text(
            &f.owner,
            "SELECT service_type FROM projects WHERE project_id=$1",
            &f.app.app_id
        )
        .await,
        Some(ServiceType::UserappBuilder.to_string())
    );
    let peer = f.peer().await;
    assert_eq!(
        peer.get(&f.app.app_id).unwrap().service_type(),
        Some(ServiceType::UserappBuilder)
    );
    assert_eq!(
        peer.get(&f.app.app_id).unwrap().container_info(),
        Some(evidence.container.clone())
    );
    f.store
        .publish_rebound_builder(Some(before), committed)
        .unwrap();
    assert_eq!(
        f.store.get(&f.app.app_id).unwrap().container_info(),
        Some(evidence.container)
    );
    assert_eq!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("familysamests"))
            .await
            .unwrap(),
        binding
    );
    assert_eq!(f.operation_bytes(&completed.operation_id).await, bytes);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    f.owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn current_typed_predecessor_and_same_workload_pod_recovery_survive_missing_old_uid() {
    for scenario in ["replacement", "wrong-pvc", "same-workload"] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let before = f.basic("typedoldpod", "typedoldsts");
        let (original, source) = f.completed(before.clone(), false).await;
        let (operation, confirmation, volumes) = (original.clone(), source.clone(), f.volumes());
        super::super::database::write_with_conflict_retry(&f.store.database, move |tx| {
            let (operation, confirmation, volumes) =
                (operation.clone(), confirmation.clone(), volumes.clone());
            Box::pin(async move {
                crate::userapp_lifecycle::bind_completed_builder_registration(
                    tx,
                    &operation,
                    &confirmation,
                    &volumes,
                )
                .await
            })
        })
        .await
        .unwrap();
        let original_binding = f
            .control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("typedoldsts"))
            .await
            .unwrap();
        let mut legacy = before;
        legacy.workload_uid = None;
        let old = f.register_old(legacy).await;
        let next = f.basic(
            "typednewpod",
            if scenario == "same-workload" {
                "typedoldsts"
            } else {
                "typednewsts"
            },
        );
        let (completed, evidence) = if scenario == "same-workload" {
            f.completed(next, false).await
        } else {
            f.completed_with_source(
                next,
                true,
                Some(BuilderCreationPredecessor {
                    target: source.target,
                    volumes: f.volumes(),
                }),
            )
            .await
        };
        let bytes = f.operation_bytes(&completed.operation_id).await;
        let mut volumes = f.volumes();
        if scenario == "wrong-pvc" {
            volumes[0].uid = "actually-different-current-pvc".into();
        }
        let result = f
            .store
            .register_completed_builder_replacement(&completed, &evidence, &volumes)
            .await;
        if scenario == "wrong-pvc" {
            assert!(
                result.is_err(),
                "missing old UID cannot bypass the current typed PVC constraint"
            );
            f.assert_unchanged(&old, &completed, &bytes).await;
            assert!(
                f.control
                    .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("typednewsts"))
                    .await
                    .unwrap()
                    .is_none()
            );
        } else {
            result.unwrap();
            assert_eq!(
                f.store.get(&f.app.app_id).unwrap().container_info(),
                Some(evidence.container)
            );
        }
        assert_eq!(
            f.control
                .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("typedoldsts"))
                .await
                .unwrap(),
            original_binding
        );
        assert_eq!(f.operation_bytes(&completed.operation_id).await, bytes);
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
        f.owner.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn first_completion_is_durable_and_later_success_keeps_canonical_binding() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let (first, evidence) = f.completed(f.basic("firstpod", "firststs"), true).await;
    let original_bytes = f.operation_bytes(&first.operation_id).await;
    let peer = f.peer().await;
    assert!(peer.get(&f.app.app_id).is_none());
    f.store
        .register_completed_builder_replacement(&first, &evidence, &f.volumes())
        .await
        .unwrap();
    super::super::sync::sync_once(&peer, peer.inner(), &peer.database)
        .await
        .unwrap();
    assert_eq!(
        peer.get(&f.app.app_id).unwrap().container_info().unwrap(),
        evidence.container
    );
    let binding = f
        .control
        .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("firststs"))
        .await
        .unwrap()
        .unwrap();
    let generation = f
        .store
        .get(&f.app.app_id)
        .unwrap()
        .persistence_identity()
        .container
        .as_ref()
        .unwrap()
        .generation
        .clone();
    let (second, mut second_evidence) = f.completed(evidence.container.clone(), false).await;
    f.store
        .register_completed_builder_replacement(&second, &second_evidence, &f.volumes())
        .await
        .unwrap();
    second_evidence.target.resource_binding = Some(binding.clone());
    peer.register_completed_builder_replacement(&second, &second_evidence, &f.volumes())
        .await
        .unwrap();
    assert_eq!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("firststs"))
            .await
            .unwrap()
            .unwrap(),
        binding
    );
    assert_eq!(
        f.store
            .get(&f.app.app_id)
            .unwrap()
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        generation
    );
    assert_eq!(f.operation_bytes(&first.operation_id).await, original_bytes);
    // A same-UID inspection refreshes only endpoint observations and never
    // tombstones the live physical Pod or creates another generation.
    let mut refreshed = evidence.clone();
    refreshed.container.container_ip = "10.42.0.22".into();
    refreshed.container.service_url = "http://10.42.0.22:60000".into();
    f.store
        .register_completed_builder_replacement(&first, &refreshed, &f.volumes())
        .await
        .unwrap();
    let refreshed_info = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(
        refreshed_info.container_info().unwrap().container_ip,
        "10.42.0.22"
    );
    assert_eq!(
        refreshed_info
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        generation
    );
    super::super::sync::sync_once(&peer, peer.inner(), &peer.database)
        .await
        .unwrap();
    assert_eq!(
        peer.get(&f.app.app_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_ip,
        "10.42.0.22"
    );
    assert_eq!(f.operation_bytes(&first.operation_id).await, original_bytes);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

#[tokio::test]
async fn missing_old_history_is_diagnostic_but_foreign_binding_rolls_back_registration() {
    for foreign_binding in [false, true] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let old = f.register_old(f.basic("unprovenpod", "unprovensts")).await;
        let (new, evidence) = f.completed(f.basic("newpod", "newsts"), false).await;
        if foreign_binding {
            f.completed(f.basic("unprovenpod", "unprovensts"), false)
                .await;
            f.insert_foreign_binding("unprovensts").await;
        }
        let bytes = f.operation_bytes(&new.operation_id).await;
        let result = f
            .store
            .register_completed_builder_replacement(&new, &evidence, &f.volumes())
            .await;
        if foreign_binding {
            assert!(result.is_err());
            assert!(
                f.control
                    .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("newsts"))
                    .await
                    .unwrap()
                    .is_none()
            );
            f.assert_unchanged(&old, &new, &bytes).await;
        } else {
            result.expect("missing diagnostic history cannot veto the current completed proof");
            assert_eq!(
                f.store.get(&f.app.app_id).unwrap().container_info(),
                Some(evidence.container)
            );
            assert!(
                f.control
                    .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("newsts"))
                    .await
                    .unwrap()
                    .is_some()
            );
            assert_eq!(f.operation_bytes(&new.operation_id).await, bytes);
        }
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}

#[tokio::test]
async fn same_statefulset_pod_recreation_retires_only_old_pod_and_preserves_binding() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let (first, old_evidence) = f.completed(f.basic("oldpod", "samests"), true).await;
    f.store
        .register_completed_builder_replacement(&first, &old_evidence, &f.volumes())
        .await
        .unwrap();
    let before = f.store.get(&f.app.app_id).unwrap();
    let binding = f
        .control
        .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("samests"))
        .await
        .unwrap()
        .unwrap();
    let (second, new_evidence) = f.completed(f.basic("recreatedpod", "samests"), false).await;
    f.store
        .register_completed_builder_replacement(&second, &new_evidence, &f.volumes())
        .await
        .unwrap();
    let current = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(
        current.container_info().unwrap().container_id,
        f.uid("recreatedpod")
    );
    assert_ne!(
        current
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        before
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    assert_eq!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("samests"))
            .await
            .unwrap()
            .unwrap(),
        binding
    );
    let name = new_evidence.container.container_name.clone();
    let tombstones = f
        .owner
        .execute(move |mut db| async move {
            Ok(toasty::sql::query(
                "SELECT physical_uid FROM container_tombstones WHERE container_name=$1",
            )
            .bind(name)
            .exec(&mut db)
            .await?)
        })
        .await
        .unwrap();
    assert_eq!(tombstones.len(), 1);
    assert!(
        matches!(&tombstones[0], toasty_core::stmt::Value::Record(row) if row.fields == vec![toasty_core::stmt::Value::String(f.uid("oldpod"))])
    );
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

#[tokio::test]
async fn stop_or_delete_admission_fences_registration_and_preserves_operation() {
    for stop in [true, false] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let old_basic = f.basic("fencedoldpod", "fencedoldsts");
        f.completed(old_basic.clone(), false).await;
        let old = f.register_old(old_basic).await;
        let (done, evidence) = f
            .completed(f.basic("fencednewpod", "fencednewsts"), false)
            .await;
        let bytes = f.operation_bytes(&done.operation_id).await;
        if stop {
            f.control
                .admit_compute_control(&ComputeControlRequest {
                    builder_startup_mode: Default::default(),
                    app_id: f.app.app_id.clone(),
                    lifecycle_id: f.app.lifecycle_id.clone(),
                    scope: UserAppOperationScope::Dev,
                    operation_id: format!("stop{}", uuid::Uuid::new_v4().simple()),
                    request_id: format!("stopreq{}", uuid::Uuid::new_v4().simple()),
                    request_fingerprint: "b".repeat(64),
                    action: ComputeControlAction::Stop,
                    restart_image_roll: false,
                })
                .await
                .unwrap();
        } else {
            f.control
                .admit(&UserAppAdmission {
                    app_id: f.app.app_id.clone(),
                    lifecycle_id: Some(f.app.lifecycle_id.clone()),
                    operation_id: format!("delete{}", uuid::Uuid::new_v4().simple()),
                    request_id: None,
                    request_fingerprint: "b".repeat(64),
                    kind: UserAppOperationKind::DeleteApplication,
                    command: None,
                    metadata: None,
                    runtime_policy_on_success: None,
                })
                .await
                .unwrap();
        }
        assert!(
            f.store
                .register_completed_builder_replacement(&done, &evidence, &f.volumes())
                .await
                .is_err()
        );
        assert!(
            f.control
                .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("fencednewsts"))
                .await
                .unwrap()
                .is_none()
        );
        f.assert_unchanged(&old, &done, &bytes).await;
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}

#[tokio::test]
async fn canonical_binding_conflict_and_unpersisted_other_creator_are_rejected() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let (first, evidence) = f.completed(f.basic("boundpod", "boundsts"), false).await;
    let (other, _) = f.completed(evidence.container.clone(), false).await;
    let mut forged = evidence.clone();
    forged.target.resource_binding = Some(UserAppResourceBinding {
        app_id: f.app.app_id.clone(),
        lifecycle_id: f.app.lifecycle_id.clone(),
        service_type: ServiceType::UserappBuilder,
        physical_uid: f.uid("boundsts"),
        adopted_by_operation: other.operation_id.clone(),
    });
    assert!(
        f.store
            .register_completed_builder_replacement(&first, &forged, &f.volumes())
            .await
            .is_err()
    );
    assert!(f.store.get(&f.app.app_id).is_none());
    assert!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("boundsts"))
            .await
            .unwrap()
            .is_none()
    );
    let foreign_binding = f.insert_foreign_binding("boundsts").await;
    assert!(
        f.store
            .register_completed_builder_replacement(&first, &evidence, &f.volumes())
            .await
            .is_err()
    );
    assert!(f.store.get(&f.app.app_id).is_none());
    assert_eq!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("boundsts"))
            .await
            .unwrap()
            .unwrap(),
        foreign_binding
    );
    assert_eq!(
        f.control
            .get_operation(&f.app.app_id, &first.operation_id)
            .await
            .unwrap()
            .unwrap(),
        first
    );
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

#[tokio::test]
async fn null_confirmation_cannot_bypass_another_recorded_predecessor_or_volume_proof() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let prior = f.basic("registryoldpod", "registryoldsts");
    f.completed(prior.clone(), false).await;
    let old = f.register_old(prior).await;
    let next = f.basic("targetpod", "targetsts");
    let (source_operation, source_evidence) = f
        .completed(f.basic("differentpod", "differentsts"), false)
        .await;
    let (_, recorded) = f
        .completed_with_source(
            next.clone(),
            true,
            Some(BuilderCreationPredecessor {
                target: source_evidence.target,
                volumes: f.volumes(),
            }),
        )
        .await;
    let (null_operation, null_evidence) = f.completed(next, false).await;
    let bytes = f.operation_bytes(&null_operation.operation_id).await;
    assert!(
        f.store
            .register_completed_builder_replacement(&null_operation, &null_evidence, &f.volumes())
            .await
            .is_err()
    );
    assert!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("targetsts"))
            .await
            .unwrap()
            .is_none()
    );
    f.assert_unchanged(&old, &null_operation, &bytes).await;
    assert_eq!(
        f.control
            .get_operation(&f.app.app_id, &source_operation.operation_id)
            .await
            .unwrap()
            .unwrap(),
        source_operation
    );
    assert_eq!(
        recorded
            .registration_predecessor
            .as_ref()
            .unwrap()
            .target
            .workload
            .as_ref()
            .unwrap()
            .uid,
        f.uid("differentsts")
    );
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

#[tokio::test]
async fn existing_physical_pod_without_workload_uid_is_normalized_without_retirement() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let basic = f.basic("livepod", "livests");
    let (done, evidence) = f.completed(basic.clone(), false).await;
    let mut missing_uid = basic;
    missing_uid.workload_uid = None;
    let before = f.register_old(missing_uid).await;
    let peer = f.peer().await;
    let original_bytes = f.operation_bytes(&done.operation_id).await;
    f.store
        .register_completed_builder_replacement(&done, &evidence, &f.volumes())
        .await
        .unwrap();
    let current = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(current.container_info().unwrap(), evidence.container);
    assert_eq!(
        current.persistence_identity().generation,
        before.persistence_identity().generation
    );
    assert_eq!(
        current
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        before
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    assert!(
        current
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .revision
            > before
                .persistence_identity()
                .container
                .as_ref()
                .unwrap()
                .revision
    );
    let name = evidence.container.container_name.clone();
    let tombstones = f
        .owner
        .execute(move |mut db| async move {
            Ok(toasty::sql::query(
                "SELECT physical_uid FROM container_tombstones WHERE container_name=$1",
            )
            .bind(name)
            .exec(&mut db)
            .await?)
        })
        .await
        .unwrap();
    assert!(
        tombstones.is_empty(),
        "normalizing a missing controller UID must not retire the live Pod"
    );
    super::super::sync::sync_once(&peer, peer.inner(), &peer.database)
        .await
        .unwrap();
    assert_eq!(
        peer.get(&f.app.app_id).unwrap().container_info().unwrap(),
        evidence.container
    );
    assert_eq!(f.operation_bytes(&done.operation_id).await, original_bytes);
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

#[tokio::test]
async fn missing_old_workload_uid_uses_current_proof_and_respects_foreign_binding() {
    for scenario in 0..4 {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let old_basic = f.basic("missingoldpod", "knownoldsts");
        if scenario != 3 {
            f.completed(old_basic.clone(), false).await;
        }
        if scenario == 1 {
            f.completed(f.basic("missingoldpod", "contradictingsts"), false)
                .await;
        }
        let mut unbound_old = old_basic;
        unbound_old.workload_uid = None;
        let old = f.register_old(unbound_old).await;
        let (done, evidence) = f
            .completed(f.basic("replacementpod", "replacementsts"), false)
            .await;
        if scenario == 2 {
            f.insert_foreign_binding("knownoldsts").await;
        }
        let bytes = f.operation_bytes(&done.operation_id).await;
        let result = f
            .store
            .register_completed_builder_replacement(&done, &evidence, &f.volumes())
            .await;
        if scenario != 2 {
            // Missing or inconsistent old diagnostic UIDs do not authorize a
            // physical write, and cannot veto this independently proven route.
            result.unwrap();
            assert_eq!(
                f.store
                    .get(&f.app.app_id)
                    .unwrap()
                    .container_info()
                    .unwrap(),
                evidence.container
            );
        } else {
            assert!(result.is_err());
            assert!(
                f.control
                    .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("replacementsts"))
                    .await
                    .unwrap()
                    .is_none()
            );
            f.assert_unchanged(&old, &done, &bytes).await;
        }
        assert_eq!(f.operation_bytes(&done.operation_id).await, bytes);
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}

#[tokio::test]
async fn null_confirmation_cannot_bypass_recorded_volume_preservation() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let old_basic = f.basic("volumeoldpod", "volumeoldsts");
    let (_, source) = f.completed(old_basic.clone(), false).await;
    let old = f.register_old(old_basic).await;
    let next = f.basic("volumenewpod", "volumenewsts");
    f.completed_with_source(
        next.clone(),
        true,
        Some(BuilderCreationPredecessor {
            target: source.target,
            volumes: f.volumes(),
        }),
    )
    .await;
    let (done, confirmation) = f.completed(next, false).await;
    let bytes = f.operation_bytes(&done.operation_id).await;
    let mut wrong_volumes = f.volumes();
    wrong_volumes[0].uid = "differentpvc".into();
    assert!(
        f.store
            .register_completed_builder_replacement(&done, &confirmation, &wrong_volumes)
            .await
            .is_err()
    );
    assert!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("volumenewsts"))
            .await
            .unwrap()
            .is_none()
    );
    f.assert_unchanged(&old, &done, &bytes).await;
    f.store
        .register_completed_builder_replacement(&done, &confirmation, &f.volumes())
        .await
        .unwrap();
    assert_eq!(
        f.store
            .get(&f.app.app_id)
            .unwrap()
            .container_info()
            .unwrap(),
        confirmation.container
    );
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

#[tokio::test]
async fn unreadable_older_completion_does_not_rollback_current_pg_registration() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let old_basic = f.basic("historyoldpod", "historyoldsts");
    let (previous, _) = f.completed(old_basic.clone(), false).await;
    let old = f.register_old(old_basic).await;
    let stale_peer = f.peer().await;
    let next = f.basic("historynewpod", "historynewsts");
    let (historical, _) = f.completed(next.clone(), true).await;
    let mut checkpoint = historical.checkpoint.clone();
    checkpoint["builder_creation_evidence"] =
        serde_json::json!("private historical value must stay opaque");
    let historical = f.replace_checkpoint(&historical, checkpoint).await;
    let historical_bytes = f.operation_bytes(&historical.operation_id).await;
    let (completed, evidence) = f.completed(next, false).await;
    let completed_bytes = f.operation_bytes(&completed.operation_id).await;
    let candidates = f
        .store
        .completed_builder_registration_candidates(
            &f.app.app_id,
            &f.app.lifecycle_id,
            &evidence.container.container_id,
            evidence.container.workload_uid.as_deref().unwrap(),
        )
        .await
        .unwrap();
    assert!(
        candidates.iter().any(|candidate| candidate == &historical),
        "the real SQL scan must include the malformed older same-Pod completion"
    );
    assert!(
        serde_json::from_value::<BuilderCreationEvidence>(
            historical.checkpoint["builder_creation_evidence"].clone()
        )
        .is_err()
    );

    f.store
        .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    let current = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(current.container_info().unwrap(), evidence.container);
    assert_eq!(
        current.persistence_identity().generation,
        old.persistence_identity().generation
    );
    assert_eq!(current.sessions(), old.sessions());
    let generation = current
        .persistence_identity()
        .container
        .as_ref()
        .unwrap()
        .generation
        .clone();
    // A replica with the old private view must repeat the same real root-CAS
    // transaction, including the historical scan, without a second replacement.
    stale_peer
        .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    f.store
        .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
        .await
        .unwrap();
    for store in [&f.store, &stale_peer] {
        let registered = store.get(&f.app.app_id).unwrap();
        assert_eq!(registered.container_info().unwrap(), evidence.container);
        assert_eq!(
            registered
                .persistence_identity()
                .container
                .as_ref()
                .unwrap()
                .generation,
            generation
        );
    }
    assert_eq!(
        f.control
            .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("historynewsts"))
            .await
            .unwrap()
            .unwrap()
            .adopted_by_operation,
        completed.operation_id
    );
    assert_eq!(
        f.control
            .get_operation(&f.app.app_id, &historical.operation_id)
            .await
            .unwrap()
            .unwrap(),
        historical
    );
    assert_eq!(
        f.operation_bytes(&historical.operation_id).await,
        historical_bytes
    );
    assert_eq!(
        f.operation_bytes(&completed.operation_id).await,
        completed_bytes
    );
    assert_eq!(
        f.control
            .get_operation(&f.app.app_id, &previous.operation_id)
            .await
            .unwrap()
            .unwrap(),
        previous
    );
    let persisted = f.peer().await;
    assert_eq!(
        persisted
            .get(&f.app.app_id)
            .unwrap()
            .container_info()
            .unwrap(),
        evidence.container
    );
    assert!(
        persisted
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    assert!(
        stale_peer
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
}

#[tokio::test]
async fn unreadable_history_keeps_parsed_pod_workload_and_pvc_conflicts_in_pg_protected() {
    for conflict in ["pod", "workload", "pvc"] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let old_basic = f.basic("conflictoldpod", "conflictoldsts");
        let (_, source) = f.completed(old_basic.clone(), false).await;
        let old = f.register_old(old_basic).await;
        let next = f.basic("conflictnewpod", "conflictnewsts");
        let (unreadable, _) = f.completed(next.clone(), true).await;
        let mut checkpoint = unreadable.checkpoint.clone();
        checkpoint["builder_creation_evidence"] = serde_json::json!(17);
        let unreadable = f.replace_checkpoint(&unreadable, checkpoint).await;
        let unreadable_bytes = f.operation_bytes(&unreadable.operation_id).await;
        let (parsed, mut constraint) = f
            .completed_with_source(
                next.clone(),
                true,
                Some(BuilderCreationPredecessor {
                    target: source.target,
                    volumes: f.volumes(),
                }),
            )
            .await;
        let expected_error = match conflict {
            "pod" => {
                constraint.container.container_id = "another-actual-pod".into();
                "Builder completion physical identity mismatch"
            }
            "workload" => {
                constraint.target.workload.as_mut().unwrap().uid = "another-actual-workload".into();
                "Builder completion pod or workload identity missing"
            }
            "pvc" => {
                constraint
                    .registration_predecessor
                    .as_mut()
                    .unwrap()
                    .volumes[0]
                    .uid = "another-actual-pvc".into();
                "volume identity changed"
            }
            _ => unreachable!(),
        };
        let mut checkpoint = parsed.checkpoint.clone();
        checkpoint["builder_creation_evidence"] = serde_json::to_value(&constraint).unwrap();
        let parsed = f.replace_checkpoint(&parsed, checkpoint).await;
        let parsed_bytes = f.operation_bytes(&parsed.operation_id).await;
        let (completed, evidence) = f.completed(next, false).await;
        let completed_bytes = f.operation_bytes(&completed.operation_id).await;
        let error = f
            .store
            .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains(expected_error),
            "{conflict}: {error:#}"
        );
        assert!(
            f.control
                .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("conflictnewsts"))
                .await
                .unwrap()
                .is_none(),
            "conflicting actual identities must roll back the tentative binding"
        );
        f.assert_unchanged(&old, &completed, &completed_bytes).await;
        for (operation, bytes) in [(&unreadable, unreadable_bytes), (&parsed, parsed_bytes)] {
            assert_eq!(
                f.control
                    .get_operation(&f.app.app_id, &operation.operation_id)
                    .await
                    .unwrap()
                    .unwrap(),
                *operation
            );
            assert_eq!(f.operation_bytes(&operation.operation_id).await, bytes);
        }
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}
#[tokio::test]
async fn legacy_identity_projection_recovers_corrupt_diagnostics_without_rewriting_history() {
    for missing_uid in [false, true] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let basic = f.basic("projectionoldpod", "projectionoldsts");
        let (historical, _) = f.completed(basic.clone(), false).await;
        let mut checkpoint = historical.checkpoint.clone();
        checkpoint["created_at"] = serde_json::json!({"old_format": true});
        checkpoint["container_ip"] = serde_json::json!(["unreadable", "diagnostic"]);
        checkpoint["internal_port"] = serde_json::Value::Null;
        checkpoint["service_url"] = serde_json::json!({"private": "retained old URL"});
        let historical = f.replace_checkpoint(&historical, checkpoint).await;
        let historical_bytes = f.operation_bytes(&historical.operation_id).await;
        let (unreadable, _) = f.completed(basic.clone(), false).await;
        let mut checkpoint = unreadable.checkpoint.clone();
        checkpoint["workload_uid"] = serde_json::json!(false);
        let unreadable = f.replace_checkpoint(&unreadable, checkpoint).await;
        let unreadable_bytes = f.operation_bytes(&unreadable.operation_id).await;
        assert!(
            serde_json::from_value::<ContainerBasicInfo>(historical.checkpoint.clone()).is_err()
        );
        let mut old_basic = basic;
        if missing_uid {
            old_basic.workload_uid = None;
        }
        let old = f.register_old(old_basic).await;
        let (completed, evidence) = f
            .completed(f.basic("projectionnewpod", "projectionnewsts"), false)
            .await;
        let completed_bytes = f.operation_bytes(&completed.operation_id).await;
        f.store
            .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
            .await
            .unwrap();
        let registered = f.store.get(&f.app.app_id).unwrap();
        assert_eq!(registered.container_info().unwrap(), evidence.container);
        assert_eq!(
            registered.persistence_identity().generation,
            old.persistence_identity().generation
        );
        assert_eq!(registered.sessions(), old.sessions());
        assert_eq!(
            registered
                .persistence_identity()
                .container
                .as_ref()
                .unwrap()
                .predecessor
                .as_deref(),
            Some(
                old.persistence_identity()
                    .container
                    .as_ref()
                    .unwrap()
                    .generation
                    .as_str()
            )
        );
        assert_eq!(
            f.control
                .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("projectionnewsts"))
                .await
                .unwrap()
                .unwrap()
                .adopted_by_operation,
            completed.operation_id
        );
        for (operation, bytes) in [
            (&historical, historical_bytes),
            (&unreadable, unreadable_bytes),
        ] {
            assert_eq!(
                f.control
                    .get_operation(&f.app.app_id, &operation.operation_id)
                    .await
                    .unwrap()
                    .unwrap(),
                *operation
            );
            assert_eq!(f.operation_bytes(&operation.operation_id).await, bytes);
        }
        assert_eq!(
            f.operation_bytes(&completed.operation_id).await,
            completed_bytes
        );
        let persisted = f.peer().await;
        assert_eq!(
            persisted
                .get(&f.app.app_id)
                .unwrap()
                .container_info()
                .unwrap(),
            evidence.container
        );
        assert_eq!(
            persisted.get(&f.app.app_id).unwrap().sessions(),
            old.sessions()
        );
        assert!(
            persisted
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}

#[tokio::test]
async fn legacy_diagnostic_projection_does_not_override_current_completed_proof() {
    for conflict in ["foreign-project", "workload", "only-unreadable"] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let basic = f.basic("projectionguardpod", "projectionguardsts");
        if conflict != "only-unreadable" {
            f.completed(basic.clone(), false).await;
        }
        let (historical, _) = f.completed(basic.clone(), false).await;
        let mut checkpoint = historical.checkpoint.clone();
        match conflict {
            "foreign-project" => {
                checkpoint["project_id"] = serde_json::json!("different-actual-application");
            }
            "workload" => {
                checkpoint["workload_uid"] = serde_json::json!("different-actual-workload");
            }
            "only-unreadable" => {
                checkpoint["workload_uid"] = serde_json::json!(["no proven old UID"]);
            }
            _ => unreachable!(),
        }
        checkpoint["created_at"] = serde_json::Value::Null;
        let historical = f.replace_checkpoint(&historical, checkpoint).await;
        let historical_bytes = f.operation_bytes(&historical.operation_id).await;
        let mut unbound_old = basic;
        unbound_old.workload_uid = None;
        f.register_old(unbound_old).await;
        let (completed, evidence) = f
            .completed(
                f.basic("projectionguardnewpod", "projectionguardnewsts"),
                false,
            )
            .await;
        let completed_bytes = f.operation_bytes(&completed.operation_id).await;
        f.store
            .register_completed_builder_replacement(&completed, &evidence, &f.volumes())
            .await
            .expect("old diagnostic fields are not the current physical or binding authority");
        assert_eq!(
            f.store.get(&f.app.app_id).unwrap().container_info(),
            Some(evidence.container)
        );
        assert!(
            f.control
                .get_resource_binding(
                    &ServiceType::UserappBuilder,
                    &f.uid("projectionguardnewsts")
                )
                .await
                .unwrap()
                .is_some()
        );
        assert_eq!(
            f.operation_bytes(&completed.operation_id).await,
            completed_bytes
        );
        assert_eq!(
            f.control
                .get_operation(&f.app.app_id, &historical.operation_id)
                .await
                .unwrap()
                .unwrap(),
            historical
        );
        assert_eq!(
            f.operation_bytes(&historical.operation_id).await,
            historical_bytes
        );
        assert!(
            f.store
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
    }
}
async fn commit_registration_without_publishing(
    f: &Fixture,
    operation: &UserAppOperationRecord,
    evidence: &BuilderCreationEvidence,
) -> ProjectAndContainerInfo {
    let operation = operation.clone();
    let evidence = evidence.clone();
    let volumes = f.volumes();
    super::super::database::write_with_conflict_retry(&f.store.database, move |tx| {
        let operation = operation.clone();
        let evidence = evidence.clone();
        let volumes = volumes.clone();
        Box::pin(async move { rebind(tx, &operation, &evidence, &volumes).await })
    })
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn committed_builder_registration_acknowledges_a_delayed_old_mirror_snapshot() {
    let f = Fixture::new().await.expect("disposable PG DSN required");
    let old_basic = f.basic("mirroroldpod", "mirroroldsts");
    let (historical, _) = f.completed(old_basic.clone(), false).await;
    let original = f.register_old(old_basic).await;
    let historical_bytes = f.operation_bytes(&historical.operation_id).await;
    // This real snapshot was fetched before the newer verified registration,
    // as can happen in sync_once after its drain/baseline observation.
    let (containers, projects, _) = super::super::database::snapshot(&f.store.database)
        .await
        .unwrap();
    let row = projects
        .iter()
        .find(|row| row.project_id == f.app.app_id)
        .unwrap();
    let old_snapshot =
        load::hydrate_project(row, &load::container_rows_to_map(containers)).unwrap();
    assert_eq!(old_snapshot.container_info(), original.container_info());
    let (completed, evidence) = f
        .completed(f.basic("mirrornewpod", "mirrornewsts"), false)
        .await;
    let original_completed_bytes = f.operation_bytes(&completed.operation_id).await;
    let committed = commit_registration_without_publishing(&f, &completed, &evidence).await;
    let durable_peer = f.peer().await;
    assert_eq!(
        durable_peer.get(&f.app.app_id).unwrap().container_info(),
        Some(evidence.container.clone())
    );
    // Apply exactly those old persisted rows as a different Arc, not a forged
    // physical identity or a mocked database success.
    f.store
        .inner
        .insert(f.app.app_id.clone(), Arc::new(old_snapshot))
        .unwrap();
    let current = f.store.get(&f.app.app_id).unwrap();
    assert!(!Arc::ptr_eq(&original, &current));
    assert_eq!(current.container_info(), original.container_info());
    f.store
        .publish_rebound_builder(Some(original), committed)
        .expect("a cache snapshot race cannot turn a verified committed registration into failure");
    assert!(
        Arc::ptr_eq(&current, &f.store.get(&f.app.app_id).unwrap()),
        "keep the mirror which won the local publication race"
    );
    super::super::sync::sync_once(&f.store, &f.store.inner, &f.store.database)
        .await
        .unwrap();
    assert_eq!(
        f.store.get(&f.app.app_id).unwrap().container_info(),
        Some(evidence.container)
    );
    assert_eq!(
        f.operation_bytes(&completed.operation_id).await,
        original_completed_bytes
    );
    assert_eq!(
        f.operation_bytes(&historical.operation_id).await,
        historical_bytes
    );
    assert!(
        durable_peer
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    f.owner.shutdown().await.unwrap();
}

#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL test database"]
async fn committed_builder_registration_preserves_a_later_mirror_generation_and_metadata() {
    let f = Fixture::new().await.expect("disposable PG DSN required");
    let old_basic = f.basic("mirrorfirstpod", "mirrorfirststs");
    f.completed(old_basic.clone(), false).await;
    let original = f.register_old(old_basic).await;
    let (completed, evidence) = f
        .completed(f.basic("mirrorsecondpod", "mirrorsecondsts"), false)
        .await;
    let committed = commit_registration_without_publishing(&f, &completed, &evidence).await;
    let completed_bytes = f.operation_bytes(&completed.operation_id).await;
    let (later, later_evidence) = f
        .completed(f.basic("mirrorlaterpod", "mirrorlatersts"), false)
        .await;
    f.store
        .register_completed_builder_replacement(&later, &later_evidence, &f.volumes())
        .await
        .unwrap();
    let latest = f.store.get(&f.app.app_id).unwrap();
    assert_ne!(
        latest
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation,
        committed
            .persistence_identity()
            .container
            .as_ref()
            .unwrap()
            .generation
    );
    let mut local = (*latest).clone();
    local.set_request_id(Some("later-local-metadata".into()));
    f.store
        .insert(f.app.app_id.clone(), Arc::new(local))
        .unwrap();
    assert!(f.store.wait_drained(Duration::from_secs(5)).await);
    let current = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(current.request_id(), Some("later-local-metadata"));
    f.store.publish_rebound_builder(Some(original), committed)
        .expect("an older committed response must acknowledge success without overwriting a newer local registration");
    assert!(Arc::ptr_eq(&current, &f.store.get(&f.app.app_id).unwrap()));
    assert_eq!(
        f.store.get(&f.app.app_id).unwrap().container_info(),
        Some(later_evidence.container.clone())
    );
    assert_eq!(
        f.operation_bytes(&completed.operation_id).await,
        completed_bytes
    );
    super::super::sync::sync_once(&f.store, &f.store.inner, &f.store.database)
        .await
        .unwrap();
    let synchronized = f.store.get(&f.app.app_id).unwrap();
    assert_eq!(
        synchronized.container_info(),
        Some(later_evidence.container)
    );
    assert_eq!(synchronized.request_id(), Some("later-local-metadata"));
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    f.owner.shutdown().await.unwrap();
}
struct RegistrationCommitReplyRelay {
    port: u16,
    armed: Arc<std::sync::atomic::AtomicBool>,
    observe_commits: Arc<std::sync::atomic::AtomicBool>,
    commits: Arc<std::sync::atomic::AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for RegistrationCommitReplyRelay {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl RegistrationCommitReplyRelay {
    async fn start(host: String, port: u16) -> Self {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::{TcpListener, TcpStream};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let listen_port = listener.local_addr().unwrap().port();
        let armed = Arc::new(AtomicBool::new(false));
        let observe_commits = Arc::new(AtomicBool::new(false));
        let commits = Arc::new(AtomicUsize::new(0));
        let (drop_commit, observing, count) =
            (armed.clone(), observe_commits.clone(), commits.clone());
        let task = tokio::spawn(async move {
            let mut sessions = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, _) = accepted.unwrap();
                        let host = host.clone();
                        let (drop_commit, observing, count) = (drop_commit.clone(), observing.clone(), count.clone());
                        sessions.spawn(async move {
                            let server = TcpStream::connect((host.as_str(), port)).await.unwrap();
                            let (mut cr, mut cw) = client.into_split();
                            let (mut sr, mut sw) = server.into_split();
                            tokio::select! {
                                _ = tokio::io::copy(&mut cr, &mut sw) => {},
                                _ = async {
                                    loop {
                                        // The exact backend COMMIT response proves the
                                        // server committed before its acknowledgement
                                        // is deliberately lost. No guessed SQL timing.
                                        let mut header = [0u8; 5];
                                        sr.read_exact(&mut header).await?;
                                        let length = u32::from_be_bytes(header[1..].try_into().unwrap());
                                        assert!((4..=1_048_576).contains(&length));
                                        let mut body = vec![0u8; (length - 4) as usize];
                                        sr.read_exact(&mut body).await?;
                                        if header[0] == b'C' && body == b"COMMIT\0" {
                                            if observing.load(Ordering::Acquire) { count.fetch_add(1, Ordering::AcqRel); }
                                            if drop_commit.swap(false, Ordering::AcqRel) { return Ok::<(), std::io::Error>(()); }
                                        }
                                        cw.write_all(&header).await?;
                                        cw.write_all(&body).await?;
                                    }
                                } => {},
                            }
                        });
                    },
                    Some(result) = sessions.join_next(), if !sessions.is_empty() => { result.unwrap(); }
                }
            }
        });
        Self {
            port: listen_port,
            armed,
            observe_commits,
            commits,
            task,
        }
    }
}

#[tokio::test]
#[ignore = "requires explicit disposable PostgreSQL fault fixture"]
async fn builder_registration_unknown_commit_does_not_publish_or_replay() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new().await.expect("disposable PG DSN required");
    let old_basic = f.basic("ackoldpod", "ackoldsts");
    f.completed(old_basic.clone(), false).await;
    let old = f.register_old(old_basic).await;
    let (completed, evidence) = f.completed(f.basic("acknewpod", "acknewsts"), false).await;
    let operation_bytes = f.operation_bytes(&completed.operation_id).await;
    let parsed: tokio_postgres::Config = f.dsn.parse().unwrap();
    let [tokio_postgres::config::Host::Tcp(host)] = parsed.get_hosts() else {
        panic!("real commit fault fixture requires exactly one TCP host");
    };
    let relay = RegistrationCommitReplyRelay::start(
        host.clone(),
        parsed.get_ports().first().copied().unwrap_or(5432),
    )
    .await;
    let mut config = crate::config::PostgresConfig {
        host: Some("127.0.0.1".into()),
        port: Some(relay.port),
        username: parsed.get_user().map(str::to_owned),
        password: parsed
            .get_password()
            .map(|value| String::from_utf8(value.to_vec()).unwrap()),
        database: parsed.get_dbname().map(str::to_owned),
        max_connections: Some(1),
        min_connections: Some(1),
        connect_timeout_secs: Some(2),
        ..Default::default()
    };
    // This protocol fault fixture inspects plaintext PG frames, matching the
    // existing userApp commit-reply relay; TLS negotiation is out of scope.
    config.url = Some(format!(
        "{}?sslmode=disable&application_name=registrationcommit{}",
        config.to_dsn().unwrap(),
        uuid::Uuid::new_v4().simple()
    ));
    let uncertain = PgStore::connect(
        &config,
        "registration-commit-loss".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    let before = uncertain.get(&f.app.app_id).unwrap();
    let before_container = uncertain.container_registration.lock().unwrap().clone();
    relay.observe_commits.store(true, Ordering::Release);
    relay.armed.store(true, Ordering::Release);
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        uncertain.register_completed_builder_replacement(&completed, &evidence, &f.volumes()),
    )
    .await
    .unwrap();
    assert!(
        result.is_err(),
        "an unacknowledged commit cannot be reported successful"
    );
    assert!(
        !relay.armed.load(Ordering::Acquire),
        "the server's actual COMMIT response was intercepted"
    );
    assert_eq!(
        relay.commits.load(Ordering::Acquire),
        1,
        "unknown commit must never replay the database registration"
    );
    relay.observe_commits.store(false, Ordering::Release);
    assert!(
        Arc::ptr_eq(&before, &uncertain.get(&f.app.app_id).unwrap()),
        "unknown commit cannot publish a guessed memory result"
    );
    assert_eq!(
        *uncertain.container_registration.lock().unwrap(),
        before_container
    );
    assert_eq!(
        uncertain.get(&f.app.app_id).unwrap().container_info(),
        old.container_info()
    );
    // A separate real connection observes the commit. These facts belong to
    // this fault fixture; they do not turn the unknown caller response success.
    let peer = f.peer().await;
    let committed = peer.get(&f.app.app_id).unwrap();
    assert_eq!(committed.container_info(), Some(evidence.container));
    assert_eq!(
        committed.persistence_identity().revision,
        old.persistence_identity().revision + 1
    );
    assert_eq!(
        f.operation_bytes(&completed.operation_id).await,
        operation_bytes
    );
    assert!(peer.writer().flush_and_stop(Duration::from_secs(5)).await);
    assert!(
        uncertain
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    assert!(
        f.store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    f.owner.shutdown().await.unwrap();
}
