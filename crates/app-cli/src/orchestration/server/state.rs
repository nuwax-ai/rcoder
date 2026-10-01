use super::*;

/// server 全局状态（api 层与主循环共享；读多写少，std RwLock 短临界区不跨 await）。
pub struct ServerState {
    pub(super) control_token: std::sync::OnceLock<String>,
    pub(super) execution_project: std::sync::OnceLock<std::path::PathBuf>,
    /// Immutable startup workspace for legacy URL receipts without a target tag.
    pub(super) owner_execution_workspace: std::sync::OnceLock<std::path::PathBuf>,
    pub(super) admission: std::sync::Mutex<()>,
    pub(super) accepting: std::sync::atomic::AtomicBool,
    pub(super) auxiliary_writers: std::sync::atomic::AtomicUsize,
    pub(super) shutdown_unconfirmed: std::sync::atomic::AtomicBool,
    // High-water marks retain the previous generation's stop requirements while
    // a replacement release is being prepared/published.
    pub(super) shutdown_grace_seconds: std::sync::atomic::AtomicU64,
    pub(super) shutdown_group_count: std::sync::atomic::AtomicU64,
    /// 启动恢复未完成（P1-01）：API 先 bind 后、恢复完成前，写端点与 /ready
    /// 就绪判定被门控——恢复期不受理运行态变更、不以 Idle 语义应答探针。
    pub(super) initializing: std::sync::atomic::AtomicBool,
    pub(super) preparations: Arc<preparation::Preparations>,
    pub(super) journal: std::sync::Mutex<Option<Journal>>,
    /// Legacy run/tests have no durable journal; preserve same-process replays.
    pub(super) volatile_deploy_replays: std::sync::Mutex<super::deploy_replay::History>,
    /// 统一 owner（recovery v2）：业务会话代次 id。跨会话可重置（RwLock），
    /// 读取方经 [`ServerState::generation_value`]。
    pub(super) generation: RwLock<String>,
    pub(super) phase: RwLock<ServerPhase>,
    pub(super) release: RwLock<Option<ReleaseLock>>,
    pub(super) ready: RuntimeStatusService,
    /// 最近一次部署的进度快照（/v1/deploy/status 消费）。
    pub(super) deploy_status: RwLock<DeployStatus>,
    /// 热部署受理通道（api 端点 → 主循环）。
    pub(super) deploy_tx: tokio::sync::mpsc::UnboundedSender<DeployRequest>,
    pub(super) deploy_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<DeployRequest>>,
    /// 统一 owner：跨业务会话可续期（会话结束即换新），避免一次取消永久
    /// 卡死后续会话。访问经 [`ServerState::cancel_token`] 系列方法。
    pub(super) cancel: RwLock<CancellationToken>,
    /// 日志布局（跟随服务托管引擎；serve 探测后设置，legacy 默认 Builtin）。
    pub(super) log_layout: RwLock<LogLayout>,
    /// 运行操作内核槽位（阶段二：serve 在 ownership 认领后注入；legacy 形态
    /// 恒 None——api 层 /v1/runtime/* 相应 503）。
    pub(super) runtime_kernel: std::sync::OnceLock<Arc<crate::runtime_kernel::RuntimeKernel>>,
    /// R05：启动路径是否**尝试过**装配运行内核。true 且 runtime_kernel 为
    /// None = 状态根打开失败（可信状态不可读）——所有写入口 fail-closed；
    /// false = 从未尝试（测试/无内核上下文）——保持旧语义。
    pub(super) kernel_required: std::sync::atomic::AtomicBool,
    /// V04：server 级恢复门禁——运行操作**终态持久化失败**（结果未知）时
    /// 挂起：保留执行身份、关闭部署受理、压低 ready，直至进程重启由内核
    /// 恢复裁决。未知结果标记只升不降；缺少脱敏凭据的独立标记可经
    /// 已确认 Source 操作的显式凭据受理清除。
    // Bit 0: missing redacted credentials; bit 1: other uncertain state.
    pub(super) runtime_recovery_hold: std::sync::atomic::AtomicU8,
    /// Identity of the request temporarily consuming a credentials-only hold.
    pub(super) credential_recovery_operation: std::sync::Mutex<Option<String>>,
    /// R08：当前运行操作的 dev profile（None = 操作未指定，legacy 直跑/
    /// env 兜底）。编排生效命令选择的显式依据。
    pub(super) pending_dev_profile: std::sync::Mutex<Option<bool>>,
    /// Profile and workspace actually selected for the current orchestration.
    pub(super) proxy_context: RwLock<Option<crate::proxy::compiler::RuntimeProxyContext>>,
    /// R08：每操作 PG 凭据槽（settle 写入，supervisor spawn 取走）。
    pub(super) pending_run_config: std::sync::Mutex<Option<shared_types::StartPgCredential>>,
    /// 运行控制信号通道（源码编排/停止业务——api → 主循环；与部署通道并行）。
    pub(super) control_tx: tokio::sync::mpsc::UnboundedSender<ControlSignal>,
    pub(super) control_rx: tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<ControlSignal>>,
    pub(super) supervision_driver_started: std::sync::atomic::AtomicBool,
    pub(super) supervision_probe_tx: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
    pub(super) supervision_probe_rx:
        tokio::sync::Mutex<tokio::sync::mpsc::Receiver<tokio::sync::oneshot::Sender<()>>>,
    /// 当前执行中的运行操作 ID（dispatch 写入，主循环边界收束）。
    pub(super) current_runtime_operation: RwLock<Option<String>>,
    /// 统一 owner（recovery v2）：本业务会话是否允许消费一次性部署声明
    /// env（APP_DEPLOY_*）。恢复式重启必须为 false——进程内无法像进程模式
    /// 那样在派生时剥除 env，以该标志等效门禁。
    pub(super) deploy_inputs_eligible: std::sync::atomic::AtomicBool,
    /// 统一 owner：业务重启通知（运行操作受理后，若当前无业务会话在跑，
    /// 经此触发会话重建）。由 owner 装配时注入。
    pub(super) business_relaunch: std::sync::OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// R3：原生会话正处启动恢复活跃相位（Reconciling/Starting/Stopping/
    /// CleanupPending）。管理查询门控 = initializing ∨ 本位（围栏清理期
    /// deploy/status 503；降级驻留 RecoveryRequired 不阻塞——证据可查）。
    pub(super) business_recovery_active: std::sync::atomic::AtomicBool,
}

