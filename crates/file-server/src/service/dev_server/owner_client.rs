//! app-cli 运行态 owner 客户端（P3-02：平台复用既有 owner，不再二次 spawn）。
//!
//! 探测/凭据/操作三件套（cross-platform.md §3）：
//! - [`probe_owner`]：GET /v1/runtime/identity（无需认证）——发现线索，
//!   身份核验后才算命中；
//! - [`read_owner_token`]：状态根 `token` 文件（owner 侧 APP_CLI_DEPLOY_TOKEN
//!   启用时落盘，Unix 0600）；
//! - [`OwnerClient`]：status/submit/wait——运行操作提交与查询（X-Deploy-Token）。
//!
//! 所有请求 no_proxy——本机 owner 探测不得经系统 HTTP 代理转发（XP10）。

use anyhow::{Context, Result, bail};
use shared_types::{
    RUNTIME_CONTROL_PROTOCOL_VERSION, RunProfileInput, RuntimeIdentityView, RuntimeOperationKind,
    RuntimeOperationRequest, RuntimeOperationView, RuntimeStatusView,
};

/// 探测既有 owner（无认证；任何 HTTP 层失败视为无 owner，不区分原因——
/// 调用方在"无 owner"时走本地 spawn 路径，探测失败≠可以抢锁）。
pub(super) async fn probe_owner(address: &str) -> Option<RuntimeIdentityView> {
    let url = format!("http://{address}/v1/runtime/identity");
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

/// 读取状态根的 owner 凭据文件（owner 未启用写端点时不存在）。
pub(super) fn read_owner_token(state_root: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(state_root.join("token"))
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
}

/// 平台侧解析状态根（与 app-cli RuntimeStore::resolve_root 同一规则：
/// env 权威 → 缺省 {卷根}/.app-cli-state/{application_id}）。
pub(super) fn owner_state_root(
    workspace: &std::path::Path,
    application_id: &str,
) -> Result<std::path::PathBuf> {
    if let Some(explicit) = std::env::var_os("APP_CLI_STATE_ROOT").filter(|value| !value.is_empty())
    {
        return Ok(std::path::PathBuf::from(explicit));
    }
    let volume_root = workspace
        .parent()
        .context("workspace has no volume root for owner state")?;
    Ok(volume_root.join(".app-cli-state").join(application_id))
}

/// 认证后的运行操作客户端。
pub(super) struct OwnerClient {
    address: String,
    token: String,
    client: reqwest::Client,
}

impl OwnerClient {
    pub(super) fn new(address: &str, token: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .no_proxy()
            .build()
            .context("build owner api client")?;
        Ok(Self {
            address: address.to_string(),
            token: token.to_string(),
            client,
        })
    }

    /// 当前 revision（提交操作的期望值来源）。
    pub(super) async fn status(&self) -> Result<RuntimeStatusView> {
        let url = format!("http://{}/v1/runtime/status", self.address);
        let body = self
            .client
            .get(&url)
            .header("X-Deploy-Token", &self.token)
            .send()
            .await
            .with_context(|| format!("query owner status at {}", self.address))?
            .error_for_status()
            .with_context(|| format!("owner status rejected at {}", self.address))?
            .json::<serde_json::Value>()
            .await
            .context("parse status response")?;
        let data = body
            .get("data")
            .cloned()
            .context("status response missing data")?;
        serde_json::from_value(data).context("decode status view")
    }

    /// 提交源码 Restart（平台 start/restart 复用既有 owner 的执行路径）。
    /// instance_id 来自探测到的 identity——owner 校验后拒绝旧实例请求。
    pub(super) async fn submit_restart_source(
        &self,
        operation_id: &str,
        workspace_id: &str,
        expected_revision: u64,
        instance_id: &str,
    ) -> Result<RuntimeOperationView> {
        self.submit(
            operation_id,
            workspace_id,
            expected_revision,
            instance_id,
            RuntimeOperationKind::Restart,
        )
        .await
    }

    /// 提交 Stop（external owner 的停止路径——不经进程信号）。
    pub(super) async fn submit_stop(
        &self,
        operation_id: &str,
        workspace_id: &str,
        expected_revision: u64,
        instance_id: &str,
    ) -> Result<RuntimeOperationView> {
        self.submit(
            operation_id,
            workspace_id,
            expected_revision,
            instance_id,
            RuntimeOperationKind::Stop,
        )
        .await
    }

    async fn submit(
        &self,
        operation_id: &str,
        workspace_id: &str,
        expected_revision: u64,
        instance_id: &str,
        kind: RuntimeOperationKind,
    ) -> Result<RuntimeOperationView> {
        let request = RuntimeOperationRequest {
            operation_id: operation_id.to_string(),
            expected_runtime_instance_id: instance_id.to_string(),
            expected_revision,
            workspace_id: workspace_id.to_string(),
            kind,
            profile: RunProfileInput::Source {
                workspace_id: workspace_id.to_string(),
            },
            request_context: None,
        };
        let url = format!("http://{}/v1/runtime/operations", self.address);
        let response = self
            .client
            .post(&url)
            .header("X-Deploy-Token", &self.token)
            .json(&request)
            .send()
            .await
            .with_context(|| format!("submit runtime operation to {}", self.address))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.context("parse submit response")?;
        if !status.is_success() {
            bail!(
                "owner rejected operation: {} ({})",
                body.get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
                body.get("code").and_then(|v| v.as_str()).unwrap_or("ERR"),
            );
        }
        let data = body
            .get("data")
            .cloned()
            .context("submit response missing data")?;
        serde_json::from_value(data).context("decode accepted operation")
    }

    /// 查询操作状态。
    pub(super) async fn operation(&self, operation_id: &str) -> Result<RuntimeOperationView> {
        let url = format!(
            "http://{}/v1/runtime/operations/{operation_id}",
            self.address
        );
        let response = self
            .client
            .get(&url)
            .header("X-Deploy-Token", &self.token)
            .send()
            .await
            .with_context(|| format!("query operation {operation_id}"))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.context("parse operation response")?;
        if !status.is_success() {
            bail!(
                "owner operation query failed: {}",
                body.get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
            );
        }
        let data = body
            .get("data")
            .cloned()
            .context("operation response missing data")?;
        serde_json::from_value(data).context("decode operation view")
    }

    /// 轮询操作至终态（有界）。
    pub(super) async fn wait_terminal(
        &self,
        operation_id: &str,
        timeout: std::time::Duration,
    ) -> Result<RuntimeOperationView> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let view = self.operation(operation_id).await?;
            if view.state.is_terminal() {
                return Ok(view);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "runtime operation {operation_id} did not reach terminal state in {timeout:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

/// 协议兼容性预检：owner 协议版本必须与平台一致。
pub(super) fn protocol_compatible(identity: &RuntimeIdentityView) -> bool {
    identity.protocol_version == RUNTIME_CONTROL_PROTOCOL_VERSION
}
