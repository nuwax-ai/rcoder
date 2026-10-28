//! Recovery is exercised through actual scanner/cleaner sweeps and registration.
use super::*;

#[tokio::test]
async fn missing_compute_retires_stale_shared_registration_without_physical_stop() {
    let mut f = Fixture::new(2).await;
    f.activity.unreadable.store(true, Ordering::SeqCst);
    f.runtime.missing.store(true, Ordering::SeqCst);
    let result = f.cleaner.cleanup_once().await.unwrap();
    assert!(f.cleaner.state.projects.iter().is_empty());
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    assert_eq!(
        result.containers_destroyed, 0,
        "metadata convergence must not claim a physical Stop"
    );
    f.close().await;
}

#[tokio::test]
async fn observation_error_preserves_registration_and_original_scan_recovers() {
    let mut f = Fixture::new(2).await;
    f.activity.unreadable.store(true, Ordering::SeqCst);
    f.runtime.missing.store(true, Ordering::SeqCst);
    f.runtime.absence_unreadable.store(true, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert_eq!(f.cleaner.state.projects.iter().len(), 2);
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.runtime.absence_unreadable.store(false, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    assert!(f.cleaner.state.projects.iter().is_empty());
    f.close().await;
}

#[tokio::test]
async fn unavailable_rpc_with_present_compute_keeps_registration() {
    let mut f = Fixture::new(2).await;
    f.activity.unreadable.store(true, Ordering::SeqCst);
    for _ in 0..3 {
        f.cleaner.cleanup_once().await.unwrap();
    }
    assert_eq!(f.cleaner.state.projects.iter().len(), 2);
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}

#[tokio::test]
async fn presence_reappearing_under_admission_prevents_registration_retirement() {
    let f = Fixture::new(1).await;
    f.runtime.missing.store(true, Ordering::SeqCst);
    f.runtime.presence_at_check.store(2, Ordering::SeqCst);
    let old = f.cleaner.state.get_project("idle-project-0").unwrap();
    assert!(
        !f.cleaner
            .state
            .retire_absent_ordinary_registration(&old, Duration::from_secs(60))
            .await
            .unwrap()
    );
    assert_eq!(f.cleaner.state.projects.iter().len(), 1);
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}

#[tokio::test]
async fn fresh_shared_reference_prevents_retiring_missing_compute_registration() {
    let f = Fixture::new(2).await;
    f.runtime.missing.store(true, Ordering::SeqCst);
    let old = f.cleaner.state.get_project("idle-project-0").unwrap();
    f.cleaner.state.update_activity("idle-project-1");
    assert!(
        !f.cleaner
            .state
            .retire_absent_ordinary_registration(&old, Duration::from_secs(60))
            .await
            .unwrap()
    );
    assert_eq!(f.cleaner.state.projects.iter().len(), 2);
    f.close().await;
}

#[tokio::test]
async fn delayed_absence_snapshot_cannot_retire_replaced_registration() {
    let f = Fixture::new(1).await;
    let old = f.cleaner.state.get_project("idle-project-0").unwrap();
    f.runtime.missing.store(true, Ordering::SeqCst);
    f.runtime.pause_absence.store(true, Ordering::SeqCst);
    let state = f.cleaner.state.clone();
    let retiring = tokio::spawn(async move {
        state
            .retire_absent_ordinary_registration(&old, Duration::from_secs(60))
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), f.runtime.absence_entered.notified())
        .await
        .unwrap();
    f.replace_physical("successor-pod").await;
    f.runtime.pause_absence.store(false, Ordering::SeqCst);
    f.runtime.absence_release.notify_one();
    assert!(!retiring.await.unwrap().unwrap());
    assert_eq!(
        f.cleaner
            .state
            .get_project("idle-project-0")
            .unwrap()
            .container_info()
            .unwrap()
            .container_id,
        "successor-pod"
    );
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}

#[cfg(feature = "rcoder-pg")]
#[tokio::test]
async fn missing_compute_registration_retirement_is_durable_and_peer_cannot_route_old_session() {
    let dsn = std::env::var("RCODER_PG_TEST_DSN")
        .ok()
        .filter(|value| !value.is_empty());
    assert!(
        dsn.is_some() || std::env::var("RCODER_PG_TEST_STRICT").as_deref() != Ok("1"),
        "Strict PG recovery requires an isolated database"
    );
    let Some(dsn) = dsn else { return };
    let mut f = Fixture::new(1).await;
    let old = f.cleaner.state.get_project("idle-project-0").unwrap();
    let id = format!("absent-pg-project-{}", uuid::Uuid::new_v4());
    let workload = format!("absent-pg-workload-{}", uuid::Uuid::new_v4());
    *f.runtime.workload_name.lock().unwrap() = workload.clone();
    let mut physical = old.container_info().unwrap();
    physical.container_name = workload;
    physical.container_id = format!("absent-pg-pod-{}", uuid::Uuid::new_v4());
    let sid = format!("absent-pg-session-{}", uuid::Uuid::new_v4());
    let project = ProjectAndContainerInfo::from_parts(
        id.clone(),
        Some("idle-owner".into()),
        None,
        Some(sid.clone()),
        Some(physical),
        ProjectExtendedFields {
            service_type: Some(ServiceType::ComputerAgentRunner),
            created_at: Some(Utc::now() - chrono::Duration::hours(2)),
            last_activity: Some(Utc::now() - chrono::Duration::hours(1)),
            ..Default::default()
        },
    );
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
    backend
        .insert_project_registration_durable(id.clone(), Arc::new(project))
        .await
        .unwrap();
    // Seed historical routing atomically with its old activity time. Session
    // append correctly touches activity and is not a historical-data fixture.
    let before =
        rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
            .await
            .unwrap();
    assert!(
        before
            .get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .is_some()
    );
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
    f.runtime.missing.store(true, Ordering::SeqCst);
    f.activity.unreadable.store(true, Ordering::SeqCst);
    f.cleaner.cleanup_once().await.unwrap();
    let peer =
        rcoder_storage::pg::PgStore::connect(&config, "test-ns".into(), "cluster.local".into())
            .await
            .unwrap();
    assert!(
        peer.get_by_session_id_with_fetch_result(&sid)
            .await
            .unwrap()
            .is_none()
    );
    assert!(f.cleaner.state.get_project(&id).is_none());
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}

#[tokio::test]
async fn absence_observation_timeout_leaves_no_permanent_recovery_gate() {
    let f = Fixture::new(1).await;
    let mut state = (*f.cleaner.state).clone();
    state.config.cleanup_config.orphan_scan_budget_seconds = 1;
    f.runtime.missing.store(true, Ordering::SeqCst);
    f.runtime.pause_absence.store(true, Ordering::SeqCst);
    let old = state.get_project("idle-project-0").unwrap();
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        state.retire_absent_ordinary_registration(&old, Duration::from_secs(60)),
    )
    .await
    .unwrap();
    assert!(result.is_err());
    assert!(state.get_project("idle-project-0").is_some());
    f.runtime.pause_absence.store(false, Ordering::SeqCst);
    assert!(
        state
            .retire_absent_ordinary_registration(&old, Duration::from_secs(60))
            .await
            .unwrap()
    );
    assert_eq!(f.runtime.stops.load(Ordering::SeqCst), 0);
    f.close().await;
}