pub(crate) struct AuxiliaryWriter<'a> {
    pub(super) state: &'a ServerState,
    pub(super) confirmed: bool,
}

/// Restores the credential fence unless a replacement request was handed to
/// the execution loop. Unknown-state bits are never cleared or overwritten.
pub(super) struct CredentialAdmission<'a> {
    pub(super) state: &'a ServerState,
    pub(super) operation_id: Option<String>,
}

impl Drop for CredentialAdmission<'_> {
    fn drop(&mut self) {
        if let Some(operation_id) = &self.operation_id {
            self.state.settle_credential_recovery(operation_id, false);
        }
    }
}
impl AuxiliaryWriter<'_> {
    pub(crate) fn confirm(&mut self) {
        self.confirmed = true;
    }
}
impl Drop for AuxiliaryWriter<'_> {
    fn drop(&mut self) {
        if !self.confirmed {
            self.state
                .runtime_recovery_hold
                .fetch_or(2, std::sync::atomic::Ordering::AcqRel);
        }
        self.state
            .auxiliary_writers
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
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
        /// R08：每操作 PG 凭据（注入服务 env；不落盘不进日志）
        pg: Option<shared_types::StartPgCredential>,
    },
    /// 停止业务服务（保持管理面）。
    StopBusiness { operation_id: String },
}

