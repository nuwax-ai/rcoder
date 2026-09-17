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
    /// 启动恢复未完成（P1-01）：API 先 bind 后、恢复完成前，写端点与 /ready
    /// 就绪判定被门控——恢复期不受理运行态变更、不以 Idle 语义应答探针。
    initializing: std::sync::atomic::AtomicBool,
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
    /// 运行操作内核槽位（阶段二：serve 在 ownership 认领后注入；legacy 形态
    /// 恒 None——api 层 /v1/runtime/* 相应 503）。
    runtime_kernel: std::sync::OnceLock<Arc<crate::runtime_kernel::RuntimeKernel>>,
    /// R05：启动路径是否**尝试过**装配运行内核。true 且 runtime_kernel 为
    /// None = 状态根打开失败（可信状态不可读）——所有写入口 fail-closed；
    /// false = 从未尝试（测试/无内核上下文）——保持旧语义。
    kernel_required: std::sync::atomic::AtomicBool,
    /// V04：server 级恢复门禁——运行操作**终态持久化失败**（结果未知）时
    /// 挂起：保留执行身份、关闭部署受理、压低 ready，直至进程重启由内核
    /// 恢复裁决。只升不降（解除只经重启）。
    runtime_recovery_hold: std::sync::atomic::AtomicBool,
    /// R08：当前运行操作的 dev profile（None = 操作未指定，legacy 直跑/
    /// env 兜底）。编排生效命令选择的显式依据。
    pending_dev_profile: std::sync::Mutex<Option<bool>>,
    /// 运行控制信号通道（源码编排/停止业务——api → 主循环；与部署通道并行）。
    control_tx: tokio::sync::mpsc::UnboundedSender<ControlSignal>,
    control_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ControlSignal>>,
    /// 当前执行中的运行操作 ID（dispatch 写入，主循环边界收束）。
    current_runtime_operation: RwLock<Option<String>>,
}

