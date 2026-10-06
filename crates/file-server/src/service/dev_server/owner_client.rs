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

use anyhow::{Context, Result, bail, ensure};
use shared_types::{
    RUNTIME_CONTROL_PROTOCOL_VERSION, RuntimeIdentityView, RuntimeOperationRequest,
    RuntimeOperationView, RuntimeStatusView,
};

/// Transport observations are not proof that the previous process tree stopped.
#[derive(Debug)]
pub(super) enum OwnerProbe {
    Ready(RuntimeIdentityView),
    Absent,
    Initializing,
    Legacy,
}

pub(super) async fn observe_owner(address: &str) -> Result<OwnerProbe> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .no_proxy()
        .build()?;
    let response = match client
        .get(format!("http://{address}/v1/runtime/identity"))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(&error);
            while let Some(current) = cause {
                if current
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::ConnectionRefused)
                {
                    return Ok(OwnerProbe::Absent);
                }
                cause = current.source();
            }
            return Err(error).context("owner observation failed; process state is unknown");
        }
    };
    let status = response.status();
    // Older app-cli releases have deploy/status but no runtime identity route.
    // A positive legacy response classifies a live process; it never grants
    // permission to clean registrations or start another owner.
    if status == reqwest::StatusCode::NOT_FOUND
        && super::start::legacy_app_cli_responds(address).await
    {
        return Ok(OwnerProbe::Legacy);
    }
    let body: shared_types::HttpResult<RuntimeIdentityView> =
        response.json().await.context("invalid owner response")?;
    if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
        // 0.3.10 的 run 形态按设计禁用 runtime API。
        if body.code == "ERR_PROTOCOL_UNSUPPORTED" {
            return Ok(OwnerProbe::Legacy);
        }
        // 引导窗口的两种短暂形态都交给调用方有界等待：API 层门控的
        // ERR_INITIALIZING，以及 kernel 缺席路径的 ERR_RECOVERY_REQUIRED
        // （compose 闲置回收实测：回收后新 owner 引导期 start 被秒拒）。
        // 其余 503 码保持显式错误，与 run 形态拒绝/未知服务区分（见
        // observation_tests）。
        if matches!(
            body.code.as_str(),
            "ERR_INITIALIZING" | "ERR_RECOVERY_REQUIRED"
        ) {
            return Ok(OwnerProbe::Initializing);
        }
        bail!("owner identity rejected with HTTP {status}: {}", body.code);
    }
    ensure!(
        status.is_success(),
        "owner identity rejected with HTTP {status}"
    );
    let identity = body.data.context("owner identity missing")?;
    Ok(OwnerProbe::Ready(identity))
}

pub(crate) async fn probe_owner(address: &str) -> Result<Option<RuntimeIdentityView>> {
    match observe_owner(address).await? {
        OwnerProbe::Ready(identity) => Ok(Some(identity)),
        OwnerProbe::Absent | OwnerProbe::Legacy => Ok(None),
        OwnerProbe::Initializing => bail!("runtime owner is still initializing"),
    }
}

pub(super) async fn probe_owner_for_stop(address: &str) -> Result<Option<RuntimeIdentityView>> {
    probe_owner(address).await
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
            && identity.service_family == "userapp-dev"
            && identity.application_id == application_id,
        "different app-cli owner: project or application identity mismatch"
    );
    let source_origin = runtime_state_layout::resolve_project_origin(source)?;
    let requested_origin = runtime_state_layout::resolve_project_origin(workspace)?;
    anyhow::ensure!(
        source_origin == requested_origin,
        "different app-cli owner: project origin mismatch"
    );
    if runtime_state_layout::canonical_project_root(source)
        != runtime_state_layout::canonical_project_root(workspace)
    {
        anyhow::ensure!(
            identity
                .capabilities
                .iter()
                .any(|capability| capability == "project-origin-execution"),
            "app-cli owner must be upgraded before reusing a local build directory for source execution"
        );
    }
    Ok(())
}

