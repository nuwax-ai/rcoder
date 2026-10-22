use super::*;
use crate::{
    ProjectStoreBackend,
    config::PostgresConfig,
    pg::{PgStore, test_support},
};
use shared_types::{ContainerBasicInfo, ProjectAndContainerInfo, ProjectStore};

struct Fixture {
    first: Arc<PgStore>,
    peer: Arc<PgStore>,
    admin: tokio_postgres::Client,
    driver: tokio::task::JoinHandle<Result<(), tokio_postgres::Error>>,
    schema: String,
}
impl Fixture {
    async fn new() -> Option<Self> {
        let dsn = test_support::test_dsn().await?;
        let (admin, connection) = tokio_postgres::connect(&dsn, tokio_postgres::NoTls)
            .await
            .unwrap();
        let driver = tokio::spawn(connection);
        let schema = format!("workload_{}", uuid::Uuid::new_v4().simple());
        admin
            .batch_execute(&format!("CREATE SCHEMA {schema}"))
            .await
            .unwrap();
        let mut url = url::Url::parse(&dsn).unwrap();
        let pairs = url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect::<Vec<_>>();
        let options = pairs
            .iter()
            .filter(|(key, _)| key == "options")
            .map(|(_, value)| value.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        {
            let mut query = url.query_pairs_mut();
            query.clear();
            query.extend_pairs(
                pairs
                    .iter()
                    .filter(|(key, _)| key != "options")
                    .map(|(key, value)| (key.as_str(), value.as_str())),
            );
            query.append_pair("options", &format!("{options} -csearch_path={schema}"));
        }
        let query = url.query().map(|value| value.replace('+', "%20"));
        url.set_query(query.as_deref());
        let config = PostgresConfig {
            url: Some(url.into()),
            max_connections: Some(2),
            min_connections: Some(0),
            ..Default::default()
        };
        let first = Arc::new(
            PgStore::connect(&config, "test".into(), "cluster.local".into())
                .await
                .unwrap(),
        );
        let peer = Arc::new(
            PgStore::connect(&config, "test".into(), "cluster.local".into())
                .await
                .unwrap(),
        );
        Some(Self {
            first,
            peer,
            admin,
            driver,
            schema,
        })
    }
    fn backend(&self) -> ProjectStoreBackend {
        ProjectStoreBackend::Postgres(self.first.clone())
    }
    fn key(&self) -> WorkloadKey {
        WorkloadKey::new("test", ServiceType::ComputerAgentRunner, "runner-owner").unwrap()
    }
    async fn register(&self, id: &str, uid: &str) {
        let mut info = ProjectAndContainerInfo::new(id.into());
        info.set_user_id(Some("owner".into()));
        info.set_service_type(Some(ServiceType::ComputerAgentRunner));
        info.set_container(Some(ContainerBasicInfo {
            container_id: uid.into(),
            container_name: "runner-owner".into(),
            workload_uid: Some("controller".into()),
            container_ip: String::new(),
            internal_port: 50051,
            external_port: 0,
            project_id: "owner".into(),
            service_url: "http://runner-owner".into(),
            status: "running".into(),
            created_at: chrono::Utc::now(),
        }));
        assert_eq!(
            self.peer
                .insert_project_registration_durable(id.into(), Arc::new(info))
                .await
                .unwrap(),
            PersistenceWriteOutcome::Committed
        );
    }
    async fn count(&self, table: &str) -> i64 {
        self.admin
            .query_one(
                &format!("SELECT count(*) FROM {}.{table}", self.schema),
                &[],
            )
            .await
            .unwrap()
            .get(0)
    }
    async fn finish(self) {
        assert!(
            self.first
                .shutdown_flush_outcome(Duration::from_secs(5))
                .await
                .is_complete()
        );
        assert!(
            self.peer
                .shutdown_flush_outcome(Duration::from_secs(5))
                .await
                .is_complete()
        );
        self.admin
            .batch_execute(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .await
            .unwrap();
        drop(self.admin);
        self.driver.await.unwrap().unwrap();
    }
}
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(5)
}

#[test]
fn pg_workload_connector_configuration_errors_preserve_reason_without_dsn_credentials() {
    let config = PostgresConfig {
        url: Some(
            "mysql://private_user:private_password@localhost/control?password=query_secret".into(),
        ),
        ..Default::default()
    };
    let error = AdmissionPool::new(&config)
        .err()
        .expect("invalid PostgreSQL URL must fail before opening a session");
    let message = format!("{error:#}");
    assert!(message.contains("configuration"), "{message}");
    assert!(message.contains("postgresql"), "{message}");
    for secret in ["private_user", "private_password", "query_secret"] {
        assert!(!message.contains(secret), "{message}");
    }
}

#[tokio::test]
#[ignore = "requires tools/test_pg_storage_tls.py disposable PostgreSQL 17 TLS fixture"]
async fn pg_workload_verify_full_pins_tls_guard_and_rejects_wrong_ca_and_hostname() {
    let tls_config = |name: &str| PostgresConfig {
        url: Some(std::env::var(name).expect("isolated PostgreSQL TLS fixture DSN")),
        max_connections: Some(1),
        min_connections: Some(0),
        connect_timeout_secs: Some(5),
        statement_timeout_secs: Some(5),
        ..Default::default()
    };
    let store = Arc::new(
        PgStore::connect(
            &tls_config("RCODER_TLS_GOOD_DSN"),
            "tls-test".into(),
            "cluster.local".into(),
        )
        .await
        .expect("valid verify-full must also initialize the workload connector"),
    );
    let backend = ProjectStoreBackend::Postgres(store.clone());
    let key = WorkloadKey::new(
        "tls-test",
        ServiceType::ComputerAgentRunner,
        &format!("runner-tls-{}", uuid::Uuid::new_v4().simple()),
    )
    .unwrap();
    let guard = backend
        .acquire_workload_admission(key.clone(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .expect("the actual advisory-lock session must accept verify-full and sslrootcert");
    let Guard::Postgres(pg) = &guard.inner else {
        panic!("PostgreSQL admission guard expected");
    };
    let (pid, encrypted) = pg.request(Command::BackendTls).await.unwrap();
    assert!(encrypted, "the exact advisory-lock backend must use TLS");
    assert!(!guard.has_canonical_references().await.unwrap());
    let frame = backend
        .capture_workload_registration(&guard, key.logical_name())
        .await
        .unwrap();
    assert!(frame.physical_uid().is_none());
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(pg.request(Command::BackendPid).await.unwrap(), pid);
    guard.verify_current().await.unwrap();
    drop(guard);
    assert!(
        store
            .shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );

    // The same fixture explicitly allows plaintext. Verification errors must
    // fail the real guard handshake instead of falling back to that transport.
    for name in ["RCODER_TLS_BAD_CA_DSN", "RCODER_TLS_BAD_HOST_DSN"] {
        let pool = AdmissionPool::new(&tls_config(name))
            .expect("these TLS URLs are valid configuration, not unsupported syntax");
        let error = pool
            .acquire(key.clone(), WorkloadAdmissionMode::Shared, deadline())
            .await
            .err()
            .expect("invalid certificate identity must never grant admission");
        assert!(format!("{error:#}").contains("connection/TLS handshake"));
        pool.shutdown(deadline()).await.unwrap();
    }
}

#[tokio::test]
async fn pg_workload_committed_retirement_prunes_exact_local_mirror_immediately() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.register("original", "old-pod").await;
    fixture
        .first
        .refresh_project_registration("original")
        .await
        .unwrap();
    assert!(fixture.first.get("original").is_some());
    let backend = fixture.backend();
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let frame = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(
        fixture.first.get("original").is_none(),
        "no periodic sync is run between retirement and this read"
    );
    assert!(fixture.first.get_all_container_records().is_empty());
    assert_eq!(fixture.count("projects").await, 0);
    drop(guard);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_confirmed_retirement_preserves_same_uid_new_local_session() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.register("original", "pod").await;
    fixture
        .first
        .refresh_project_registration("original")
        .await
        .unwrap();
    let backend = fixture.backend();
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let frame = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    let Guard::Postgres(pg) = &guard.inner else {
        panic!("PG guard expected")
    };
    // Exact SQL completion, then a late local publication before mirror prune.
    // No test hook or change to the persistent retirement transaction is used.
    assert_eq!(
        pg.retire(frame.clone()).await.unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(
        fixture
            .first
            .inner()
            .add_session_to_project("original", "new-local-session")
    );
    fixture.first.prune_committed_workload_frame(&frame);
    let retained = fixture.first.get("original").unwrap();
    assert_eq!(retained.container_info().unwrap().container_id, "pod");
    assert!(retained.sessions().contains("new-local-session"));
    assert_eq!(
        fixture
            .first
            .get_by_session_id("new-local-session")
            .unwrap()
            .project_id(),
        "original"
    );
    assert_eq!(
        fixture.count("projects").await,
        0,
        "local retention never changes the committed SQL outcome"
    );
    drop(guard);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_guard_reads_peer_registration_without_mirror_or_json_hydration() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.register("original", "old-pod").await;
    assert!(fixture.first.get("original").is_none());
    fixture
        .admin
        .batch_execute(&format!(
            "UPDATE {}.projects SET model_provider_json='{{',agent_status_json='not-json'",
            fixture.schema
        ))
        .await
        .unwrap();
    let backend = fixture.backend();
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    assert!(guard.has_canonical_references().await.unwrap());
    let frame = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert_eq!(frame.physical_uid(), Some("old-pod"));
    assert!(fixture.first.get("original").is_none());
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert!(!guard.has_canonical_references().await.unwrap());
    assert_eq!(fixture.count("projects").await, 0);
    drop(guard);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_empty_frame_preserves_later_peer_rows() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let backend = fixture.backend();
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let empty = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    // Simulate an older writer which does not yet participate in admission.
    fixture.register("later", "new-pod").await;
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &empty)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert_eq!(fixture.count("projects").await, 1);
    assert!(guard.has_canonical_references().await.unwrap());
    drop(guard);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_null_uid_is_preserved_and_cannot_retire_newly_bound_identity() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.register("original", "old-pod").await;
    fixture
        .admin
        .batch_execute(&format!(
            "UPDATE {}.containers SET container_id=NULL",
            fixture.schema
        ))
        .await
        .unwrap();
    let backend = fixture.backend();
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let null = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert!(null.physical_uid().is_none());
    fixture
        .admin
        .batch_execute(&format!(
            "UPDATE {}.containers SET container_id='new-pod',row_revision=row_revision+1",
            fixture.schema
        ))
        .await
        .unwrap();
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &null)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert_eq!(fixture.count("containers").await, 1);
    drop(guard);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_admission_is_cross_store_and_lost_session_never_reconnects() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    let first = fixture.backend();
    let peer = ProjectStoreBackend::Postgres(fixture.peer.clone());
    let one = first
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Shared, deadline())
        .await
        .unwrap();
    let two = peer
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Shared, deadline())
        .await
        .unwrap();
    let local_held = first
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Shared, deadline())
        .await
        .unwrap();
    assert!(
        peer.acquire_workload_admission(
            fixture.key(),
            WorkloadAdmissionMode::Exclusive,
            Instant::now() + Duration::from_millis(50)
        )
        .await
        .is_err()
    );
    let Guard::Postgres(pg) = &one.inner else {
        panic!("PG guard expected")
    };
    let pid: i32 = pg.request(Command::BackendPid).await.unwrap();
    assert!(
        fixture
            .admin
            .query_one("SELECT pg_terminate_backend($1)", &[&pid])
            .await
            .unwrap()
            .get::<_, bool>(0)
    );
    // Both local pool slots were occupied. The dead session must release its
    // slot even while its public guard is still alive and is never queried.
    let replacement = first
        .acquire_workload_admission(
            WorkloadKey::new("test", ServiceType::ComputerAgentRunner, "other-workload").unwrap(),
            WorkloadAdmissionMode::Shared,
            deadline(),
        )
        .await
        .unwrap();
    assert!(one.verify_current().await.is_err());
    assert!(one.has_canonical_references().await.is_err());
    drop(one);
    drop(two);
    drop(local_held);
    drop(replacement);
    let exclusive = peer
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    exclusive.verify_current().await.unwrap();
    drop(exclusive);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_same_uid_peer_new_session_supersedes_old_frame() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.register("original", "pod").await;
    let backend = fixture.backend();
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let old = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert!(
        fixture
            .peer
            .add_session_durable("original", "fresh-sdk-session")
            .await
            .unwrap()
    );
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &old)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    let current = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert_eq!(current.session_ids(), vec!["fresh-sdk-session"]);
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &current)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Committed
    );
    assert_eq!(fixture.count("sessions").await, 0);
    drop(guard);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_captured_physical_identity_never_retires_successor() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.register("original", "old-pod").await;
    let backend = fixture.backend();
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let old = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    let changed = fixture.admin.execute(&format!("UPDATE {}.containers SET container_id='replacement-pod',workload_uid='replacement-controller',row_revision=row_revision+1 WHERE container_name='runner-owner'", fixture.schema), &[]).await.unwrap();
    assert_eq!(changed, 1);
    let actual: String = fixture
        .admin
        .query_one(
            &format!(
                "SELECT container_id FROM {}.containers WHERE container_name='runner-owner'",
                fixture.schema
            ),
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(actual, "replacement-pod");
    assert_eq!(
        backend
            .retire_workload_registration(&guard, &old)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Superseded
    );
    assert_eq!(fixture.count("projects").await, 1);
    drop(guard);
    fixture.finish().await;
}

