//! Standalone startup review fixture, compiled by the review launcher against
//! the real file_server crate in an isolated source snapshot.
//! Real HTTP Router -> DevServerManager -> real shell child/ExitStatus. The
//! owner identity endpoint and business HTTP 503 are controlled protocol fixtures;
//! this is not a real app-cli orchestration or container E2E acceptance test.
//! The fake owner starts after child spawn to avoid preflight owner reuse.
//! PROJECT_ID/APP_CLI_STATE_ROOT must be unset. The launcher sets TMPDIR inside
//! RCODER_REVIEW_WORK_DIR; the fixture verifies their physical containment.

use anyhow::{Context, Result, ensure};
use axum::{Router, http::StatusCode, routing::get};
use file_server::{Config, FileServer};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::task::JoinHandle;

struct FixtureTask(JoinHandle<()>);
impl Drop for FixtureTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

struct OwnedWorkspace(PathBuf);
#[async_trait::async_trait]
impl file_server::WorkspaceResolver for OwnedWorkspace {
    async fn resolve_project(
        &self,
        _: &file_server::ProjectContext,
    ) -> file_server::error::AppResult<PathBuf> {
        Ok(self.0.clone())
    }
    async fn resolve_computer(
        &self,
        _: &file_server::ComputerContext,
    ) -> file_server::error::AppResult<PathBuf> {
        Ok(self.0.clone())
    }
}

async fn case(exit_code: i32) -> Result<Value> {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().context("create isolated fixture")?;
    let root = temp.path();
    // UserApp registration is never subject to the legacy manager Drop PID kill.
    let project_id = format!("userapp:handover-review-{exit_code}");
    let workspace = root.join("projects").join(&project_id);
    std::fs::create_dir_all(&workspace)?;
    std::fs::write(
        workspace.join("workspace.manifest.toml"),
        "# manifest routing marker\n",
    )?;
    let gate = workspace.join("owner-gate");
    let executable = root.join("fixture-run.sh");
    std::fs::write(
        &executable,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$$\" > run-pid\ntouch owner-gate\nsleep 0.15\necho 'fixture Source result (exit {exit_code})' >&2\nexit {exit_code}\n"
        ),
    )?;
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755))?;

    let unused = std::net::TcpListener::bind("127.0.0.1:0")?;
    let owner_addr = unused.local_addr()?;
    drop(unused); // preflight must observe ConnectionRefused, not an HTTP substitute.
    let identity = shared_types::RuntimeIdentityView {
        application_id: "unknown-app".into(),
        service_family: "userapp-dev".into(),
        workspace_id: workspace.display().to_string(),
        source_root: workspace.display().to_string(),
        runtime_instance_id: format!("owned-fixture-{exit_code}"),
        deployment_generation_id: format!("owned-generation-{exit_code}"),
        protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
        capabilities: Vec::new(),
    };
    let identity = Arc::new(identity);
    let owner = FixtureTask(tokio::spawn({
        let gate = gate.clone();
        move || async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            while !gate.exists() && tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            if !gate.exists() {
                return;
            }
            let listener = tokio::net::TcpListener::bind(owner_addr)
                .await
                .expect("bind owned fixture owner");
            let router = Router::new().route(
                "/v1/runtime/identity",
                get(move || {
                    let identity = identity.clone();
                    async move {
                        axum::Json(shared_types::HttpResult::success(identity.as_ref().clone()))
                    }
                }),
            );
            axum::serve(listener, router)
                .await
                .expect("serve owned fixture owner");
        }
    }()));

    let config = Config {
        project_source_dir: root.join("projects"),
        computer_workspace_dir: root.join("computer"),
        userapp_workspace_dir: root.join("userapp"),
        log_base_dir: root.join("logs"),
        service_log_dir: root.join("file-server-logs"),
        init_project_dir: root.join("init"),
        upload_project_dir: root.join("uploads"),
        dist_target_dir: root.join("dist"),
        app_cli_bin: Some(executable.display().to_string()),
        app_cli_admin_probe_addr: owner_addr.to_string(),
        dev_alive_max_wait_ms: 700,
        dev_alive_check_timeout_ms: 100,
        dev_alive_poll_interval_ms: 50,
        ..Config::default()
    };
    let file_server = FileServer::builder(config)
        .with_workspace_resolver(Arc::new(OwnedWorkspace(workspace.clone())))
        .build()?;
    let manager = file_server.dev_server_manager();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let http_addr = listener.local_addr()?;
    let router = file_server.router()?;
    let http = FixtureTask(tokio::spawn(async move {
        axum::serve(listener, router)
            .await
            .expect("serve actual file-server router");
    }));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(12))
        .build()?;
    let url = format!("http://{http_addr}/api/build/start-dev?projectId={project_id}");
    // Capture the real supervision handle before a baseline failure may retire
    // the registration; the status remains independently observable afterward.
    let capture = tokio::spawn({
        let manager = manager.clone();
        let project_id = project_id.clone();
        async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(child) = manager.supervised_child(&project_id) {
                    return Some(child);
                }
                if tokio::time::Instant::now() >= deadline {
                    return None;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        }
    });
    let first = client.get(&url).send().await?;
    let first_status = first.status().as_u16();
    let first_body: Value = first.json().await?;
    let child = capture.await?;
    let real_exit = match child.as_ref() {
        Some(child) => child
            .wait_exit(Duration::from_secs(2))
            .await
            .map(|exit| exit.describe()),
        None => None,
    };
    let registration = manager
        .list_dev()
        .map_err(|error| anyhow::anyhow!("list_dev: {error:?}"))?
        .into_iter()
        .find(|entry| entry.project_id == project_id);
    let registered_pid = registration.as_ref().map(|entry| entry.pid);
    let pid_running = registered_pid.map(file_server::service::dev_server::is_process_running);
    let has_external_owner = registration
        .as_ref()
        .map(|entry| entry.external_owner.is_some());
    let second = client.get(&url).send().await?;
    let second_status = second.status().as_u16();
    let second_body: Value = second.json().await?;
    let owner_still_answers = client
        .get(format!("http://{owner_addr}/v1/runtime/identity"))
        .send()
        .await?
        .status()
        .is_success();
    let result = json!({
        "fixture_exit_code": exit_code,
        "first_status": first_status, "first_body": first_body,
        "child_exit": real_exit, "registered_pid": registered_pid,
        "pid_running": pid_running, "has_external_owner": has_external_owner,
        "second_status": second_status, "second_body": second_body,
        "owner_still_answers": owner_still_answers
    });
    // All HTTP listeners/tasks are this fixture's own, no process/port/name kill.
    drop(http);
    drop(owner);
    drop(file_server);
    drop(manager);
    drop(temp);
    Ok(result)
}

