//! 常驻 server：状态机 + 编排主循环（`app-cli serve`）。
//!
//! 形态演进：legacy 直跑（无子命令）是"一次性编排进程"——无 lock 起不来、服务
//! 崩整组重启；server 形态把 app-cli 变成**常驻管理服务**：无论是否部署都在
//! （Idle 态照常应答探针，空容器不 CrashLoop），部署/编排是状态机的一个阶段，
//! 新部署请求可打断当前编排（热切换）。
//!
//! 状态机：
//! ```text
//!  Idle ──(env APP_DEPLOY_URL | /v1/deploy)──▶ Deploying ──▶ Orchestrating ──▶ Running
//!   ▲                                                                    │
//!   └──────────────────── 新部署请求（先停旧服务）◀───────────────────────┘
//!  任一阶段失败 → Failed（/ready 503 摘流、/health 200 不杀容器，可再次部署）
//! ```
//!
//! 探针语义（kubelet 契约）：`/health` 恒 200（进程活）；`/ready` = Idle 200
//! （基础设施就绪——PG/ttyd/dbx 由 supervisord 固定 program 自治）/ Running 跟随
//! bridge readiness / 其余 503（摘流不杀）。

use std::sync::{Arc, RwLock};

use anyhow::{Context, Result};
#[path = "server_journal.rs"]
mod journal;
#[path = "server_preparation.rs"]
mod preparation;
use journal::{ActiveVersion, Boundary, Journal, Receipt};
use shared_types::AppCliDeployPhase;
use shared_types::app_cli_deploy::AppDeploymentStage;
use tokio_util::sync::CancellationToken;

use crate::config::CliArgs;
use crate::log::service::LogLayout;
use crate::manifest::ReleaseLock;
use crate::runtime_status::RuntimeStatusService;
use crate::supervisor;
use crate::supervisord_host::SupervisordHost;

#[cfg(unix)]
const DEPLOY_PROTOCOL: u32 = shared_types::app_cli_deploy::APP_CLI_UNIFIED_DEPLOY_PROTOCOL;
// Non-Unix supervision cannot yet prove descendant process-group quiescence.
#[cfg(not(unix))]
const DEPLOY_PROTOCOL: u32 = shared_types::app_cli_deploy::APP_CLI_OPERATION_ID_DEPLOY_PROTOCOL;

/// server 全局状态（api 层与主循环共享；读多写少，std RwLock 短临界区不跨 await）。
pub struct ServerState {
    admission: std::sync::Mutex<()>,
    accepting: std::sync::atomic::AtomicBool,
    shutdown_unconfirmed: std::sync::atomic::AtomicBool,
    preparations: Arc<preparation::Preparations>,
    journal: std::sync::Mutex<Option<Journal>>,
    generation: String,
    phase: RwLock<ServerPhase>,
    release: RwLock<Option<ReleaseLock>>,
    ready: RuntimeStatusService,
    /// 最近一次部署的进度快照（/v1/deploy/status 消费）。
    deploy_status: RwLock<DeployStatus>,
    /// 热部署受理通道（api 端点 → 主循环）。
    deploy_tx: tokio::sync::mpsc::UnboundedSender<DeployRequest>,
    deploy_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<DeployRequest>>,
    cancel: CancellationToken,
    /// 日志布局（跟随服务托管引擎；serve 探测后设置，legacy 默认 Builtin）。
    log_layout: RwLock<LogLayout>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServerPhase {
    /// 未部署：基础设施（PG/ttyd/dbx）可用，等待首次部署。
    Idle,
    /// 下载/解压/校验/换 code 进行中（旧服务在下载成功前不受影响）。
    Deploying,
    /// 服务编排中（migrate → start → pingap → readiness）。
    Orchestrating,
    /// 服务运行中（supervise 阻塞）。
    Running,
    /// 最近一次部署/编排失败（现场保留，可再次部署）。
    Failed(String),
}

impl ServerPhase {
    pub fn as_str(&self) -> &'static str {
        // wire 值单一事实源在 shared_types（消费方 app_manager 同枚举判据）
        AppCliDeployPhase::from(self).as_str()
    }

    /// /ready 判定（Idle=基础设施就绪；Running=bridge readiness；其余摘流）。
    fn readiness_ok(&self, service_ready: bool) -> bool {
        match self {
            ServerPhase::Idle => true,
            ServerPhase::Running => service_ready,
            _ => false,
        }
    }

    /// 热部署可受理的相位（进行中拒绝，防双部署竞争）。
    fn accepts_deploy(&self) -> bool {
        !matches!(self, ServerPhase::Deploying | ServerPhase::Orchestrating)
    }
}

/// /v1/deploy 受理请求（api 端点反序列化后转发主循环）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeployRequest {
    pub url: String,
    pub release_id: String,
    pub sha256: Option<String>,
}

#[derive(Debug)]
pub(crate) enum AdmissionError {
    Busy(String),
    Failed(String),
}
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(message) | Self::Failed(message) => f.write_str(message),
        }
    }
}
impl std::error::Error for AdmissionError {}
impl From<String> for AdmissionError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}
impl From<&str> for AdmissionError {
    fn from(message: &str) -> Self {
        Self::Failed(message.into())
    }
}

/// 部署进度快照（/v1/deploy/status 响应体）。`phase` 为共享 wire 枚举
/// （rcoder 侧同枚举穷尽 match，新增相位编译期强制同步决策）。
#[derive(Debug, Clone, Default, serde::Serialize, utoipa::ToSchema)]
pub(crate) struct DeployStatus {
    pub protocol_version: u32,
    pub operation: Option<shared_types::AppDeploymentOperation>,
    pub phase: AppCliDeployPhase,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_id: Option<String>,
    /// 令牌回显：驱动当前部署代的请求方 id（env `APP_RELEASE_ID` / 热部署受理的
    /// `req.release_id`）。与 [`Self::release_id`]（包内构建 id，内容身份）是两层
    /// 语义——rcoder 部署等待以本字段确认"应答的 pod 带着它发的环境启动"，
    /// 不做内容身份比对。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_release_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 内部状态机 → wire 相位（穷尽：新增 `ServerPhase` 变体时编译错，
/// 强制同步 wire 契约；`Failed` 的 error 负载走 `DeployStatus.error`）。
impl From<&ServerPhase> for AppCliDeployPhase {
    fn from(phase: &ServerPhase) -> Self {
        match phase {
            ServerPhase::Idle => AppCliDeployPhase::Idle,
            ServerPhase::Deploying => AppCliDeployPhase::Deploying,
            ServerPhase::Orchestrating => AppCliDeployPhase::Orchestrating,
            ServerPhase::Running => AppCliDeployPhase::Running,
            ServerPhase::Failed(_) => AppCliDeployPhase::Failed,
        }
    }
}

impl ServerState {
    pub fn new(ready: RuntimeStatusService) -> Self {
        let (deploy_tx, deploy_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            admission: std::sync::Mutex::new(()),
            accepting: std::sync::atomic::AtomicBool::new(true),
            shutdown_unconfirmed: std::sync::atomic::AtomicBool::new(false),
            preparations: Arc::new(preparation::Preparations::default()),
            journal: std::sync::Mutex::new(None),
            generation: std::env::var(shared_types::APP_DEPLOY_GENERATION_ID)
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            phase: RwLock::new(ServerPhase::Idle),
            release: RwLock::new(None),
            ready,
            deploy_status: RwLock::new(DeployStatus {
                phase: AppCliDeployPhase::Idle,
                protocol_version: DEPLOY_PROTOCOL,
                ..Default::default()
            }),
            deploy_tx,
            deploy_rx: tokio::sync::Mutex::new(deploy_rx),
            cancel: CancellationToken::new(),
            log_layout: RwLock::new(LogLayout::Builtin),
        }
    }

    fn close_admission(&self) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.accepting
            .store(false, std::sync::atomic::Ordering::Release);
        self.cancel.cancel();
    }

