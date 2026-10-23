use super::*;

async fn builder_fixture() -> (Fixture, String) {
    let f = Fixture::new_with_family(1, ServiceType::UserappBuilder).await;
    let (app_id, old) = f.cleaner.state.projects.iter().into_iter().next().unwrap();
    f.cleaner
        .state
        .userapp_store
        .ensure_identity(&app_id)
        .await
        .unwrap();
    let basic = old.container_info().unwrap();
    *f.runtime.builder.lock().unwrap() = Some(RuntimeContainerInfo {
        container_id: "successor-pod".into(),
        container_name: basic.container_name,
        container_ip: basic.container_ip,
        status: container_runtime_api::ContainerRuntimeStatus::Running,
        created_at: Utc::now(),
        env_vars: None,
        service_type: Some(ServiceType::UserappBuilder),
        project_id: Some(app_id.clone()),
        app_id: Some(app_id.clone()),
        user_id: None,
        pod_id: None,
        workload_uid: Some("same-controller".into()),
    });
    (f, app_id)
}

#[tokio::test]
async fn builder_successor_gets_registration_and_new_idle_grace_without_stop_or_creation() {
    let (mut f, app_id) = builder_fixture().await;
    f.activity.unreadable.store(true, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    let fresh = f.cleaner.state.get_project(&app_id).unwrap();
    assert_eq!(
        fresh.container_info().unwrap().container_id,
        "successor-pod"
    );
    assert!(
        Utc::now()
            .signed_duration_since(fresh.last_activity())
            .num_seconds()
            < 60
    );
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.activity.unreadable.store(false, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    assert!(f.cleaner.state.get_project(&app_id).is_some());
    f.close().await;
}

#[tokio::test]
async fn builder_foreign_controller_or_family_is_not_adopted_by_idle_repair() {
    let (mut f, app_id) = builder_fixture().await;
    f.runtime
        .builder
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .workload_uid = Some("foreign-controller".into());
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(
        f.cleaner
            .state
            .get_project(&app_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "physical-original"
    );
    f.runtime
        .builder
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .workload_uid = Some("same-controller".into());
    f.runtime
        .builder
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .service_type = Some(ServiceType::WebAgentRunner);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(
        f.cleaner
            .state
            .get_project(&app_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "physical-original"
    );
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}

#[tokio::test]
async fn builder_inflight_stop_keeps_original_registration_and_physical_execution() {
    let (mut f, app_id) = builder_fixture().await;
    let input = shared_types::UserAppAdmission {
        app_id: app_id.clone(),
        lifecycle_id: None,
        request_id: Some("inflight-stop".into()),
        operation_id: "stop-protection".into(),
        request_fingerprint: "a".repeat(64),
        kind: shared_types::UserAppOperationKind::StopBuilder,
        command: None,
        metadata: None,
        runtime_policy_on_success: None,
    };
    f.cleaner.state.userapp_store.admit(&input).await.unwrap();
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(
        f.cleaner
            .state
            .get_project(&app_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "physical-original"
    );
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn builder_same_controller_pod_repair_is_durable_in_real_pg_and_preserves_session() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|value| !value.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG recovery requires an isolated database"
    );
    let Some(dsn) = dsn else { return };
    let (mut f, app_id) = builder_fixture().await;
    let config = rcoder_storage::config::PostgresConfig {
        url: Some(dsn),
        ..Default::default()
    };
    let store = Arc::new(
        rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
            .await
            .unwrap(),
    );
    let backend = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(store));
    let workload = format!("builder-pg-workload-{}", uuid::Uuid::new_v4());
    *f.runtime.workload_name.lock().unwrap() = workload.clone();
    f.runtime
        .builder
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .container_name = workload.clone();
    let mut original = (*f.cleaner.state.get_project(&app_id).unwrap()).clone();
    let mut physical = original.container_info().unwrap();
    physical.container_name = workload;
    physical.container_id = format!("old-builder-pg-pod-{}", uuid::Uuid::new_v4());
    let current_pod = format!("new-builder-pg-pod-{}", uuid::Uuid::new_v4());
    f.runtime
        .builder
        .lock()
        .unwrap()
        .as_mut()
        .unwrap()
        .container_id = current_pod.clone();
    original.set_container(Some(physical));
    let original = Arc::new(original);
    backend
        .insert_project_registration_durable(app_id.clone(), original)
        .await
        .unwrap();
    let old = backend.get(&app_id).unwrap();
    let sid = format!("builder-pod-recovery-{}", uuid::Uuid::new_v4());
    assert_eq!(
        backend
            .append_session_for_container_durable(&old, &sid)
            .await
            .unwrap(),
        shared_types::persistence::PersistenceWriteOutcome::Committed
    );
    // Appending a SID is activity. Seed the intended historical idle record
    // after that acknowledged append, rather than expecting an active row to GC.
    let mut idle = (*backend.get(&app_id).unwrap()).clone();
    idle.set_timestamps(
        Utc::now() - chrono::Duration::hours(2),
        Utc::now() - chrono::Duration::hours(1),
    );
    backend
        .insert_project_registration_durable(app_id.clone(), Arc::new(idle))
        .await
        .unwrap();
    drop(
        tracing_subscriber::fmt()
            .with_env_filter("rcoder_engine=debug,rcoder_storage=debug")
            .with_test_writer()
            .try_init(),
    );
    let mut state = (*f.cleaner.state).clone();
    state.projects = backend;
    let state = Arc::new(state);
    f.cleaner.state = state.clone();
    f.cleaner.agent_scanner.state = state;
    f.cleaner.cleanup_once().await.unwrap();
    let peer =
        rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
            .await
            .unwrap();
    let fresh = peer
        .get_by_session_id_with_fetch_result(&sid)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fresh.project_id(), app_id);
    assert_eq!(fresh.container_info().unwrap().container_id, current_pod);
    assert_eq!(
        fresh.container_info().unwrap().workload_uid.as_deref(),
        Some("same-controller")
    );
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}

#[tokio::test]
async fn builder_observation_timeout_releases_admission_and_original_observation_recovers() {
    let (f, app_id) = builder_fixture().await;
    let mut state = (*f.cleaner.state).clone();
    state.config.cleanup_config.orphan_scan_budget_seconds = 1;
    f.runtime.pause_builder_at.store(2, Ordering::SeqCst);
    let old = state.get_project(&app_id).unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        crate::userapp_builder::refresh_idle_builder_pod(&state, &old),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    let lease = tokio::time::timeout(
        Duration::from_secs(1),
        state.acquire_agent_workload(
            &app_id,
            &ServiceType::UserappBuilder,
            rcoder_storage::WorkloadAdmissionMode::Shared,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    drop(lease);
    f.runtime.pause_builder_at.store(0, Ordering::SeqCst);
    assert!(
        crate::userapp_builder::refresh_idle_builder_pod(&state, &old)
            .await
            .unwrap()
    );
    assert_eq!(
        state
            .get_project(&app_id)
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "successor-pod"
    );
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}