#[tokio::main]
async fn main() -> Result<()> {
    ensure!(
        cfg!(unix),
        "fixture requires POSIX shell and executable permissions"
    );
    ensure!(
        std::env::var_os("PROJECT_ID").is_none(),
        "unset PROJECT_ID for the isolated unknown-app fixture"
    );
    ensure!(
        std::env::var_os("APP_CLI_STATE_ROOT").is_none(),
        "unset APP_CLI_STATE_ROOT to avoid unrelated state"
    );
    let work_dir = std::fs::canonicalize(
        std::env::var("RCODER_REVIEW_WORK_DIR")
            .context("review launcher work directory is missing")?,
    )
    .context("resolve review work directory")?;
    let temp_dir = std::fs::canonicalize(
        std::env::var("TMPDIR").context("review launcher temporary directory is missing")?,
    )
    .context("resolve review temporary directory")?;
    ensure!(
        temp_dir.starts_with(&work_dir),
        "TMPDIR must be inside RCODER_REVIEW_WORK_DIR"
    );
    // Fixed production 9080 probe gets a deterministic owned HTTP 503. Refuse
    // if occupied; never replace or signal an existing listener.
    let business_listener = tokio::net::TcpListener::bind("127.0.0.1:9080")
        .await
        .context("9080 must be free for the owned nonready business fixture")?;
    let business = FixtureTask(tokio::spawn(async move {
        let router = Router::new().fallback(|| async { StatusCode::SERVICE_UNAVAILABLE });
        axum::serve(business_listener, router)
            .await
            .expect("serve owned nonready business fixture");
    }));
    let failed = case(17).await?;
    let completed = case(0).await?;
    drop(business);
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({"failed": failed, "completed": completed}))?
    );
    if std::env::args().any(|arg| arg == "--observe") {
        return Ok(());
    }
    ensure!(
        failed["first_body"]["success"] != true,
        "nonzero Source exit must not be HTTP success while management remains alive"
    );
    // This read-only owner fixture has no credentials/operation POST API, so a
    // corrected repeat may report that concrete capability error. It must reach
    // owner discovery rather than be blocked by the old dead run registration.
    ensure!(
        !completed["second_body"]
            .to_string()
            .contains("local orchestrator is already registered"),
        "completed handover must not block repeated Start behind a dead run registration"
    );
    Ok(())
}
