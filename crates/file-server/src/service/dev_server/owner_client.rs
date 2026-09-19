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

/// Verify physical project identity before loading or transmitting owner credentials.
pub(super) fn verify_project_identity(
    identity: &RuntimeIdentityView,
    workspace: &std::path::Path,
    application_id: &str,
) -> Result<()> {
    let source = std::path::Path::new(&identity.source_root);
    anyhow::ensure!(protocol_compatible(identity), "incompatible owner protocol");
    anyhow::ensure!(
        !identity.source_root.trim().is_empty()
            && source.is_absolute()
            && !identity.workspace_id.trim().is_empty()
            && !identity.runtime_instance_id.trim().is_empty()
            && identity.application_id == application_id
            && runtime_state_layout::canonical_project_root(source)
                == runtime_state_layout::canonical_project_root(workspace),
        "different app-cli owner: project or application identity mismatch"
    );
    Ok(())
}

/// 读取状态根的 owner 凭据文件（owner 未启用写端点时不存在）。
pub(super) fn read_owner_token(state_root: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(state_root.join("token"))
        .ok()
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty())
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
    /// `pg`（R08）：每操作 PG 凭据——用户改密后的新凭据经 owner 到达服务
    /// 进程 env（不进摘要/日志；owner 侧持久化时脱敏）。
    pub(super) async fn submit_restart_source(
        &self,
        operation_id: &str,
        workspace_id: &str,
        expected_revision: u64,
        instance_id: &str,
        pg: Option<&shared_types::StartPgCredential>,
    ) -> Result<RuntimeOperationView> {
        self.submit_restart(
            operation_id,
            workspace_id,
            expected_revision,
            instance_id,
            pg,
            RunProfileInput::Source {
                workspace_id: workspace_id.to_string(),
            },
        )
        .await
    }

    /// R03：提交制品 Restart——平台只登记制品（共享卷 builds/ zip），激活由
    /// owner 在身份/revision 核验通过后执行；拒绝不改变 active 运行目录。
    pub(super) async fn submit_restart_artifact(
        &self,
        operation_id: &str,
        workspace_id: &str,
        expected_revision: u64,
        instance_id: &str,
        pg: Option<&shared_types::StartPgCredential>,
        artifact_id: &str,
    ) -> Result<RuntimeOperationView> {
        self.submit_restart(
            operation_id,
            workspace_id,
            expected_revision,
            instance_id,
            pg,
            RunProfileInput::Artifact {
                artifact: shared_types::ArtifactInput::ArtifactId {
                    artifact_id: artifact_id.to_string(),
                },
            },
        )
        .await
    }

    async fn submit_restart(
        &self,
        operation_id: &str,
        workspace_id: &str,
        expected_revision: u64,
        instance_id: &str,
        pg: Option<&shared_types::StartPgCredential>,
        profile: RunProfileInput,
    ) -> Result<RuntimeOperationView> {
        self.submit(
            operation_id,
            workspace_id,
            expected_revision,
            instance_id,
            RuntimeOperationKind::Restart,
            pg,
            profile,
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
            None,
            RunProfileInput::Source {
                workspace_id: workspace_id.to_string(),
            },
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn submit(
        &self,
        operation_id: &str,
        workspace_id: &str,
        expected_revision: u64,
        instance_id: &str,
        kind: RuntimeOperationKind,
        pg: Option<&shared_types::StartPgCredential>,
        profile: RunProfileInput,
    ) -> Result<RuntimeOperationView> {
        let request = RuntimeOperationRequest {
            operation_id: operation_id.to_string(),
            expected_runtime_instance_id: instance_id.to_string(),
            expected_revision,
            workspace_id: workspace_id.to_string(),
            kind,
            profile,
            run_config: pg.map(|pg| shared_types::OperationRunConfig {
                pg: Some(pg.clone()),
            }),
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
        let accepted: shared_types::RuntimeOperationAccepted =
            serde_json::from_value(data).context("decode accepted operation")?;
        anyhow::ensure!(
            accepted.operation_id == operation_id
                && accepted.poll == format!("/v1/runtime/operations/{operation_id}"),
            "owner returned a mismatched operation receipt"
        );
        let view = self.operation(operation_id).await?;
        anyhow::ensure!(
            view.operation_id == operation_id
                && view.runtime_instance_id == instance_id
                && view.kind == kind,
            "owner returned a mismatched operation identity"
        );
        Ok(view)
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

    /// 按 after_seq 游标重放操作事件（R06：端点是有限重放非长连流——
    /// 轮询 + 游标天然支持断线续传，无 SSE 总超时/EOF 竞态问题）。
    pub(super) async fn events_after(
        &self,
        operation_id: &str,
        after_seq: u64,
    ) -> Result<Vec<shared_types::RuntimeEventRecord>> {
        let url = format!(
            "http://{}/v1/runtime/operations/{operation_id}/events?after_seq={after_seq}",
            self.address
        );
        let response = self
            .client
            .get(&url)
            .header("X-Deploy-Token", &self.token)
            .send()
            .await
            .with_context(|| format!("replay events for {operation_id} after {after_seq}"))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.context("parse events response")?;
        if !status.is_success() {
            bail!(
                "owner events replay failed: {}",
                body.get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown"),
            );
        }
        let data = body
            .get("data")
            .cloned()
            .context("events response missing data")?;
        let events = data
            .get("events")
            .cloned()
            .context("events response missing events array")?;
        serde_json::from_value(events).context("decode event records")
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

/// owner 凭据文件定位（R09：与 app-cli 同一解析契约——显式 env /
/// PROJECT_ID 应用段 / standalone 项目登记表；`.run`/symlink 经契约折叠，
/// 不再父目录/当前目录各猜一次）。契约未命中时回落 legacy 双探测（旧布局
/// 残留兼容），命中即返回含 token 文件的状态根。
pub(super) fn find_owner_token(
    workspace: &std::path::Path,
    application_id: &str,
) -> Option<(std::path::PathBuf, String)> {
    let explicit = std::env::var_os("APP_CLI_STATE_ROOT");
    let project_id =
        (application_id != "unknown-app").then(|| std::ffi::OsString::from(application_id));
    if let Ok(Some(root)) = runtime_state_layout::resolve_state_root(
        workspace,
        explicit.as_deref(),
        project_id.as_deref(),
    ) && let Some(token) = read_owner_token(&root)
    {
        return Some((root, token));
    }
    // legacy 双探测（旧布局残留）。以契约的规范化项目根为基准（`.run`
    // 折叠到源码根后取其卷根——制品态 owner 的状态根挂源码卷，而非 .run
    // 的字面 parent），避免制品/源码两入口探测到不同目录。
    let project_root = runtime_state_layout::canonical_project_root(workspace);
    let mut candidates = Vec::new();
    if let Some(parent) = project_root.parent() {
        candidates.push(parent.join(".app-cli-state").join(application_id));
    }
    if let Some(parent) = workspace.parent() {
        candidates.push(parent.join(".app-cli-state").join(application_id));
    }
    candidates.push(workspace.join(".app-cli-state").join(application_id));
    for root in candidates {
        if let Some(token) = read_owner_token(&root) {
            return Some((root, token));
        }
    }
    None
}

/// 运行事件转发（R06）：按 after_seq 游标轮询重放（端点是有限重放非长连
/// SSE——轮询天然支持断线续传，无总超时/EOF 竞态；UTF-8 由完整记录的
/// serde 反序列化保证，无跨 chunk 切割问题）。终态判定在调用方
/// （`wait_terminal`）；`cursor` 记录已消费 sequence（调用方终态后经
/// [`drain_operation_events`] 按 cursor 排空剩余事件——终态事件在状态
/// 可见后落 journal，盲目 abort 会丢终局）。
pub(super) async fn forward_operation_events(
    client: &OwnerClient,
    operation_id: &str,
    mut on_line: impl FnMut(String) + Send,
    stop: &tokio_util::sync::CancellationToken,
    cursor: &std::sync::atomic::AtomicU64,
    poll_interval: std::time::Duration,
) {
    use std::sync::atomic::Ordering;
    let mut after_seq = cursor.load(Ordering::SeqCst);
    loop {
        match client.events_after(operation_id, after_seq).await {
            Ok(records) => {
                for record in &records {
                    after_seq = after_seq.max(record.sequence);
                    cursor.store(after_seq, Ordering::SeqCst);
                    if let Some(legacy) = to_legacy_evt(record) {
                        on_line(legacy);
                    }
                }
            }
            // 轮询失败不终止转发（owner 短暂不可达时下一轮游标继续）；
            // 终态与终局由调用方裁决
            Err(error) => tracing::warn!(%error, "owner event replay poll failed (will retry)"),
        }
        if stop.is_cancelled() {
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(poll_interval) => {}
            _ = stop.cancelled() => return,
        }
    }
}

/// 终态后的最后一轮排空（R06：终态事件按 sequence 全部送达后再完成；
/// 至多重放一轮，坏记录不静默丢——解析失败记 warn 保留失败清单）。
pub(super) async fn drain_operation_events(
    client: &OwnerClient,
    operation_id: &str,
    after_seq: u64,
    mut on_line: impl FnMut(String) + Send,
) -> Result<u64> {
    let records = client.events_after(operation_id, after_seq).await?;
    let mut last = after_seq;
    for record in &records {
        last = last.max(record.sequence);
        if let Some(legacy) = to_legacy_evt(record) {
            on_line(legacy);
        }
    }
    Ok(last)
}

/// RuntimeEventRecord → 旧 EVT 行（map_app_cli_evt 消费的同构 JSON）。
/// - 服务级事件（service_starting 等）原样透传；
/// - 操作终态（Completed/Failed）映射为平台的 `orchestration_done` 终局
///   （R06：真实 owner 成功也必须产生平台所需 Done，Failed 携带错误明细）；
/// - 无 event_name 的纯 stage 记录跳过（旧管道无对应消费者）。
pub(super) fn to_legacy_evt(record: &shared_types::RuntimeEventRecord) -> Option<String> {
    let name = record.event_name.as_deref()?;
    if name == "Completed" || name == "Failed" {
        let failed = if name == "Failed" {
            let error = record
                .payload
                .as_ref()
                .and_then(|payload| payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("runtime operation failed");
            vec![serde_json::json!({"service": "orchestrator", "error": error})]
        } else {
            Vec::new()
        };
        return Some(
            serde_json::json!({"event": "orchestration_done", "failed": failed}).to_string(),
        );
    }
    let mut value = serde_json::json!({"event": name});
    if let Some(service) = &record.service {
        value["service"] = serde_json::Value::String(service.clone());
    }
    if let Some(payload) = &record.payload
        && let Some(object) = payload.as_object()
    {
        for (key, item) in object {
            value[key] = item.clone();
        }
    }
    Some(value.to_string())
}

#[cfg(test)]
mod r09_tests {
    use super::*;

    #[test]
    fn identity_uses_canonical_project_not_workspace_leaf() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("中文 project");
        let run = project.join(".run");
        std::fs::create_dir_all(&run).unwrap();
        let mut identity = RuntimeIdentityView {
            application_id: "app".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-stable-hash".into(),
            source_root: project.to_string_lossy().into(),
            runtime_instance_id: "instance".into(),
            deployment_generation_id: "generation".into(),
            protocol_version: RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        };
        assert!(verify_project_identity(&identity, &run, "app").is_ok());
        assert!(verify_project_identity(&identity, &project, "other-app").is_err());
        let different = root.path().join("other").join("中文 project");
        std::fs::create_dir_all(&different).unwrap();
        assert!(verify_project_identity(&identity, &different, "app").is_err());
        identity.source_root.clear();
        assert!(verify_project_identity(&identity, &project, "app").is_err());
        identity.source_root = "relative".into();
        assert!(verify_project_identity(&identity, &project, "app").is_err());
    }

    /// R09：standalone（无 PROJECT_ID）凭据查找经项目登记表——兄弟项目
    /// 各自命中自己的 token；`.run` 入口折叠回源码根（同锁域同凭据）。
    #[test]
    fn find_owner_token_uses_project_registry_for_siblings() {
        let volume = tempfile::tempdir().unwrap();
        let ws_a = volume.path().join("sibling-a");
        let ws_b = volume.path().join("sibling-b");
        std::fs::create_dir_all(&ws_a).unwrap();
        std::fs::create_dir_all(&ws_b).unwrap();
        // 经 app-cli 同源契约登记（ensure 侧）
        let root_a = runtime_state_layout::ensure_state_root(&ws_a, None, None).unwrap();
        let root_b = runtime_state_layout::ensure_state_root(&ws_b, None, None).unwrap();
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        std::fs::write(root_a.join("token"), "token-a\n").unwrap();
        std::fs::write(root_b.join("token"), "token-b\n").unwrap();

        let (found_a_root, token_a) = find_owner_token(&ws_a, "unknown-app").expect("token a");
        assert_eq!(token_a, "token-a");
        assert_eq!(found_a_root, root_a);
        let (_, token_b) = find_owner_token(&ws_b, "unknown-app").expect("token b");
        assert_eq!(token_b, "token-b");

        // .run 入口（产物态）折叠回源码项目根 → 同一凭据
        let run_a = ws_a.join(".run");
        std::fs::create_dir_all(&run_a).unwrap();
        let (_, token_run) = find_owner_token(&run_a, "unknown-app").expect("token via .run");
        assert_eq!(token_run, "token-a");
    }
}