#[async_trait::async_trait]
impl runtime_supervisor::WorkerControl for ServerState {
    fn ready(&self) -> bool {
        !self.initializing()
    }
    fn shutdown_grace(&self) -> std::time::Duration {
        self.shutdown_budget(matches!(self.log_layout(), LogLayout::Supervisord))
    }
    async fn probe(&self) -> Result<()> {
        {
            let _guard = self
                .admission
                .try_lock()
                .map_err(|_| anyhow::anyhow!("runtime admission is not responsive"))?;
        }
        if self
            .supervision_driver_started
            .load(std::sync::atomic::Ordering::Acquire)
            && self.accepting.load(std::sync::atomic::Ordering::Acquire)
        {
            let (acknowledge, response) = tokio::sync::oneshot::channel();
            self.supervision_probe_tx
                .try_send(acknowledge)
                .map_err(|_| anyhow::anyhow!("runtime control loop is not responsive"))?;
            response.await.context("runtime control loop exited")?;
        }
        Ok(())
    }
    async fn shutdown(&self) -> Result<()> {
        // Physical shutdown cannot wait forever for a business admission lock.
        // The retained driver handle or an external supervisor owns the
        // precise termination path if this hangs.
        self.trigger_cancel();
        let _guard = self
            .admission
            .try_lock()
            .map_err(|_| anyhow::anyhow!("runtime admission is busy during shutdown"))?;
        self.accepting
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(())
    }
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

impl ServerState {
    /// 当前业务会话代次（快照读取）。
    pub(crate) fn generation_value(&self) -> String {
        self.generation
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// 覆写会话代次（journal 回执延续；统一 owner 会话以启动代次为准）。
    pub(crate) fn set_generation(&self, generation: String) {
        *self
            .generation
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = generation;
    }

    /// 当前取消令牌（克隆共享同一来源；跨 await 持有请先 clone）。
    pub(crate) fn cancel_token(&self) -> CancellationToken {
        self.cancel
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn cancel_child_token(&self) -> CancellationToken {
        self.cancel_token().child_token()
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancel_token().is_cancelled()
    }

    pub(crate) fn trigger_cancel(&self) {
        self.cancel
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .cancel();
    }

    /// 换新取消令牌（业务会话 re-arm 专用；须在 admission 锁内调用）。
    pub(super) fn renew_cancel_locked(&self) {
        *self
            .cancel
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = CancellationToken::new();
    }

    /// 统一 owner：本会话是否允许消费一次性部署声明 env。
    pub(crate) fn deploy_inputs_eligible(&self) -> bool {
        self.deploy_inputs_eligible
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// R3：部署状态查询的启动恢复门（业务会话恢复活跃期）。
    pub(crate) fn business_recovery_active(&self) -> bool {
        self.business_recovery_active
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// 统一 owner：通知 owner 会话重建业务（运行操作已受理但无会话在跑）。
    pub(crate) fn request_business_relaunch(&self) {
        if let Some(notify) = self.business_relaunch.get() {
            notify();
        }
    }
}

impl ServerPhase {
    pub fn as_str(&self) -> &'static str {
        // wire 值单一事实源在 shared_types（消费方 app_manager 同枚举判据）
        AppCliDeployPhase::from(self).as_str()
    }

    /// /ready 判定（Idle=基础设施就绪；Running=bridge readiness；其余摘流）。
    pub(super) fn readiness_ok(&self, service_ready: bool) -> bool {
        match self {
            ServerPhase::Idle => true,
            ServerPhase::Running => service_ready,
            _ => false,
        }
    }

    /// 热部署可受理的相位（进行中拒绝，防双部署竞争）。
    pub(super) fn accepts_deploy(&self) -> bool {
        !matches!(self, ServerPhase::Deploying | ServerPhase::Orchestrating)
    }
}

/// A persisted internal execution target, never an arbitrary client path.
#[derive(Debug, Clone, Copy, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExecutionTarget {
    Source,
    ProjectRun,
}

/// /v1/deploy 受理请求（api 端点反序列化后转发主循环）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DeployRequest {
    #[serde(skip)]
    pub(crate) runtime_operation_id: Option<String>,
    pub url: String,
    pub release_id: String,
    pub sha256: Option<String>,
    /// R03：登记的本地构建制品（共享卷 `builds/` 目录的 zip）——跳过网络
    /// 下载，直接校验/解压；legacy `/v1/deploy`（URL）为 None。
    pub local_path: Option<std::path::PathBuf>,
    #[serde(default)]
    pub(crate) execution_target: Option<ExecutionTarget>,
    #[serde(default, serialize_with = "serialize_redacted_run_pg")]
    pub(crate) run_pg: Option<shared_types::StartPgCredential>,
}

// Retain only the fact that explicit credentials were required. Recovery must
// never confuse a redacted request with an environment-only request.
pub(super) fn serialize_redacted_run_pg<S: serde::Serializer>(
    pg: &Option<shared_types::StartPgCredential>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    use serde::Serialize as _;
    let redacted = pg.as_ref().map(|pg| shared_types::StartPgCredential {
        username: pg.username.clone(),
        password: String::new(),
    });
    redacted.serialize(serializer)
}

#[derive(Debug)]
pub(crate) enum AdmissionError {
    Busy(String),
    Conflict(String),
    Failed(String),
}
impl std::fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Busy(message) | Self::Conflict(message) | Self::Failed(message) => {
                f.write_str(message)
            }
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
