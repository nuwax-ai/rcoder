//! Canonical Empty cleanup must not depend on this replica's routing mirror.
use super::*;

struct Fixture {
    runtime: Arc<Runtime>,
    first: Arc<rcoder_storage::pg::PgStore>,
    peer: Arc<rcoder_storage::pg::PgStore>,
    control: Arc<rcoder_storage::userapp_lifecycle::TursoUserAppStore>,
    projects: Arc<crate::storage::ProjectStoreBackend>,
    schema: String,
    admin: toasty::Db,
}

impl Fixture {
    async fn new() -> Option<Self> {
        let dsn = std::env::var("RCODER_PG_TEST_DSN")
            .ok()
            .filter(|value| !value.is_empty());
        assert!(
            dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
            "Strict Empty cleanup PG contract requires a disposable DSN"
        );
        let Some(dsn) = dsn else {
            eprintln!("[skip] RCODER_PG_TEST_DSN missing for canonical Empty cleanup contract");
            return None;
        };
        let schema = format!("empty_cleanup_{}", uuid::Uuid::new_v4().simple());
        eprintln!("Canonical Empty cleanup fixture schema: {schema}");
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
        let postgres_query = scoped.query().map(|query| query.replace('+', "%20"));
        scoped.set_query(postgres_query.as_deref());
        let config = rcoder_storage::config::PostgresConfig {
            url: Some(scoped.into()),
            ..Default::default()
        };
        let control = Arc::new(
            rcoder_storage::userapp_lifecycle::TursoUserAppStore::connect(&config)
                .await
                .unwrap(),
        );
        let runtime = Runtime::new(format!(
            "empty{}",
            &uuid::Uuid::new_v4().simple().to_string()[..12]
        ));
        runtime.empty.store(true, Ordering::SeqCst);
        control.ensure_identity(&runtime.app).await.unwrap();
        // Connect the executing replica first. A peer then commits the row;
        // neither startup hydration nor a synthetic local insert can supply it.
        let first = rcoder_storage::pg::PgStore::connect(
            &config,
            "empty-first".into(),
            "cluster.local".into(),
        )
        .await
        .unwrap();
        let first = Arc::new(first);
        assert!(first.get(&runtime.app).is_none());
        let peer = rcoder_storage::pg::PgStore::connect(
            &config,
            "empty-peer".into(),
            "cluster.local".into(),
        )
        .await
        .unwrap();
        let peer = Arc::new(peer);
        assert_eq!(
            peer.insert_project_registration_durable(
                runtime.app.clone(),
                Arc::new(runtime.project("old-pod-p1"))
            )
            .await
            .unwrap(),
            shared_types::persistence::PersistenceWriteOutcome::Committed
        );
        assert!(
            first.get(&runtime.app).is_none(),
            "peer commit must not silently hydrate first mirror"
        );
        let projects = Arc::new(crate::storage::ProjectStoreBackend::Postgres(first.clone()));
        Some(Self {
            runtime,
            first,
            peer,
            control,
            projects,
            schema,
            admin,
        })
    }

    async fn task(&self) -> tokio::task::JoinHandle<Result<(), String>> {
        let ticket = capture_cleanup(&self.runtime, &self.projects, self.control.clone()).await;
        assert!(
            self.first.get(&self.runtime.app).is_none(),
            "read-only canonical capture must not publish routing history"
        );
        let mut task = tokio::spawn(ticket.cleanup());
        tokio::select! {
            result=&mut task=>panic!("Empty cleanup returned before its exact original deletion barrier: {result:?}"),
            result=tokio::time::timeout(Duration::from_secs(5),self.runtime.entered.notified())=>result.expect("Empty cleanup physical observation did not complete"),
        }
        task
    }

    async fn replace_after_capture(&mut self) {
        // Fault-inject a real new canonical physical UID, not a cache-only edit.
        // It cannot be retired merely because the earlier runtime was Empty.
        let changed = toasty::sql::query(format!(
            "UPDATE {}.containers SET container_id=$3,row_revision=row_revision+1 WHERE container_name=$1 AND container_id=$2 RETURNING container_id",
            self.schema
        ))
        .bind(self.runtime.basic("old-pod-p1").container_name)
        .bind(self.runtime.basic("old-pod-p1").container_id)
        .bind(self.runtime.basic("new-pod-p3").container_id)
        .exec(&mut self.admin)
        .await
        .unwrap();
        assert_eq!(
            changed.len(),
            1,
            "fault update must affect the original exact canonical P1 row"
        );
        self.assert_canonical_p3().await;
    }

