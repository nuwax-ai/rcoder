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

/// 探测既有 owner 的运行身份（无认证端点；返回 None = 无应答）。
async fn probe_identity(admin_addr: &str) -> Option<RuntimeIdentityView> {
    let url = format!("http://{admin_addr}/v1/runtime/identity");
    let response = reqwest::Client::builder()
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

/// legacy app-cli 探测（仅 /v1/deploy/status 应答且带 protocol_version）。
async fn legacy_app_cli_responds(admin_addr: &str) -> bool {
    let url = format!("http://{admin_addr}/v1/deploy/status");
    let Ok(client) = reqwest::Client::builder()
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
    // R09/XP10：发现记录（endpoint.json）是线索不是证明——身份核验全字段
    let Some(identity) = probe_identity(admin_addr).await else {
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

    if identity.protocol_version != RUNTIME_CONTROL_PROTOCOL_VERSION {
        bail!(
            "app-cli owner at {admin_addr} speaks incompatible protocol v{} (expected v{}); \
             upgrade it before dispatching",
            identity.protocol_version,
            RUNTIME_CONTROL_PROTOCOL_VERSION
        );
    }
    let expected_ws = workspace
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    if identity.workspace_id != expected_ws || identity.application_id != application_id {
        bail!(
            "admin port {admin_addr} is held by a different app-cli owner \
             (app {}/{}, expected {}/{expected_ws}); refusing to start a \
             competing orchestrator",
            identity.application_id,
            identity.workspace_id,
            application_id
        );
    }

    // 凭据：owner 启用写端点时落盘状态根（与平台侧同一读取契约）
    let token = crate::runtime_kernel::RuntimeStore::read_token(state_root);
    let Some(token) = token else {
        bail!(
            "workspace is already managed by an app-cli owner whose runtime API \
             credentials are unavailable (APP_CLI_DEPLOY_TOKEN not enabled); \
             stop it or restart it with the token enabled"
        );
    };
    // 发现记录交叉核验（记录存在但不匹配当前身份 = 陈旧记录——线索作废
    // 不阻断（HTTP 身份已是权威），但记录不匹配时更新留待 owner 侧）
    if let Ok(record) = serde_json::from_slice::<crate::runtime_kernel::EndpointRecord>(
        &std::fs::read(state_root.join("endpoint.json")).unwrap_or_default(),
    ) && !crate::runtime_kernel::endpoint_matches_identity(&record, &identity)
    {
        tracing::warn!(
            "stale endpoint discovery record at {} (recorded instance differs from live identity);              trusting the live HTTP identity",
            state_root.join("endpoint.json").display()
        );
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .no_proxy()
        .build()
        .context("build owner dispatch client")?;

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
    let mut view: RuntimeOperationView =
        serde_json::from_value(envelope_data(&body)?).context("decode accepted operation")?;

    // 等待终态（有界；受 owner 侧启动预算约束）
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        if view.state.is_terminal() {
            break;
        }
        anyhow::ensure!(
            tokio::time::Instant::now() < deadline,
            "dispatched operation {operation_id} did not reach terminal state in 600s"
        );
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let url = format!("http://{admin_addr}/v1/runtime/operations/{operation_id}");
        let body: serde_json::Value = client
            .get(&url)
            .header("X-Deploy-Token", &token)
            .send()
            .await
            .context("poll dispatch operation")?
            .json()
            .await
            .context("parse operation poll")?;
        view = serde_json::from_value(envelope_data(&body)?).context("decode operation view")?;
    }
    Ok(OwnerDispatch::Terminal(view))
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
        let state_root = dir.path().join(".app-cli-state").join("app-d");
        std::fs::create_dir_all(&state_root).unwrap();
        std::fs::write(state_root.join("token"), "tok\n").unwrap();

        let identity = RuntimeIdentityView {
            application_id: "app-d".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-d".into(),
            source_root: "/ws".into(),
            runtime_instance_id: "inst-d".into(),
            deployment_generation_id: "g".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: Vec::new(),
        };
        let identity_for_probe = identity.clone();
        let submitted = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = submitted.clone();
        let app = axum::Router::new()
            .route(
                "/v1/runtime/identity",
                axum::routing::get(move || {
                    let identity = identity_for_probe.clone();
                    async move { axum::Json(envelope(&identity)) }
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
                        sink.lock().unwrap().push(body);
                        let view = RuntimeOperationView {
                            operation_id: "op-d".into(),
                            kind: RuntimeOperationKind::Start,
                            state: RuntimeOperationState::Accepted,
                            request_digest: "d".repeat(64),
                            revision: 3,
                            runtime_instance_id: "inst-d".into(),
                            error_code: None,
                            error_message: None,
                            failure_detail: None,
                        };
                        axum::Json(envelope(&view))
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
        match dispatch_to_owner(&addr, &ws, &state_root, "app-d")
            .await
            .unwrap()
        {
            OwnerDispatch::Terminal(view) => {
                assert_eq!(view.state, RuntimeOperationState::Succeeded);
            }
            other => panic!("expected Terminal, got {other:?}"),
        }
        let posted = submitted.lock().unwrap()[0].clone();
        assert!(posted.contains("\"kind\":\"start\""), "posted: {posted}");
        assert!(
            posted.contains("\"expected_revision\":3"),
            "posted: {posted}"
        );

        // 身份不符（应用不同）→ 拒绝（不猜、不杀；server 仍在线）
        let error = dispatch_to_owner(&addr, &ws, &state_root, "app-OTHER")
            .await
            .unwrap_err();
        assert!(
            error.to_string().contains("different app-cli owner"),
            "got: {error:#}"
        );
        task.abort();
    }
}