    pub fn phase(&self) -> ServerPhase {
        self.phase
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_phase(&self, phase: ServerPhase) {
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.set_phase_locked(phase);
    }

    fn set_phase_locked(&self, phase: ServerPhase) {
        // Ordinary runtime transitions cannot prove that an unconfirmed writer
        // stopped. Keep admission closed until explicit shutdown reconciliation.
        if self
            .deploy_status()
            .operation
            .as_ref()
            .and_then(|operation| operation.recovery.as_ref())
            .is_some_and(|status| status.status == "pending")
        {
            return;
        }
        let mut guard = self
            .phase
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = phase.clone();
        drop(guard);
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.phase = AppCliDeployPhase::from(&phase);
        if let ServerPhase::Failed(err) = &phase {
            status.error = Some(err.clone());
        }
        if let Some(op) = &mut status.operation
            && op.phase != AppCliDeployPhase::Failed
        {
            op.phase = AppCliDeployPhase::from(&phase);
            if let ServerPhase::Failed(error) = &phase {
                op.error = Some(error.clone());
                if op.deploy_stage == AppDeploymentStage::Pending {
                    op.deploy_stage = AppDeploymentStage::Failed;
                }
            }
        }
    }

    fn begin_failure(&self, error: String, shutdown_unconfirmed: bool) {
        if shutdown_unconfirmed {
            self.shutdown_unconfirmed
                .store(true, std::sync::atomic::Ordering::Release);
        }
        let _admission = self
            .admission
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Publish failure and shutdown uncertainty atomically. The retained
        // recovery.pending wire value is a quiescence fence, not rollback intent.
        let phase = if shutdown_unconfirmed {
            ServerPhase::Orchestrating
        } else {
            ServerPhase::Failed(error.clone())
        };
        *self
            .phase
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = phase.clone();
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.phase = AppCliDeployPhase::from(&phase);
        status.error = Some(error.clone());
        if let Some(operation) = &mut status.operation {
            operation.phase = AppCliDeployPhase::Failed;
            operation.error = Some(error);
            if operation.deploy_stage == AppDeploymentStage::Pending {
                operation.deploy_stage = AppDeploymentStage::Failed;
            }
            operation.recovery =
                shutdown_unconfirmed.then(|| shared_types::AppDeploymentRecovery {
                    status: "pending".into(),
                    error: None,
                    database_migrations_reversed: false,
                });
        }
    }

    /// 当前 release（部署编排成功后置入；幂等恢复路径直接从 lock 文件读入）。
    pub fn release(&self) -> Option<ReleaseLock> {
        self.release
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub fn set_release(&self, release: ReleaseLock) {
        let rid = release.release_id.clone();
        *self
            .release
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(release);
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.release_id = Some(rid.clone());
        if let Some(op) = &mut status.operation
            && op.phase != AppCliDeployPhase::Failed
        {
            op.artifact_release_id = Some(rid);
        }
    }

    /// 令牌回显登记（见 [`DeployStatus::request_release_id`]）：env 启动部署在进入
    /// Deploying 时调用；热部署路径在 [`Self::try_accept_deploy_with_id`] 受理时
    /// 已随 operation 同步登记。幂等重写，不随换代清除（标识最近一次请求）。
    pub(crate) fn set_request_release_id(&self, request_release_id: &str) {
        self.deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .request_release_id = Some(request_release_id.to_owned());
    }

    /// 部署代（日志游标 boot_id 语义：换代后旧 cursor 失效重放）。
    pub(crate) fn boot_id(&self) -> String {
        self.release()
            .map(|r| r.release_id)
            .unwrap_or_else(|| "idle".to_string())
    }

    /// /ready 判定（api 探针 handler 消费）。
    pub(crate) fn readiness_ok(&self) -> bool {
        self.ready.is_ready() || self.phase().readiness_ok(false)
    }

    /// 热部署受理（api 端点调用）：相位守卫 + 通知主循环。
    #[cfg(test)]
    pub(crate) fn try_accept_deploy(&self, req: DeployRequest) -> Result<(), AdmissionError> {
        self.try_accept_deploy_with_id(req, uuid::Uuid::new_v4().simple().to_string())
    }

    pub(crate) fn try_accept_deploy_with_id(
        &self,
        req: DeployRequest,
        operation_id: String,
    ) -> Result<(), AdmissionError> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| "deployment admission lock poisoned")?;
        if !self.accepting.load(std::sync::atomic::Ordering::Acquire) {
            return Err(AdmissionError::Busy(
                "server is shutting down; deployment was not accepted".into(),
            ));
        }
        let phase = self.phase();
        if !phase.accepts_deploy() {
            return Err(AdmissionError::Busy(format!(
                "deploy in progress (phase={}); retry after terminal",
                phase.as_str()
            )));
        }
        let previous = self.deploy_status();
        let operation = shared_types::AppDeploymentOperation {
            operation_id,
            deployment_generation_id: self.generation.clone(),
            deploy_stage: AppDeploymentStage::Pending,
            persisted: false,
            request_release_id: req.release_id.clone(),
            artifact_release_id: None,
            recovery: None,
            phase: AppCliDeployPhase::Deploying,
            error: None,
        };
        let mut journal_guard = self
            .journal
            .lock()
            .map_err(|_| "deployment journal lock poisoned")?;
        let old_receipt = journal_guard.as_ref().and_then(|j| j.receipt.clone());
        if let Some(journal) = journal_guard.as_mut() {
            journal
                .write(Receipt {
                    generation: self.generation.clone(),
                    operation: operation.clone(),
                    request: req.clone(),
                    boundary: Boundary::Preparing,
                    active: old_receipt
                        .as_ref()
                        .filter(|r| r.generation == self.generation)
                        .and_then(|r| r.active.clone())
                        .or_else(|| {
                            (phase == ServerPhase::Running)
                                .then(|| self.release())
                                .flatten()
                                .map(|release| ActiveVersion {
                                    artifact_release_id: release.release_id,
                                    request: None,
                                })
                        }),
                })
                .map_err(|error| format!("persist deployment admission: {error:#}"))?;
        }
        let mut status = self
            .deploy_status
            .write()
            .map_err(|_| "deployment status lock poisoned")?;
        *status = DeployStatus {
            protocol_version: DEPLOY_PROTOCOL,
            operation: Some(operation),
            phase: AppCliDeployPhase::Deploying,
            release_id: previous.release_id.clone(),
            request_release_id: Some(req.release_id.clone()),
            error: None,
        };
        *self
            .phase
            .write()
            .map_err(|_| "deployment phase lock poisoned")? = ServerPhase::Deploying;
        if self.deploy_tx.send(req).is_err() {
            *self
                .phase
                .write()
                .map_err(|_| "deployment phase lock poisoned")? = phase;
            *status = previous;
            if let Some(journal) = journal_guard.as_mut() {
                match old_receipt {
                    Some(receipt) => journal.write(receipt),
                    None => journal.clear(),
                }
                .map_err(|e| format!("restore admission receipt: {e:#}"))?;
            }
            return Err("server loop exited".into());
        }
        Ok(())
    }

    pub(crate) fn matches_generation(&self, generation: &str) -> bool {
        generation == self.generation
    }

    fn persist_boundary(&self, boundary: Boundary) -> Result<()> {
        let mut guard = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        let Some(journal) = guard.as_mut() else {
            return Ok(());
        };
        let Some(mut receipt) = journal.receipt.clone() else {
            return Ok(());
        };
        let Some(operation) = self.deploy_status().operation else {
            // Existing code without an attached deployment attempt must never
            // promote a stale or foreign operation merely because it is healthy.
            return Ok(());
        };
        anyhow::ensure!(
            receipt.generation == self.generation
                && operation.deployment_generation_id == self.generation
                && receipt.operation.operation_id == operation.operation_id,
            "deployment receipt does not belong to the current operation"
        );
        receipt.boundary = boundary.clone();
        receipt.operation = operation;
        if matches!(boundary, Boundary::Activated | Boundary::Active) {
            receipt.operation.deploy_stage = AppDeploymentStage::Succeeded;
            receipt.operation.persisted = true;
        }
        if boundary == Boundary::Active {
            receipt.active = Some(ActiveVersion {
                request: Some(receipt.request.clone()),
                artifact_release_id: receipt
                    .operation
                    .artifact_release_id
                    .clone()
                    .context("activated operation has no artifact identity")?,
            });
            receipt.operation.phase = AppCliDeployPhase::Running;
        }
        journal.write(receipt)
    }