    async fn assert_canonical_p3(&mut self) {
        let current = toasty::sql::query(format!(
            "SELECT 1 FROM {}.containers WHERE container_name=$1 AND container_id=$2",
            self.schema
        ))
        .bind(self.runtime.basic("new-pod-p3").container_name)
        .bind(self.runtime.basic("new-pod-p3").container_id)
        .exec(&mut self.admin)
        .await
        .unwrap();
        assert_eq!(
            current.len(),
            1,
            "canonical SQL must contain exact new P3, independently of mirror publication"
        );
    }

    async fn reject_invalid_current_generation(&mut self) {
        let sessions = toasty::sql::query(format!(
            "SELECT 1 FROM {}.sessions WHERE project_id=$1",
            self.schema
        ))
        .bind(self.runtime.app.clone())
        .exec(&mut self.admin)
        .await
        .unwrap();
        assert!(
            sessions.is_empty(),
            "this fixture must not confuse a session FK failure with generation validation"
        );
        let generation = self
            .peer
            .get(&self.runtime.app)
            .unwrap()
            .persistence_identity()
            .generation
            .clone();
        let outcome = toasty::sql::query(format!(
            "UPDATE {}.projects SET generation='',row_revision=row_revision+1 WHERE project_id=$1 AND generation=$2 RETURNING generation",
            self.schema
        ))
        .bind(self.runtime.app.clone()).bind(generation.clone()).exec(&mut self.admin).await;
        let Err(error) = outcome else {
            panic!("normal canonical generation constraint must reject an empty current row")
        };
        assert!(
            format!("{error:#}").contains("projects_generation_check"),
            "must fail on the actual generation constraint: {error:#}"
        );
        let observed = toasty::sql::query(format!(
            "SELECT 1 FROM {}.projects WHERE project_id=$1 AND generation=$2",
            self.schema
        ))
        .bind(self.runtime.app.clone())
        .bind(generation)
        .exec(&mut self.admin)
        .await
        .unwrap();
        assert_eq!(
            observed.len(),
            1,
            "rejected invalid write must leave the original canonical generation intact"
        );
    }

    async fn corrupt_unrelated_history(&mut self) {
        let changed = toasty::sql::query(format!(
            "UPDATE {}.projects SET model_provider_json=$2,agent_status_json=$2 WHERE project_id=$1 RETURNING project_id",
            self.schema
        ))
        .bind(self.runtime.app.clone())
        .bind("{not-valid-json")
        .exec(&mut self.admin)
        .await
        .unwrap();
        assert_eq!(
            changed.len(),
            1,
            "only this fixture's canonical history must be corrupted"
        );
    }

    async fn finish(mut self) {
        self.control.shutdown().await.unwrap();
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
}

#[tokio::test]
async fn pg_captured_cleanup_empty_canonical_p1_without_local_mirror_finishes() {
    let Some(f) = Fixture::new().await else {
        return;
    };
    let task = f.task().await;
    let result = finish_cleanup(&f.runtime, task).await;
    assert!(
        result.is_ok(),
        "canonical old P1 must retire despite this replica's missing mirror: {result:?}"
    );
    assert!(
        f.peer
            .refresh_project_registration(&f.runtime.app)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.runtime.physical_deletes.load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
async fn pg_captured_cleanup_empty_canonical_p3_after_capture_is_preserved() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    let task = f.task().await;
    f.replace_after_capture().await;
    let error = finish_cleanup(&f.runtime, task).await.unwrap_err();
    assert!(
        error.contains("appeared after the empty deletion capture"),
        "must protect the actual new P3: {error}"
    );
    f.assert_canonical_p3().await;
    assert_eq!(f.runtime.physical_deletes.load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
async fn pg_captured_cleanup_empty_canonical_p1_invalid_history_does_not_hydrate_mirror() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    // Both stores were connected before this controlled corruption. A full
    // project hydrate now would fail; the original capture only needs identity.
    f.corrupt_unrelated_history().await;
    let task = f.task().await;
    let result = finish_cleanup(&f.runtime, task).await;
    assert!(
        result.is_ok(),
        "unrelated old JSON must not gate canonical Empty cleanup: {result:?}"
    );
    assert!(
        f.first.get(&f.runtime.app).is_none(),
        "read-only capture must not publish malformed history"
    );
    assert!(
        f.peer
            .refresh_project_registration(&f.runtime.app)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.runtime.physical_deletes.load(Ordering::SeqCst), 0);
    f.finish().await;
}

#[tokio::test]
async fn pg_captured_cleanup_empty_rejects_invalid_current_generation_then_finishes() {
    let Some(mut f) = Fixture::new().await else {
        return;
    };
    f.reject_invalid_current_generation().await;
    let task = f.task().await;
    finish_cleanup(&f.runtime, task).await.unwrap();
    assert!(
        f.peer
            .refresh_project_registration(&f.runtime.app)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(f.runtime.physical_deletes.load(Ordering::SeqCst), 0);
    f.finish().await;
}