/// 运行控制信号（阶段二 dispatch 目标；server_loop 解释执行）。
#[derive(Debug, Clone)]
pub(crate) enum ControlSignal {
    /// 源码编排（workspace 当前内容 + release lock）。
    OrchestrateSource {
        operation_id: String,
        /// R08：操作级 dev profile（Source 形态 = dev）——编排生效命令选择
        /// 的显式依据，不再读 serve 进程 env 猜测
        dev_profile: bool,
    },
    /// 停止业务服务（保持管理面）。
    StopBusiness { operation_id: String },
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
    /// 能力声明（独立稳定字段；值如 `"progress_v1"`）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub capabilities: Vec<String>,
    /// 部署进度（progress_v1 能力声明后有值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<shared_types::AppDeploymentProgress>,
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
        let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            runtime_kernel: std::sync::OnceLock::new(),
            kernel_required: std::sync::atomic::AtomicBool::new(false),
            runtime_recovery_hold: std::sync::atomic::AtomicBool::new(false),
            pending_dev_profile: std::sync::Mutex::new(None),
            control_tx,
            control_rx: tokio::sync::Mutex::new(control_rx),
            current_runtime_operation: RwLock::new(None),
            admission: std::sync::Mutex::new(()),
            accepting: std::sync::atomic::AtomicBool::new(true),
            shutdown_unconfirmed: std::sync::atomic::AtomicBool::new(false),
            initializing: std::sync::atomic::AtomicBool::new(true),
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
                capabilities: vec!["progress_v1".into()],
                ..Default::default()
            }),
            deploy_tx,
            deploy_rx: tokio::sync::Mutex::new(deploy_rx),
            cancel: CancellationToken::new(),
            log_layout: RwLock::new(LogLayout::Builtin),
        }
    }

    /// 运行操作内核（api 层 /v1/runtime/* 消费；未注入返回 None）。
    pub(crate) fn mark_kernel_required(&self) {
        self.kernel_required
            .store(true, std::sync::atomic::Ordering::Release);
    }

    /// R08：记录/读取当前操作的 dev profile。
    pub(crate) fn set_pending_dev_profile(&self, dev_profile: bool) {
        *self
            .pending_dev_profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(dev_profile);
    }

    pub(crate) fn take_pending_dev_profile(&self) -> Option<bool> {
        self.pending_dev_profile
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
    }

    /// V04：终态持久化失败（结果未知）——挂起 server 写入口与 ready。
    pub(crate) fn begin_runtime_recovery_hold(&self) {
        self.runtime_recovery_hold
            .store(true, std::sync::atomic::Ordering::Release);
        self.ready.set_ready(false);
    }

    pub(crate) fn runtime_recovery_hold_active(&self) -> bool {
        self.runtime_recovery_hold
            .load(std::sync::atomic::Ordering::Acquire)
    }

    pub(crate) fn kernel_unavailable(&self) -> bool {
        self.kernel_required
            .load(std::sync::atomic::Ordering::Acquire)
            && self.runtime_kernel.get().is_none()
    }

    pub(crate) fn runtime_kernel(&self) -> Option<Arc<crate::runtime_kernel::RuntimeKernel>> {
        self.runtime_kernel.get().cloned()
    }

    /// 注入运行操作内核（serve 在 ownership 认领后调用；幂等拒绝二次注入）。
    pub(crate) fn set_runtime_kernel(
        &self,
        kernel: Arc<crate::runtime_kernel::RuntimeKernel>,
    ) -> bool {
        self.runtime_kernel.set(kernel).is_ok()
    }

    /// 当前执行中的运行操作（dispatch 设置；主循环边界收束后清除）。
    ///
    /// 语义（R01 修复）：这是**正在执行**的操作身份，只在为空时由新动作占据；
    /// Stop 受理（active 期间允许）不覆盖在执行身份——停止操作由主循环在
    /// 边界显式执行并按自身 ID 收束（见 [`Self::finish_runtime_operation_by_id`]）。
    pub(crate) fn set_current_runtime_operation(&self, operation_id: Option<String>) {
        let mut guard = self
            .current_runtime_operation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // 仅允许 占空→有值 或 清空；有值被覆盖（迟到启动/停止并发）拒绝。
        if guard.is_some() && operation_id.is_some() {
            tracing::warn!(
                existing = ?guard,
                incoming = ?operation_id,
                "runtime operation identity is executing; refusing to overwrite (R01)"
            );
            return;
        }
        *guard = operation_id;
    }

    pub(crate) fn current_runtime_operation(&self) -> Option<String> {
        self.current_runtime_operation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 按显式 ID 收束运行操作（R01）：只收束指定操作，不误完成并发受理的
    /// 其他操作；在执行槽匹配时一并清除。
    ///
    /// V04：终态**持久化失败向上传播**——保留执行身份（不清 current）、
    /// 挂起 server 恢复门禁（写入口关闭 + ready 压低），调用方必须显式处理；
    /// 不允许记日志后按成功继续。
    pub(crate) async fn finish_runtime_operation_by_id(
        &self,
        operation_id: &str,
        state: shared_types::RuntimeOperationState,
        error: Option<(String, String)>,
    ) -> Result<(), String> {
        let Some(kernel) = self.runtime_kernel() else {
            return Ok(());
        };
        let sequence = kernel
            .store()
            .replay_events(operation_id, u64::MAX)
            .map(|events| events.len() as u64 + 1)
            .unwrap_or(2);
        if let Err(persist_error) = kernel
            .finish(operation_id, state, error, None, sequence.max(2))
            .await
        {
            let message = format!(
                "runtime operation terminal persist failed (op {operation_id}): {persist_error:#}"
            );
            tracing::error!("{message}");
            self.begin_runtime_recovery_hold();
            return Err(message);
        }
        if self.current_runtime_operation().as_deref() == Some(operation_id) {
            self.set_current_runtime_operation(None);
        }
        Ok(())
    }

    /// 当前执行操作是否已被请求取消（编排完成提交边界检查）。
    pub(crate) fn current_operation_cancelled(&self) -> bool {
        let Some(operation_id) = self.current_runtime_operation() else {
            return false;
        };
        self.runtime_kernel()
            .is_some_and(|kernel| kernel.is_cancelled(&operation_id))
    }

    /// 取消检查点（R03）：操作在执行副作用开始前已被取消 → 直接收束为
    /// Cancelled（无副作用，无需清理），返回 true（调用方跳过执行）。
    pub(crate) async fn settle_cancelled_before_execution(&self, operation_id: &str) -> bool {
        let Some(kernel) = self.runtime_kernel() else {
            return false;
        };
        if !kernel.is_cancelled(operation_id) {
            return false;
        }
        tracing::info!("runtime operation {operation_id} cancelled before execution");
        if let Err(error) = self
            .finish_runtime_operation_by_id(
                operation_id,
                shared_types::RuntimeOperationState::Cancelled,
                None,
            )
            .await
        {
            // V04：零副作用取消的终态都写不进——结果不可记录，写入口挂起
            tracing::error!("{error}; suppressing further execution");
        }
        true
    }

    /// 主循环边界收束运行操作（成功/失败/恢复保护三态；先持久化终态再清槽）。
    /// V04：持久化失败向上传播（身份保留 + 门禁已在底层挂起）。
    pub(crate) async fn finish_current_runtime_operation(
        &self,
        state: shared_types::RuntimeOperationState,
        error: Option<(String, String)>,
    ) -> Result<(), String> {
        let Some(operation_id) = self.current_runtime_operation() else {
            return Ok(());
        };
        self.finish_runtime_operation_by_id(&operation_id, state, error)
            .await
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

    /// 启动恢复是否仍在进行（P1-01：API bind 先于恢复，写端点/ready 门控依据）。
    pub fn initializing(&self) -> bool {
        self.initializing.load(std::sync::atomic::Ordering::Acquire)
    }

    /// 标记启动恢复完成：开放写端点受理与 ready 判定。
    /// legacy 直跑形态在 API bind 成功后立即调用（无恢复窗口）。
    pub fn mark_initialized(&self) {
        self.initializing
            .store(false, std::sync::atomic::Ordering::Release);
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

    /// 更新部署进度（progress_v1 能力协议）。
    pub(crate) fn set_deploy_progress(&self, progress: shared_types::AppDeploymentProgress) {
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.progress = Some(progress);
    }

    /// 清除部署进度（部署结束/重启时重置）。
    #[allow(dead_code)] // 由部署结束/重启路径消费（batch 2b/2c 进度协议）
    pub(crate) fn clear_deploy_progress(&self) {
        let mut status = self
            .deploy_status
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        status.progress = None;
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
        // B05：恢复保护约束**所有**写入口——旧部署链不得绕过（损坏记录/
        // 未终态操作/部分提交围栏期间，legacy 受理同样拒绝；显式部署也
        // 必须等操作员裁决恢复后进行）。
        // R05：kernel 不可用（状态根打开失败）同样 fail-closed——旧链只在
        // Some(kernel) 上检查保护会把"可信状态不可读"当成"无保护可查"放行。
        match self.runtime_kernel() {
            Some(kernel) if kernel.recovery_protection_active() => {
                return Err(AdmissionError::Busy(
                    "runtime state requires recovery; resolve held operations before deploying"
                        .into(),
                ));
            }
            _ if self.kernel_unavailable() => {
                return Err(AdmissionError::Busy(
                    "runtime state unavailable (state root could not be opened); deployment \
                     admission closed until recovery"
                        .into(),
                ));
            }
            _ if self.runtime_recovery_hold_active() => {
                // V04：运行操作终态持久化失败（结果未知）——身份保留中，
                // 新部署不得受理，直至重启恢复
                return Err(AdmissionError::Busy(
                    "runtime operation outcome could not be persisted; recovery required \
                     before deploying"
                        .into(),
                ));
            }
            _ => {}
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
            capabilities: vec!["progress_v1".into()],
            progress: None,
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
///
/// `--attach` 标志启用附着模式：已有实例占用端口时核验身份并等待退出，
/// 然后重新执行本二进制成为新 owner。无 `--attach` 时端口冲突立即 fail-fast。
pub async fn serve(args: &CliArgs) -> Result<()> {
    if args.attach {
        return attach_to_existing_owner(args).await;
    }
    serve_without_attach(args).await
}

/// 附着模式：核验已有实例身份并等待退出，然后重新执行为 owner。
///
/// 流程：
/// 1. 尝试 TCP connect 管理端口——不可达 = 无实例，直接走正常 serve 流程
/// 2. GET /v1/runtime/identity 核验 application_id + workspace_id
/// 3. 身份匹配 → 等待端口释放（轮询 connect）→ 重新执行本二进制（无 --attach）
/// 4. 身份不匹配 / API 不可达 / 503 → 立即退出（exit 1）
async fn attach_to_existing_owner(args: &CliArgs) -> Result<()> {
    use std::process::exit;

    let addr = &args.admin_addr;
    let url = format!("http://{addr}/v1/runtime/identity");

    // 核验已有实例身份
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .context("build identity probe client")?;

    tracing::info!("attach mode: checking existing instance at {addr}");

    // 带重试的连接检测（新实例启动需要时间完成 API bind）
    for attempt in 0..15 {
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                // 解析身份并核验
                let body: serde_json::Value = resp
                    .json()
                    .await
                    .context("parse identity response")?;
                return verify_identity_and_attach(args, &body, addr).await;
            }
            Ok(resp) => {
                tracing::error!(
                    "attach mode: existing instance at {addr} returned {} (identity unavailable); \
                     cannot verify ownership, exiting",
                    resp.status()
                );
                exit(1);
            }
            Err(_) if attempt < 14 => {
                // 连接失败——可能是端口空闲（无实例）或实例正在启动
                if std::net::TcpListener::bind(addr.as_str()).is_ok() {
                    // 端口空闲 = 无运行实例
                    tracing::info!(
                        "attach mode: no existing instance at {addr}, proceeding as owner"
                    );
                    return serve_without_attach(args).await;
                }
                // 端口被占但 API 不可达——实例正在启动中，等待
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
            Err(e) => {
                tracing::error!("attach mode: failed to connect to {addr}: {e}");
                exit(1);
            }
        }
    }

    tracing::error!("attach mode: could not reach existing instance at {addr} after retries");
    exit(1);
}

/// 核验已有实例身份，匹配则等待并 re-exec，不匹配则退出。
async fn verify_identity_and_attach(
    args: &CliArgs,
    body: &serde_json::Value,
    addr: &str,
) -> Result<()> {
    use std::os::unix::process::CommandExt as _;
    use std::process::exit;

    let identity = body.get("data");
    let remote_app = identity
        .and_then(|d| d.get("application_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let remote_ws = identity
        .and_then(|d| d.get("workspace_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let local_app = std::env::var("PROJECT_ID")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".to_string());
    let local_ws = args
        .workspace
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());

    if remote_app != local_app || remote_ws != local_ws {
        tracing::error!(
            "attach mode: identity mismatch — local {local_app}/{local_ws} \
             vs remote {remote_app}/{remote_ws}; refusing to attach"
        );
        exit(1);
    }

    tracing::info!(
        "attach mode: existing instance matches identity {local_app}/{local_ws}; \
         waiting for it to exit..."
    );

    // 等待端口释放（已有实例退出后端口变为可绑定）
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(300);
    loop {
        if tokio::time::Instant::now() >= deadline {
            tracing::error!("attach mode: timed out waiting for existing instance to exit");
            exit(1);
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        if std::net::TcpListener::bind(addr).is_ok() {
            tracing::info!("attach mode: port {addr} released, re-executing as owner");
            break;
        }
    }

    // 重新执行本二进制（无 --attach）成为新 owner
    let current_exe = std::env::current_exe().context("get current executable path")?;
    let mut cmd = std::process::Command::new(current_exe);
    for arg in std::env::args().skip(1) {
        if arg != "--attach" && arg != "attach" {
            cmd.arg(arg);
        }
    }
    let error = cmd.exec();
    tracing::error!("attach mode: re-exec failed: {error}");
    exit(1);
}

/// serve 核心逻辑（无附着检测）：journal → API bind → 状态机主循环。
async fn serve_without_attach(args: &CliArgs) -> Result<()> {
    let journal = Journal::open(&args.workspace)?;
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

    // 管理 API 预绑定（P1-01）：bind 成功才继续部署清理/启动恢复/业务停启——
    // 端口冲突在一切运行态副作用之前 fail-fast（未 commit_coordinator、未
    // stop_all；journal 随 Drop 释放锁，不写 Quiescent）。恢复完成前写端点由
    // initializing 门控拒绝、/ready 以 initializing 摘流（探针早有人应答的价值
    // 保留——/health 恒 200 覆盖 kubelet liveness）。serve future 运行期故障 →
    // cancel 主循环受控收束，不留"无管理面的运行态"。
    let api_state = state.clone();
    let api_addr = args.admin_addr.clone();
    let api_workspace = args.workspace.clone();
    let api_log_dir = args.log_dir.clone();
    let api_pingap_bin = args.pingap_bin.clone();
    let (api_listener, api_app) = crate::api::bind(
        &api_addr,
        api_workspace,
        api_log_dir,
        api_pingap_bin,
        api_state,
    )
    .await?;
    let api_failure = Arc::new(std::sync::Mutex::new(None::<String>));
    let api_monitor_failure = api_failure.clone();
    let api_monitor_state = state.clone();
    let api_handle = tokio::spawn(async move {
        let result = axum::serve(api_listener, api_app)
            .await
            .context("serve app-cli management API");
        if let Err(error) = result {
            tracing::error!("app-cli management API failed: {error:#}");
            if let Ok(mut slot) = api_monitor_failure.lock() {
                *slot = Some(format!("{error:#}"));
            }
            api_monitor_state.cancel.cancel();
        }
    });

    crate::deploy::cleanup_startup(&args.workspace).await?;

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
    // B05：内核装配**先于**启动决策——恢复裁决（损坏记录/未终态操作/desired
    // 读取）必须先于 initialize_startup，否则 Existing 自动启动在保护生效前
    // 已进入 first_request。
    let mut kernel_recovery_hold = false;
    if ownership_claimed {
        // R05：尝试装配即标记——失败（kernel=None）时写入口 fail-closed
        state.mark_kernel_required();
        match assemble_runtime_kernel(&state, args).await {
            Ok(kernel) => {
                let recovered = kernel
                    .recover()
                    .await
                    .context("recover runtime operation state")?;
                if !recovered.is_empty() {
                    tracing::warn!("runtime operations held for recovery: {:?}", recovered);
                }
                if kernel.recovery_protection_active() {
                    // B05：恢复保护（含损坏记录 blocked）压制自动启动——
                    // 不撤销已受理的操作记录（可查询），业务保持 Idle 直至
                    // 操作员裁决。显式 env 部署同样被 try_accept 门控拒绝。
                    kernel_recovery_hold = true;
                    tracing::error!(
                        "runtime state requires recovery; automatic business startup suppressed"
                    );
                }
                state.set_runtime_kernel(kernel);
            }
            Err(error) => {
                // B05：状态根不可用 = 运行态可信状态不可读——不再仅关闭新 API
                // 放行旧链。fail-closed：压住自动启动；旧部署受理也会因
                // desired/记录不可读而不可信（deploy admission 检查
                // runtime_kernel 为 None 时见下述显式阻断）。
                kernel_recovery_hold = true;
                tracing::error!(
                    "runtime kernel unavailable ({error:#}); automatic business startup suppressed"
                );
            }
        }
    }
    let mut first_request = if let Some(error) = startup_error.as_ref() {
        state.begin_failure(format!("startup shutdown unconfirmed: {error:#}"), true);
        None
    } else if kernel_recovery_hold {
        state.set_phase(ServerPhase::Idle);
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
    // R03/B05：desired 读取先于启动决策；**读取失败同样压制自动启动**
    //（损坏 desired 等价于不可信状态——不允许"读错当 Running 继续起"）。
    // R05：读错时必须清除已生成的 first_request——否则自动恢复（journal
    // resume/卷上 release.lock 的 Existing 路径）仍会在不可信状态上启动。
    if ownership_claimed && !kernel_recovery_hold {
        let mut desired_unreadable = false;
        let desired = match state
            .runtime_kernel()
            .map(|kernel| kernel.store().load_desired())
        {
            Some(Ok((desired, _))) => Some(desired),
            Some(Err(error)) => {
                tracing::error!(
                    "desired state unreadable ({error:#}); automatic business recovery suppressed"
                );
                desired_unreadable = true;
                state.set_phase(ServerPhase::Idle);
                None
            }
            None => None,
        };
        if desired_unreadable && first_request.is_some() {
            first_request = None;
            tracing::error!(
                "pre-generated startup action discarded: desired state unreadable (R05)"
            );
        }
        if desired == Some(shared_types::DesiredState::Stopped)
            && matches!(first_request, Some(InitialAction::Existing))
        {
            // 用户 stop（spec §5）压制**自动恢复**（journal resume/卷上
            // release.lock 的 Existing 路径），保持 Idle；显式 env 部署
            //（Deploy 路径）是新的部署意图，不受 Stopped 压制。
            first_request = None;
            tracing::info!(
                "desired state is Stopped; automatic business recovery suppressed (staying Idle)"
            );
            state.set_phase(ServerPhase::Idle);
        }
    }

    // 启动恢复完成（含 Failed 相位——可再次部署修复的合法可查状态）：开放
    // 写端点受理与 /ready 判定。quiescence 失败路径不开放（进程保护现场至退出）。
    if ownership_claimed {
        state.mark_initialized();
    }

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
    // API 运行期故障（监控记录）：即使主循环已正常收尾也按失败退出——
    // 无管理面的实例不可宣称成功（P1-01）。正常关停路径 api task 被
    // abort，不会写入故障记录。
    if let Ok(slot) = api_failure.lock()
        && let Some(api_error) = slot.as_ref()
    {
        return Err(
            anyhow::anyhow!("app-cli management API terminated: {api_error}").context("serve"),
        );
    }
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

/// 装配运行操作内核（serve 专用；dispatch 把内核动作翻译进既有执行通道）。
async fn assemble_runtime_kernel(
    state: &Arc<ServerState>,
    args: &crate::config::CliArgs,
) -> Result<Arc<crate::runtime_kernel::RuntimeKernel>> {
    use crate::runtime_kernel::{DispatchAction, RuntimeKernel, RuntimeStore};
    let application_id = std::env::var("PROJECT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "unknown-app".to_string());
    // B04：显式状态根（env 权威；缺省按应用隔离）——source/.run/别名同域
    let root = RuntimeStore::resolve_root(&args.workspace, &application_id)?;
    let store = RuntimeStore::open_with_root(root, &args.workspace)?;
    let workspace_id = args
        .workspace
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "workspace".to_string());
    let source_root = args
        .workspace
        .canonicalize()
        .unwrap_or_else(|_| args.workspace.clone())
        .to_string_lossy()
        .to_string();
    let identity = store.load_or_init_identity(
        application_id,
        "userapp-dev".to_string(),
        workspace_id,
        source_root,
        state.generation.clone(),
    )?;
    let dispatch_state = state.clone();
    let dispatch = Box::new(move |action: DispatchAction| match action {
        DispatchAction::DeployArtifact {
            operation_id,
            url,
            sha256,
        } => {
            // R03 取消检查点：排队期间被取消 → 不进部署链（dispatch 是同步
            // 闭包，终态收束经 spawn 落盘）
            let settle_state = dispatch_state.clone();
            if settle_state
                .runtime_kernel()
                .is_some_and(|kernel| kernel.is_cancelled(&operation_id))
            {
                tokio::spawn(async move {
                    settle_state
                        .settle_cancelled_before_execution(&operation_id)
                        .await;
                });
                return;
            }
            dispatch_state.set_current_runtime_operation(Some(operation_id.clone()));
            // release_id 语义 = 调用方请求标识（request_release_id 驱动等待方
            // 确认）；以 runtime 操作 ID 承载，形成 API 侧可观察的关联。
            let marker = format!("runtime-{operation_id}");
            let request = DeployRequest {
                url,
                release_id: marker,
                sha256,
            };
            if dispatch_state.deploy_tx.send(request).is_err() {
                tracing::error!("runtime dispatch: deploy channel closed ({operation_id})");
            }
        }
        DispatchAction::OrchestrateSource {
            operation_id,
            dev_profile,
        } => {
            dispatch_state.set_current_runtime_operation(Some(operation_id.clone()));
            if dispatch_state
                .control_tx
                .send(ControlSignal::OrchestrateSource {
                    operation_id: operation_id.clone(),
                    dev_profile,
                })
                .is_err()
            {
                tracing::error!("runtime dispatch: control channel closed ({operation_id})");
            }
        }
        DispatchAction::StopBusiness { operation_id } => {
            // R01：Stop 在 active 期间受理（意图屏障），但**不抢占执行身份**——
            // 主循环在下一个边界消费本信号并按此 ID 显式执行/收束停止操作。
            if dispatch_state
                .control_tx
                .send(ControlSignal::StopBusiness {
                    operation_id: operation_id.clone(),
                })
                .is_err()
            {
                tracing::error!("runtime dispatch: control channel closed ({operation_id})");
            }
        }
    });
    Ok(Arc::new(RuntimeKernel::new(store, identity, dispatch)))
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
    /// 运行控制 stop：停止业务服务（保持管理面）。携带受理操作 ID——
    /// 从受理、排队、执行到终态完整传递（B01：Stop 不设 current，按
    /// 自身 ID 收束，绝不依赖"最近一次"全局值）。
    StopBusiness {
        operation_id: String,
    },
    /// 排队期已取消的操作：已按自身 ID 收束 Cancelled，零副作用——
    /// 不停止无关运行实例、不派发 Stop（R04：取消收束不得变成 Stop 执行）。
    Settled,
}

/// 控制信号在服务已停止后的收束（R01）：Reorchestrate 占据执行身份由外层
/// 重编排；Stopped 按信号自身 ID 收束（不触碰在执行的其他操作）。
async fn settle_control_signal(state: &ServerState, signal: ControlSignal) -> InitialAction {
    match signal {
        ControlSignal::OrchestrateSource {
            operation_id,
            dev_profile,
        } => {
            // R03/R04 取消检查点：派发排队期间被取消 → 按自身 ID 收束
            // Cancelled 后零动作返回——不编排、不派发 Stop（取消收束变成
            // Stop 执行会用 Succeeded 覆盖 Cancelled 并停止无关运行实例）
            if state.settle_cancelled_before_execution(&operation_id).await {
                return InitialAction::Settled;
            }
            state.set_current_runtime_operation(Some(operation_id));
            state.set_pending_dev_profile(dev_profile);
            InitialAction::Existing
        }
        ControlSignal::StopBusiness { operation_id } => {
            InitialAction::StopBusiness { operation_id }
        }
    }
}

/// Prepare while the existing supervisor continues serving. Failed requests do
/// not leave this wait loop and never reach the stop/activate boundary.
async fn next_prepared(
    args: &CliArgs,
    state: &Arc<ServerState>,
    rx: &mut tokio::sync::mpsc::UnboundedReceiver<DeployRequest>,
) -> Option<InitialAction> {
    loop {
        let request = rx.recv().await?;
        let state_clone = state.clone();
        let progress_cb: crate::deploy::ProgressCallback =
            std::sync::Arc::new(move |p: shared_types::AppDeploymentProgress| {
                state_clone.set_deploy_progress(p);
            });
        match state
            .preparations
            .run(args.workspace.clone(), request, Some(progress_cb))
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
    if let Err(settle_error) = state
        .finish_current_runtime_operation(
            shared_types::RuntimeOperationState::Failed,
            Some(("ERR_BACKEND_ERROR".to_string(), error.clone())),
        )
        .await
    {
        // V04：失败终态都写不进——结果未知，走恢复保护路径
        hold_unconfirmed(state, format!("{error}; {settle_error}")).await;
        return;
    }
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
    if let Err(settle_error) = state
        .finish_current_runtime_operation(
            shared_types::RuntimeOperationState::RecoveryRequired,
            Some((
                shared_types::ERR_RECOVERY_REQUIRED.to_string(),
                error.clone(),
            )),
        )
        .await
    {
        // V04：连 RecoveryRequired 都不可持久化——身份保留 + server 门禁
        // （finish 内已挂起）；此处如实记录后维持保护现场
        tracing::error!("{settle_error}; holding without durable terminal state");
    }
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
    // R06：先清理、确认后才发布终态——原顺序先记 Failed 再清理，清理未知
    // 时操作身份已被清除，恢复保护无从挂起。清理失败路径经 hold_unconfirmed
    // 以 RecoveryRequired 收束（身份保留至该点），成功路径最后记 Failed。
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
        return None;
    }
    if let Err(settle_error) = state
        .finish_current_runtime_operation(
            shared_types::RuntimeOperationState::Failed,
            Some(("ERR_BACKEND_ERROR".to_string(), error.clone())),
        )
        .await
    {
        // V04：清理已确认但失败终态写不进——结果未知，保持恢复保护
        tracing::error!("{settle_error}; operation held for recovery");
    }
    None
}

/// 屏障裁决后的取消原因辅助（V02）：仅用于收束记录的 reason 文案；
/// 权威判定已在屏障内完成。
fn commit_running_barrier_reason_cancelled(state: &ServerState) -> bool {
    state.current_operation_cancelled()
}

/// 内核提交屏障结果（B03 收敛形态）。
#[derive(PartialEq, Eq, Clone, Copy, Debug)]
enum BarrierOutcome {
    /// 屏障通过（已收束 Succeeded）/ 无在途操作——调用方继续正常流转。
    Passed,
    /// 观察到取消意图（V02：内核**未写终态**）——调用方必须先停服并确认
    /// 清理，再按 ID 收束 Cancelled；清理未知走 RecoveryRequired。
    CancelledByRequest,
    /// Stop 已受理（revision 推进）：调用方必须停服、按 ID 收束 Cancelled。
    Superseded,
}

/// Running 入口的统一提交屏障（B03）：无内核/无在途操作时直通。
/// 屏障失败（持久化错误）→ 按原终态语义收束 RecoveryRequired 并视为 Passed
/// （fail-closed：不报成功，保护保留在内核）。
///
/// R01：屏障通过即清理**server 侧执行身份**（内核 finish 只清内核 active 槽，
/// 残留身份会让后续 Restart B 的 set_current 被旧 A 拒绝、B 完成时误收束 A，
/// B 永久 Accepted）。
/// R02：NotActive 不等同提交成功——操作已终态（其他路径收束）时幂等放行；
/// 非终态则 fail-closed 收束 RecoveryRequired（执行身份丢失，不报成功）。
async fn commit_running_barrier(state: &ServerState) -> BarrierOutcome {
    let Some(kernel) = state.runtime_kernel() else {
        return BarrierOutcome::Passed;
    };
    let Some(operation_id) = state.current_runtime_operation() else {
        return BarrierOutcome::Passed;
    };
    let settle_identity = || {
        if state.current_runtime_operation().as_deref() == Some(operation_id.as_str()) {
            state.set_current_runtime_operation(None);
        }
    };
    match kernel.commit_execution(&operation_id).await {
        Ok(crate::runtime_kernel::CommitBarrierOutcome::Committed) => {
            // R01：内核已收束 Succeeded——同步清理 server 执行身份
            settle_identity();
            BarrierOutcome::Passed
        }
        Ok(crate::runtime_kernel::CommitBarrierOutcome::Cancelled) => {
            // V02：取消意图已观察到但**终态未写**——不清身份；调用方先停服，
            // 确认后按 ID 收束（清理未知 → RecoveryRequired）
            BarrierOutcome::CancelledByRequest
        }
        Ok(crate::runtime_kernel::CommitBarrierOutcome::Superseded) => BarrierOutcome::Superseded,
        Ok(crate::runtime_kernel::CommitBarrierOutcome::NotActive) => {
            match kernel.get(&operation_id).await {
                Ok(Some(view)) if view.state.is_terminal() => {
                    // 幂等：操作已被其他路径收束——清理残留身份后放行
                    settle_identity();
                    BarrierOutcome::Passed
                }
                _ => {
                    // R02：active 不在本操作且无终态——执行身份丢失，fail-closed
                    tracing::error!(
                        "runtime commit barrier: operation {operation_id} lost execution \
                         identity before commit; settling RecoveryRequired"
                    );
                    if let Err(settle_error) = state
                        .finish_runtime_operation_by_id(
                            &operation_id,
                            shared_types::RuntimeOperationState::RecoveryRequired,
                            Some((
                                shared_types::ERR_RECOVERY_REQUIRED.into(),
                                "execution identity lost before commit barrier".into(),
                            )),
                        )
                        .await
                    {
                        tracing::error!("{settle_error}; recovery hold engaged");
                    }
                    BarrierOutcome::Passed
                }
            }
        }
        Err(error) => {
            tracing::error!("runtime commit barrier failed (op {operation_id}): {error:#}");
            if let Err(settle_error) = state
                .finish_runtime_operation_by_id(
                    &operation_id,
                    shared_types::RuntimeOperationState::RecoveryRequired,
                    Some((
                        shared_types::ERR_RECOVERY_REQUIRED.into(),
                        format!("commit barrier persistence failed: {error:#}"),
                    )),
                )
                .await
            {
                tracing::error!("{settle_error}; recovery hold engaged");
            }
            BarrierOutcome::Passed
        }
    }
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
                let mut control = state.control_rx.lock().await;
                let action = tokio::select! {
                    maybe = rx.recv() => match maybe {
                        Some(req) => Some(InitialAction::Deploy(req)),
                        None => return Ok(()), // api 层全退（不可能，防御）
                    },
                    signal = control.recv() => match signal {
                        Some(ControlSignal::OrchestrateSource { operation_id, dev_profile }) => {
                            // B01：ID 必须完整传递——Idle 消费即占据执行身份
                            //（取消检查点在 settle_control_signal 内）
                            Some(settle_control_signal(
                                state,
                                ControlSignal::OrchestrateSource { operation_id, dev_profile },
                            ).await)
                        }
                        Some(ControlSignal::StopBusiness { operation_id }) => {
                            Some(InitialAction::StopBusiness { operation_id })
                        }
                        None => {
                            // api 层全退（防御）；部署通道仍存活时继续等
                            tokio::select! {
                                maybe = rx.recv() => match maybe {
                                    Some(req) => Some(InitialAction::Deploy(req)),
                                    None => return Ok(()),
                                },
                                () = state.cancel.cancelled() => return Ok(()),
                            }
                        }
                    },
                    () = state.cancel.cancelled() => return Ok(()),
                };
                // control/防御分支产出 Option；展开为统一 InitialAction
                match action {
                    Some(action) => action,
                    None => continue,
                }
            }
        };

        let run_migrations = true;
        if matches!(action, InitialAction::Settled) {
            // R04：排队期取消已收束——零副作用回 Idle
            if !matches!(state.phase(), ServerPhase::Failed(_)) {
                state.set_phase(ServerPhase::Idle);
            }
            continue;
        }
        if let InitialAction::StopBusiness { operation_id } = action {
            // stop：停止业务服务（保持管理面）。B01：按**自身受理 ID**收束——
            // Stop 从不占据 current 执行槽，禁止 finish_current（它会读
            // current=None 而静默丢终态，Stop 永远 Accepted）。
            let stopped = async {
                if let Some(host) = host.as_ref() {
                    host.stop_all().await?;
                }
                crate::static_hosting::reconcile(&[], &args.workspace, false).await
            }
            .await;
            match stopped {
                Ok(()) => {
                    // V04：先持久化终态再切相位——收束失败（结果未知）不得
                    // 以 Idle 成功面貌继续
                    match state
                        .finish_runtime_operation_by_id(
                            &operation_id,
                            shared_types::RuntimeOperationState::Succeeded,
                            None,
                        )
                        .await
                    {
                        Ok(()) => state.set_phase(ServerPhase::Idle),
                        Err(settle_error) => {
                            tracing::error!("{settle_error}");
                            state.set_phase(ServerPhase::Failed(settle_error));
                        }
                    }
                }
                Err(error) => {
                    tracing::error!("runtime stop business failed: {error:#}");
                    state.set_phase(ServerPhase::Failed(format!("stop: {error:#}")));
                    if let Err(settle_error) = state
                        .finish_runtime_operation_by_id(
                            &operation_id,
                            shared_types::RuntimeOperationState::RecoveryRequired,
                            Some((
                                shared_types::ERR_BACKEND_ERROR.into(),
                                format!("stop business unconfirmed: {error:#}"),
                            )),
                        )
                        .await
                    {
                        tracing::error!("{settle_error}; recovery hold engaged");
                    }
                }
            }
            continue;
        }
        let deployment_attempt = matches!(
            action,
            InitialAction::Deploy(_) | InitialAction::Prepared(_)
        );
        let prepared = match action {
            InitialAction::StopBusiness { .. } | InitialAction::Settled => {
                unreachable!("handled above")
            }
            InitialAction::Deploy(request) => {
                state.set_phase(ServerPhase::Deploying);
                state.set_request_release_id(&request.release_id);
                let state_ref = state.clone();
                let progress_cb: crate::deploy::ProgressCallback =
                    std::sync::Arc::new(move |p: shared_types::AppDeploymentProgress| {
                        state_ref.set_deploy_progress(p);
                    });
                match state
                    .preparations
                    .run(args.workspace.clone(), request, Some(progress_cb))
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
        // R08：本次操作的 dev profile（Source 形态编排显式传递；未指定 =
        // legacy/部署路径 → env 兜底）。take 一次性消费——每次编排对应一次取值
        let run_dev_profile = state
            .take_pending_dev_profile()
            .unwrap_or_else(crate::supervisor::dev_run_profile);
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
                .orchestrate(
                    args,
                    &release,
                    &runtime_status,
                    run_migrations,
                    run_dev_profile,
                )
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
            // V02：取消观察并入提交屏障（内核锁内检查，消除外层检查与提交
            // 之间的竞争窗口）——Cancelled/Superseded 都必须**先停服确认**
            // 再按 ID 收束 Cancelled；清理未知走 hold_unconfirmed。
            match commit_running_barrier(state).await {
                BarrierOutcome::Passed => {}
                BarrierOutcome::CancelledByRequest | BarrierOutcome::Superseded => {
                    let reason = if commit_running_barrier_reason_cancelled(state) {
                        "cancelled during orchestration"
                    } else {
                        "superseded by an admitted stop operation"
                    };
                    if let Err(error) = host.stop_all().await {
                        hold_unconfirmed(
                            state,
                            format!("stop after cancelled/superseded startup failed: {error:#}"),
                        )
                        .await;
                        return Ok(());
                    }
                    if let Some(operation_id) = state.current_runtime_operation()
                        && let Err(settle_error) = state
                            .finish_runtime_operation_by_id(
                                &operation_id,
                                shared_types::RuntimeOperationState::Cancelled,
                                Some((shared_types::ERR_RECOVERY_REQUIRED.into(), reason.into())),
                            )
                            .await
                    {
                        tracing::error!("{settle_error}; recovery hold engaged");
                        state.set_phase(ServerPhase::Failed(settle_error));
                        continue;
                    }
                    state.set_phase(ServerPhase::Idle);
                    continue;
                }
            }
            // R01：supervisord Running 等待也消费运行控制信号（Stop/重启编排
            // 不再只能等 Idle）。锁序与 Idle 分支一致：deploy 先、control 后。
            let mut control_rx = state.control_rx.lock().await;
            let next = tokio::select! {
                maybe = next_prepared(args, state, &mut hot_rx) => match maybe {
                    Some(action) => Next::Redeploy(action),
                    None => Next::Exit,
                },
                signal = control_rx.recv() => match signal {
                    Some(signal) => {
                        // R04：排队期已取消的启动/重启——零副作用收束，不停
                        // 在跑业务（取消收束不得变成 Stop 执行）
                        if let ControlSignal::OrchestrateSource { operation_id, .. } = &signal
                            && state
                                .runtime_kernel()
                                .is_some_and(|kernel| kernel.is_cancelled(operation_id))
                        {
                            state.settle_cancelled_before_execution(operation_id).await;
                            Next::Wait
                        } else {
                            state.ready.set_ready(false);
                            if let Err(error) = host.stop_all().await {
                                hold_unconfirmed(
                                    state,
                                    format!("stop before runtime control failed: {error:#}"),
                                )
                                .await;
                                return Ok(());
                            }
                            // B01：动作整体交回主循环——Existing 重编排、
                            // StopBusiness{ID} 由 loop-top 唯一收束路径按 ID 完成
                            //（stop_all 幂等，重复执行无害）。
                            Next::Redeploy(settle_control_signal(state, signal).await)
                        }
                    }
                    None => Next::Wait,
                },
                () = state.cancel.cancelled() => Next::Exit,
            };
            drop(control_rx);
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
            run_dev_profile,
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
                if result.is_ok() {
                    // V02：取消观察并入提交屏障（内核锁内检查，消除外层检查与
                    // 提交之间的窗口）；Cancelled/Superseded 统一"先停本组服务
                    // 确认，再按 ID 收束 Cancelled"，清理未知走 hold_unconfirmed。
                    // B02：join 后 JoinHandle 不得再 poll。
                    let barrier = commit_running_barrier(state).await;
                    if barrier != BarrierOutcome::Passed {
                        let reason = if barrier == BarrierOutcome::CancelledByRequest {
                            "cancelled during orchestration"
                        } else {
                            "superseded by an admitted stop operation"
                        };
                        cancel.cancel();
                        if let Err(error) = join_supervisor(&mut sup, &mut sup_joined).await {
                            hold_unconfirmed(
                                state,
                                format!("stop after cancelled/superseded startup failed: {error:#}"),
                            )
                            .await;
                            return Ok(());
                        }
                        if let Some(operation_id) = state.current_runtime_operation()
                            && let Err(settle_error) = state
                                .finish_runtime_operation_by_id(
                                    &operation_id,
                                    shared_types::RuntimeOperationState::Cancelled,
                                    Some((shared_types::ERR_RECOVERY_REQUIRED.into(), reason.into())),
                                )
                                .await
                            {
                                tracing::error!("{settle_error}; recovery hold engaged");
                                state.set_phase(ServerPhase::Failed(settle_error));
                                continue;
                            }
                        state.set_phase(ServerPhase::Idle);
                        continue;
                    }
                }
                let mut builtin_control = state.control_rx.lock().await;
                tokio::select! {
                    outcome = &mut sup, if !sup_joined => { sup_joined = true; match outcome {
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
            // R01：builtin Running 等待也消费运行控制信号（先停本组服务再收束）
            signal = builtin_control.recv() => match signal {
                Some(signal) => {
                    state.ready.set_ready(false);
                    cancel.cancel();
                    if let Err(error) = join_supervisor(&mut sup, &mut sup_joined).await {
                        hold_unconfirmed(state, format!("stop before runtime control failed: {error:#}")).await;
                        return Ok(());
                    }
                    // B01：同 supervisord 分支——动作整体交回主循环唯一收束路径
                    Next::Redeploy(settle_control_signal(state, signal).await)
                }
                None => Next::Wait,
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

    // ===== R01/R02/R04：执行身份生命周期与屏障裁决 =====

    fn kernel_for(dir: &std::path::Path) -> std::sync::Arc<crate::runtime_kernel::RuntimeKernel> {
        let workspace = dir.join("workspace");
        std::fs::create_dir_all(&workspace).unwrap();
        let identity = shared_types::RuntimeIdentityView {
            application_id: "app1".into(),
            service_family: "userapp-dev".into(),
            workspace_id: "ws-1".into(),
            source_root: "/workspace".into(),
            runtime_instance_id: "instance-1".into(),
            deployment_generation_id: "gen-1".into(),
            protocol_version: shared_types::RUNTIME_CONTROL_PROTOCOL_VERSION,
            capabilities: vec![],
        };
        let store =
            crate::runtime_kernel::RuntimeStore::open_with_root(dir.join("state-root"), &workspace)
                .expect("store");
        std::sync::Arc::new(crate::runtime_kernel::RuntimeKernel::new(
            store,
            identity,
            Box::new(|_| {}),
        ))
    }

    fn runtime_request(
        kind: shared_types::RuntimeOperationKind,
        operation_id: &str,
        revision: u64,
    ) -> shared_types::RuntimeOperationRequest {
        shared_types::RuntimeOperationRequest {
            operation_id: operation_id.into(),
            expected_runtime_instance_id: "instance-1".into(),
            expected_revision: revision,
            workspace_id: "ws-1".into(),
            kind,
            profile: shared_types::RunProfileInput::Source {
                workspace_id: "ws-1".into(),
            },
            request_context: None,
        }
    }

    #[tokio::test]
    async fn start_restart_sequence_keeps_execution_identity_per_operation() {
        // R01：Start A 提交成功必须清理 server 执行身份——否则 Restart B 的
        // set_current 被旧 A 拒绝、B 完成时误收束 A，B 永久 Accepted。
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());

        // A：受理 → 占据身份 → 提交
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-a",
                0,
            ))
            .await
            .expect("admit A");
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-a".into(),
                dev_profile: true,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Existing));
        assert_eq!(state.current_runtime_operation().as_deref(), Some("op-a"));
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        // R01 核心：屏障通过后 server 身份必须清理（修复前残留 op-a）
        assert_eq!(state.current_runtime_operation(), None);

        // B：同一序列的 Restart 不被旧身份阻塞，正常提交
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Restart,
                "op-b",
                0,
            ))
            .await
            .expect("admit B");
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-b".into(),
                dev_profile: true,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Existing));
        assert_eq!(state.current_runtime_operation().as_deref(), Some("op-b"));
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        for operation in ["op-a", "op-b"] {
            let view = kernel.get(operation).await.unwrap().expect("record");
            assert_eq!(view.state, shared_types::RuntimeOperationState::Succeeded);
        }
    }

    #[tokio::test]
    async fn barrier_not_active_on_terminal_operation_is_idempotent_pass() {
        // R02 幂等分支：操作已被其他路径收束（终态）——NotActive 清残留身份放行
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-t",
                0,
            ))
            .await
            .expect("admit");
        state.set_current_runtime_operation(Some("op-t".into()));
        // 其他路径已把内核 active 收束（先于 server 屏障）
        kernel
            .finish(
                "op-t",
                shared_types::RuntimeOperationState::Succeeded,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        assert_eq!(state.current_runtime_operation(), None);
        let view = kernel.get("op-t").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Succeeded);
    }

    #[tokio::test]
    async fn barrier_not_active_on_live_operation_fails_closed() {
        // R02 fail-closed 分支保留：active 不在本操作且无终态（执行身份丢失）
        // ——不得当 Passed 报成功；本例构造终态已存在的幂等场景。身份丢失
        // 且无终态的 fail-closed 分支由 V03 结构性消除（Stop 不再抢 active）。
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-x",
                0,
            ))
            .await
            .expect("admit start");
        // 其他路径已收束（终态存在），server 身份残留——幂等清理放行
        kernel
            .finish(
                "op-x",
                shared_types::RuntimeOperationState::Succeeded,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        state.set_current_runtime_operation(Some("op-x".into()));
        assert_eq!(commit_running_barrier(&state).await, BarrierOutcome::Passed);
        assert_eq!(state.current_runtime_operation(), None);
        let view = kernel.get("op-x").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Succeeded);
    }

    #[tokio::test]
    async fn barrier_cancelled_by_request_keeps_identity_until_settle() {
        // V02：屏障观察到取消意图 → CancelledByRequest——内核**未写终态**、
        // server 身份保留；停服确认后 finish 才收束 Cancelled
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-cxl",
                0,
            ))
            .await
            .expect("admit");
        state.set_current_runtime_operation(Some("op-cxl".into()));
        kernel.request_cancel("op-cxl").await.unwrap();
        assert_eq!(
            commit_running_barrier(&state).await,
            BarrierOutcome::CancelledByRequest
        );
        // 终态未写 + 身份保留（等待停服确认）
        let view = kernel.get("op-cxl").await.unwrap().expect("record");
        assert!(!view.state.is_terminal(), "屏障不得预写终态（V02）");
        assert_eq!(state.current_runtime_operation().as_deref(), Some("op-cxl"));
        // 停服确认后按 ID 收束 Cancelled（终态单调保住不被迟到成功覆盖）
        state
            .finish_runtime_operation_by_id(
                "op-cxl",
                shared_types::RuntimeOperationState::Cancelled,
                None,
            )
            .await
            .unwrap();
        let view = kernel.get("op-cxl").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Cancelled);
        assert_eq!(state.current_runtime_operation(), None);
    }

    #[tokio::test]
    async fn terminal_persist_failure_keeps_identity_and_gates_admission() {
        // V04：终态持久化失败——Err 上抛、身份保留、恢复门禁挂起（部署受理
        // 拒绝、ready 压低），不允许日志后按成功继续
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-persist",
                0,
            ))
            .await
            .expect("admit");
        state.set_current_runtime_operation(Some("op-persist".into()));
        // 注入：对不存在记录收束 → kernel.finish 报错（持久化失败替身）
        let result = state
            .finish_runtime_operation_by_id(
                "op-never-recorded",
                shared_types::RuntimeOperationState::Failed,
                None,
            )
            .await;
        let error = result.expect_err("persist failure must propagate");
        assert!(error.contains("persist failed"));
        // 身份保留 + 门禁挂起
        assert_eq!(
            state.current_runtime_operation().as_deref(),
            Some("op-persist"),
            "持久化失败不得清除执行身份"
        );
        assert!(state.runtime_recovery_hold_active());
        // 部署受理被门禁拒绝
        let dir2 = tempfile::tempdir().unwrap();
        *state.journal.lock().unwrap() = Some(Journal::open(&dir2.path().join("code")).unwrap());
        state.set_phase(ServerPhase::Running);
        assert!(matches!(
            state.try_accept_deploy_with_id(request(), "gated".into()),
            Err(AdmissionError::Busy(_))
        ));
    }

    #[tokio::test]
    async fn stop_admission_does_not_steal_executor_identity() {
        // V03：Stop 受理（revision 推进）不抢走执行者身份——A 的提交屏障
        // 得到 Superseded（而非 NotActive），A 停服后按自身 ID 收束 Cancelled，
        // Stop 执行完成清 pending，此后新操作可受理
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-a",
                0,
            ))
            .await
            .expect("admit A");
        state.set_current_runtime_operation(Some("op-a".into()));
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Stop,
                "op-stop",
                0,
            ))
            .await
            .expect("stop admitted during A");
        assert_eq!(
            commit_running_barrier(&state).await,
            BarrierOutcome::Superseded
        );
        state
            .finish_runtime_operation_by_id(
                "op-a",
                shared_types::RuntimeOperationState::Cancelled,
                None,
            )
            .await
            .unwrap();
        kernel
            .finish(
                "op-stop",
                shared_types::RuntimeOperationState::Succeeded,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-c",
                1,
            ))
            .await
            .expect("C admitted after A/B settled");
        let a = kernel.get("op-a").await.unwrap().expect("record");
        assert_eq!(a.state, shared_types::RuntimeOperationState::Cancelled);
    }

    #[tokio::test]
    async fn uncertain_a_recovery_protection_latches_while_stop_pending() {
        // V03：恢复保护由结果未知决定——A 以 RecoveryRequired 收束时 Stop 仍
        // 待执行（pending 占据），保护必须挂起且新操作被拒
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-a2",
                0,
            ))
            .await
            .expect("admit A");
        state.set_current_runtime_operation(Some("op-a2".into()));
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Stop,
                "op-stop2",
                0,
            ))
            .await
            .expect("stop admitted");
        kernel
            .finish(
                "op-a2",
                shared_types::RuntimeOperationState::RecoveryRequired,
                None,
                None,
                2,
            )
            .await
            .unwrap();
        assert!(
            kernel.recovery_protection_active(),
            "未知结果必须挂起恢复保护（不依赖 active 匹配）"
        );
        let rejected = kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-c2",
                1,
            ))
            .await
            .expect_err("recovery protection rejects");
        assert_eq!(rejected.code, "ERR_RECOVERY_REQUIRED");
    }

    #[tokio::test]
    async fn kernel_unavailable_closes_legacy_deploy_admission() {
        // R05：状态根打开失败（尝试装配但 kernel=None）——legacy 部署受理
        // fail-closed；从未尝试装配的上下文保持旧语义
        let dir = tempfile::tempdir().unwrap();
        let legacy = state();
        *legacy.journal.lock().unwrap() = Some(Journal::open(&dir.path().join("code")).unwrap());
        legacy.set_phase(ServerPhase::Running);
        // 未标记：旧语义放行
        assert!(
            legacy
                .try_accept_deploy_with_id(request(), "legacy-ok".into())
                .is_ok()
        );
        // 标记后无 kernel：可信状态不可读，拒绝
        let dir2 = tempfile::tempdir().unwrap();
        let blocked = state();
        *blocked.journal.lock().unwrap() = Some(Journal::open(&dir2.path().join("code")).unwrap());
        blocked.set_phase(ServerPhase::Running);
        blocked.mark_kernel_required();
        assert!(matches!(
            blocked.try_accept_deploy_with_id(request(), "blocked".into()),
            Err(AdmissionError::Busy(_))
        ));
    }

    #[tokio::test]
    async fn orchestrate_source_records_request_dev_profile() {
        // R08：Source 形态操作的 dev profile 随派发传递并记录——编排引擎
        // 据此选择生效命令（devrun 优先），不再读 serve 进程 env 猜测
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Start,
                "op-prof",
                0,
            ))
            .await
            .expect("admit");
        assert_eq!(state.take_pending_dev_profile(), None, "未编排前无记录");
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-prof".into(),
                dev_profile: true,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Existing));
        assert_eq!(
            state.take_pending_dev_profile(),
            Some(true),
            "Source 形态必须记录 dev profile 供编排消费"
        );
        // 一次性消费：取后清空
        assert_eq!(state.take_pending_dev_profile(), None);
    }

    #[tokio::test]
    async fn cancelled_queued_orchestration_settles_without_dispatching_stop() {
        // R04：排队期取消的启动/重启——按自身 ID 收束 Cancelled，返回 Settled
        //（不派发 StopBusiness：那会执行无关业务停止并用 Succeeded 覆盖）
        let dir = tempfile::tempdir().unwrap();
        let state = state();
        let kernel = kernel_for(dir.path());
        state.set_runtime_kernel(kernel.clone());
        kernel
            .admit(runtime_request(
                shared_types::RuntimeOperationKind::Restart,
                "op-queued",
                0,
            ))
            .await
            .expect("admit");
        kernel.request_cancel("op-queued").await.unwrap();
        let action = settle_control_signal(
            &state,
            ControlSignal::OrchestrateSource {
                operation_id: "op-queued".into(),
                dev_profile: true,
            },
        )
        .await;
        assert!(matches!(action, InitialAction::Settled));
        assert_eq!(state.current_runtime_operation(), None);
        let view = kernel.get("op-queued").await.unwrap().expect("record");
        assert_eq!(view.state, shared_types::RuntimeOperationState::Cancelled);
    }
}
