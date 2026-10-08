//! legacy 无子命令形态的 owner 分派（R02）。
//!
//! 同一 workspace 已有 serve owner（OwnerGuard 被活进程持有）时，重复的
//! legacy CLI 调用**不再绕过所有权**本地起第二套编排——转为运行 API 客户端：
//! 提交 Start/Restart(Source) 并等待终态（“重复启动是正常操作”）。身份不符、
//! 协议不兼容、凭据缺失 → 明确拒绝（不杀对方、不换端口、不删锁文件）。
//!
//! 普通 run 无 owner 时由 run_client 拉起 serve --control-only，再提交
//! 明确 Source 或 Artifact 操作。serve 自身承载 owner；内核锁保证唯一性。

use anyhow::{Context, Result, bail};
use shared_types::{
    RUNTIME_CONTROL_PROTOCOL_VERSION, RunProfileInput, RuntimeIdentityView, RuntimeOperationKind,
    RuntimeOperationRequest, RuntimeOperationState, RuntimeOperationView,
};

/// Listener wildcard addresses describe binding, not a routable client target.
fn connect_address(address: &str) -> String {
    match address.parse::<std::net::SocketAddr>() {
        Ok(mut socket) => {
            if socket.ip().is_unspecified() {
                socket.set_ip(match socket.ip() {
                    std::net::IpAddr::V4(_) => std::net::Ipv4Addr::LOCALHOST.into(),
                    std::net::IpAddr::V6(_) => std::net::Ipv6Addr::LOCALHOST.into(),
                });
            }
            socket.to_string()
        }
        Err(_) => address.to_owned(),
    }
}

async fn discover_owner(
    configured_address: &str,
    state_root: &std::path::Path,
    budget: std::time::Duration,
) -> Option<(String, RuntimeIdentityView)> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        // Read again on each attempt: the first owner may still be publishing
        // its actual ephemeral listener address. Never send a token here.
        let record = crate::runtime_kernel::RuntimeStore::read_endpoint(state_root);
        let mut candidates = Vec::new();
        if let Some(record) = record.as_ref() {
            candidates.push((connect_address(&record.address), Some(record)));
        }
        let configured = connect_address(configured_address);
        if !candidates.iter().any(|(address, _)| address == &configured) {
            candidates.push((configured, None));
        }
        for (address, record) in candidates {
            match tokio::time::timeout_at(deadline, probe_identity(&address)).await {
                Ok(Some(identity))
                    if record.is_none_or(|record| {
                        crate::runtime_kernel::endpoint_matches_identity(record, &identity)
                    }) =>
                {
                    return Some((address, identity));
                }
                Err(_) => return None,
                _ => {}
            }
        }
        if tokio::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep_until(std::cmp::min(
            deadline,
            tokio::time::Instant::now() + std::time::Duration::from_millis(100),
        ))
        .await;
    }
}

/// 探测既有 owner 的运行身份（无认证端点；返回 None = 无应答）。
async fn probe_identity(admin_addr: &str) -> Option<RuntimeIdentityView> {
    let url = format!("http://{admin_addr}/v1/runtime/identity");
    let response = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(3))
        .no_proxy()
        .build()
        .ok()?
        .get(&url)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let body: serde_json::Value = response.json().await.ok()?;
    serde_json::from_value(body.get("data")?.clone()).ok()
}

/// Identity publication can precede admission becoming available. Business
/// NotReady is allowed: only initialization prevents submitting control work.
async fn wait_until_initialized(
    client: &reqwest::Client,
    address: &str,
    budget: std::time::Duration,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let probe = async {
            let response = client
                .get(format!("http://{address}/ready"))
                .send()
                .await
                .context("query owner initialization")?;
            let status = response.status();
            let body: serde_json::Value = response
                .json()
                .await
                .context("decode owner initialization")?;
            match (
                status,
                body.get("status").and_then(serde_json::Value::as_str),
            ) {
                (reqwest::StatusCode::OK, Some("ready"))
                | (reqwest::StatusCode::SERVICE_UNAVAILABLE, Some("not_ready")) => Ok(true),
                (reqwest::StatusCode::SERVICE_UNAVAILABLE, Some("initializing")) => Ok(false),
                _ => bail!("owner returned an unsupported initialization response"),
            }
        };
        if tokio::time::timeout_at(deadline, probe)
            .await
            .context("owner initialization deadline exceeded")??
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("owner initialization deadline exceeded");
        }
        tokio::time::sleep_until(std::cmp::min(
            deadline,
            tokio::time::Instant::now() + std::time::Duration::from_millis(100),
        ))
        .await;
    }
}

/// legacy app-cli 探测（仅 /v1/deploy/status 应答且带 protocol_version）。
async fn legacy_app_cli_responds(admin_addr: &str) -> bool {
    let url = format!("http://{admin_addr}/v1/deploy/status");
    let Ok(client) = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(2))
        .no_proxy()
        .build()
    else {
        return false;
    };
    let Ok(response) = client.get(&url).send().await else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    response
        .json::<serde_json::Value>()
        .await
        .is_ok_and(|body| body.get("protocol_version").is_some())
}

fn envelope_data(body: &serde_json::Value) -> Result<serde_json::Value> {
    body.get("data")
        .cloned()
        .context("runtime API response missing data envelope")
}

/// 既有 owner 的分派结局。
#[derive(Debug)]
pub enum OwnerDispatch {
    /// 提交已被 owner 受理并到达终态（Succeeded = 启动生效）。
    Terminal(RuntimeOperationView),
    /// 已确认锁释放或错误位置的 owner 收束——调用方重新获取管理 owner。
    NoOwner,
}

