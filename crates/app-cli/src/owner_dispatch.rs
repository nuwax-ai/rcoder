//! legacy 无子命令形态的 owner 分派（R02）。
//!
//! 同一 workspace 已有 serve owner（OwnerGuard 被活进程持有）时，重复的
//! legacy CLI 调用**不再绕过所有权**本地起第二套编排——转为运行 API 客户端：
//! 提交 Start/Restart(Source) 并等待终态（“重复启动是正常操作”）。身份不符、
//! 协议不兼容、凭据缺失 → 明确拒绝（不杀对方、不换端口、不删锁文件）。
//!
//! 无人持锁 → 调用方走本地 legacy 编排（行为不变，且进程持锁期间第三个
//! CLI 同样被分派/拒绝——owner 唯一性由锁保证，不是靠端口探测猜测）。

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
    /// 无人持锁/无 owner 应答——调用方走本地 legacy 编排。
    NoOwner,
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
    let owner_root = std::path::Path::new(&identity.source_root)
        .canonicalize()
        .context("resolve owner workspace identity")?;
    if runtime_state_layout::canonical_project_root(&owner_root)
        != runtime_state_layout::canonical_project_root(&expected_root)
        || identity.application_id != application_id
    {
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
    let operation_id = format!("cli-dispatch-{}", uuid::Uuid::new_v4().simple());
    let request = RuntimeOperationRequest {
        operation_id: operation_id.clone(),
        expected_runtime_instance_id: identity.runtime_instance_id.clone(),
        expected_revision: status.revision,
        workspace_id: expected_ws.clone(),
        kind: RuntimeOperationKind::Start,
        profile: RunProfileInput::Source {
            workspace_id: expected_ws.clone(),
        },
        run_config: None,
        request_context: None,
    };
    let submit_url = format!("http://{admin_addr}/v1/runtime/operations");
    let response = client
        .post(&submit_url)
        .header("X-Deploy-Token", &token)
        .json(&request)
        .send()
        .await
        .context("submit dispatch operation")?;
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
            .context("dispatched operation observation deadline exceeded")??;
        anyhow::ensure!(
            view.operation_id == operation_id
                && view.runtime_instance_id == identity.runtime_instance_id
                && view.kind == RuntimeOperationKind::Start,
            "owner returned a different operation or runtime identity while observing {operation_id}"
        );
        if view.state.is_terminal() {
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
        RuntimeOperationState::Cancelled => bail!(
            "dispatched start was cancelled by the owner (operation {})",
            view.operation_id
        ),
        other => bail!(
            "dispatched start failed: {other:?} ({})",
            view.error_message.as_deref().unwrap_or("no detail"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        std::fs::create_dir_all(&ws).unwrap();
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
        task.abort();
    }
}