/// Stop has authority over this application's workspace tree, including a
/// manually assembled legacy deploy directory. Canonicalize both paths so a
/// symlink to another application's directory cannot grant that authority.
/// This does not authorize deploy/start to reinterpret that directory as source.
pub(super) fn verify_stop_identity(
    identity: &RuntimeIdentityView,
    workspace: &std::path::Path,
    application_id: &str,
) -> Result<()> {
    let source = std::path::Path::new(&identity.source_root);
    anyhow::ensure!(
        protocol_compatible(identity)
            && identity.application_id == application_id
            && identity.service_family == "userapp-dev"
            && !identity.workspace_id.trim().is_empty()
            && !identity.runtime_instance_id.trim().is_empty()
            && source.is_absolute(),
        "different app-cli owner: application or protocol identity mismatch"
    );
    // Decode origin even for contained paths: corrupted provenance is not bypassed.
    let origin = runtime_state_layout::resolve_project_origin(source)?;
    let expected = runtime_state_layout::resolve_project_origin(workspace)?;
    let physical_source = std::fs::canonicalize(source).context("resolve owner workspace")?;
    let physical_workspace =
        std::fs::canonicalize(workspace).context("resolve requested workspace")?;
    anyhow::ensure!(
        origin == expected || physical_source.starts_with(&physical_workspace),
        "different app-cli owner: workspace is outside the application's project"
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

#[derive(Debug, thiserror::Error)]
#[error("owner rejected operation: {message} ({code})")]
pub(super) struct SubmissionRejected {
    pub code: String,
    pub message: String,
    pub status: reqwest::StatusCode,
}
impl SubmissionRejected {
    pub(super) fn proves_not_admitted(&self) -> bool {
        // runtime_kernel::admit checks durable operation history before these branches.
        // Identity/protocol/backend/recovery/id-conflict errors do not provide that proof.
        self.status == reqwest::StatusCode::CONFLICT
            && matches!(
                self.code.as_str(),
                shared_types::ERR_REVISION_MISMATCH | shared_types::ERR_OPERATION_IN_PROGRESS
            )
    }
}

/// 认证后的运行操作客户端。
pub(crate) struct OwnerClient {
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

    /// Only available after the owner acquired its lock and confirmed old process
    /// cleanup. A short initializing window (unified owner booting its first
    /// business session, e.g. a concurrent dispatched start) is retried with a
    /// bounded budget instead of failing the build preflight（recovery v2 §9：
    /// 短暂启动窗口使用有界等待）。
    pub(super) async fn recovery(&self) -> Result<shared_types::RuntimeRecoveryView> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
        loop {
            let response = self
                .client
                .get(format!("http://{}/v1/runtime/recovery", self.address))
                .header("X-Deploy-Token", &self.token)
                .send()
                .await?;
            let status = response.status();
            if status == reqwest::StatusCode::SERVICE_UNAVAILABLE {
                // 503 body 仍是标准信封：解码失败本身即异常响应，如实上抛
                // 而不是当"未知 body"吞掉（类型化后无需字符串匹配 code）。
                let envelope: shared_types::HttpResult<serde_json::Value> =
                    response
                        .json()
                        .await
                        .context("decode owner initializing envelope")?;
                if envelope.code == "ERR_INITIALIZING" && tokio::time::Instant::now() < deadline {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
                anyhow::bail!(
                    "owner recovery evidence unavailable (initializing window exceeded); code={}",
                    envelope.code
                );
            }
            if !status.is_success() {
                anyhow::bail!("owner recovery rejected with HTTP {status}");
            }
            let envelope: shared_types::HttpResult<shared_types::RuntimeRecoveryView> =
                response.json().await?;
            return envelope.data.context("recovery evidence missing");
        }
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

    pub(super) async fn submit_request(
        &self,
        request: &RuntimeOperationRequest,
    ) -> Result<RuntimeOperationView> {
        let operation_id = request.operation_id.as_str();
        let instance_id = request.expected_runtime_instance_id.as_str();
        let kind = request.kind;
        let url = format!("http://{}/v1/runtime/operations", self.address);
        let response = self
            .client
            .post(&url)
            .header("X-Deploy-Token", &self.token)
            .json(request)
            .send()
            .await
            .with_context(|| format!("submit runtime operation to {}", self.address))?;
        let status = response.status();
        let body: serde_json::Value = response.json().await.context("parse submit response")?;
        if !status.is_success() {
            return Err(SubmissionRejected {
                code: body
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or("ERR")
                    .to_string(),
                message: body
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown")
                    .to_string(),
                status,
            }
            .into());
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
        self.operation_if_exists(operation_id)
            .await?
            .context("owner operation not found")
    }

    pub(super) async fn operation_if_exists(
        &self,
        operation_id: &str,
    ) -> Result<Option<RuntimeOperationView>> {
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
        if status == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
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
        serde_json::from_value(data)
            .map(Some)
            .context("decode operation view")
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
        self.wait_terminal_until(operation_id, tokio::time::Instant::now() + timeout)
            .await
    }

    pub(super) async fn wait_terminal_until(
        &self,
        operation_id: &str,
        deadline: tokio::time::Instant,
    ) -> Result<RuntimeOperationView> {
        loop {
            // The operation was just submitted to this owner; its record may
            // not be visible on the very first polls (slow shared filesystem,
            // loaded machine). Absence before the deadline is "keep waiting",
            // not a failure — the deadline still bounds the total budget.
            let observation = tokio::time::timeout_at(deadline, self.operation_if_exists(operation_id))
                .await.with_context(|| format!("runtime operation {operation_id} observation exceeded launch deadline; query this original operation before retrying"))??;
            if let Some(view) = observation
                && (view.state.is_terminal()
                    // RecoveryRequired 是持久的可查询结论（结果未知→需显
                    // 式恢复），不会自行演进成其他状态——按终态返回，由
                    // 调用方呈现物理清理未确认或独立未知写入等具体原因，
                    // 不等满预算超时。
                    || view.state
                        == shared_types::RuntimeOperationState::RecoveryRequired)
            {
                return Ok(view);
            }
            anyhow::ensure!(
                tokio::time::Instant::now() < deadline,
                "runtime operation {operation_id} did not reach terminal before launch deadline; query this original operation before retrying"
            );
            tokio::time::sleep_until(
                deadline.min(tokio::time::Instant::now() + std::time::Duration::from_millis(500)),
            )
            .await;
        }
    }
}

/// A foreground owner can exit after committing Stop but before the caller
/// reads its reply. Read the original operation from the root captured while
/// that owner was authenticated; a transport failure alone never proves Stop.
pub(super) fn read_durable_stop_outcome(
    state_root: &std::path::Path,
    request: &RuntimeOperationRequest,
) -> Result<Option<RuntimeOperationView>> {
    ensure!(
        request.kind == shared_types::RuntimeOperationKind::Stop,
        "durable stop observation requires the original Stop request"
    );
    shared_types::validate_runtime_operation_request(request).map_err(anyhow::Error::msg)?;
    let path = state_root
        .join("operations")
        .join(format!("{}.json", request.operation_id));
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("read original durable Stop outcome"),
    };
    let record: serde_json::Value =
        serde_json::from_slice(&bytes).context("decode original durable Stop outcome")?;
    let stored_request: RuntimeOperationRequest = serde_json::from_value(
        record
            .get("request")
            .cloned()
            .context("durable Stop request missing")?,
    )
    .context("decode original durable Stop request")?;
    let view: RuntimeOperationView = serde_json::from_value(
        record
            .get("view")
            .cloned()
            .context("durable Stop view missing")?,
    )
    .context("decode original durable Stop view")?;
    super::external_store::verify_view(&view, request)?;
    let original_digest =
        shared_types::runtime_request_digest(request).map_err(anyhow::Error::msg)?;
    let stored_digest =
        shared_types::runtime_request_digest(&stored_request).map_err(anyhow::Error::msg)?;
    ensure!(
        stored_digest == original_digest
            && view.request_digest == original_digest
            && view.revision == request.expected_revision,
        "durable Stop request binding changed; original outcome remains unconfirmed"
    );
    Ok((view.state.is_terminal()
        || view.state == shared_types::RuntimeOperationState::RecoveryRequired)
        .then_some(view))
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
/// One forwarder writes these markers after delivery. The caller joins that
/// forwarder before reading them, so sequence and event kind form a stable
/// snapshot without a lock or a full-history replay.
pub(super) struct OwnerTerminalCursor {
    operation_id: String,
    runtime_instance_id: String,
    sequence: std::sync::atomic::AtomicU64,
    kind: std::sync::atomic::AtomicU8,
}

impl OwnerTerminalCursor {
    pub(super) fn new(operation_id: &str, runtime_instance_id: &str) -> Self {
        Self {
            operation_id: operation_id.to_owned(),
            runtime_instance_id: runtime_instance_id.to_owned(),
            sequence: std::sync::atomic::AtomicU64::new(0),
            kind: std::sync::atomic::AtomicU8::new(0),
        }
    }

    fn verify_record(&self, record: &shared_types::RuntimeEventRecord) -> Result<()> {
        ensure!(
            record.operation_id == self.operation_id
                && record.runtime_instance_id == self.runtime_instance_id
                && record.sequence > 0,
            "event replay identity or sequence changed for original operation {}; query this original operation before retrying",
            self.operation_id
        );
        Ok(())
    }

    fn record_delivery(&self, record: &shared_types::RuntimeEventRecord) {
        use std::sync::atomic::Ordering;
        let kind = match (record.stage.as_str(), record.event_name.as_deref()) {
            ("terminal", Some("Completed")) => 1,
            ("terminal", Some("Failed")) => 2,
            _ => return,
        };
        self.kind.store(kind, Ordering::SeqCst);
        self.sequence.store(record.sequence, Ordering::SeqCst);
    }

    fn matches_view(&self, view: &RuntimeOperationView, consumed: u64) -> Result<bool> {
        use std::sync::atomic::Ordering;
        ensure!(
            view.operation_id == self.operation_id
                && view.runtime_instance_id == self.runtime_instance_id,
            "terminal view changed original operation identity {}",
            self.operation_id
        );
        let wanted = match view.state {
            shared_types::RuntimeOperationState::Succeeded => 1,
            shared_types::RuntimeOperationState::Failed
            | shared_types::RuntimeOperationState::Cancelled
            | shared_types::RuntimeOperationState::RecoveryRequired => 2,
            _ => bail!(
                "original operation {} has no terminal view",
                self.operation_id
            ),
        };
        let sequence = self.sequence.load(Ordering::SeqCst);
        if sequence == 0 {
            return Ok(false);
        }
        ensure!(
            sequence <= consumed && self.kind.load(Ordering::SeqCst) == wanted,
            "original operation {} terminal event disagrees with observed {:?}; query this original operation before retrying",
            self.operation_id,
            view.state
        );
        Ok(true)
    }
}

pub(super) async fn forward_operation_events(
    client: &OwnerClient,
    operation_id: &str,
    mut on_line: impl FnMut(String) + Send,
    stop: &tokio_util::sync::CancellationToken,
    cursor: &std::sync::atomic::AtomicU64,
    terminal: &OwnerTerminalCursor,
    poll_interval: std::time::Duration,
) {
    use std::sync::atomic::Ordering;
    let mut after_seq = cursor.load(Ordering::SeqCst);
    loop {
        match client.events_after(operation_id, after_seq).await {
            Ok(records) => {
                for record in &records {
                    if let Err(error) = terminal.verify_record(record) {
                        tracing::warn!(%error, "original owner event replay rejected");
                        return;
                    }
                    if record.sequence <= after_seq {
                        continue;
                    }
                    if let Some(legacy) = to_legacy_evt(record) {
                        on_line(legacy);
                    }
                    // Never mark a terminal or advance its sequence before
                    // the original callback has actually accepted the record.
                    terminal.record_delivery(record);
                    after_seq = record.sequence;
                    cursor.store(after_seq, Ordering::SeqCst);
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

/// A terminal view can become visible before its final event is appended.
/// Join the forwarder first, then continue its original incremental cursor
/// until the matching durable terminal record is delivered within the same
/// launch deadline. An already-delivered terminal still gets one tail replay.
pub(super) async fn drain_operation_events(
    client: &OwnerClient,
    view: &RuntimeOperationView,
    after_seq: u64,
    terminal: &OwnerTerminalCursor,
    deadline: tokio::time::Instant,
    mut on_line: impl FnMut(String) + Send,
) -> Result<u64> {
    let mut last = after_seq;
    loop {
        let records = tokio::time::timeout_at(deadline, client.events_after(&view.operation_id, last))
            .await.with_context(|| format!("original operation {} {:?} terminal event observation exceeded launch deadline; query this original operation before retrying", view.operation_id, view.state))??;
        for record in &records {
            terminal.verify_record(record)?;
            if record.sequence <= last {
                continue;
            }
            if let Some(legacy) = to_legacy_evt(record) {
                on_line(legacy);
            }
            terminal.record_delivery(record);
            last = record.sequence;
        }
        if terminal.matches_view(view, last)? {
            return Ok(last);
        }
        ensure!(
            tokio::time::Instant::now() < deadline,
            "original operation {} {:?} has no delivered terminal event before launch deadline; query this original operation before retrying",
            view.operation_id,
            view.state
        );
        tokio::time::sleep_until(
            deadline.min(tokio::time::Instant::now() + std::time::Duration::from_millis(50)),
        )
        .await;
    }
}

/// RuntimeEventRecord → 旧 EVT 行（map_app_cli_evt 消费的同构 JSON;
/// UA-07: typed 构造走 `shared_types::app_cli_evt` 唯一契约）。
/// - 服务级事件（service_starting 等）typed 透传；
/// - 操作终态（Completed/Failed）映射为平台的 `orchestration_done` 终局
///   （R06：真实 owner 成功也必须产生平台所需 Done，Failed 携带错误明细）；
/// - 契约外 event_name → None（旧管道消费者按未知扩展忽略）。
pub(super) fn to_legacy_evt(record: &shared_types::RuntimeEventRecord) -> Option<String> {
    use shared_types::{
        AppCliEvtDecodeError, AppCliFailedService, AppCliOrchestrationEvent as Evt,
    };
    let name = record.event_name.as_deref()?;
    let event = match name {
        "Completed" => Evt::OrchestrationDone { failed: Vec::new() },
        "Failed" => {
            let error = record
                .payload
                .as_ref()
                .and_then(|payload| payload.get("error"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("runtime operation failed");
            Evt::OrchestrationDone {
                failed: vec![AppCliFailedService {
                    service: "orchestrator".into(),
                    error: error.into(),
                }],
            }
        }
        "log" | "service_starting" | "service_start_ok" | "service_start_fail"
        | "orchestration_done" => {
            let mut value = serde_json::json!({"event": name});
            if let Some(service) = &record.service {
                value["service"] = serde_json::Value::String(service.clone());
            }
            let payload_key = match name {
                "log" => Some("line"),
                "orchestration_done" => Some("failed"),
                "service_start_fail" => Some("error"),
                _ => None,
            };
            if let Some(key) = payload_key
                && let Some(field) = record.payload.as_ref().and_then(|payload| payload.get(key))
            {
                value[key] = field.clone();
            }
            let line = value.to_string();
            match Evt::decode(&line) {
                Ok(event) => event,
                // Keep known malformed events visible to the downstream
                // decoder. Missing/wrong fields must not become a fabricated
                // default error or disappear as an unknown extension.
                Err(AppCliEvtDecodeError::MalformedKnownEvent { .. }) => return Some(line),
                Err(error) => {
                    tracing::warn!(?error, "owner event adapter could not decode a known event");
                    return None;
                }
            }
        }
        // 契约外 stage/扩展记录: 无对应消费者, 跳过
        _ => return None,
    };
    Some(event.encode())
}

#[cfg(test)]
mod r09_tests {
    use super::*;

    #[test]
    fn migration_output_adapter_preserves_original_service_and_full_line() {
        let line = format!("[migrate stderr] {} end-marker", "x".repeat(9000));
        let mut record = runtime_event("log", Some(serde_json::json!({"line": line})));
        record.service = Some("backend".into());
        let json = to_legacy_evt(&record).unwrap();
        assert_eq!(
            shared_types::AppCliOrchestrationEvent::decode(&json).unwrap(),
            shared_types::AppCliOrchestrationEvent::Log {
                service: "backend".into(),
                line
            }
        );
        record.payload = Some(serde_json::json!({"line": 7}));
        assert!(
            matches!(shared_types::AppCliOrchestrationEvent::decode(&to_legacy_evt(&record).unwrap()), Err(shared_types::AppCliEvtDecodeError::MalformedKnownEvent { event, .. }) if event == "log")
        );
    }

    fn runtime_event(
        name: &str,
        payload: Option<serde_json::Value>,
    ) -> shared_types::RuntimeEventRecord {
        shared_types::RuntimeEventRecord {
            operation_id: "operation".into(),
            sequence: 1,
            runtime_instance_id: "instance".into(),
            stage: "orchestration".into(),
            service: None,
            event_name: Some(name.into()),
            payload,
        }
    }

    #[test]
    fn runtime_orchestration_done_preserves_failed_services() {
        let record = runtime_event(
            "orchestration_done",
            Some(
                serde_json::json!({"failed": [{"service": "api", "error": "readiness timed out"}]}),
            ),
        );
        let line = to_legacy_evt(&record).expect("known terminal summary must be forwarded");
        assert_eq!(
            shared_types::AppCliOrchestrationEvent::decode(&line).unwrap(),
            shared_types::AppCliOrchestrationEvent::OrchestrationDone {
                failed: vec![shared_types::AppCliFailedService {
                    service: "api".into(),
                    error: "readiness timed out".into(),
                }],
            },
        );
    }

    #[test]
    fn known_malformed_runtime_event_remains_distinct_from_unknown_extension() {
        let mut wrong_error =
            runtime_event("service_start_fail", Some(serde_json::json!({"error": 42})));
        wrong_error.service = Some("api".into());
        let mut missing_error = runtime_event("service_start_fail", None);
        missing_error.service = Some("api".into());
        for record in [
            runtime_event("orchestration_done", None),
            runtime_event("service_start_ok", None),
            wrong_error,
            missing_error,
        ] {
            let line = to_legacy_evt(&record).expect("known damaged event remains observable");
            assert!(matches!(
                shared_types::AppCliOrchestrationEvent::decode(&line),
                Err(shared_types::AppCliEvtDecodeError::MalformedKnownEvent { .. }),
            ));
        }
        assert!(to_legacy_evt(&runtime_event("future_extension", None)).is_none());
    }

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
        let local_deploy = project.join(".local-deploy");
        std::fs::create_dir_all(&local_deploy).unwrap();
        identity.source_root = local_deploy.to_string_lossy().into();
        assert!(verify_stop_identity(&identity, &project, "app").is_ok());
        assert!(
            verify_project_identity(&identity, &project, "app").is_err(),
            "stopping a descendant does not authorize treating it as the source workspace"
        );
        identity.source_root = different.to_string_lossy().into();
        assert!(verify_stop_identity(&identity, &project, "app").is_err());
        #[cfg(unix)]
        {
            let link = project.join("external-link");
            std::os::unix::fs::symlink(&different, &link).unwrap();
            identity.source_root = link.to_string_lossy().into();
            assert!(verify_stop_identity(&identity, &project, "app").is_err());
        }
        runtime_state_layout::record_project_origin(&project, &different).unwrap();
        identity.source_root = different.to_string_lossy().into();
        assert!(verify_stop_identity(&identity, &project, "app").is_ok());
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

#[cfg(test)]
mod observation_tests {
    use super::*;

    #[tokio::test]
    async fn refused_owner_connection_requires_the_exact_durable_stop_receipt() {
        let root = tempfile::tempdir().unwrap();
        let operations = root.path().join("operations");
        std::fs::create_dir_all(&operations).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        drop(listener);
        let client = OwnerClient::new(&address, "test-token").unwrap();
        let request = RuntimeOperationRequest {
            operation_id: "stop-original".into(),
            expected_runtime_instance_id: "original-instance".into(),
            expected_revision: 7,
            workspace_id: "original-workspace".into(),
            kind: shared_types::RuntimeOperationKind::Stop,
            profile: shared_types::RunProfileInput::Source {
                workspace_id: "original-workspace".into(),
            },
            run_config: None,
            request_context: None,
        };
        assert!(
            client
                .wait_terminal(&request.operation_id, std::time::Duration::from_secs(1))
                .await
                .is_err()
        );
        assert!(
            read_durable_stop_outcome(root.path(), &request)
                .unwrap()
                .is_none()
        );
        let path = operations.join(format!("{}.json", request.operation_id));
        let digest = shared_types::runtime_request_digest(&request).unwrap();
        let mut view = RuntimeOperationView {
            operation_id: request.operation_id.clone(),
            kind: request.kind,
            state: shared_types::RuntimeOperationState::Stopping,
            request_digest: digest,
            revision: 7,
            runtime_instance_id: request.expected_runtime_instance_id.clone(),
            error_code: None,
            error_message: None,
            failure_detail: None,
        };
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"view":view,"request":request})).unwrap(),
        )
        .unwrap();
        assert!(
            read_durable_stop_outcome(root.path(), &request)
                .unwrap()
                .is_none()
        );
        view.state = shared_types::RuntimeOperationState::Succeeded;
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"view":view,"request":request})).unwrap(),
        )
        .unwrap();
        assert_eq!(
            read_durable_stop_outcome(root.path(), &request)
                .unwrap()
                .unwrap()
                .state,
            shared_types::RuntimeOperationState::Succeeded
        );
        let mut different_request = request.clone();
        different_request.workspace_id = "different-workspace".into();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"view":view,"request":different_request}))
                .unwrap(),
        )
        .unwrap();
        assert!(read_durable_stop_outcome(root.path(), &request).is_err());
        view.runtime_instance_id = "different-instance".into();
        std::fs::write(
            &path,
            serde_json::to_vec(&serde_json::json!({"view":view,"request":request})).unwrap(),
        )
        .unwrap();
        assert!(read_durable_stop_outcome(root.path(), &request).is_err());
        std::fs::write(&path, b"invalid").unwrap();
        assert!(read_durable_stop_outcome(root.path(), &request).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"invalid");
    }

    #[tokio::test]
    async fn explicit_run_mode_refusal_and_unknown_service_remain_distinct() {
        for (code, legacy) in [
            ("ERR_PROTOCOL_UNSUPPORTED", true),
            // 引导期 kernel 缺席的窗口码 → 有界等待（compose 闲置回收）。
            ("ERR_RECOVERY_REQUIRED", false),
            ("ERR_BACKEND_ERROR", false),
        ] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap().to_string();
            let app = axum::Router::new().route(
                "/v1/runtime/identity",
                axum::routing::get(move || async move {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        axum::Json(serde_json::json!({"code":code,"message":"fixture"})),
                    )
                }),
            );
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let result = observe_owner(&address).await;
            if legacy {
                assert!(matches!(result.unwrap(), OwnerProbe::Legacy));
            } else if code == "ERR_RECOVERY_REQUIRED" {
                assert!(matches!(result.unwrap(), OwnerProbe::Initializing));
            } else {
                assert!(result.is_err());
            }
            server.abort();
            drop(server.await);
            assert!(matches!(
                observe_owner(&address).await.unwrap(),
                OwnerProbe::Absent
            ));
        }
    }
}