/// A control-only bootstrap has no business operation to report.
#[derive(Debug, PartialEq, Eq)]
pub enum ManagementReuse {
    /// The same native authority and initialized API were re-observed.
    Ready,
    /// An authorized misdirected owner was retired; bootstrap must reacquire.
    NoOwner,
}

pub(crate) struct EnvironmentDeployment {
    pub request: crate::server::DeployRequest,
    pub operation_id: String,
    pub generation: String,
}

/// Preserve the existing environment artifact contract, independently of
/// Source manifests and their rebuildable cache. Every observation belongs to
/// this original operation and captured owner; a newer deployment is unrelated.
pub(crate) async fn dispatch_environment_deployment(
    args: &crate::RuntimeArgs,
    state_root: &std::path::Path,
    application_id: &str,
    deployment: &EnvironmentDeployment,
    deadline: tokio::time::Instant,
) -> Result<()> {
    tokio::time::timeout_at(deadline, async {
        let (address, identity) = discover_owner(&args.admin_addr, state_root, std::time::Duration::from_secs(10)).await.context("discover artifact deployment owner")?;
        anyhow::ensure!(identity.protocol_version == RUNTIME_CONTROL_PROTOCOL_VERSION && identity.application_id == application_id && identity.service_family == "userapp-dev", "artifact owner application or protocol changed before submission");
        anyhow::ensure!(identity.capabilities.iter().any(|cap| cap == "deploy-artifact-url"), "runtime owner cannot deploy artifact URLs");
        let expected_root = runtime_state_layout::resolve_project_origin(&args.workspace)?;
        let owner_root = runtime_state_layout::resolve_project_origin(std::path::Path::new(&identity.source_root))?;
        anyhow::ensure!(runtime_state_layout::canonical_project_root(&expected_root) == runtime_state_layout::canonical_project_root(&owner_root), "artifact owner workspace changed before submission");
        verify_saved_management_identity(state_root, &identity)?;
        let native = runtime_supervisor::control(state_root, runtime_supervisor::Request::new(runtime_supervisor::Action::Status)).await?;
        anyhow::ensure!(native.binding.component == "app-cli" && runtime_state_layout::canonical_project_root(&native.binding.resource) == runtime_state_layout::canonical_project_root(&expected_root), "native artifact authority belongs to another workspace");
        let token = crate::runtime_kernel::RuntimeStore::read_token(state_root).context("artifact owner credentials are unavailable")?;
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(10)).no_proxy().build()?;
        wait_until_initialized(&client, &address, std::time::Duration::from_secs(10)).await?;
        let observed = probe_identity(&address).await.context("artifact owner disappeared before submission")?;
        anyhow::ensure!(same_management_identity(&identity, &observed), "artifact owner changed before submission; no operation was submitted");
        let pg = shared_types::resolve_source_run_pg(None).context("capture current explicit artifact credentials")?;
        let id = &deployment.operation_id;
        println!("observing artifact deployment operation {id}");
        let response = client.post(format!("http://{address}/v1/deploy")).header("X-Deploy-Token", &token)
            .json(&serde_json::json!({"operation_id":id, "expected_runtime_instance_id":identity.runtime_instance_id, "deployment_generation_id":deployment.generation, "url":deployment.request.url, "release_id":deployment.request.release_id, "sha256":deployment.request.sha256, "pg":pg}))
            .send().await.with_context(|| format!("submit artifact operation {id}; query this ID before retrying an unconfirmed submission"))?;
        let status = response.status();
        let accepted: serde_json::Value = response.json().await.context("decode artifact acceptance")?;
        anyhow::ensure!(status.is_success() && accepted["success"] == true, "artifact operation {id} rejected: {} ({})", accepted["message"].as_str().unwrap_or("no detail"), accepted["code"].as_str().unwrap_or("ERR"));
        let poll = format!("/v1/deploy/status?operation_id={id}");
        anyhow::ensure!(accepted["data"]["operation_id"] == id.as_str() && accepted["data"]["poll"] == poll, "artifact acceptance refers to another operation");
        loop {
            runtime_supervisor::control_verified(state_root, runtime_supervisor::Request::new(runtime_supervisor::Action::Status), &native.supervisor_id).await.context("artifact owner changed while observing original operation")?;
            let observed = probe_identity(&address).await.context("artifact management identity disappeared while observing original operation")?;
            anyhow::ensure!(same_management_identity(&identity, &observed), "artifact operation {id} owner changed; its original result must be queried, not replayed on a new owner");
            let response = client.get(format!("http://{address}{poll}")).header("X-Deploy-Token", &token).send().await?;
            let status = response.status();
            let body: serde_json::Value = response.json().await?;
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE && body["code"] == "ERR_INITIALIZING" {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            anyhow::ensure!(status.is_success() && body["success"] == true, "artifact operation {id} observation failed ({})", body["code"].as_str().unwrap_or("ERR"));
            let operation: shared_types::AppDeploymentOperation = serde_json::from_value(body["data"]["operation"].clone()).context("decode original artifact operation")?;
            anyhow::ensure!(operation.operation_id == *id && operation.request_release_id == deployment.request.release_id && operation.deployment_generation_id == deployment.generation, "artifact owner returned a different operation or deployment generation");
            match operation.phase {
                shared_types::AppCliDeployPhase::Running if operation.persisted => { println!("artifact operation {id} completed on the running owner"); return Ok(()); }
                shared_types::AppCliDeployPhase::Failed => bail!("artifact operation {id} failed: {}", operation.error.as_deref().unwrap_or("no detail")),
                _ => tokio::time::sleep(std::time::Duration::from_millis(200)).await,
            }
        }
    }).await.with_context(|| format!("artifact operation {} deadline exceeded; query its original result before retrying", deployment.operation_id))?
}

