//! Real PostgreSQL session loss through the actual background-task assembly.
//! Runtime business methods are controlled; no daily RCoder or application is used.
use super::*;
use container_runtime_api::{
    AgentContainerRuntime, ContainerCreateParams, ContainerRuntimeResult, DeploymentStatus,
    RemovedContainerInfo, RuntimeContainerInfo, UserAppDeploymentRuntime, WorkspaceRuntime,
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Self;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
    fn count(&self, needle: &str) -> usize {
        self.text().matches(needle).count()
    }
}

#[derive(Default)]
struct Runtime {
    syncs: AtomicUsize,
    scans: AtomicUsize,
}
#[async_trait::async_trait]
impl AgentContainerRuntime for Runtime {
    async fn create_container(
        &self,
        _: ContainerCreateParams,
    ) -> ContainerRuntimeResult<shared_types::ContainerBasicInfo> {
        panic!("background fault test cannot create applications")
    }
    async fn get_container_info(
        &self,
        _: &str,
    ) -> ContainerRuntimeResult<Option<shared_types::ContainerBasicInfo>> {
        Ok(None)
    }
    async fn find_container(
        &self,
        _: &str,
        _: &shared_types::ServiceType,
    ) -> ContainerRuntimeResult<Option<RuntimeContainerInfo>> {
        Ok(None)
    }
    async fn stop_container(&self, _: &str) -> ContainerRuntimeResult<()> {
        panic!("background fault test cannot stop applications")
    }
    async fn is_container_running(&self, _: &str) -> ContainerRuntimeResult<bool> {
        Ok(false)
    }
    async fn list_containers(&self) -> ContainerRuntimeResult<Vec<RuntimeContainerInfo>> {
        Ok(vec![])
    }
    async fn cleanup_all(&self) -> ContainerRuntimeResult<()> {
        panic!("background fault test cannot clean shared resources")
    }
    async fn health_check(&self) -> ContainerRuntimeResult<()> {
        Ok(())
    }
    async fn sync_states(&self) -> ContainerRuntimeResult<(u32, Vec<RemovedContainerInfo>)> {
        self.syncs.fetch_add(1, Ordering::SeqCst);
        Ok((0, vec![]))
    }
}
#[async_trait::async_trait]
impl WorkspaceRuntime for Runtime {}
#[async_trait::async_trait]
impl UserAppDeploymentRuntime for Runtime {
    async fn list_deployments(&self) -> ContainerRuntimeResult<Vec<DeploymentStatus>> {
        self.scans.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
}

async fn sql(container: &str, user: &str, database: &str, query: &str) -> String {
    let output = tokio::process::Command::new("docker")
        .args([
            "exec",
            container,
            "psql",
            "-XqAt",
            "-v",
            "ON_ERROR_STOP=1",
            "-U",
            user,
            "-d",
            database,
            "-c",
            query,
        ])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "run-owned PG query failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

async fn wait_for(capture: &Capture, condition: impl Fn() -> bool, label: &str) {
    let result = tokio::time::timeout(Duration::from_secs(35), async {
        while !condition() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    assert!(result.is_ok(), "{label}: {}", capture.text());
}

#[tokio::test]
#[ignore = "requires a run-owned local Docker/PostgreSQL fixture; use leader_pg_fixture.py"]
async fn real_pg_session_loss_restarts_four_tasks_on_the_same_app_state() {
    let container = std::env::var("RCODER_PG_FAULT_CONTAINER").expect("run-owned PG container");
    let run = std::env::var("RCODER_PG_FAULT_RUN_ID").expect("run ownership marker");
    let dsn = std::env::var("RCODER_PG_TEST_DSN").expect("run-owned PG DSN");
    let user = std::env::var("RCODER_PG_FAULT_USER").unwrap_or_else(|_| "contract".into());
    let database = std::env::var("RCODER_PG_FAULT_DATABASE").unwrap_or_else(|_| "contract".into());
    assert!(run.len() == 32 && run.bytes().all(|byte| byte.is_ascii_hexdigit()));
    let identity = tokio::process::Command::new("docker")
        .args([
            "inspect",
            "--format",
            "{{index .Config.Labels \"rcoder.e2e.run\"}} {{.Id}}",
            &container,
        ])
        .output()
        .await
        .unwrap();
    assert!(identity.status.success());
    assert_eq!(
        String::from_utf8(identity.stdout).unwrap().trim(),
        format!("{run} {container}")
    );
    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let temporary = tempfile::tempdir().unwrap();
    let userapp = Arc::new(
        rcoder_storage::userapp_lifecycle::TursoUserAppStore::open_exclusive(
            &temporary.path().join("userapp.db"),
        )
        .await
        .unwrap(),
    );
    let runtime = Arc::new(Runtime::default());
    let mut state =
        crate::userapp_builder::builder_runtime_test_state(runtime.clone(), userapp.clone(), None)
            .await;
    let mut config = state.config.clone();
    let mut url = url::Url::parse(&dsn).unwrap();
    assert_eq!(
        url.host_str(),
        Some("127.0.0.1"),
        "fault fixture cannot target a remote PG"
    );
    let port = tokio::process::Command::new("docker")
        .args(["port", &container, "5432/tcp"])
        .output()
        .await
        .unwrap();
    assert!(port.status.success());
    assert_eq!(
        String::from_utf8(port.stdout).unwrap().trim(),
        format!("127.0.0.1:{}", url.port().expect("explicit fixture port")),
        "DSN must belong to the captured run-owned PostgreSQL"
    );
    let application = format!("leaderfault{run}");
    url.query_pairs_mut()
        .append_pair("application_name", &application)
        .append_pair("sslmode", "disable");
    config.storage.backend = crate::config::StorageBackend::Postgres;
    config.storage.postgres = rcoder_storage::config::PostgresConfig {
        url: Some(url.into()),
        ..Default::default()
    };
    config.cleanup_config.enabled = true;
    config.cleanup_config.cleanup_interval_seconds = 1;
    config.cleanup_config.idle_timeout_seconds = 86400;
    config.cleanup_config.log_cleanup.log_dir =
        temporary.path().join("logs").to_string_lossy().into_owned();
    config.userapp_recycle.enabled = true;
    config.userapp_recycle.scan_interval_seconds = 1;
    config.userapp_recycle.idle_timeout_seconds = 86400;
    let pg = rcoder_storage::pg::PgStore::connect(
        &config.storage.postgres,
        "fault-test".into(),
        "cluster.local".into(),
    )
    .await
    .unwrap();
    let pg = Arc::new(pg);
    let unique = Arc::get_mut(&mut state).unwrap();
    unique.config = config.clone();
    unique.projects = Arc::new(rcoder_storage::ProjectStoreBackend::Postgres(pg.clone()));
    let state_identity = Arc::as_ptr(&state);
    let (shutdown, _) = tokio::sync::broadcast::channel(4);
    let handles = start_all_background_tasks(&config, state.clone(), shutdown.clone())
        .await
        .unwrap();
    let markers = [
        "[cleaner] cleanup task already started",
        "[STATUS_CHECKER] Starting container status checker",
        "[CONTAINER_SYNC] Starting container state sync task",
        "[USERAPP_RECYCLE] scanner started",
    ];
    wait_for(
        &capture,
        || markers.iter().all(|marker| capture.count(marker) >= 1),
        "first actual generation started",
    )
    .await;
    let key = rcoder_storage::pg::leader_selection::LEADER_LOCK_KEY;
    let holder_query = format!(
        "SELECT a.pid FROM pg_locks l JOIN pg_stat_activity a USING(pid) WHERE l.locktype='advisory' AND l.granted AND a.datname=current_database() AND a.application_name='{application}' AND l.classid::bigint={} AND l.objid::bigint={}",
        key >> 32,
        key & 0xffff_ffff
    );
    let old_pid = sql(&container, &user, &database, &holder_query)
        .await
        .parse::<i32>()
        .unwrap();
    let mut blocker = tokio::process::Command::new("docker")
        .args([
            "exec",
            "-i",
            &container,
            "psql",
            "-XqAt",
            "-v",
            "ON_ERROR_STOP=1",
            "-U",
            &user,
            "-d",
            &database,
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut input = blocker.stdin.take().unwrap();
    input.write_all(format!("SET application_name='barrier{run}';\nSELECT pg_advisory_lock({key});\nSELECT 'barrier-acquired';\n").as_bytes()).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let waiting=sql(&container,&user,&database,&format!("SELECT count(*) FROM pg_stat_activity WHERE application_name='barrier{run}' AND wait_event_type='Lock' AND datname=current_database()")).await;
            if waiting=="1" {break;}
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }).await.expect("independent barrier must be waiting before fault injection");
    let killed=sql(&container,&user,&database,&format!("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE pid={old_pid} AND application_name='{application}' AND datname=current_database()")).await;
    assert_eq!(killed, "t");
    let mut output = tokio::io::BufReader::new(blocker.stdout.take().unwrap()).lines();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let line = output
                .next_line()
                .await
                .unwrap()
                .expect("barrier session exited before acquiring its lock");
            if line == "barrier-acquired" {
                break;
            }
        }
    })
    .await
    .unwrap();
    wait_for(
        &capture,
        || capture.count("leader stepped down") >= 1,
        "real PG session loss stopped the first generation",
    )
    .await;
    let stop_markers = [
        "[cleaner] shutdown",
        "[STATUS_CHECKER] shutdown",
        "[CONTAINER_SYNC] shutdown",
        "[USERAPP_RECYCLE] shutdown",
    ];
    wait_for(
        &capture,
        || stop_markers.iter().all(|marker| capture.count(marker) >= 1),
        "all first-generation workers actually exited",
    )
    .await;
    let prior_syncs = runtime.syncs.load(Ordering::SeqCst);
    let prior_scans = runtime.scans.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_secs(6)).await;
    assert!(
        sql(&container, &user, &database, &holder_query)
            .await
            .is_empty(),
        "barrier must prevent reacquisition during the follower window"
    );
    assert_eq!(
        runtime.syncs.load(Ordering::SeqCst),
        prior_syncs,
        "sync must not execute as follower"
    );
    assert_eq!(
        runtime.scans.load(Ordering::SeqCst),
        prior_scans,
        "recycle must not execute as follower"
    );
    let restart_log_offset = capture.text().len();
    input
        .write_all(format!("SELECT pg_advisory_unlock({key});\n\\q\n").as_bytes())
        .await
        .unwrap();
    drop(input);
    assert!(
        tokio::time::timeout(Duration::from_secs(5), blocker.wait())
            .await
            .unwrap()
            .unwrap()
            .success()
    );
    wait_for(
        &capture,
        || markers.iter().all(|marker| capture.count(marker) >= 2),
        "second actual generation started on the same AppState",
    )
    .await;
    wait_for(
        &capture,
        || {
            let logs = capture.text();
            let resumed = &logs[restart_log_offset..];
            resumed.contains("[cleaner] Found 0 idle agents to clean")
                && resumed.contains("[STATUS_CHECKER] No containers to check")
                && runtime.syncs.load(Ordering::SeqCst) > prior_syncs
                && runtime.scans.load(Ordering::SeqCst) > prior_scans
        },
        "all four actual workers executed after reacquisition",
    )
    .await;
    let new_pid = sql(&container, &user, &database, &holder_query)
        .await
        .parse::<i32>()
        .unwrap();
    assert_ne!(new_pid, old_pid);
    assert_eq!(Arc::as_ptr(&state), state_identity);
    assert!(
        runtime.syncs.load(Ordering::SeqCst) >= 2,
        "actual container sync ran in both generations"
    );
    assert!(
        runtime.scans.load(Ordering::SeqCst) >= 2,
        "actual UserApp scanner queried its service"
    );
    assert!(
        !capture.text().contains("leader task spawn failed"),
        "{}",
        capture.text()
    );
    let _ = shutdown.send(());
    handles
        .drain(tokio::time::Instant::now() + Duration::from_secs(15))
        .await
        .unwrap();
    assert!(
        sql(&container, &user, &database, &holder_query)
            .await
            .is_empty()
    );
    assert!(
        pg.shutdown_flush_outcome(Duration::from_secs(5))
            .await
            .is_complete()
    );
    userapp.shutdown().await.unwrap();
    if let Some(path) = std::env::var_os("RCODER_PG_FAULT_REPORT") {
        let report = serde_json::json!({"success":true,"scope":"real_pg_session_and_actual_engine_tasks_controlled_runtime","same_app_state":true,"old_backend_pid":old_pid,"new_backend_pid":new_pid,"task_starts":markers.map(|marker|(marker,capture.count(marker))),"sync_calls":runtime.syncs.load(Ordering::SeqCst),"scanner_calls":runtime.scans.load(Ordering::SeqCst)});
        std::fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        std::fs::write(
            std::path::Path::new(&path).with_extension("log"),
            capture.text(),
        )
        .unwrap();
    }
}