    fn fail_operation(&self, error: String, boundary: Boundary) -> Result<()> {
        let _admission = self
            .admission
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment admission lock poisoned"))?;
        let mut snapshot = self.deploy_status();
        snapshot.phase = AppCliDeployPhase::Failed;
        snapshot.error = Some(error.clone());
        if let Some(operation) = snapshot.operation.as_mut() {
            operation.phase = AppCliDeployPhase::Failed;
            operation.error = Some(error.clone());
            if operation.deploy_stage == AppDeploymentStage::Pending {
                operation.deploy_stage = AppDeploymentStage::Failed;
            }
            operation.persisted = true;
        }
        let mut guard = self
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        if let Some(journal) = guard.as_mut()
            && let Some(mut receipt) = journal.receipt.clone()
            && let Some(operation) = snapshot.operation.as_ref()
        {
            anyhow::ensure!(
                receipt.generation == self.generation
                    && operation.deployment_generation_id == self.generation
                    && receipt.operation.operation_id == operation.operation_id,
                "failure receipt does not belong to the current operation"
            );
            if boundary == Boundary::Failed {
                // Previous artifacts remain on disk, but are not a serving version
                // eligible for automatic restoration after activation has started.
                receipt.active = None;
            }
            receipt.boundary = boundary;
            if let Some(operation) = snapshot.operation.clone() {
                receipt.operation = operation;
            }
            journal.write(receipt)?;
        }
        *self
            .phase
            .write()
            .map_err(|_| anyhow::anyhow!("deployment phase lock poisoned"))? =
            ServerPhase::Failed(error);
        *self
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))? = snapshot;
        Ok(())
    }

    fn complete_stage(&self) -> Result<()> {
        self.persist_boundary(Boundary::Activated)?;
        let mut status = self
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))?;
        if let Some(operation) = status.operation.as_mut() {
            operation.deploy_stage = AppDeploymentStage::Succeeded;
            operation.persisted = true;
        }
        Ok(())
    }

    fn complete_running(&self) -> Result<()> {
        // An earlier prepare failure is still the result of that attempt; merely
        // restoring service health cannot turn it into a successful deployment.
        let failed = self
            .deploy_status()
            .operation
            .as_ref()
            .is_some_and(|op| op.phase == AppCliDeployPhase::Failed);
        if !failed {
            self.persist_boundary(Boundary::Active)?;
        } else {
            self.persist_boundary(Boundary::Preparing)?;
        }
        self.set_phase(ServerPhase::Running);
        Ok(())
    }

    pub(crate) fn deploy_status(&self) -> DeployStatus {
        self.deploy_status
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// RuntimeStatusService 句柄（编排器 set_ready / api /ready 消费共享）。
    pub(crate) fn runtime_status(&self) -> RuntimeStatusService {
        self.ready.clone()
    }

    pub(crate) fn log_layout(&self) -> LogLayout {
        *self
            .log_layout
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn set_log_layout(&self, layout: LogLayout) {
        *self
            .log_layout
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = layout;
    }
}

/// serve 主入口：api 常驻 + 状态机主循环（阻塞至 SIGTERM）。
pub async fn serve(args: &CliArgs) -> Result<()> {
    let journal = Journal::open(&args.workspace)?;
    crate::deploy::cleanup_startup(&args.workspace).await?;
    let ready = RuntimeStatusService::default();
    let mut initial_state = ServerState::new(ready.clone());
    if std::env::var(shared_types::APP_DEPLOY_GENERATION_ID).is_err()
        && let Some(receipt) = journal.receipt.as_ref()
    {
        initial_state.generation = receipt.generation.clone();
    }
    let state = Arc::new(initial_state);
    *state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))? = Some(journal);

    let signal_state = state.clone();
    let signal_task = tokio::spawn(async move {
        tokio::select! {
            () = crate::supervisor::sigterm_watch() => {},
            _ = tokio::signal::ctrl_c() => {},
        }
        // Serialize with any already accepted blocking journal commit.
        let closer = signal_state.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || closer.close_admission()).await {
            signal_state.begin_failure(format!("shutdown admission task failed: {error}"), true);
            signal_state.close_admission();
        }
    });
    let (host, mut startup_error) = match SupervisordHost::detect().await {
        Ok(host) => (host, None),
        Err(error) => (None, Some(error)),
    };
    if startup_error.is_none() {
        startup_error = establish_startup_quiescence(&state, host.is_some(), async {
            if let Some(host) = host.as_ref() {
                host.stop_all().await?;
            }
            crate::static_hosting::reconcile(&[], &args.workspace, false).await
        })
        .await
        .err();
    }
    let ownership_claimed = startup_error.is_none();
    let first_request = if let Some(error) = startup_error.as_ref() {
        state.begin_failure(format!("startup shutdown unconfirmed: {error:#}"), true);
        None
    } else {
        match initialize_startup(args, &state).await {
            Ok(action) => action,
            Err(error) => {
                tracing::error!(%error, "Deployment startup reconciliation failed");
                state.ready.set_ready(false);
                if let Err(persist_error) =
                    state.fail_operation(format!("deployment startup: {error:#}"), Boundary::Failed)
                {
                    state.begin_failure(
                        format!("deployment startup: {error:#}; persist: {persist_error:#}"),
                        true,
                    );
                }
                None
            }
        }
    };

    // 管理 API 常驻（含探针——Idle/Deploying 态即有人应答，取代 legacy 的
    // idle.rs / LivenessHold 端口托管）
    let api_state = state.clone();
    let api_addr = args.admin_addr.clone();
    let api_workspace = args.workspace.clone();
    let api_log_dir = args.log_dir.clone();
    let api_pingap_bin = args.pingap_bin.clone();
    let api_handle = tokio::spawn(async move {
        if let Err(error) = crate::api::serve(
            &api_addr,
            api_workspace,
            api_log_dir,
            api_pingap_bin,
            api_state,
        )
        .await
        {
            tracing::error!("app-cli server API failed: {error}");
        }
    });

    // 服务托管引擎探测：supervisord socket 可用（容器形态）→ 动态 program 托管
    //（per-service 隔离重启）；否则 builtin（裸跑/dev，与 legacy 同引擎）。
    state.set_log_layout(if host.is_some() {
        LogLayout::Supervisord
    } else {
        LogLayout::Builtin
    });

    let result = if let Some(error) = startup_error {
        state.cancel.cancelled().await;
        Err(error).context("startup ownership was not claimed")
    } else {
        let driver_args = args.clone();
        let driver_state = state.clone();
        let mut driver = tokio::spawn(async move {
            server_loop(&driver_args, &driver_state, host, first_request).await
        });
        let mut joined = false;
        let (driver_result, shutdown_deadline) = tokio::select! {
            result = &mut driver => {
                joined = true;
                state.close_admission();
                (result.context("server driver panicked").and_then(|result| result), tokio::time::Instant::now() + std::time::Duration::from_secs(30))
            },
            () = state.cancel.cancelled() => {
                let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
                let result = match tokio::time::timeout_at(deadline, &mut driver).await {
                    Ok(result) => { joined = true; result.context("server driver panicked").and_then(|result| result) },
                    Err(error) => Err(error).context("server shutdown confirmation timed out"),
                };
                (result, deadline)
            },
        };
        if !joined {
            driver.abort();
        }
        match driver_result {
            Ok(()) => match tokio::time::timeout_at(
                shutdown_deadline,
                finish_clean_shutdown(args, &state, ownership_claimed),
            )
            .await
            {
                Ok(result) => result,
                Err(error) => Err(error).context("remaining shutdown writers did not stop"),
            },
            Err(error) => Err(error),
        }
    };
    state.close_admission();
    api_handle.abort();
    signal_task.abort();
    result
}

async fn finish_clean_shutdown(
    args: &CliArgs,
    state: &ServerState,
    ownership_claimed: bool,
) -> Result<()> {
    anyhow::ensure!(ownership_claimed, "coordinator ownership was not claimed");
    anyhow::ensure!(
        !state.accepting.load(std::sync::atomic::Ordering::Acquire),
        "deployment admission must be closed before shutdown confirmation"
    );
    anyhow::ensure!(
        !state
            .shutdown_unconfirmed
            .load(std::sync::atomic::Ordering::Acquire),
        "an earlier shutdown failure remains unconfirmed"
    );
    state.preparations.drain().await?;
    crate::static_hosting::reconcile(&[], &args.workspace, false).await?;
    {
        let mut receiver = state.deploy_rx.lock().await;
        receiver.close();
        while receiver.try_recv().is_ok() {}
    }
    let mut guard = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
    guard
        .as_mut()
        .context("deployment journal missing")?
        .commit_quiescent()
}