/// Reuse only management. In particular, this path never reads release.lock or
/// submits a Source operation, so Stopped and invalid/empty source remain intact.
pub async fn reuse_management_owner(
    admin_addr: &str,
    workspace: &std::path::Path,
    state_root: &std::path::Path,
    application_id: &str,
) -> Result<ManagementReuse> {
    let normalized = runtime_state_layout::normalize_management_workspace(workspace)?;
    let workspace = normalized.as_path();
    tokio::time::timeout(std::time::Duration::from_secs(45), async {
        let (address, identity) =
            discover_owner(admin_addr, state_root, std::time::Duration::from_secs(10))
                .await
                .context("owner lock is held but no management identity is available")?;
        anyhow::ensure!(
            identity.protocol_version == RUNTIME_CONTROL_PROTOCOL_VERSION
                && identity.application_id == application_id
                && identity.service_family == "userapp-dev",
            "management owner identity does not match this application and protocol"
        );
        shared_types::validate_identifier(&identity.workspace_id, "workspace_id")
            .map_err(anyhow::Error::msg)?;
        let managed = runtime_state_layout::ManagedWorkspace::from_env(workspace, state_root)?;
        let expected_root = runtime_state_layout::resolve_project_origin(workspace)?;
        let owner_root = match &managed {
            Some(managed) => {
                managed.verify_contained_workspace(std::path::Path::new(&identity.source_root))?
            }
            None => runtime_state_layout::resolve_project_origin(std::path::Path::new(
                &identity.source_root,
            ))?,
        };
        if runtime_state_layout::canonical_project_root(&owner_root)
            != runtime_state_layout::canonical_project_root(&expected_root)
        {
            if let Some(managed) = &managed {
                crate::control::managed_owner::retire_misdirected(managed, &identity).await?;
                return Ok(ManagementReuse::NoOwner);
            }
            bail!("management owner belongs to another workspace");
        }
        verify_saved_management_identity(state_root, &identity)?;
        let before = runtime_supervisor::control(
            state_root,
            runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
        )
        .await?;
        loop {
            let snapshot = runtime_supervisor::control_verified(
                state_root,
                runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                &before.supervisor_id,
            )
            .await?;
            anyhow::ensure!(
                snapshot.binding.component == "app-cli",
                "native management authority belongs to another component"
            );
            let native_root = match &managed {
                Some(managed) => managed.verify_contained_workspace(&snapshot.binding.resource)?,
                None => runtime_state_layout::resolve_project_origin(&snapshot.binding.resource)?,
            };
            let root_matches = runtime_state_layout::canonical_project_root(&native_root)
                == runtime_state_layout::canonical_project_root(&expected_root);
            anyhow::ensure!(
                root_matches || managed.is_some(),
                "native management authority belongs to another workspace"
            );
            // This is management reuse, not permission to execute business.
            // A fenced/initializing owner still owns the scope and serves Stop
            // and diagnostics; its project health cannot cause a second owner.
            if root_matches && snapshot.intent != runtime_supervisor::Intent::Shutdown {
                let observed = probe_identity(&address)
                    .await
                    .context("management identity disappeared during readiness verification")?;
                anyhow::ensure!(
                    same_management_identity(&identity, &observed),
                    "management runtime changed while reusing owner"
                );
                verify_saved_management_identity(state_root, &observed)?;
                // Read native state again after the API probes, so a queued
                // stop/rebind is not mistaken for completed management recovery.
                let after = runtime_supervisor::control_verified(
                    state_root,
                    runtime_supervisor::Request::new(runtime_supervisor::Action::Status),
                    &before.supervisor_id,
                )
                .await?;
                if after.binding == snapshot.binding
                    && after.intent != runtime_supervisor::Intent::Shutdown
                {
                    return Ok(ManagementReuse::Ready);
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    })
    .await
    .context("management-only owner reuse deadline exceeded; no business request was submitted")?
}

fn same_management_identity(left: &RuntimeIdentityView, right: &RuntimeIdentityView) -> bool {
    left.application_id == right.application_id
        && left.service_family == right.service_family
        && left.workspace_id == right.workspace_id
        && left.source_root == right.source_root
        && left.runtime_instance_id == right.runtime_instance_id
        && left.deployment_generation_id == right.deployment_generation_id
        && left.protocol_version == right.protocol_version
        && left.capabilities == right.capabilities
}

fn verify_saved_management_identity(
    state_root: &std::path::Path,
    identity: &RuntimeIdentityView,
) -> Result<()> {
    let saved: RuntimeIdentityView = serde_json::from_slice(
        &std::fs::read(state_root.join("identity.json"))
            .context("read native management identity record")?,
    )
    .context("decode native management identity record")?;
    anyhow::ensure!(
        same_management_identity(&saved, identity),
        "native authority does not own the observed management API identity"
    );
    Ok(())
}

/// OwnerGuard 被占时的分派决策（R02 主路径）。
///
/// `state_root` 用于读取 owner 凭据文件（token；与 file-server 同一契约）。
pub async fn dispatch_to_owner(
    admin_addr: &str,
    workspace: &std::path::Path,
    state_root: &std::path::Path,
    application_id: &str,
) -> Result<OwnerDispatch> {
    let normalized = runtime_state_layout::normalize_management_workspace(workspace)?;
    let workspace = normalized.as_path();
    dispatch_to_owner_inner(admin_addr, workspace, state_root, application_id, true).await
}

async fn dispatch_to_owner_inner(
    admin_addr: &str,
    workspace: &std::path::Path,
    state_root: &std::path::Path,
    application_id: &str,
    allow_recovery: bool,
) -> Result<OwnerDispatch> {
    let connect_addr = connect_address(admin_addr);
    let admin_addr = connect_addr.as_str();
    // R09/XP10：发现记录（endpoint.json）是线索不是证明——身份核验全字段
    let Some((discovered_address, identity)) =
        discover_owner(admin_addr, state_root, std::time::Duration::from_secs(10)).await
    else {
        // 无 runtime identity：legacy 应答 → 明确拒绝；否则锁被持有但无
        // 管理面（foreign/僵持）→ 同样拒绝，不猜
        if legacy_app_cli_responds(admin_addr).await {
            bail!(
                "admin port {admin_addr} is held by a legacy app-cli without the runtime API; \
                 stop it before starting another instance"
            );
        }
        bail!(
            "owner lock is held but no management API answers at {admin_addr}; \
             refusing to start a competing orchestrator (inspect the lock holder \
             at {} before retrying)",
            state_root.join("owner.lock").display()
        );
    };
    let admin_addr = discovered_address.as_str();

    if identity.protocol_version != RUNTIME_CONTROL_PROTOCOL_VERSION {
        bail!(
            "app-cli owner at {admin_addr} speaks incompatible protocol v{} (expected v{}); \
             upgrade it before dispatching",
            identity.protocol_version,
            RUNTIME_CONTROL_PROTOCOL_VERSION
        );
    }
    let expected_root = workspace
        .canonicalize()
        .context("resolve dispatch workspace")?;
    let managed = runtime_state_layout::ManagedWorkspace::from_env(workspace, state_root)?;
    let owner_root = match &managed {
        Some(managed) => {
            managed.verify_contained_workspace(std::path::Path::new(&identity.source_root))?
        }
        None => std::path::Path::new(&identity.source_root)
            .canonicalize()
            .context("resolve owner workspace identity")?,
    };
    if runtime_state_layout::canonical_project_root(&owner_root)
        != runtime_state_layout::canonical_project_root(&expected_root)
        || identity.application_id != application_id
    {
        if let Some(managed) = managed {
            crate::control::managed_owner::retire_misdirected(&managed, &identity).await?;
            return Ok(OwnerDispatch::NoOwner);
        }
        bail!(
            "admin port {admin_addr} is held by a different app-cli owner \
             (app {}/{}, expected {}); refusing to start a \
             competing orchestrator",
            identity.application_id,
            identity.workspace_id,
            application_id
        );
    }
    let expected_ws = identity.workspace_id.clone();
    crate::build_deploy::source_lock::require_owner_capability(workspace, &identity.capabilities)?;

    // 凭据：owner 启用写端点时落盘状态根（与平台侧同一读取契约）
    let token = crate::runtime_kernel::RuntimeStore::read_token(state_root);
    let Some(token) = token else {
        bail!(
            "workspace is already managed by an app-cli owner whose runtime API \
             credentials have not been published or cannot be read; \
             wait for owner initialization or check state directory access"
        );
    };
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .context("build owner dispatch client")?;

    wait_until_initialized(&client, admin_addr, std::time::Duration::from_secs(10)).await?;

    // 当前 revision + 提交 Start（源码形态——legacy 入口无制品输入）
    let status_url = format!("http://{admin_addr}/v1/runtime/status");
    let status: shared_types::RuntimeStatusView = {
        let body: serde_json::Value = client
            .get(&status_url)
            .header("X-Deploy-Token", &token)
            .send()
            .await
            .context("query owner status")?
            .error_for_status()
            .context("owner status rejected")?
            .json()
            .await
            .context("parse owner status")?;
        serde_json::from_value(envelope_data(&body)?).context("decode status view")?
    };
    anyhow::ensure!(
        status.runtime_instance_id == identity.runtime_instance_id,
        "owner changed while preparing dispatch; no operation was submitted"
    );
    if allow_recovery && state_root.join("supervisor.json").try_exists()? {
        let response = client
            .get(format!("http://{admin_addr}/v1/runtime/recovery"))
            .header("X-Deploy-Token", &token)
            .send()
            .await?
            .error_for_status()?;
        let body: serde_json::Value = response.json().await?;
        let recovery: shared_types::RuntimeRecoveryView =
            serde_json::from_value(envelope_data(&body)?)?;
        if status.recovery_protection
            || (recovery.owner_protected && !recovery.credentials_required)
        {
            runtime_supervisor::stop_work_with_cleanup(
                state_root,
                &runtime_supervisor::Binding {
                    component: "app-cli".into(),
                    resource: runtime_state_layout::resolve_project_origin(workspace)?,
                },
                std::time::Duration::from_secs(90),
                &crate::supervision::cleanup_command(state_root)?,
            )
            .await
            .context("restore owner before explicit start")?;
            // No request was submitted to the old owner. Discover fresh identity
            // and credentials after its captured execution has actually stopped.
            return Box::pin(dispatch_to_owner_inner(
                admin_addr,
                workspace,
                state_root,
                application_id,
                false,
            ))
            .await;
        }
    }
    let operation_id = format!("cli-dispatch-{}", uuid::Uuid::new_v4().simple());
    println!("observing Source operation {operation_id}");
    let request = RuntimeOperationRequest {
        operation_id: operation_id.clone(),
        expected_runtime_instance_id: identity.runtime_instance_id.clone(),
        expected_revision: status.revision,
        workspace_id: expected_ws.clone(),
        kind: RuntimeOperationKind::Start,
        profile: RunProfileInput::Source {
            workspace_id: expected_ws.clone(),
        },
        run_config: shared_types::resolve_source_run_pg(None)
            .context("capture current Source operation runtime credentials")?
            .map(|pg| shared_types::OperationRunConfig { pg: Some(pg) }),
        request_context: None,
    };
    let submit_url = format!("http://{admin_addr}/v1/runtime/operations");
    let response = client
        .post(&submit_url)
        .header("X-Deploy-Token", &token)
        .json(&request)
        .send()
        .await
        .with_context(|| format!("submit Source operation {operation_id}; query this ID before retrying an unconfirmed submission"))?;
    let status_code = response.status();
    let body: serde_json::Value = response.json().await.context("parse submit response")?;
    if !status_code.is_success() {
        bail!(
            "owner rejected dispatch: {} ({})",
            body.get("message")
                .and_then(|value| value.as_str())
                .unwrap_or("unknown"),
            body.get("code")
                .and_then(|value| value.as_str())
                .unwrap_or("ERR"),
        );
    }
    let accepted: shared_types::RuntimeOperationAccepted =
        serde_json::from_value(envelope_data(&body)?)
            .context("decode operation acceptance receipt")?;
    let poll_path = format!("/v1/runtime/operations/{operation_id}");
    anyhow::ensure!(
        accepted.operation_id == operation_id && accepted.poll == poll_path,
        "owner returned a different operation acceptance receipt"
    );

    // A 202 receipt is not a full operation view, even on terminal replay.
    // Fetch the authoritative view before trusting its instance, kind or outcome.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    let mut event_cursor: u64 = 0;
    // R1：journal done 的持留捕获——原操作终态确认后经一致性校验才发出，
    // 终局绑定权威视图、恰好一次。
    let mut held_done: Option<serde_json::Value> = None;
    loop {
        let poll = async {
            let body: serde_json::Value = client
                .get(format!("http://{admin_addr}{poll_path}"))
                .header("X-Deploy-Token", &token)
                .send()
                .await
                .context("poll dispatch operation")?
                .error_for_status()
                .context("owner rejected operation poll")?
                .json()
                .await
                .context("parse operation poll")?;
            serde_json::from_value::<RuntimeOperationView>(envelope_data(&body)?)
                .context("decode operation view")
        };
        let view = tokio::time::timeout_at(deadline, poll)
            .await
            .with_context(|| format!("Source operation {operation_id} observation deadline exceeded; query its original result before retrying"))??;
        anyhow::ensure!(
            view.operation_id == operation_id
                && view.runtime_instance_id == identity.runtime_instance_id
                && view.kind == RuntimeOperationKind::Start,
            "owner returned a different operation or runtime identity while observing {operation_id}"
        );
        // run 客户端是 file-server 管道的流端点（spawn 路径 stdout 是唯一
        // 通道）：serve 内编排产生的事件经 journal 读出后重新呈现在客户端
        // stdout 上（转发是尽力而为——读不到不阻塞观察）。journal 的
        // orchestration_done 早于操作状态机提交终局，只捕获不转发；
        // 终局 Done 在视图终态时恰好一次地发出（捕获件一致则保留原服务
        // 明细，否则按视图合成——同源于本操作的权威结果）。
        if let Some(done) = super::dispatch_events::forward_operation_events(
            &client,
            admin_addr,
            &token,
            &operation_id,
            &mut event_cursor,
        )
        .await
        {
            held_done = Some(done);
        }
        if view.state.is_terminal() || view.state == RuntimeOperationState::RecoveryRequired {
            match held_done.take() {
                Some(done) if super::dispatch_events::captured_done_matches(&done, &view.state) => {
                    super::dispatch_events::emit_captured_done(&done);
                }
                _ => super::dispatch_events::emit_terminal_done_event(&view),
            }
            return Ok(OwnerDispatch::Terminal(view));
        }
        tokio::time::sleep_until(std::cmp::min(
            deadline,
            tokio::time::Instant::now() + std::time::Duration::from_millis(500),
        ))
        .await;
    }
}

/// 终态视图的 CLI 友好呈现（错误信息面向操作者；无凭据内容）。
pub fn describe_terminal(view: &RuntimeOperationView) -> Result<()> {
    match view.state {
        RuntimeOperationState::Succeeded => {
            println!("dispatched start completed on the running owner");
            Ok(())
        }
        // R4：被更新的受理取代（plan §8"新显式请求可替代旧的可取消业务
        // 任务"）是转交的正常结局，以 0 退出——nonzero 会让 supervisord 的
        // serve 程序重启并再次转交，形成"转交-取代-退出-重启"循环，
        // 反复提交新 Start 抢占用户请求。
        RuntimeOperationState::Cancelled => {
            println!(
                "dispatched start was superseded by a newer request (operation {})",
                view.operation_id
            );
            Ok(())
        }
        other => bail!(
            "dispatched start operation {} failed: {other:?} ({})",
            view.operation_id,
            view.error_message.as_deref().unwrap_or("no detail"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Protocol fixture only: real TCP and a real held owner lock, with a
    /// deliberately fenced native snapshot. It proves management reuse does
    /// not claim business readiness or perform any business/control mutation.
    #[tokio::test]
    async fn control_only_reuses_recovery_required_management_without_business_operations() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

        struct FixtureTask(tokio::task::JoinHandle<()>);
        impl Drop for FixtureTask {
            fn drop(&mut self) {
                self.0.abort();
            }
        }

        let temporary = tempfile::tempdir().unwrap();
        let source = temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("fenced-project");
        let state_root = temporary.path().canonicalize().unwrap().join("state");
        std::fs::create_dir_all(&source).unwrap();
        let _owner = runtime_supervisor::Owner::try_acquire(&state_root)
            .unwrap()
            .unwrap();
        assert!(
            runtime_supervisor::Owner::try_acquire(&state_root)
                .unwrap()
                .is_none()
        );
        let source_bytes = b"invalid project configuration; management must remain usable";
        std::fs::write(source.join("release.lock.toml"), source_bytes).unwrap();
        std::fs::create_dir_all(state_root.join("operations")).unwrap();
        let operation_bytes = b"protocol fixture: preserve this operation byte for byte";
        let operation_path = state_root.join("operations/original-fixture.json");
        std::fs::write(&operation_path, operation_bytes).unwrap();
        let identity = RuntimeIdentityView {
            application_id: "management-fixture".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "fenced-project".into(),
            source_root: source.display().to_string(),
            runtime_instance_id: "original-runtime-instance".into(),
            deployment_generation_id: "original-deployment".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec!["operations".into()],
        };
        let identity_bytes = serde_json::to_vec(&identity).unwrap();
        std::fs::write(state_root.join("identity.json"), &identity_bytes).unwrap();
        let unrelated_http = Arc::new(AtomicUsize::new(0));
        let unrelated = unrelated_http.clone();
        let wire_identity = identity.clone();
        let http = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = wire_identity.clone();
                    async move { axum::Json(envelope(&identity)) }
                }),
            )
            .fallback(move || {
                let calls = unrelated.clone();
                async move {
                    calls.fetch_add(1, Ordering::SeqCst);
                    axum::http::StatusCode::BAD_REQUEST
                }
            });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_address = listener.local_addr().unwrap().to_string();
        let mut http_task = FixtureTask(tokio::spawn(async move {
            axum::serve(listener, http).await.unwrap();
        }));
        let native_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let native_address = native_listener.local_addr().unwrap().to_string();
        let snapshot = runtime_supervisor::Snapshot {
            version: 1,
            binding: runtime_supervisor::Binding {
                component: "app-cli".into(),
                resource: source.clone(),
            },
            supervisor_id: "original-native-instance".into(),
            generation: Some("unconfirmed-generation".into()),
            phase: runtime_supervisor::Phase::RecoveryRequired,
            intent: runtime_supervisor::Intent::Stopped,
            operation_id: None,
            error: Some("old cleanup remains unconfirmed".into()),
            problem: Some(runtime_supervisor::Problem {
                code: runtime_supervisor::FailureCode::CleanupUnconfirmed,
                message: "cleanup evidence is still missing".into(),
            }),
        };
        let discovery_bytes = serde_json::to_vec(&serde_json::json!({
            "version":2, "instance":snapshot.supervisor_id, "address":native_address,
            "token":"protocol-fixture-token", "snapshot":snapshot, "requests":[],
        }))
        .unwrap();
        std::fs::write(state_root.join("supervisor.json"), &discovery_bytes).unwrap();
        let native_requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let requests = native_requests.clone();
        let native_snapshot = snapshot.clone();
        let mut native_task = FixtureTask(tokio::spawn(async move {
            loop {
                let (stream, _) = native_listener.accept().await.unwrap();
                let mut stream = BufReader::new(stream);
                let mut line = String::new();
                stream.read_line(&mut line).await.unwrap();
                let envelope: serde_json::Value = serde_json::from_str(&line).unwrap();
                assert_eq!(envelope["version"], 2);
                assert_eq!(envelope["instance"], native_snapshot.supervisor_id);
                assert_eq!(envelope["token"], "protocol-fixture-token");
                let request: runtime_supervisor::Request =
                    serde_json::from_value(envelope["request"].clone()).unwrap();
                requests.lock().await.push(request.action);
                let mut reply = serde_json::to_vec(&serde_json::json!({
                    "instance":native_snapshot.supervisor_id, "snapshot":native_snapshot, "error":null,
                })).unwrap();
                reply.push(b'\n');
                stream.get_mut().write_all(&reply).await.unwrap();
            }
        }));
        let reused = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            reuse_management_owner(
                &http_address,
                &source,
                &state_root,
                &identity.application_id,
            ),
        )
        .await
        .expect("management reuse must not wait for the business fence to clear")
        .unwrap();
        assert_eq!(reused, ManagementReuse::Ready);
        let observed = native_requests.lock().await.clone();
        assert!(!observed.is_empty());
        assert!(
            observed
                .iter()
                .all(|action| *action == runtime_supervisor::Action::Status),
            "management reuse must not send Stop/Recover/Shutdown: {observed:?}"
        );
        assert_eq!(
            unrelated_http.load(Ordering::SeqCst),
            0,
            "no readiness/Start/Stop/business API calls"
        );
        assert_eq!(
            std::fs::read(source.join("release.lock.toml")).unwrap(),
            source_bytes
        );
        assert_eq!(
            std::fs::read_dir(&source).unwrap().count(),
            1,
            "management reuse must not write source files"
        );
        assert_eq!(std::fs::read(&operation_path).unwrap(), operation_bytes);
        assert_eq!(
            std::fs::read_dir(state_root.join("operations"))
                .unwrap()
                .count(),
            1
        );
        assert_eq!(
            std::fs::read(state_root.join("identity.json")).unwrap(),
            identity_bytes
        );
        assert_eq!(
            std::fs::read(state_root.join("supervisor.json")).unwrap(),
            discovery_bytes
        );
        let retained = runtime_supervisor::last_snapshot(&state_root).unwrap();
        assert_eq!(retained.phase, runtime_supervisor::Phase::RecoveryRequired);
        assert_eq!(
            retained.problem, snapshot.problem,
            "reuse cannot clear the business fence"
        );
        assert!(
            runtime_supervisor::Owner::try_acquire(&state_root)
                .unwrap()
                .is_none()
        );
        for task in [&mut http_task, &mut native_task] {
            task.0.abort();
            assert!((&mut task.0).await.unwrap_err().is_cancelled());
        }
    }

    #[test]
    fn wildcard_listener_is_connected_through_loopback() {
        assert_eq!(connect_address("0.0.0.0:3010"), "127.0.0.1:3010");
        assert_eq!(connect_address("[::]:3010"), "[::1]:3010");
        assert_eq!(connect_address("127.0.0.1:3010"), "127.0.0.1:3010");
        assert_eq!(connect_address("localhost:3010"), "localhost:3010");
    }

    #[tokio::test]
    async fn identity_initialization_wait_is_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        // Socket accepts connections but never serves HTTP.
        let started = tokio::time::Instant::now();
        assert!(
            discover_owner(
                &address,
                tempfile::tempdir().unwrap().path(),
                std::time::Duration::from_millis(80),
            )
            .await
            .is_none()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[tokio::test]
    async fn dispatch_waits_for_initialization_but_not_business_readiness() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let probes = Arc::new(AtomicUsize::new(0));
        let state = probes.clone();
        let router = axum::Router::new().route(
            "/ready",
            axum::routing::get(move || {
                let state = state.clone();
                async move {
                    let status = if state.fetch_add(1, Ordering::SeqCst) < 2 {
                        "initializing"
                    } else {
                        "not_ready"
                    };
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({"status":status})),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let server = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        wait_until_initialized(&client, &address, std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(probes.load(Ordering::SeqCst), 3);
        server.abort();

        let silent = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let started = tokio::time::Instant::now();
        assert!(
            wait_until_initialized(
                &client,
                &silent.local_addr().unwrap().to_string(),
                std::time::Duration::from_millis(80)
            )
            .await
            .is_err()
        );
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    fn envelope<T: serde::Serialize>(data: &T) -> serde_json::Value {
        serde_json::json!({"success": true, "code": "OK", "data": data, "message": "ok"})
    }

    /// R02 反例：锁被 serve owner 持有时，legacy CLI 重复调用**转交唯一
    /// owner**（提交 Start + 等终态），不再本地起第二套编排；身份/协议/
    /// 凭据不符则明确拒绝。
    #[tokio::test]
    async fn legacy_cli_dispatches_to_running_owner() {
        // mock owner：身份匹配 + token 落盘 + 受理→Succeeded
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws-d");
        std::fs::create_dir_all(ws.join("backend-go")).unwrap();
        std::fs::write(
            ws.join("workspace.manifest.toml"),
            "schema_version=1\n[workspace]\nname='demo'\n",
        )
        .unwrap();
        let current_project = "schema_version=1\n[project]\nservice_id='backend-go'\nname='Go Backend'\ntype='go'\n[build]\ncommand=['true']\nartifact='server.zip'\n[run]\ncommand=['./server']\nshutdown_timeout_seconds=3\n[health]\nstartup_path='/health'\nreadiness_path='/ready'\nliveness_path='/health'\n[proxy]\npath='/api/go/'\nstrip_prefix=true\n";
        let project_path = ws.join("backend-go/project.manifest.toml");
        std::fs::write(&project_path, current_project).unwrap();
        std::fs::write(
            ws.join("release.lock.toml"),
            include_str!("../../../workspace-manifest/tests/fixtures/lock_v1.toml"),
        )
        .unwrap();
        let application_id = std::env::var("PROJECT_ID")
            .ok()
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "unknown-app".into());
        let state_root =
            crate::runtime_kernel::RuntimeStore::resolve_root(&ws, &application_id).unwrap();
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("token"), "tok\n").unwrap();

        let identity = RuntimeIdentityView {
            application_id: application_id.clone(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-d".into(),
            source_root: ws.canonicalize().unwrap().to_string_lossy().into_owned(),
            runtime_instance_id: "inst-d".into(),
            deployment_generation_id: "g".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let identity_for_probe = identity.clone();
        let probes = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let identity_probes = probes.clone();
        let submitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = submitted.clone();
        let app = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity_for_probe.clone();
                    let ready =
                        identity_probes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) >= 2;
                    async move {
                        if ready {
                            (axum::http::StatusCode::OK, axum::Json(envelope(&identity)))
                        } else {
                            (
                                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                                axum::Json(serde_json::json!({"code":"ERR_PROTOCOL_UNSUPPORTED"})),
                            )
                        }
                    }
                }),
            )
            .route(
                "/ready",
                axum::routing::get(|| async {
                    // Business not-ready is not an initialization gate.
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(
                            serde_json::json!({"status":"not_ready","phase":"Orchestrating"}),
                        ),
                    )
                }),
            )
            .route(
                "/v1/runtime/status",
                axum::routing::get(|| async {
                    let status = shared_types::RuntimeStatusView {
                        desired: shared_types::DesiredState::Running,
                        observed: shared_types::ObservedHealth::Ready,
                        active_target: None,
                        revision: 3,
                        active_operation_id: None,
                        recovery_protection: false,
                        runtime_instance_id: "inst-d".into(),
                    };
                    axum::Json(envelope(&status))
                }),
            )
            .route(
                "/v1/runtime/operations",
                axum::routing::post(move |body: String| {
                    let sink = sink.clone();
                    async move {
                        let request: RuntimeOperationRequest = serde_json::from_str(&body).unwrap();
                        sink.lock().unwrap().push(body);
                        let receipt = shared_types::RuntimeOperationAccepted {
                            poll: format!("/v1/runtime/operations/{}", request.operation_id),
                            operation_id: request.operation_id,
                            state: RuntimeOperationState::Accepted,
                        };
                        (
                            axum::http::StatusCode::ACCEPTED,
                            axum::Json(envelope(&receipt)),
                        )
                    }
                }),
            )
            .route(
                "/v1/runtime/operations/{id}",
                axum::routing::get(
                    |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        let view = RuntimeOperationView {
                            operation_id: id,
                            kind: RuntimeOperationKind::Start,
                            state: RuntimeOperationState::Succeeded,
                            request_digest: "d".repeat(64),
                            revision: 4,
                            runtime_instance_id: "inst-d".into(),
                            error_code: None,
                            error_message: None,
                            failure_detail: None,
                        };
                        axum::Json(envelope(&view))
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let task = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        // 转交：终态 Succeeded + 请求形态 Start/Source/revision=3
        match dispatch_to_owner(&addr, &ws, &state_root, &application_id)
            .await
            .unwrap()
        {
            OwnerDispatch::Terminal(view) => {
                assert_eq!(view.state, RuntimeOperationState::Succeeded);
            }
            other => panic!("expected Terminal, got {other:?}"),
        }
        let posted = submitted.lock().unwrap()[0].clone();
        assert!(probes.load(std::sync::atomic::Ordering::SeqCst) >= 3);
        assert!(posted.contains("\"kind\":\"start\""), "posted: {posted}");
        assert!(
            posted.contains("\"expected_revision\":3"),
            "posted: {posted}"
        );

        // Exercise the public serve entry while a different owner holds the
        // lock: it must submit to that owner, without opening its own listener.
        let _guard = crate::platform::owner_guard::OwnerGuard::acquire(&state_root).unwrap();
        let args = crate::RuntimeArgs {
            workspace: ws.clone(),
            log_dir: dir.path().join("logs"),
            admin_addr: addr.clone(),
            pingap_bin: dir.path().join("must-not-execute"),
            attach: false,
            control_only: false,
        };
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            crate::server::serve(&args),
        )
        .await
        .expect("serve dispatch is bounded")
        .expect("serve forwards instead of rebinding");
        assert_eq!(submitted.lock().unwrap().len(), 2);
        let record = crate::runtime_kernel::EndpointRecord {
            protocol_version: identity.protocol_version,
            application_id: identity.application_id.clone(),
            workspace_id: identity.workspace_id.clone(),
            runtime_instance_id: identity.runtime_instance_id.clone(),
            address: addr.clone(),
        };
        std::fs::write(
            state_root.join("endpoint.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        // The caller does not know the ephemeral port. Dispatch must discover
        // the recorded listener and still validate the complete project identity.
        let discovered = dispatch_to_owner("127.0.0.1:0", &ws, &state_root, &application_id)
            .await
            .unwrap();
        assert!(matches!(discovered, OwnerDispatch::Terminal(_)));
        assert_eq!(submitted.lock().unwrap().len(), 3);

        let stale = crate::runtime_kernel::EndpointRecord {
            runtime_instance_id: "previous-instance".into(),
            ..record
        };
        std::fs::write(
            state_root.join("endpoint.json"),
            serde_json::to_vec(&stale).unwrap(),
        )
        .unwrap();
        assert!(
            discover_owner(
                "127.0.0.1:0",
                &state_root,
                std::time::Duration::from_millis(100)
            )
            .await
            .is_none(),
            "a stale instance record must never authorize token dispatch"
        );
        assert_eq!(submitted.lock().unwrap().len(), 3);
        std::fs::remove_file(state_root.join("endpoint.json")).unwrap();
        assert!(
            crate::platform::owner_guard::OwnerGuard::try_acquire(&state_root)
                .unwrap()
                .is_none()
        );

        // 身份不符（应用不同）→ 拒绝（不猜、不杀；server 仍在线）
        let error = dispatch_to_owner(&addr, &ws, &state_root, "app-OTHER")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("different app-cli owner"),
            "got: {error:#}"
        );
        // Identical leaf names do not establish project identity.
        let other_ws = dir.path().join("other").join("ws-d");
        std::fs::create_dir_all(&other_ws).unwrap();
        assert!(
            dispatch_to_owner(&addr, &other_ws, &state_root, &application_id)
                .await
                .unwrap_err()
                .to_string()
                .contains("different app-cli owner")
        );
        assert_eq!(submitted.lock().unwrap().len(), 3);

        // Capability follows current Source input, not the old derived lock.
        // Keep this older owner's empty capability list: an unsupported new
        // strategy must fail before admission, and corrected input may retry.
        let cached = std::fs::read(ws.join("release.lock.toml")).unwrap();
        std::fs::write(
            &project_path,
            current_project.replace("[health]", "[health]\nstartup_probe='http'"),
        )
        .unwrap();
        let error = dispatch_to_owner(&addr, &ws, &state_root, &application_id)
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains(workspace_manifest::STARTUP_PROBE_CAPABILITY),
            "got: {error:#}"
        );
        assert_eq!(submitted.lock().unwrap().len(), 3);
        assert_eq!(std::fs::read(ws.join("release.lock.toml")).unwrap(), cached);
        std::fs::write(&project_path, current_project).unwrap();
        assert!(matches!(
            dispatch_to_owner(&addr, &ws, &state_root, &application_id)
                .await
                .unwrap(),
            OwnerDispatch::Terminal(_)
        ));
        assert_eq!(submitted.lock().unwrap().len(), 4);
        assert_eq!(std::fs::read(ws.join("release.lock.toml")).unwrap(), cached);
        task.abort();
    }
}
