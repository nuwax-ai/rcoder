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
        let (store, _) = PgStore::connect(&config, "receipt-test".into(), "cluster.local".into())
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
        .0
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
async fn missing_old_history_and_conflicting_binding_roll_back_completed_registration() {
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
        assert!(
            f.store
                .register_completed_builder_replacement(&new, &evidence, &f.volumes())
                .await
                .is_err()
        );
        assert!(
            f.control
                .get_resource_binding(&ServiceType::UserappBuilder, &f.uid("newsts"))
                .await
                .unwrap()
                .is_none()
        );
        f.assert_unchanged(&old, &new, &bytes).await;
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
async fn missing_old_workload_uid_requires_unique_history_and_respects_foreign_binding() {
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
        if scenario == 0 {
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