async fn join_supervisor(
    task: &mut tokio::task::JoinHandle<Result<()>>,
    joined: &mut bool,
) -> Result<()> {
    if *joined {
        return Ok(());
    }
    let result = task.await;
    *joined = true;
    result.context("supervisor task panicked")?
}

async fn establish_startup_quiescence(
    state: &ServerState,
    supervised: bool,
    stop_owned: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    if !supervised {
        let guard = state
            .journal
            .lock()
            .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
        if let Some(journal) = guard.as_ref() {
            journal.require_fresh_process_scope()?;
        }
    }
    stop_owned
        .await
        .context("confirm startup business process shutdown")?;
    let mut guard = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?;
    if let Some(journal) = guard.as_mut() {
        journal.commit_coordinator()?;
    }
    Ok(())
}

async fn initialize_startup(args: &CliArgs, state: &ServerState) -> Result<Option<InitialAction>> {
    if crate::deploy::deploy_requested() {
        let generation = std::env::var(shared_types::APP_DEPLOY_GENERATION_ID)
            .context("APP_DEPLOY_GENERATION_ID is required")?;
        anyhow::ensure!(
            !generation.trim().is_empty(),
            "APP_DEPLOY_GENERATION_ID is empty"
        );
    }
    let saved = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_ref()
        .context("deployment journal missing")?
        .receipt
        .clone()
        .filter(|receipt| receipt.generation == state.generation);
    if let Some(receipt) = saved.as_ref() {
        let mut status = state
            .deploy_status
            .write()
            .map_err(|_| anyhow::anyhow!("deployment status lock poisoned"))?;
        status.request_release_id = Some(receipt.operation.request_release_id.clone());
        status.error = receipt.operation.error.clone();
        status.operation = Some(receipt.operation.clone());
    }
    let resume = state
        .journal
        .lock()
        .map_err(|_| anyhow::anyhow!("deployment journal lock poisoned"))?
        .as_ref()
        .context("deployment journal missing")?
        .resume(&state.generation)?;
    if let Some(receipt) = resume {
        let release = crate::manifest::read_release_lock(&args.workspace)?;
        anyhow::ensure!(
            receipt
                .active
                .as_ref()
                .map(|active| active.artifact_release_id.as_str())
                == Some(release.release_id.as_str()),
            "active artifact does not match deployment journal"
        );
        if receipt.boundary == Boundary::Preparing
            && receipt.operation.phase != AppCliDeployPhase::Failed
        {
            state.fail_operation(
                "deployment preparation interrupted by restart".into(),
                Boundary::Preparing,
            )?;
        }
        state.set_release(release);
        state.persist_boundary(Boundary::Switching)?;
        state.set_phase(ServerPhase::Orchestrating);
        return Ok(Some(InitialAction::Existing));
    }
    if crate::deploy::deploy_requested() {
        let request = crate::deploy::request_from_env()?;
        let operation_id = std::env::var(shared_types::APP_DEPLOY_OPERATION_ID)
            .context("APP_DEPLOY_OPERATION_ID is required")?;
        anyhow::ensure!(
            !operation_id.trim().is_empty(),
            "APP_DEPLOY_OPERATION_ID is empty"
        );
        state
            .try_accept_deploy_with_id(request, operation_id)
            .map_err(anyhow::Error::msg)?;
        let request = state
            .deploy_rx
            .lock()
            .await
            .try_recv()
            .context("initial deployment was not queued")?;
        return Ok(Some(InitialAction::Deploy(request)));
    }
    if tokio::fs::try_exists(args.workspace.join("release.lock.toml")).await? {
        return Ok(Some(InitialAction::Existing));
    }
    Ok(None)
}

/// 等待结果三态（两引擎共用）。
enum Next {
    /// 回外层等待（Failed/服务退出保持等待，可再部署）。
    Wait,
    Redeploy(InitialAction),
    Exit,
}

/// 状态机主循环：初始动作（env 部署 / 卷上既有版本直接编排 / 空容器挂 Idle）→
/// 编排 supervise；期间可被新部署请求打断（停旧服务 → 换 code → 重新编排）。
enum InitialAction {
    /// env/热部署触发：下载制品后编排。
    Deploy(DeployRequest),
    Prepared(crate::deploy::PreparedDeploy),
    /// 卷上既有 release.lock（Pod 重建恢复）：跳过下载直接编排。
    Existing,
}

/// Prepare while the existing supervisor continues serving. Failed requests do
/// not leave this wait loop and never reach the stop/activate boundary.
async fn next_prepared(
    args: &CliArgs,
    state: &ServerState,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeployRequest>,
) -> Option<InitialAction> {
    loop {
        let request = rx.recv().await?;
        match state
            .preparations
            .run(args.workspace.clone(), request)
            .await
        {
            Ok(Some(prepared)) => {
                if let Err(error) = state.persist_boundary(Boundary::Switching) {
                    fail_preparation(state, format!("persist switch: {error:#}")).await;
                    continue;
                }
                return Some(InitialAction::Prepared(prepared));
            }
            Ok(None) => match crate::manifest::read_release_lock(&args.workspace) {
                Ok(release) => {
                    state.set_release(release);
                    if let Err(error) = state
                        .complete_stage()
                        .and_then(|()| state.complete_running())
                    {
                        fail_preparation(state, format!("persist unchanged deployment: {error:#}"))
                            .await;
                    }
                }
                Err(error) => {
                    fail_preparation(state, format!("read unchanged release: {error:#}")).await
                }
            },
            Err(error) => fail_preparation(state, format!("prepare: {error:#}")).await,
        }
    }
}

async fn fail_preparation(state: &ServerState, error: String) {
    if state.preparations.is_poisoned() {
        hold_unconfirmed(state, error).await;
        return;
    }
    if let Err(persist_error) = state.fail_operation(error.clone(), Boundary::Preparing) {
        hold_unconfirmed(
            state,
            format!("{error}; persist preparation failure: {persist_error:#}"),
        )
        .await;
    }
}

/// Keep the API alive and admission closed when a writer may still be active.
/// No recovery directory writes or releasable terminal status follow this point.
async fn hold_unconfirmed(state: &ServerState, error: String) {
    state.ready.set_ready(false);
    state.begin_failure(error.clone(), true);
    tracing::error!(%error, "Deployment remains pending until process shutdown is confirmed; operator recovery required");
    state.cancel.cancelled().await;
}

async fn fail_activation(
    args: &CliArgs,
    state: &ServerState,
    error: String,
) -> Option<InitialAction> {
    state.ready.set_ready(false);
    if let Err(error) = state.preparations.drain().await {
        hold_unconfirmed(state, format!("preparation shutdown: {error:#}")).await;
        return None;
    }
    if let Err(stop_error) = crate::static_hosting::reconcile(&[], &args.workspace, false).await {
        hold_unconfirmed(state, format!("{error}; static shutdown: {stop_error:#}")).await;
        return None;
    }
    if let Err(persist_error) = state.fail_operation(error.clone(), Boundary::Failed) {
        hold_unconfirmed(
            state,
            format!("{error}; persist failure: {persist_error:#}"),
        )
        .await;
    }
    None
}

