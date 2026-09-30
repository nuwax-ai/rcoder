//! Recover the management process before reconciling stale transport registrations.
//! The owner itself proves exclusion and child cleanup; a refused TCP connection
//! only authorizes attempting bootstrap, never deleting runtime journals.
use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result, ensure};
use shared_types::RuntimeIdentityView;

use super::{
    DevServerManager,
    owner_client::{self, OwnerClient, OwnerProbe},
    support::lock,
};
use crate::error::{AppError, AppResult};

pub(super) struct AvailableOwner {
    pub address: String,
    pub identity: RuntimeIdentityView,
}

impl DevServerManager {
    pub(super) async fn recover_owner_if_needed(
        &self,
        project: &str,
        workspace: &Path,
    ) -> AppResult<Option<AvailableOwner>> {
        self.recover_owner_inner(project, workspace)
            .await
            .map_err(|error| AppError::owner_error("recover app-cli management service", error))
    }

    async fn recover_owner_inner(
        &self,
        project: &str,
        workspace: &Path,
    ) -> Result<Option<AvailableOwner>> {
        let snapshot = self.read_external_state()?;
        let address = snapshot.owner_address(project, &self.config.app_cli_admin_probe_addr);
        let Some(identity) = self
            .ensure_owner_available(project, workspace, address, snapshot.needs_owner(project))
            .await?
        else {
            return Ok(None);
        };
        if snapshot.has_replaced_registration(project, &identity.runtime_instance_id) {
            let app = std::env::var("PROJECT_ID")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            owner_client::verify_project_identity(&identity, workspace, &app)?;
            let (_, token) = owner_client::find_owner_token(Path::new(&identity.source_root), &app)
                .context("recovered owner control credentials unavailable")?;
            let client = OwnerClient::new(address, &token)?;
            let evidence = client
                .recovery()
                .await
                .context("successor startup cleanup is not confirmed")?;
            ensure!(
                evidence.runtime_instance_id == identity.runtime_instance_id,
                "owner changed while reading recovery evidence"
            );
            // No replay of old writes. Unknown deployment/migration results stay
            // in the authoritative owner's kernel and remain queryable there.
            self.retire_replaced_registration(project, &snapshot, &identity.runtime_instance_id)?;
        }
        Ok(Some(AvailableOwner {
            address: address.to_owned(),
            identity,
        }))
    }

    pub(super) async fn ensure_owner_available(
        &self,
        project: &str,
        workspace: &Path,
        address: &str,
        required: bool,
    ) -> Result<Option<RuntimeIdentityView>> {
        let observation = owner_client::observe_owner(address).await?;
        let identity = match observation {
            OwnerProbe::Ready(identity) => identity,
            OwnerProbe::Legacy => return Ok(None), // registered local run is stopped by its retained child
            OwnerProbe::Absent if !required => return Ok(None),
            // recovery v2 §5.4：活 owner 的初始化窗口（统一 owner 首个业务
            // 会话恢复中）只做有界等待——原生控制通道活着就不是"无 owner"，
            // 派生竞争 bootstrap 会与常驻 serve 抢锁/转交，制造多余进程。
            OwnerProbe::Initializing => {
                let root = runtime_state_layout::ensure_state_root(
                    workspace,
                    std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
                    std::env::var_os("PROJECT_ID").as_deref(),
                )?;
                std::fs::create_dir_all(&root)?;
                self.wait_for_recovery_owner(project, workspace, address, &root)
                    .await?
            }
            OwnerProbe::Absent => {
                let root = runtime_state_layout::ensure_state_root(
                    workspace,
                    std::env::var_os("APP_CLI_STATE_ROOT").as_deref(),
                    std::env::var_os("PROJECT_ID").as_deref(),
                )?;
                std::fs::create_dir_all(&root)?;
                let bootstrap = std::fs::OpenOptions::new()
                    .create(true)
                    .truncate(false)
                    .read(true)
                    .write(true)
                    .open(root.join("owner-bootstrap.lock"))?;
                match bootstrap.try_lock() {
                    Ok(()) => {}
                    Err(std::fs::TryLockError::WouldBlock) => {
                        // Another caller is restoring the same management service.
                        // Observe its result instead of failing an ordinary start.
                        return self
                            .wait_for_recovery_owner(project, workspace, address, &root)
                            .await
                            .map(Some);
                    }
                    Err(error) => return Err(error).context("lock owner bootstrap"),
                }
                // Recheck after claiming bootstrap. app-cli also takes its own
                // authoritative OwnerGuard before binding or any runtime write.
                match owner_client::observe_owner(address).await? {
                    OwnerProbe::Ready(identity) => identity,
                    OwnerProbe::Legacy => return Ok(None),
                    observation => {
                        if matches!(observation, OwnerProbe::Absent) {
                            self.spawn_recovery_owner(project, workspace, address)
                                .await?;
                        }
                        self.wait_for_recovery_owner(project, workspace, address, &root)
                            .await?
                    }
                }
            }
        };
        Ok(Some(identity))
    }