#[tokio::test]
async fn pg_workload_retirement_commit_timeout_is_deferred_and_fences_its_session() {
    let Some(fixture) = Fixture::new().await else {
        return;
    };
    fixture.register("original", "pod").await;
    let config = PostgresConfig {
        durable_commit_timeout_ms: Some(200),
        ..fixture.first.postgres_config().clone()
    };
    let slow = Arc::new(
        PgStore::connect(&config, "test".into(), "cluster.local".into())
            .await
            .unwrap(),
    );
    fixture.admin.batch_execute(&format!("CREATE FUNCTION {s}.slow_retirement() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_sleep(0.5); RETURN OLD; END $$; CREATE CONSTRAINT TRIGGER slow_retirement AFTER DELETE ON {s}.projects DEFERRABLE INITIALLY DEFERRED FOR EACH ROW EXECUTE FUNCTION {s}.slow_retirement();", s=fixture.schema)).await.unwrap();
    let backend = ProjectStoreBackend::Postgres(slow.clone());
    let guard = backend
        .acquire_workload_admission(fixture.key(), WorkloadAdmissionMode::Exclusive, deadline())
        .await
        .unwrap();
    let frame = backend
        .capture_workload_registration(&guard, "runner-owner")
        .await
        .unwrap();
    assert!(matches!(
        backend
            .retire_workload_registration(&guard, &frame)
            .await
            .unwrap(),
        PersistenceWriteOutcome::Deferred { .. }
    ));
    assert!(guard.verify_current().await.is_err());
    // The original COMMIT may still finish. No rollback/success claim and no
    // replay are made from a timeout; all subsequent use of this guard fails.
    assert!(guard.has_canonical_references().await.is_err());
    drop(guard);
    assert!(
        slow.shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    fixture.finish().await;
}
