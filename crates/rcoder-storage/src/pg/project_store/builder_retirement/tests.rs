//! Real PostgreSQL retirement contracts; the strict PG gate requires the DSN.
use super::*;
use crate::{config::PostgresConfig, pg::test_support, userapp_lifecycle::PgUserAppStore};
use shared_types::{
    AppResourceIdentity, AppResourceKind, BuilderDeletionSnapshot, BuilderPodIdentity,
    ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore, ServiceType, UserAppAdmission,
    UserAppAdmissionOutcome, UserAppControlCommand, UserAppExecutionContext, UserAppLifecycleStore,
    UserAppOperationProgress, UserAppOperationState, UserAppStorageDestruction,
    UserappDevDeletionReceipt,
};
use std::{sync::Arc, time::Duration};

#[derive(Clone, Copy)]
enum Destruction {
    DevStorage,
    ProdStorage,
    Purge,
    Application,
}
impl Destruction {
    fn command(self) -> UserAppControlCommand {
        match self {
            Self::DevStorage => UserAppControlCommand::DestroyStorage { production: false },
            Self::ProdStorage => UserAppControlCommand::DestroyStorage { production: true },
            Self::Purge => UserAppControlCommand::DeleteResources {
                purge: true,
                expected_resource_version: None,
            },
            Self::Application => UserAppControlCommand::DeleteApplication,
        }
    }
}

struct Fixture {
    dsn: String,
    first: PgStore,
    peer: PgStore,
    target: BuilderControlTarget,
    basic: ContainerBasicInfo,
    schema: String,
    admin: toasty::Db,
}

impl Fixture {
    async fn new() -> Option<Self> {
        Self::new_kind(Destruction::DevStorage).await
    }

    async fn new_kind(destruction: Destruction) -> Option<Self> {
        Self::new_capture(destruction, false).await
    }

    async fn new_capture(destruction: Destruction, empty: bool) -> Option<Self> {
        Self::new_capture_with_history(destruction, empty, false).await
    }