    async fn spawn_recovery_owner(
        &self,
        project: &str,
        workspace: &Path,
        address: &str,
    ) -> Result<()> {
        if lock(&self.owner_children)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .get(project)
            .is_some_and(|child| child.exited().is_none())
        {
            return Ok(());
        }
        // This helper starts locally; a remote discovery address cannot authorize
        // launching a different local coordinator.
        let address: std::net::SocketAddr = address
            .parse()
            .context("owner bootstrap requires a numeric loopback address")?;
        ensure!(
            address.ip().is_loopback(),
            "owner bootstrap requires a local loopback endpoint"
        );
        let directory = super::log::log_dir(&self.config, project).join("app-cli");
        tokio::fs::create_dir_all(&directory).await?;
        let log_path = directory.join("owner-recovery.log");
        let output = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let mut command =
            tokio::process::Command::new(self.config.app_cli_bin.as_deref().unwrap_or("app-cli"));
        let bind = std::net::SocketAddr::new(
            if address.is_ipv4() {
                std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)
            } else {
                std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)
            },
            address.port(),
        );
        command
            .arg("serve")
            .arg("--control-only")
            .arg("--workspace")
            .arg(workspace)
            .arg("--log-dir")
            .arg(&directory)
            .arg("--admin-addr")
            .arg(bind.to_string())
            .current_dir(workspace)
            .stdin(std::process::Stdio::null())
            .stdout(output.try_clone()?)
            .stderr(output);
        process_utils::command_authority::detach_command(&mut command);
        #[cfg(unix)]
        command.process_group(0);
        let child = command
            .spawn()
            .context("spawn app-cli serve --control-only (requires matching app-cli build)")?;
        let tracked = super::SupervisedChild::adopt(
            child,
            Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new())),
        );
        lock(&self.owner_children)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .insert(project.into(), tracked);
        tracing::info!(%project, log = %log_path.display(), "started app-cli management recovery without business autostart");
        Ok(())
    }

    async fn wait_for_recovery_owner(
        &self,
        project: &str,
        workspace: &Path,
        address: &str,
        root: &Path,
    ) -> Result<RuntimeIdentityView> {
        let origin = runtime_state_layout::resolve_project_origin(workspace)?;
        let mut diagnostic = None;
        let mut resumed_shutdown = false;
        let mut recovery_request: Option<(String, runtime_supervisor::Request)> = None;
        let result = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                // Check the winner first: another managed supervisor can win the
                // OwnerGuard race while our control-only candidate exits.
                if let OwnerProbe::Ready(identity) = owner_client::observe_owner(address).await? {
                    return Ok(identity);
                }
                let observation = runtime_supervisor::control(
                    root,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                )
                .await;
                let observation = match observation {
                    Err(error)
                        if error
                            .downcast_ref::<runtime_supervisor::Problem>()
                            .is_some_and(|problem| {
                                problem.code == runtime_supervisor::FailureCode::ProtocolMismatch
                            }) =>
                    {
                        return Err(error);
                    }
                    result => result,
                };
                if let Ok(snapshot) = observation {
                    ensure!(
                        snapshot.binding.component == "app-cli"
                            && snapshot.binding.resource == origin,
                        "recovery supervisor belongs to another workspace"
                    );
                    diagnostic = Some(snapshot.diagnostic());
                    if snapshot.phase == runtime_supervisor::Phase::RecoveryRequired {
                        // A persisted failure describes the previous attempt.
                        // Ask this exact supervisor/generation to reassess once;
                        // it owns the worker-exit and cleanup proof. Lost replies
                        // reuse the same request, never restart a replacement.
                        if snapshot.problem.as_ref().is_none_or(|problem| {
                            problem.code != runtime_supervisor::FailureCode::CleanupUnconfirmed
                        }) || recovery_request.as_ref().is_some_and(|(_, request)| {
                            snapshot.operation_id.as_deref() == Some(&request.request_id)
                        }) {
                            return Err(snapshot.recovery_error());
                        }
                        let (instance, request) = recovery_request.get_or_insert_with(|| {
                            let mut request = runtime_supervisor::Request::new(
                                runtime_supervisor::Action::Recover,
                            );
                            request.expected_generation = snapshot.generation.clone();
                            (snapshot.supervisor_id.clone(), request)
                        });
                        if let Err(error) =
                            runtime_supervisor::control_verified(root, request.clone(), instance)
                                .await
                        {
                            if let Some(problem) =
                                error.downcast_ref::<runtime_supervisor::Problem>()
                                && problem.code != runtime_supervisor::FailureCode::Busy
                            {
                                return Err(error);
                            }
                            diagnostic =
                                Some(format!("recovery request awaiting confirmation: {error:#}"));
                        }
                    }
                }
                let exited = lock(&self.owner_children)
                    .map_err(|e| anyhow::anyhow!("{e}"))?
                    .get(project)
                    .and_then(|child| child.exited());
                if let Some(exit) = exited {
                    // The first bootstrap may finish the original container's
                    // already accepted Shutdown. Only its confirmed terminal
                    // receipt permits one new management-only launch. This does
                    // not queue/replay a business operation or erase its journal.
                    if !resumed_shutdown
                        && matches!(&exit, super::ChildExit::Exited(status) if status.success())
                        && let Ok(snapshot) = runtime_supervisor::last_snapshot(root)
                        && snapshot.binding.component == "app-cli"
                        && snapshot.binding.resource == origin
                        && snapshot.phase == runtime_supervisor::Phase::Stopped
                        && snapshot.intent == runtime_supervisor::Intent::Shutdown
                        && snapshot.operation_id.is_some()
                    {
                        if let Some(generation) = &snapshot.generation {
                            runtime_supervisor::verify_local_quiescent(root, generation)?;
                        }
                        resumed_shutdown = true;
                        self.spawn_recovery_owner(project, workspace, address)
                            .await?;
                        continue;
                    }
                    anyhow::bail!(
                        "app-cli management recovery {}; see {}/app-cli/owner-recovery.log",
                        exit.describe(),
                        super::log::log_dir(&self.config, project).display()
                    );
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
        .await;
        result.with_context(|| {
            format!(
                "app-cli management recovery timed out; original runtime state retained; {}",
                diagnostic
                    .as_deref()
                    .unwrap_or("supervisor control is unavailable")
            )
        })?
    }

    /// Restore a usable management plane before building. Old business failure
    /// is diagnostic, not a permanent ban on a new build or explicit activation.
    pub async fn preflight_userapp_build(&self, project: &str, workspace: &Path) -> AppResult<()> {
        let mut owner = self.recover_owner_if_needed(project, workspace).await?;
        self.ensure_new_build_admissible(project)?;
        if let Some(available) = &owner {
            let identity = &available.identity;
            let expected_app = std::env::var("PROJECT_ID")
                .ok()
                .filter(|app| !app.trim().is_empty())
                .unwrap_or_else(|| "unknown-app".into());
            let app = expected_app.as_str();
            owner_client::verify_project_identity(identity, workspace, app)
                .map_err(|e| AppError::owner_error("verify build owner", e))?;
            let (_, token) = owner_client::find_owner_token(Path::new(&identity.source_root), app)
                .ok_or_else(|| AppError::business("owner control credentials unavailable"))?;
            let evidence = async {
                OwnerClient::new(&available.address, &token)?
                    .recovery()
                    .await
            }
            .await
            .map_err(|e| AppError::owner_error("inspect build owner", e))?;
            if evidence.kernel_protected
                || (evidence.owner_protected && !evidence.credentials_required)
            {
                // StopWork captures the actual worker generation and confirms its
                // exit before rebooting management. No process-name cleanup and
                // no replay of an uncertain deployment or migration.
                self.stop_supervised_owner(project, workspace)
                    .await
                    .map_err(|e| AppError::owner_error("restore build control service", e))?;
                owner = self.recover_owner_if_needed(project, workspace).await?;
            }
        }
        let address = owner
            .as_ref()
            .map(|owner| owner.address.as_str())
            .unwrap_or(&self.config.app_cli_admin_probe_addr);
        self.capture_owner_expectation_at(project, workspace, address)
            .await;
        if let Some(super::types::OwnerExpectation::ObservationFailed { reason }) =
            lock(&self.owner_expectations)?.get(project)
        {
            return Err(AppError::business(format!(
                "owner preflight failed: {reason}"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use runtime_supervisor::{Action, FailureCode, Intent, Phase, Problem, Request, Snapshot};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    #[tokio::test]
    async fn historical_cleanup_failure_is_reassessed_once_before_continuing_request() {
        for cleanup_succeeds in [true, false] {
            let temp = tempfile::tempdir().unwrap();
            let root = std::fs::canonicalize(temp.path()).unwrap();
            let ready = Arc::new(AtomicBool::new(false));
            let identity = RuntimeIdentityView {
                application_id: "210".into(),
                service_family: "userapp-dev".into(),
                workspace_id: "210".into(),
                source_root: root.to_string_lossy().into_owned(),
                runtime_instance_id: "new-worker".into(),
                deployment_generation_id: "new-generation".into(),
                protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
                capabilities: Vec::new(),
            };
            let http_ready = ready.clone();
            let http = axum::Router::new().route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let ready = http_ready.clone();
                    let identity = identity.clone();
                    async move {
                        if ready.load(Ordering::SeqCst) {
                            (
                                axum::http::StatusCode::OK,
                                axum::Json(serde_json::json!({"data": identity})),
                            )
                        } else {
                            (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                axum::Json(serde_json::json!({"code": "ERR_INITIALIZING"})),
                            )
                        }
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let http_task = tokio::spawn(async move { axum::serve(listener, http).await.unwrap() });
            let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let owner_lock = std::fs::File::create(root.join("owner.lock")).unwrap();
            owner_lock.try_lock().unwrap();
            let mut snapshot = Snapshot {
                version: 1,
                binding: runtime_supervisor::Binding {
                    component: "app-cli".into(),
                    resource: root.clone(),
                },
                supervisor_id: "retained-supervisor".into(),
                generation: Some("old-generation".into()),
                phase: Phase::RecoveryRequired,
                intent: Intent::Stopped,
                operation_id: Some("old-failed-stop".into()),
                error: Some("historical cleanup failure".into()),
                problem: Some(Problem {
                    code: FailureCode::CleanupUnconfirmed,
                    message: "cleanup incomplete".into(),
                }),
            };
            std::fs::write(root.join("supervisor.json"), serde_json::to_vec(&serde_json::json!({
                "version": 2, "instance": snapshot.supervisor_id, "token": "fixture-token",
                "address": control.local_addr().unwrap().to_string(), "snapshot": snapshot, "requests": []
            })).unwrap()).unwrap();
            let calls = Arc::new(AtomicUsize::new(0));
            let recover_calls = calls.clone();
            let control_task = tokio::spawn(async move {
                loop {
                    let (mut stream, _) = control.accept().await.unwrap();
                    let mut frame = String::new();
                    BufReader::new(&mut stream)
                        .read_line(&mut frame)
                        .await
                        .unwrap();
                    let frame: serde_json::Value = serde_json::from_str(&frame).unwrap();
                    assert_eq!(frame["version"], 2);
                    assert_eq!(frame["instance"], snapshot.supervisor_id);
                    assert_eq!(frame["token"], "fixture-token");
                    let request: Request =
                        serde_json::from_value(frame["request"].clone()).unwrap();
                    if request.action == Action::Recover {
                        recover_calls.fetch_add(1, Ordering::SeqCst);
                        assert_eq!(request.expected_generation, snapshot.generation);
                        snapshot.operation_id = Some(request.request_id);
                        if cleanup_succeeds {
                            snapshot.phase = Phase::CleanupPending;
                            ready.store(true, Ordering::SeqCst);
                        }
                    }
                    let reply = serde_json::json!({"instance": snapshot.supervisor_id, "snapshot": snapshot, "error": null});
                    stream
                        .write_all(format!("{reply}\n").as_bytes())
                        .await
                        .unwrap();
                }
            });
            let manager = DevServerManager::new(Arc::new(crate::Config::from_env().unwrap()));
            let outcome = tokio::time::timeout(
                Duration::from_secs(4),
                manager.wait_for_recovery_owner("210", &root, &address, &root),
            )
            .await;
            http_task.abort();
            control_task.abort();
            let outcome = outcome.expect("recovery must stay within the current request");
            assert_eq!(outcome.is_ok(), cleanup_succeeds);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "do not loop over new recovery identities"
            );
        }
    }
}