async fn server_loop(
    args: &CliArgs,
    state: &Arc<ServerState>,
    host: Option<SupervisordHost>,
    first: Option<InitialAction>,
) -> Result<()> {
    let mut pending: Option<InitialAction> = first;
    loop {
        if state.cancel.is_cancelled() {
            return Ok(());
        }
        // 取下一个动作：有待处理的直接用，否则挂 Idle 等受理/信号
        let action = match pending.take() {
            Some(action) => action,
            None => {
                // Failed 不被 Idle 覆盖：保留失败痕迹（deploy_status.error）与
                // 摘流态（/ready 503），直到下一次部署请求进来
                if !matches!(state.phase(), ServerPhase::Failed(_)) {
                    state.set_phase(ServerPhase::Idle);
                }
                let mut rx = state.deploy_rx.lock().await;
                tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(req) => InitialAction::Deploy(req),
                        None => return Ok(()), // api 层全退（不可能，防御）
                    },
                    () = state.cancel.cancelled() => return Ok(()),
                }
            }
        };

        let run_migrations = true;
        let deployment_attempt = matches!(
            action,
            InitialAction::Deploy(_) | InitialAction::Prepared(_)
        );
        let prepared = match action {
            InitialAction::Deploy(request) => {
                state.set_phase(ServerPhase::Deploying);
                state.set_request_release_id(&request.release_id);
                match state
                    .preparations
                    .run(args.workspace.clone(), request)
                    .await
                {
                    Ok(prepared) => prepared,
                    Err(error) => {
                        fail_preparation(state, format!("prepare: {error:#}")).await;
                        continue;
                    }
                }
            }
            InitialAction::Prepared(prepared) => Some(prepared),
            InitialAction::Existing => None,
        };
        if state.cancel.is_cancelled() {
            return Ok(());
        }
        if prepared.is_some()
            && let Err(error) = state.persist_boundary(Boundary::Switching)
        {
            pending = fail_activation(args, state, format!("persist switch: {error:#}")).await;
            continue;
        }
        if let Some(prepared) = prepared
            && let Err(error) = crate::deploy::activate(&args.workspace, prepared).await
        {
            pending = fail_activation(args, state, format!("activate: {error:#}")).await;
            continue;
        }

        // ── Orchestrating：读 lock → 编排（migrate → services → pingap → readiness）──
        match crate::manifest::read_release_lock(&args.workspace) {
            Ok(release) => {
                state.set_release(release);
                if deployment_attempt && let Err(error) = state.complete_stage() {
                    pending =
                        fail_activation(args, state, format!("persist activation: {error:#}"))
                            .await;
                    continue;
                }
                state.set_phase(ServerPhase::Orchestrating);
            }
            Err(e) => {
                tracing::error!("server: read release lock after deploy: {e:#}");
                pending = fail_activation(args, state, format!("release lock: {e:#}")).await;
                continue;
            }
        }

        // ── 引擎分派：supervisord 托管（编排完成即返回，服务由 supervisord
        // per-service 重启）与 builtin（编排+supervise 阻塞在同一 task）──
        if state.cancel.is_cancelled() {
            return Ok(());
        }
        let mut hot_rx = state.deploy_rx.lock().await;
        if let Some(host) = &host {
            let runtime_status = state.runtime_status();
            let Some(release) = state.release() else {
                state.set_phase(ServerPhase::Failed(
                    "orchestration release is missing".into(),
                ));
                continue;
            };
            if let Err(e) = host
                .orchestrate(args, &release, &runtime_status, run_migrations)
                .await
            {
                tracing::error!("server: orchestration failed: {e:#}");
                if let Err(stop_error) = host.stop_all().await {
                    hold_unconfirmed(state, format!("orchestrate: {e:#}; stop: {stop_error:#}"))
                        .await;
                    return Ok(());
                }
                if e.downcast_ref::<supervisor::ShutdownUnconfirmed>()
                    .is_some()
                {
                    hold_unconfirmed(state, format!("orchestrate: {e:#}")).await;
                    return Ok(());
                }
                pending = fail_activation(args, state, format!("orchestrate: {e:#}")).await;
                continue;
            }
            if let Err(error) = state.complete_running() {
                if let Err(stop_error) = host.stop_all().await {
                    hold_unconfirmed(
                        state,
                        format!("persist running: {error:#}; stop: {stop_error:#}"),
                    )
                    .await;
                    return Ok(());
                }
                pending = fail_activation(args, state, format!("persist running: {error:#}")).await;
                continue;
            }
            let next = tokio::select! {
                maybe = next_prepared(args, state, &mut hot_rx) => match maybe {
                    Some(action) => Next::Redeploy(action),
                    None => Next::Exit,
                },
                () = state.cancel.cancelled() => Next::Exit,
            };
            match next {
                Next::Exit => {
                    host.stop_all().await?;
                    return Ok(());
                }
                Next::Wait => {}
                Next::Redeploy(action) => {
                    state.ready.set_ready(false);
                    if let Err(error) = host.stop_all().await {
                        hold_unconfirmed(
                            state,
                            format!("stop before activation failed: {error:#}"),
                        )
                        .await;
                        return Ok(());
                    }
                    pending = Some(action);
                }
            }
            continue;
        }

        // builtin：编排 supervise（可被下一次部署请求打断：cancel → 停服 → 回
        // Deploying）；编排完成进 supervise 时经 on_running 通知 → 相位切 Running。
        let cancel = state.cancel.child_token();
        let runtime_status = state.runtime_status();
        let (running_tx, mut running_rx) = tokio::sync::oneshot::channel::<()>();
        let mut sup = tokio::spawn(supervisor::run_with_cancel(
            args.clone(),
            runtime_status,
            cancel.clone(),
            Some(running_tx),
            run_migrations,
        ));
        let mut sup_joined = false;
        // 先等编排就绪（Running）；就绪后递进一轮等终态/热部署/信号。
        let next = tokio::select! {
            result = &mut running_rx => {
                if result.is_ok() && let Err(error) = state.complete_running() {
                        cancel.cancel();
                        if join_supervisor(&mut sup, &mut sup_joined).await.is_err() {
                            hold_unconfirmed(state, format!("persist running failed and shutdown unconfirmed: {error:#}")).await;
                            return Ok(());
                        }
                        pending = fail_activation(args, state, format!("persist running: {error:#}")).await;
                        continue;
                }
                tokio::select! {
                    outcome = &mut sup => { sup_joined = true; match outcome {
                        // 服务退出/信号/cancel 后 supervise 正常返回：内置引擎不自动重编排
                //（supervisord 引擎下服务崩溃由 supervisord per-service 重启，不走到这）
                Ok(Ok(())) => {
                    tracing::warn!("server: orchestration ended (service exit or signal)");
                    Next::Wait
                }
                Ok(Err(e)) => {
                    tracing::error!("server: orchestration failed: {e:#}");
                    if e.downcast_ref::<supervisor::ShutdownUnconfirmed>().is_some() {
                        hold_unconfirmed(state, format!("orchestrate: {e:#}")).await;
                        return Ok(());
                    }
                    pending = fail_activation(args, state, format!("orchestrate: {e:#}")).await;
                    Next::Wait
                }
                Err(join) => {
                    tracing::error!("server: orchestration task panicked: {join}");
                    hold_unconfirmed(state, format!("orchestrate panicked: {join}")).await;
                    return Ok(());
                }
            }},
            maybe = next_prepared(args, state, &mut hot_rx) => match maybe {
                Some(action) => {
                    tracing::info!("server: hot deploy received, stopping current services");
                    state.ready.set_ready(false);
                    cancel.cancel();
                    match join_supervisor(&mut sup, &mut sup_joined).await {
                        Ok(()) => {},
                        Err(error) => {
                            hold_unconfirmed(state, format!("stop before activation failed: {error:#}")).await;
                            return Ok(());
                        }
                    }
                    Next::Redeploy(action)
                }
                None => Next::Exit,
            },
                    () = state.cancel.cancelled() => {
                        cancel.cancel();
                        join_supervisor(&mut sup, &mut sup_joined).await?;
                        Next::Exit
                    }
                }
            }
            () = state.cancel.cancelled() => {
                cancel.cancel();
                join_supervisor(&mut sup, &mut sup_joined).await?;
                Next::Exit
            },
        };
        match next {
            Next::Exit => {
                cancel.cancel();
                join_supervisor(&mut sup, &mut sup_joined).await?;
                return Ok(());
            }
            Next::Wait => {}
            Next::Redeploy(action) => pending = Some(action),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ServerState {
        ServerState::new(RuntimeStatusService::default())
    }

    fn request() -> DeployRequest {
        DeployRequest {
            url: "http://artifact".into(),
            release_id: "caller-token".into(),
            sha256: None,
        }
    }

    fn release(rid: &str) -> workspace_manifest::ReleaseLock {
        let mut release: workspace_manifest::ReleaseLock = toml::from_str(
            r#"
schema_version = 1
release_id = "test-release-0001"
workspace_name = "demo"
minimum_app_cli_version = "0.1.3"
runtime_image_digest = "registry.example/app-runtime:0.1.140"

[pingap]
mode = "managed"
version = "0.14.1"
commit = "abc123"

[[services]]
service_id = "web"
name = "Web"
dir = "web"
type = "node"
kind = "web"
enabled = true
port = 4200

[services.run]
command = ["node", "server.js"]
migrate = []
depends_on = []
shutdown_timeout_seconds = 30

[services.health]
startup_path = "/health"
readiness_path = "/ready"
liveness_path = "/health"

[[services.logs]]
id = "application"
glob = "web*.log*"
format = "jsonl"

[services.env]
"#,
        )
        .unwrap();
        release.release_id = rid.into();
        release
    }

    #[tokio::test]
    async fn startup_resumes_hot_receipt_identity_and_published_artifact() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        let artifact = release("manifest-b");
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&artifact).unwrap(),
        )
        .unwrap();
        let mut first = state();
        first.generation = "generation-a".into();
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "hot-b".into())
            .unwrap();
        first.set_release(artifact);
        first.complete_stage().unwrap();
        first.complete_running().unwrap();
        drop(first);
        let mut restarted = state();
        restarted.generation = "generation-a".into();
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = CliArgs {
            workspace,
            ..Default::default()
        };
        assert!(matches!(
            initialize_startup(&args, &restarted).await.unwrap(),
            Some(InitialAction::Existing)
        ));
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "hot-b");
        assert_eq!(op.artifact_release_id.as_deref(), Some("manifest-b"));
        assert_eq!(op.deploy_stage, AppDeploymentStage::Succeeded);
        assert!(op.persisted);
        assert_eq!(op.phase, AppCliDeployPhase::Orchestrating);
    }

    #[tokio::test]
    async fn failed_cold_prepare_without_active_version_never_uses_existing_code() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&release("unattached-a")).unwrap(),
        )
        .unwrap();
        let mut first = state();
        first.generation = "cold-generation".into();
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "failed-cold".into())
            .unwrap();
        first
            .fail_operation("download failed".into(), Boundary::Preparing)
            .unwrap();
        drop(first);
        let mut restarted = state();
        restarted.generation = "cold-generation".into();
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = CliArgs {
            workspace,
            ..Default::default()
        };
        assert!(initialize_startup(&args, &restarted).await.is_err());
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "failed-cold");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        assert!(op.persisted);
        assert!(restarted.deploy_rx.lock().await.try_recv().is_err());
    }

    #[tokio::test]
    async fn existing_baseline_survives_first_hot_prepare_failure_and_restart() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        let artifact = release("existing-a");
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&artifact).unwrap(),
        )
        .unwrap();
        let mut first = state();
        first.generation = "existing-generation".into();
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = CliArgs {
            workspace: workspace.clone(),
            ..Default::default()
        };
        assert!(matches!(
            initialize_startup(&args, &first).await.unwrap(),
            Some(InitialAction::Existing)
        ));
        first.set_release(artifact);
        first.complete_running().unwrap();
        assert!(first.deploy_status().operation.is_none());
        first
            .try_accept_deploy_with_id(request(), "failed-b".into())
            .unwrap();
        first
            .fail_operation("invalid B".into(), Boundary::Preparing)
            .unwrap();
        drop(first);
        let mut restarted = state();
        restarted.generation = "existing-generation".into();
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        assert!(matches!(
            initialize_startup(&args, &restarted).await.unwrap(),
            Some(InitialAction::Existing)
        ));
        restarted.complete_running().unwrap();
        assert_eq!(restarted.release().unwrap().release_id, "existing-a");
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "failed-b");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        let receipt = restarted
            .journal
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .receipt
            .clone()
            .unwrap();
        assert_eq!(receipt.boundary, Boundary::Preparing);
        assert_eq!(receipt.active.unwrap().artifact_release_id, "existing-a");
        assert_eq!(receipt.operation.phase, AppCliDeployPhase::Failed);
        drop(restarted);
        let mut second_restart = state();
        second_restart.generation = "existing-generation".into();
        *second_restart.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        assert!(matches!(
            initialize_startup(&args, &second_restart).await.unwrap(),
            Some(InitialAction::Existing)
        ));
        second_restart.complete_running().unwrap();
        assert_eq!(second_restart.release().unwrap().release_id, "existing-a");
        let op = second_restart.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "failed-b");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
    }

    #[tokio::test]
    async fn existing_code_cannot_promote_foreign_generation_receipt() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        let artifact = release("existing-a");
        std::fs::write(
            workspace.join("release.lock.toml"),
            toml::to_string(&artifact).unwrap(),
        )
        .unwrap();
        let mut first = state();
        first.generation = "old-generation".into();
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "foreign-failed-b".into())
            .unwrap();
        first
            .fail_operation("invalid B".into(), Boundary::Preparing)
            .unwrap();
        drop(first);
        let before = std::fs::read(dir.path().join(".deploy-operation.json")).unwrap();
        let mut restarted = state();
        restarted.generation = "new-generation".into();
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = CliArgs {
            workspace,
            ..Default::default()
        };
        assert!(matches!(
            initialize_startup(&args, &restarted).await.unwrap(),
            Some(InitialAction::Existing)
        ));
        restarted.set_release(artifact);
        restarted.complete_running().unwrap();
        assert!(restarted.deploy_status().operation.is_none());
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-operation.json")).unwrap(),
            before
        );
        restarted
            .try_accept_deploy_with_id(request(), "new-attempt".into())
            .unwrap();
        let receipt = restarted
            .journal
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .receipt
            .clone()
            .unwrap();
        assert_eq!(receipt.generation, "new-generation");
        assert_eq!(receipt.operation.operation_id, "new-attempt");
        assert_eq!(receipt.active.unwrap().artifact_release_id, "existing-a");
    }

    #[tokio::test]
    async fn startup_interrupted_switch_retains_operation_for_error_reporting() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let mut first = state();
        first.generation = "generation-a".into();
        *first.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        first
            .try_accept_deploy_with_id(request(), "switch-b".into())
            .unwrap();
        first.persist_boundary(Boundary::Switching).unwrap();
        drop(first);
        let mut restarted = state();
        restarted.generation = "generation-a".into();
        *restarted.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        let args = CliArgs {
            workspace,
            ..Default::default()
        };
        let error = initialize_startup(&args, &restarted)
            .await
            .err()
            .expect("switch must fail closed");
        restarted
            .fail_operation(error.to_string(), Boundary::Failed)
            .unwrap();
        let op = restarted.deploy_status().operation.unwrap();
        assert_eq!(op.operation_id, "switch-b");
        assert_eq!(op.phase, AppCliDeployPhase::Failed);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        assert!(op.persisted);
    }

    #[tokio::test]
    async fn startup_requires_new_process_scope_without_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let mut journal = Journal::open(&dir.path().join("code")).unwrap();
        journal.process_scope = Some("same-container".into());
        journal.commit_coordinator().unwrap();
        *state.journal.lock().unwrap() = Some(journal);
        state
            .try_accept_deploy_with_id(request(), "interrupted".into())
            .unwrap();
        assert!(
            establish_startup_quiescence(&state, false, async { Ok(()) })
                .await
                .is_err()
        );
        state
            .journal
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .process_scope = Some("new-container".into());
        assert!(
            establish_startup_quiescence(&state, false, async { Ok(()) })
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn startup_requires_durable_owner_after_stop_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        std::fs::create_dir(dir.path().join(".deploy-coordinator.json")).unwrap();
        state.set_phase(ServerPhase::Orchestrating);
        let error = establish_startup_quiescence(&state, true, async { Ok(()) })
            .await
            .expect_err("owner persistence must gate startup");
        state.begin_failure(error.to_string(), true);
        assert_eq!(state.phase(), ServerPhase::Orchestrating);
        assert!(
            state
                .try_accept_deploy_with_id(request(), "premature".into())
                .is_err()
        );
    }

    #[tokio::test]
    async fn startup_stop_failure_does_not_allow_replacement_deploy() {
        let state = Arc::new(state());
        state.set_phase(ServerPhase::Orchestrating);
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let job_state = state.clone();
        let job_entered = entered.clone();
        let job_release = release.clone();
        let job = tokio::spawn(async move {
            let result = establish_startup_quiescence(&job_state, true, async {
                job_entered.notify_one();
                job_release.notified().await;
                anyhow::bail!("owned process group is still running")
            })
            .await;
            if let Err(error) = result {
                job_state.begin_failure(format!("startup shutdown unconfirmed: {error:#}"), true);
            }
        });
        entered.notified().await;
        assert!(
            state
                .try_accept_deploy_with_id(request(), "racing".into())
                .is_err()
        );
        release.notify_one();
        job.await.unwrap();
        assert_eq!(
            state.deploy_status().phase,
            AppCliDeployPhase::Orchestrating
        );
        assert!(
            state
                .try_accept_deploy_with_id(request(), "after-failure".into())
                .is_err()
        );
    }

    #[test]
    fn close_admission_preserves_accepted_request_and_rejects_late_request() {
        let state = Arc::new(state());
        state
            .try_accept_deploy_with_id(request(), "accepted".into())
            .unwrap();
        let guard = state.admission.lock().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let closer = state.clone();
        let thread = std::thread::spawn(move || {
            entered_tx.send(()).unwrap();
            closer.close_admission();
        });
        entered_rx.recv().unwrap();
        assert!(state.accepting.load(std::sync::atomic::Ordering::Acquire));
        drop(guard);
        thread.join().unwrap();
        assert!(!state.accepting.load(std::sync::atomic::Ordering::Acquire));
        assert_eq!(
            state.deploy_status().operation.unwrap().operation_id,
            "accepted"
        );
        assert!(matches!(
            state.try_accept_deploy_with_id(request(), "late".into()),
            Err(AdmissionError::Busy(_))
        ));
    }

    #[tokio::test]
    async fn unclaimed_shutdown_cannot_rewrite_previous_active_owner() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let mut first = Journal::open(&workspace).unwrap();
        first.commit_coordinator().unwrap();
        drop(first);
        let before = std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        state.close_admission();
        let args = CliArgs {
            workspace,
            ..Default::default()
        };
        assert!(finish_clean_shutdown(&args, &state, false).await.is_err());
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn prior_shutdown_failure_cannot_be_marked_quiescent() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        let state = state();
        let mut journal = Journal::open(&workspace).unwrap();
        journal.commit_coordinator().unwrap();
        *state.journal.lock().unwrap() = Some(journal);
        let before = std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap();
        state.begin_failure("child shutdown not confirmed".into(), true);
        state.close_admission();
        let args = CliArgs {
            workspace,
            ..Default::default()
        };
        assert!(finish_clean_shutdown(&args, &state, true).await.is_err());
        assert_eq!(
            std::fs::read(dir.path().join(".deploy-coordinator.json")).unwrap(),
            before
        );
    }

    #[tokio::test]
    async fn supervisor_join_failure_cannot_be_successful_shutdown() {
        for panic in [false, true] {
            let mut task = tokio::spawn(async move {
                assert!(!panic, "injected supervisor panic");
                anyhow::bail!("injected process shutdown failure")
            });
            let mut joined = false;
            assert!(join_supervisor(&mut task, &mut joined).await.is_err());
            assert!(joined);
        }
    }

    #[tokio::test]
    async fn durable_admission_failure_does_not_publish_or_enqueue_operation() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        std::fs::create_dir(dir.path().join(".deploy-operation.json")).unwrap();
        state.set_phase(ServerPhase::Running);
        assert!(
            state
                .try_accept_deploy_with_id(request(), "rejected".into())
                .is_err()
        );
        assert_eq!(state.phase(), ServerPhase::Running);
        assert!(state.deploy_status().operation.is_none());
        assert!(state.deploy_rx.lock().await.try_recv().is_err());
    }

    #[tokio::test]
    async fn queue_failure_restores_empty_journal_and_complete_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state.deploy_rx.lock().await.close();
        state.set_phase(ServerPhase::Running);
        assert!(
            state
                .try_accept_deploy_with_id(request(), "rejected".into())
                .is_err()
        );
        assert!(state.deploy_status().operation.is_none());
        assert!(!dir.path().join(".deploy-operation.json").exists());
        assert_eq!(state.phase(), ServerPhase::Running);
    }

    #[test]
    fn activation_receipt_failure_cannot_publish_stage_success() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state
            .try_accept_deploy_with_id(request(), "operation-a".into())
            .unwrap();
        std::fs::remove_file(dir.path().join(".deploy-operation.json")).unwrap();
        std::fs::create_dir(dir.path().join(".deploy-operation.json")).unwrap();
        assert!(state.complete_stage().is_err());
        let operation = state.deploy_status().operation.unwrap();
        assert!(!operation.persisted);
        assert_eq!(operation.deploy_stage, AppDeploymentStage::Pending);
    }

    #[test]
    fn durable_stage_is_independent_from_later_orchestration_failure() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state
            .try_accept_deploy_with_id(request(), "operation-a".into())
            .unwrap();
        state.persist_boundary(Boundary::Switching).unwrap();
        state.complete_stage().unwrap();
        let operation = state.deploy_status().operation.unwrap();
        assert!(operation.persisted);
        assert_eq!(operation.deploy_stage, AppDeploymentStage::Succeeded);
        state.begin_failure("orchestration failed".into(), false);
        let operation = state.deploy_status().operation.unwrap();
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.deploy_stage, AppDeploymentStage::Succeeded);
    }

    #[test]
    fn preparation_failure_is_durable_without_changing_serving_health() {
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        state.ready.set_ready(true);
        state
            .try_accept_deploy_with_id(request(), "bad-artifact".into())
            .unwrap();
        state
            .fail_operation("invalid zip".into(), Boundary::Preparing)
            .unwrap();
        assert!(state.readiness_ok());
        let op = state.deploy_status().operation.unwrap();
        assert!(op.persisted);
        assert_eq!(op.deploy_stage, AppDeploymentStage::Failed);
        let journal = state.journal.lock().unwrap();
        let saved = &journal
            .as_ref()
            .unwrap()
            .receipt
            .as_ref()
            .unwrap()
            .operation;
        assert_eq!(saved.operation_id, "bad-artifact");
        assert_eq!(saved.error.as_deref(), Some("invalid zip"));
        assert!(saved.persisted);
    }

    #[tokio::test]
    async fn activation_failure_does_not_restore_previous_business_code() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("code");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(workspace.join("version"), "B").unwrap();
        std::fs::create_dir(dir.path().join(".previous")).unwrap();
        std::fs::write(dir.path().join(".previous/version"), "A").unwrap();
        let state = state();
        *state.journal.lock().unwrap() = Some(Journal::open(&workspace).unwrap());
        state
            .try_accept_deploy_with_id(request(), "operation-b".into())
            .unwrap();
        assert!(
            fail_activation(
                &CliArgs {
                    workspace: workspace.clone(),
                    ..Default::default()
                },
                &state,
                "activation failed".into()
            )
            .await
            .is_none()
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("version")).unwrap(),
            "B"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join(".previous/version")).unwrap(),
            "A"
        );
        assert!(matches!(state.phase(), ServerPhase::Failed(_)));
        assert!(
            state
                .journal
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .resume(&state.generation)
                .is_err()
        );
    }

    /// 内部状态机 → wire 相位转换矩阵：as_str 委托共享枚举，Failed 负载
    /// 丢弃（error 走 DeployStatus.error 独立字段）。新增 ServerPhase 变体
    /// 时 From 实现编译错强制同步本矩阵。
    #[test]
    fn concurrent_deploy_admission_has_one_winner() {
        let state = Arc::new(state());
        let barrier = Arc::new(std::sync::Barrier::new(16));
        let handles: Vec<_> = (0..16)
            .map(|n| {
                let state = state.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    state
                        .try_accept_deploy(DeployRequest {
                            url: "http://127.0.0.1/artifact".into(),
                            release_id: format!("r{n}"),
                            sha256: None,
                        })
                        .is_ok()
                })
            })
            .collect();
        let accepted = handles
            .into_iter()
            .map(|h| usize::from(h.join().expect("thread")))
            .sum::<usize>();
        assert_eq!(accepted, 1);
        let status = state.deploy_status();
        assert_eq!(status.protocol_version, DEPLOY_PROTOCOL);
        assert_eq!(
            status.operation.expect("operation").phase,
            AppCliDeployPhase::Deploying
        );
    }

    #[test]
    fn failed_operation_is_not_completed_by_old_generation_health() {
        let state = state();
        state
            .try_accept_deploy_with_id(
                DeployRequest {
                    url: "http://x".into(),
                    release_id: "requested".into(),
                    sha256: None,
                },
                "op-a".into(),
            )
            .expect("accept");
        state.set_phase(ServerPhase::Failed("prepare failed".into()));
        state.set_phase(ServerPhase::Running);
        let operation = state.deploy_status().operation.expect("operation");
        assert_eq!(operation.operation_id, "op-a");
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.error.as_deref(), Some("prepare failed"));
    }

    #[test]
    fn runtime_phase_changes_cannot_clear_unconfirmed_shutdown() {
        let state = state();
        let request = || DeployRequest {
            url: "http://x".into(),
            release_id: "requested".into(),
            sha256: None,
        };
        state
            .try_accept_deploy_with_id(request(), "op-a".into())
            .expect("accept");
        state.begin_failure("shutdown not confirmed".into(), true);
        let snapshot = state.deploy_status();
        assert_eq!(snapshot.phase, AppCliDeployPhase::Orchestrating);
        let operation = snapshot.operation.expect("operation");
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.recovery.expect("recovery").status, "pending");
        assert!(
            state
                .try_accept_deploy_with_id(request(), "op-b".into())
                .is_err()
        );
        for phase in [
            ServerPhase::Running,
            ServerPhase::Idle,
            ServerPhase::Failed("runtime stopped".into()),
        ] {
            state.set_phase(phase);
            assert_eq!(state.phase(), ServerPhase::Orchestrating);
        }
        let snapshot = state.deploy_status();
        let operation = snapshot.operation.expect("operation");
        assert_eq!(operation.operation_id, "op-a");
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.recovery.expect("recovery").status, "pending");
        assert!(
            state
                .try_accept_deploy_with_id(request(), "op-b".into())
                .is_err()
        );
    }

    #[test]
    fn server_phase_to_wire_phase_matrix() {
        let cases = [
            (ServerPhase::Idle, AppCliDeployPhase::Idle),
            (ServerPhase::Deploying, AppCliDeployPhase::Deploying),
            (ServerPhase::Orchestrating, AppCliDeployPhase::Orchestrating),
            (ServerPhase::Running, AppCliDeployPhase::Running),
            (
                ServerPhase::Failed("boom".to_string()),
                AppCliDeployPhase::Failed,
            ),
        ];
        for (phase, wire) in cases {
            assert_eq!(AppCliDeployPhase::from(&phase), wire);
            assert_eq!(phase.as_str(), wire.as_str());
        }
    }

    /// set_phase 同步 deploy_status：phase 即时反映 + Failed 附 error 快照。
    #[test]
    fn set_phase_updates_deploy_status_snapshot() {
        let st = state();
        st.set_phase(ServerPhase::Deploying);
        {
            let status = st
                .deploy_status
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(status.phase, AppCliDeployPhase::Deploying);
            assert_eq!(status.error, None);
        }
        st.set_phase(ServerPhase::Failed("download 404".to_string()));
        {
            let status = st
                .deploy_status
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            assert_eq!(status.phase, AppCliDeployPhase::Failed);
            assert_eq!(status.error.as_deref(), Some("download 404"));
        }
    }

    #[test]
    fn readiness_matrix_per_phase() {
        let st = state();
        // Idle：基础设施就绪（空容器可服务），与 runtime_ready 无关
        st.set_phase(ServerPhase::Idle);
        assert!(st.readiness_ok());
        // Running：跟随后端 bridge readiness
        st.set_phase(ServerPhase::Running);
        assert!(!st.readiness_ok(), "runtime not ready → 503");
        st.ready.set_ready(true);
        assert!(st.readiness_ok());
        // Preparation/failure of a new deployment does not remove a healthy old app.
        st.set_phase(ServerPhase::Deploying);
        assert!(st.readiness_ok());
        st.set_phase(ServerPhase::Failed("prepare failed".into()));
        assert!(st.readiness_ok());
        // Activation explicitly stops the serving generation.
        st.ready.set_ready(false);
        for phase in [ServerPhase::Deploying, ServerPhase::Orchestrating] {
            st.set_phase(phase);
            assert!(!st.readiness_ok());
        }
        st.set_phase(ServerPhase::Failed("x".into()));
        assert!(!st.readiness_ok());
    }

    #[tokio::test]
    async fn deploy_acceptance_guards_and_channel() {
        let st = state();
        st.set_phase(ServerPhase::Running);
        let req = DeployRequest {
            url: "http://x/p.zip".into(),
            release_id: "rel-1".into(),
            sha256: None,
        };
        assert!(st.try_accept_deploy(req.clone()).is_ok());
        // 进行中相位拒绝（防双部署竞争）
        st.set_phase(ServerPhase::Deploying);
        assert!(st.try_accept_deploy(req.clone()).is_err());
        st.set_phase(ServerPhase::Orchestrating);
        assert!(st.try_accept_deploy(req).is_err());
        // 受理的请求能被主循环收到
        st.set_phase(ServerPhase::Idle);
        st.try_accept_deploy(DeployRequest {
            url: "http://x/p2.zip".into(),
            release_id: "rel-2".into(),
            sha256: None,
        })
        .unwrap();
        // 受理按序到达（Running 期受理的 rel-1 排在前——主循环串行消费）
        let first = st.deploy_rx.lock().await.recv().await.unwrap();
        assert_eq!(first.release_id, "rel-1");
        let second = st.deploy_rx.lock().await.recv().await.unwrap();
        assert_eq!(second.release_id, "rel-2");
    }

    #[test]
    fn boot_id_tracks_release_generation() {
        let st = state();
        assert_eq!(st.boot_id(), "idle");
        let mk_release = |rid: &str| workspace_manifest::ReleaseLock {
            schema_version: 1,
            release_id: rid.into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.14.1".into(),
                commit: "abc".into(),
            },
            minimum_app_cli_version: "0.0.0".into(),
            runtime_image_digest: String::new(),
            services: Vec::new(),
            bridge_service: None,
        };
        st.set_release(mk_release("rel-gen-1"));
        assert_eq!(st.boot_id(), "rel-gen-1");
        st.set_release(mk_release("rel-gen-2"));
        assert_eq!(st.boot_id(), "rel-gen-2");
        // 部署进度快照携带当前 release_id
        assert_eq!(st.deploy_status().release_id.as_deref(), Some("rel-gen-2"));
    }

    /// 令牌回显：request_release_id（请求身份）与 release_id（包内构建 id）两层
    /// 并存、互不覆盖；热部署受理与 operation.request_release_id 同源同值；
    /// 无请求方（Existing/legacy 路径）不回显。
    #[test]
    fn request_release_id_echoes_caller_token() {
        let st = state();
        assert_eq!(st.deploy_status().request_release_id, None);

        // env 启动部署：进入 Deploying 时登记令牌
        st.set_request_release_id("rel-token-a");
        assert_eq!(
            st.deploy_status().request_release_id.as_deref(),
            Some("rel-token-a")
        );

        // 热部署受理：回显与 operation.request_release_id 同步登记
        st.try_accept_deploy(DeployRequest {
            url: "http://unused".into(),
            release_id: "rel-token-b".into(),
            sha256: None,
        })
        .expect("idle accepts deploy");
        let status = st.deploy_status();
        assert_eq!(status.request_release_id.as_deref(), Some("rel-token-b"));
        assert_eq!(
            status.operation.expect("accepted").request_release_id,
            "rel-token-b"
        );

        // 部署编排完成：包内构建 id 写入 release_id，令牌回显不被覆盖
        st.set_release(workspace_manifest::ReleaseLock {
            schema_version: 1,
            release_id: "build-9".into(),
            workspace_name: "ws".into(),
            pingap: workspace_manifest::LockedPingap {
                mode: workspace_manifest::PingapMode::Managed,
                config: None,
                version: "0.14.1".into(),
                commit: "abc".into(),
            },
            minimum_app_cli_version: "0.0.0".into(),
            runtime_image_digest: String::new(),
            services: Vec::new(),
            bridge_service: None,
        });
        let status = st.deploy_status();
        assert_eq!(status.release_id.as_deref(), Some("build-9"));
        assert_eq!(status.request_release_id.as_deref(), Some("rel-token-b"));
    }
    #[tokio::test]
    async fn unconfirmed_stop_keeps_api_state_pending_and_rejects_new_deployment() {
        let state = state();
        state
            .try_accept_deploy_with_id(
                DeployRequest {
                    url: "http://unused".into(),
                    release_id: "new".into(),
                    sha256: None,
                },
                "operation".into(),
            )
            .unwrap();
        let hold = hold_unconfirmed(&state, "process group remains".into());
        tokio::pin!(hold);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut hold)
                .await
                .is_err()
        );
        let status = state.deploy_status();
        assert_eq!(status.phase, AppCliDeployPhase::Orchestrating);
        let operation = status.operation.unwrap();
        assert_eq!(operation.phase, AppCliDeployPhase::Failed);
        assert_eq!(operation.recovery.unwrap().status, "pending");
        assert!(
            state
                .try_accept_deploy(DeployRequest {
                    url: "http://unused".into(),
                    release_id: "later".into(),
                    sha256: None
                })
                .is_err()
        );
    }
}