    async fn new_capture_with_history(
        destruction: Destruction,
        empty: bool,
        empty_history_generation: bool,
    ) -> Option<Self> {
        let dsn = test_support::test_dsn().await?;
        // Every PgStore boot hydrates the whole schema. A temporary malformed
        // history witness must not poison another concurrently booting fixture.
        let schema = format!("retire_{}", uuid::Uuid::new_v4().simple());
        eprintln!("[pg retirement fixture] owned schema={schema}");
        let mut admin = toasty::Db::builder().connect(&dsn).await.unwrap();
        toasty::sql::statement(format!("CREATE SCHEMA {schema}"))
            .exec(&mut admin)
            .await
            .unwrap();
        let mut scoped = url::Url::parse(&dsn).unwrap();
        let parameters = scoped
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        let options = parameters
            .iter()
            .filter(|(key, _)| key == "options")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        {
            let mut query = scoped.query_pairs_mut();
            query.clear();
            query.extend_pairs(
                parameters
                    .iter()
                    .filter(|(key, _)| key != "options")
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            );
            query.append_pair("options", &format!("{options} -csearch_path={schema}"));
        }
        // PostgreSQL URI options use percent-encoded spaces, not form '+'.
        // query_pairs_mut already encodes a literal query '+' as %2B; only its
        // generated space markers change, never userinfo/password characters.
        let postgres_query = scoped.query().map(|query| query.replace('+', "%20"));
        scoped.set_query(postgres_query.as_deref());
        let dsn: String = scoped.into();
        let config = PostgresConfig {
            url: Some(dsn.clone()),
            // 钉住 600ms：本文件超时语义测试(如 unknown_commit)依赖 durable
            // deadline < lock_timeout(2s)，与运行期默认值(15000ms)解耦。
            durable_commit_timeout_ms: Some(600),
            ..Default::default()
        };
        let control = PgUserAppStore::connect(&config).await.unwrap();
        let app = format!("rt{}", test_support::uuid_suffix());
        let lifecycle = control.ensure_identity(&app).await.unwrap();
        let first = PgStore::connect(&config, "retirement-test".into(), "cluster.local".into())
            .await
            .unwrap();
        let basic = ContainerBasicInfo {
            project_id: app.clone(),
            container_id: format!("pod-{app}"),
            container_name: format!("builder-{app}"),
            workload_uid: Some(format!("workload-{app}")),
            container_ip: "10.42.0.10".into(),
            internal_port: 50051,
            external_port: 0,
            service_url: "http://10.42.0.10:60000".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
        };
        let mut project = ProjectAndContainerInfo::new(app.clone());
        project.set_service_type(Some(ServiceType::UserappBuilder));
        project.set_container(Some(basic.clone()));
        assert_eq!(
            first
                .insert_project_registration_durable(app.clone(), Arc::new(project))
                .await
                .unwrap(),
            shared_types::persistence::PersistenceWriteOutcome::Committed
        );
        let peer = PgStore::connect(&config, "retirement-peer".into(), "cluster.local".into())
            .await
            .unwrap();
        let operation = match control
            .admit(&UserAppAdmission {
                app_id: app.clone(),
                lifecycle_id: Some(lifecycle.lifecycle_id.clone()),
                operation_id: format!("retire-{app}"),
                kind: destruction.command().kind(),
                command: Some(destruction.command()),
                request_id: Some(format!("request-{app}")),
                request_fingerprint: "a".repeat(64),
                metadata: None,
                runtime_policy_on_success: None,
            })
            .await
            .unwrap()
        {
            UserAppAdmissionOutcome::Accepted(operation) => operation,
            other => panic!("unexpected admission {other:?}"),
        };
        let executor = format!("executor-{app}");
        let claimed = control
            .advance(&UserAppOperationProgress {
                app_id: app.clone(),
                lifecycle_id: lifecycle.lifecycle_id.clone(),
                operation_id: operation.operation_id.clone(),
                expected_revision: operation.revision,
                executor_id: executor.clone(),
                state: UserAppOperationState::Running,
                step: "claimed".into(),
                checkpoint: serde_json::Value::Null,
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        let context = UserAppExecutionContext {
            app_id: app.clone(),
            lifecycle_id: lifecycle.lifecycle_id,
            operation_id: operation.operation_id.clone(),
            executor_id: executor.clone(),
            request_fingerprint: "a".repeat(64),
        };
        let workload = AppResourceIdentity {
            kind: AppResourceKind::StatefulSet,
            name: basic.container_name.clone(),
            uid: basic.workload_uid.clone().unwrap(),
            resource_version: Some("7".into()),
        };
        let target = BuilderControlTarget {
            context: context.clone(),
            resource_binding: None,
            workload: (!empty).then_some(workload.clone()),
            pod: (!empty).then(|| BuilderPodIdentity {
                name: format!("{}-0", basic.container_name),
                uid: basic.container_id.clone(),
                resource_version: "8".into(),
            }),
            restart_image: None,
            restart_runtime_workspace: None,
            builder_startup_mode: Default::default(),
        };
        let mut development = UserappDevDeletionReceipt {
            runtime: BuilderDeletionSnapshot {
                resource_binding: None,
                app_id: app.clone(),
                operation_id: format!("physical-{app}"),
                resources: if empty { vec![] } else { vec![workload] },
                docker_bind_cleanup: false,
            },
            registry: empty.then(|| shared_types::BuilderRegistryIdentity {
                generation: first
                    .get(&app)
                    .unwrap()
                    .persistence_identity()
                    .generation
                    .clone(),
                container_id: basic.container_id.clone(),
            }),
            lease: None,
            collaborators: vec![],
            directories: None,
        };
        if empty_history_generation {
            development
                .registry
                .as_mut()
                .expect("historical Empty registry capture")
                .generation
                .clear();
        }
        let production = shared_types::AppDeletionSnapshot {
            app_id: app.clone(),
            operation_id: format!("production-{app}"),
            resources: vec![],
            directories: None,
        };
        let (step, evidence) = match destruction {
            Destruction::DevStorage => (
                "storage_captured",
                serde_json::to_value(UserAppStorageDestruction {
                    context: context.clone(),
                    production: None,
                    development,
                })
                .unwrap(),
            ),
            Destruction::ProdStorage => (
                "storage_captured",
                serde_json::to_value(UserAppStorageDestruction {
                    context: context.clone(),
                    production: Some(production),
                    development,
                })
                .unwrap(),
            ),
            Destruction::Purge | Destruction::Application => (
                "resources_captured",
                serde_json::to_value(shared_types::UserAppDeletionCheckpoint {
                    schema_version: 1,
                    stage: shared_types::UserAppDeletionStage::Captured,
                    context: context.clone(),
                    kind: destruction.command().kind(),
                    production,
                    development: Some(development),
                })
                .unwrap(),
            ),
        };
        control
            .advance(&UserAppOperationProgress {
                app_id: app,
                lifecycle_id: claimed.lifecycle_id,
                operation_id: operation.operation_id,
                expected_revision: claimed.revision,
                executor_id: executor,
                state: UserAppOperationState::Running,
                step: step.into(),
                checkpoint: evidence,
                error_code: None,
                error_message: None,
            })
            .await
            .unwrap();
        Some(Self {
            dsn,
            first,
            peer,
            target,
            basic,
            schema,
            admin,
        })
    }

    async fn finish(mut self) {
        assert!(
            self.first
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
        assert!(
            self.peer
                .writer()
                .flush_and_stop(Duration::from_secs(5))
                .await
        );
        toasty::sql::statement(format!("DROP SCHEMA {} CASCADE", self.schema))
            .exec(&mut self.admin)
            .await
            .unwrap();
    }

    async fn physical(&self) -> Option<String> {
        let app = self.target.context.app_id.clone();
        database::read(&self.peer.database, move |tx| {
            Box::pin(async move {
                let project = models::Project::filter_by_project_id(&app)
                    .first()
                    .exec(tx)
                    .await?;
                let Some(name) = project.and_then(|project| project.container_name) else {
                    return Ok(None);
                };
                Ok(models::Container::filter_by_container_name(name)
                    .first()
                    .exec(tx)
                    .await?
                    .and_then(|container| container.container_id))
            })
        })
        .await
        .unwrap()
    }

    async fn sql(&self, sql: &'static str, value: String) {
        let app = self.target.context.app_id.clone();
        self.peer
            .database
            .execute(move |mut db| async move {
                toasty::sql::statement(sql)
                    .bind(app)
                    .bind(value)
                    .exec(&mut db)
                    .await?;
                Ok(())
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn pg_builder_retirement_uses_canonical_epoch_with_and_without_mirror_refill() {
    for refill in [false, true] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        let source = f
            .peer
            .capture_project_stop_registration(&f.target.context.app_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            f.peer
                .clear_sessions_if_current_durable(&source)
                .await
                .unwrap(),
            shared_types::persistence::PersistenceWriteOutcome::Committed
        );
        if refill {
            f.first
                .refresh_project_registration(&f.target.context.app_id)
                .await
                .unwrap();
        }
        f.first
            .retire_captured_builder_registration(&f.target)
            .await
            .unwrap();
        assert!(
            f.physical().await.is_none(),
            "canonical successor must retire regardless of mirror timing"
        );
        f.finish().await;
    }
}

#[tokio::test]
async fn pg_builder_retirement_ignores_unrelated_invalid_history_payload() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    f.sql(
        "UPDATE projects SET model_provider_json=$2,agent_status_json=$2 WHERE project_id=$1",
        "{not-valid-json".into(),
    )
    .await;
    f.sql(
        "UPDATE containers SET logical_id=$2 WHERE logical_id=$1",
        "unknown-app".into(),
    )
    .await;
    f.first
        .retire_captured_builder_registration(&f.target)
        .await
        .unwrap();
    assert!(f.physical().await.is_none());
    f.finish().await;
}

#[tokio::test]
async fn pg_builder_retirement_preserves_replacement_and_foreign_family() {
    for foreign in [false, true] {
        let Some(f) = Fixture::new().await else {
            return;
        };
        if foreign {
            f.sql(
                "UPDATE projects SET service_type=$2 WHERE project_id=$1",
                ServiceType::ComputerAgentRunner.to_string(),
            )
            .await;
        } else {
            f.sql(
                "UPDATE containers SET container_id=$2 WHERE logical_id=$1",
                "actual-new-pod-p3".into(),
            )
            .await;
        }
        let error = f
            .first
            .retire_captured_builder_registration(&f.target)
            .await
            .unwrap_err();
        let expected = if foreign {
            "another service family"
        } else {
            "Pod was replaced"
        };
        assert!(
            format!("{error:#}").contains(expected),
            "wrong protection reason: {error:#}"
        );
        assert!(f.physical().await.is_some());
        f.finish().await;
    }
}

#[tokio::test]
async fn pg_builder_retirement_unknown_commit_reads_original_receipt_without_retiring_replacement()
{
    let Some(f) = Fixture::new().await else {
        return;
    };
    let (mut connection, driver) = tokio_postgres::connect(&f.dsn, tokio_postgres::NoTls)
        .await
        .unwrap();
    let driver = tokio::spawn(driver);
    let blocker = connection.transaction().await.unwrap();
    let writer_pid: i32 = blocker
        .query_one("SELECT pg_backend_pid()", &[])
        .await
        .unwrap()
        .get(0);
    blocker
        .query_one(
            "SELECT project_id FROM projects WHERE project_id=$1 FOR UPDATE",
            &[&f.target.context.app_id],
        )
        .await
        .unwrap();
    let error = f
        .first
        .retire_captured_builder_registration(&f.target)
        .await
        .unwrap_err();
    assert!(
        error
            .downcast_ref::<shared_types::OperationOutcomeUnknown>()
            .is_some(),
        "deadline must retain unknown outcome: {error:#}"
    );
    let blocked: bool = blocker.query_one("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE wait_event_type='Lock' AND $1=ANY(pg_blocking_pids(pid)))", &[&writer_pid]).await.unwrap().get(0);
    assert!(
        blocked,
        "original accepted SQL must really be blocked, not rejected before submission"
    );
    blocker.commit().await.unwrap();
    let (request, _) = receipt_identity(&f.target).unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let id = request.clone();
            if database::read(&f.peer.database, move |tx| {
                Box::pin(async move {
                    Ok(models::ProjectWriteReceipt::filter_by_request_id(id)
                        .first()
                        .exec(tx)
                        .await?
                        .is_some())
                })
            })
            .await
            .unwrap()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the original accepted transaction must complete after its row lock releases");
    assert!(f.physical().await.is_none());
    // Install a later physical registration through the ordinary project API.
    // A replay may observe the old receipt but cannot remove this new instance.
    let mut replacement = ProjectAndContainerInfo::new(f.target.context.app_id.clone());
    replacement.set_service_type(Some(ServiceType::UserappBuilder));
    let mut basic = f.basic.clone();
    basic.container_id = "actual-new-pod-p3".into();
    replacement.set_container(Some(basic));
    let config = PostgresConfig {
        url: Some(f.dsn.clone()),
        ..Default::default()
    };
    let replacement_store = PgStore::connect(
        &config,
        "retirement-replacement".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    assert_eq!(
        replacement_store
            .insert_project_registration_durable(
                f.target.context.app_id.clone(),
                Arc::new(replacement)
            )
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    let mut replay = f.target.clone();
    replay.context.executor_id = "new-observer-executor".into();
    replay.workload.as_mut().unwrap().resource_version = Some("renewed-workload-rv".into());
    replay.pod.as_mut().unwrap().resource_version = "renewed-pod-rv".into();
    f.first
        .retire_captured_builder_registration(&replay)
        .await
        .unwrap();
    assert_eq!(f.physical().await.as_deref(), Some("actual-new-pod-p3"));
    let mut retargeted = replay;
    retargeted.pod.as_mut().unwrap().uid = "actual-new-pod-p3".into();
    let error = f
        .first
        .retire_captured_builder_registration(&retargeted)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("request identity was reused"),
        "retarget must fail at the original receipt, not an unrelated authority check: {error:#}"
    );
    drop(connection);
    driver.await.unwrap().unwrap();
    assert!(
        replacement_store
            .writer()
            .flush_and_stop(Duration::from_secs(5))
            .await
    );
    f.finish().await;
}

#[tokio::test]
async fn pg_builder_retirement_shared_actual_target_blocks_before_any_mutation() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let other_id = format!("other-{}", f.target.context.app_id);
    let mut other = ProjectAndContainerInfo::new(other_id.clone());
    other.set_service_type(Some(ServiceType::UserappBuilder));
    let mut physical = f.basic.clone();
    physical.container_name = format!("alias-{}", f.basic.container_name);
    other.set_container(Some(physical));
    assert_eq!(
        f.peer
            .insert_project_registration_durable(other_id.clone(), Arc::new(other))
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    // The primary row drifts to old P1; the actual captured P2 is still shared
    // through a different name. Looking only at canonical P1 would miss it.
    f.sql("UPDATE containers SET container_id=$2 WHERE logical_id=$1 AND container_name LIKE 'builder-%'", "old-pod-p1".into()).await;
    let observed = f
        .first
        .check_captured_builder_registration(&f.target)
        .await
        .unwrap_err();
    let retired = f
        .first
        .retire_captured_builder_registration(&f.target)
        .await
        .unwrap_err();
    assert!(
        format!("{observed:#}").contains("shared references"),
        "pre-delete must identify actual P2 sharing: {observed:#}"
    );
    assert!(
        format!("{retired:#}").contains("shared references"),
        "retirement must identify actual P2 sharing: {retired:#}"
    );
    assert!(f.physical().await.is_some());
    let id = other_id;
    assert!(
        database::read(&f.peer.database, move |tx| Box::pin(async move {
            Ok(models::Project::filter_by_project_id(id)
                .first()
                .exec(tx)
                .await?
                .is_some())
        }))
        .await
        .unwrap()
    );
    f.finish().await;
}

#[tokio::test]
async fn pg_builder_retirement_unavailable_observation_is_never_absence() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    f.first.database = crate::db::owner::DatabaseOwner::closed_for_test();
    assert!(
        f.first
            .check_captured_builder_registration(&f.target)
            .await
            .is_err()
    );
    assert!(
        f.first
            .retire_captured_builder_registration(&f.target)
            .await
            .is_err()
    );
    assert_eq!(f.physical().await, Some(f.basic.container_id.clone()));
    f.finish().await;
}

#[tokio::test]
async fn pg_builder_retirement_all_destructive_entry_kinds_use_original_capture() {
    for kind in [
        Destruction::DevStorage,
        Destruction::ProdStorage,
        Destruction::Purge,
        Destruction::Application,
    ] {
        let Some(f) = Fixture::new_kind(kind).await else {
            return;
        };
        f.first
            .check_captured_builder_registration(&f.target)
            .await
            .unwrap();
        f.first
            .retire_captured_builder_registration(&f.target)
            .await
            .unwrap();
        assert!(f.physical().await.is_none());
        f.finish().await;
    }
}

#[tokio::test]
async fn pg_builder_retirement_empty_capture_retires_original_stale_physical_registry() {
    let Some(f) = Fixture::new_capture(Destruction::DevStorage, true).await else {
        return;
    };
    f.first
        .check_captured_builder_registration(&f.target)
        .await
        .unwrap();
    f.first
        .retire_captured_builder_registration(&f.target)
        .await
        .unwrap();
    assert!(f.physical().await.is_none());
    f.finish().await;
}

#[tokio::test]
async fn pg_builder_retirement_empty_capture_preserves_later_p3_registration() {
    let Some(f) = Fixture::new_capture(Destruction::DevStorage, true).await else {
        return;
    };
    f.sql(
        "UPDATE containers SET container_id=$2 WHERE logical_id=$1",
        format!("new-pod-p3-{}", f.target.context.app_id),
    )
    .await;
    let observed = f
        .first
        .check_captured_builder_registration(&f.target)
        .await
        .unwrap_err();
    let retired = f
        .first
        .retire_captured_builder_registration(&f.target)
        .await
        .unwrap_err();
    assert!(
        format!("{observed:#}").contains("appeared after the empty deletion capture"),
        "wrong P3 pre-delete reason: {observed:#}"
    );
    assert!(
        format!("{retired:#}").contains("appeared after the empty deletion capture"),
        "wrong P3 retirement reason: {retired:#}"
    );
    assert_eq!(
        f.physical().await,
        Some(format!("new-pod-p3-{}", f.target.context.app_id))
    );
    f.finish().await;
}

#[tokio::test]
async fn pg_builder_retirement_empty_history_generation_retains_current_authority_and_p3_fence() {
    for kind in [Destruction::DevStorage, Destruction::Purge] {
        for replacement in [false, true] {
            let Some(f) = Fixture::new_capture_with_history(kind, true, true).await else {
                return;
            };
            let control = PgUserAppStore::connect(&PostgresConfig {
                url: Some(f.dsn.clone()),
                ..Default::default()
            })
            .await
            .unwrap();
            let original = control
                .get_operation(&f.target.context.app_id, &f.target.context.operation_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                original.checkpoint["development"]["registry"]["generation"], "",
                "the persisted historical receipt must retain its original empty diagnostic field"
            );
            control.shutdown().await.unwrap();
            let app = f.target.context.app_id.clone();
            let generation = database::read(&f.peer.database, move |tx| {
                Box::pin(async move {
                    Ok(models::Project::filter_by_project_id(&app)
                        .first()
                        .exec(tx)
                        .await?
                        .expect("current canonical project")
                        .generation)
                })
            })
            .await
            .unwrap();
            assert!(
                !generation.is_empty(),
                "current canonical generation remains valid and authoritative"
            );
            if replacement {
                let p3 = format!("new-pod-p3-{}", f.target.context.app_id);
                f.sql("UPDATE containers SET container_id=$2,row_revision=row_revision+1 WHERE logical_id=$1", p3.clone()).await;
                assert_eq!(
                    f.physical().await,
                    Some(p3.clone()),
                    "the exact canonical P3 must really exist before retirement"
                );
                let error = f
                    .first
                    .retire_captured_builder_registration(&f.target)
                    .await
                    .unwrap_err();
                assert!(
                    format!("{error:#}").contains("appeared after the empty deletion capture"),
                    "historical generation must not weaken actual P3 protection: {error:#}"
                );
                assert_eq!(f.physical().await, Some(p3));
            } else {
                f.first
                    .retire_captured_builder_registration(&f.target)
                    .await
                    .unwrap();
                assert!(f.physical().await.is_none());
            }
            f.finish().await;
        }
    }
}
