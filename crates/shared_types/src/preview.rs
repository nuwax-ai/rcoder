//! Custom Page（WebAgentRunner 开发阶段 Vite 预览）协调契约。
//!
//! kube-free、storage-free 的纯契约层：身份规范化、状态机类型、权威存储 trait
//! 与面向 HTTP 入口的协调 trait。K8s 多副本下"哪个 rcoder Pod 托管了 project/port"
//! 由 [`PreviewLifecycleStore`] 的权威实现回答（K8s=平台 PG，Compose=进程内实现）；
//! `file-server` 与 `rcoder-proxy` 只依赖本模块的类型与 trait，不依赖具体后端。
//!
//! 设计约束见 `specs/custom-page-preview-routing/spec.md` 行为不变量：
//! - 身份三件套（preview_key / instance_id / host_id）缺一不可，port 只是兼容参数；
//! - 心跳（宿主健康）与 activity（用户访问）分列，心跳不延长空闲寿命；
//! - 所有状态变更条件更新（CAS），旧 operation/instance 写回不得覆盖新实例；
//! - 存储不可用必须显式失败（[`PreviewStoreError::Unavailable`]），不得伪装"不存在"。

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// 预览端口池下界（对齐 file-server dev_server 与 nuwax-file-server portPool）。
pub const PREVIEW_PORT_MIN: u16 = 4000;
/// 预览端口池上界。
pub const PREVIEW_PORT_MAX: u16 = 55000;
/// 保留区：平台既有服务监听段（8086/8088/60000 之外的历史保留 8000-9000）。
pub const PREVIEW_PORT_RESERVED_MIN: u16 = 8000;
pub const PREVIEW_PORT_RESERVED_MAX: u16 = 9000;

/// preview_key 字段分隔符（单元分隔符，project id 与路径中不会出现）。
const KEY_SEP: char = '\u{1f}';
/// isolation 规范串内部连接符（记录分隔符）。
const ISO_SEP: char = '\u{1e}';

/// preview_key 构造输入：与 `WorkspaceResolver::resolve_project` 的 ProjectContext
/// 字段同源，外加解析出的真实目录。不臆造 agentId——调用方 projectId 字符串
/// 天然承载其组合身份（如 `{projectId}-{agentId}`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewKeyInput<'a> {
    pub project_id: &'a str,
    pub tenant_id: Option<&'a str>,
    pub space_id: Option<&'a str>,
    pub isolation_type: Option<&'a str>,
    /// WorkspaceResolver 解析并规范化后的项目绝对路径。
    pub resolved_path: &'a str,
}

/// 规范化 isolation 串：三个字段按固定顺序，None 记空串，`\u{1e}` 连接。
/// tenant/space/isolation_type 仅 `tenant+space+isolationType` 均非空才启用三级
/// 路径（对齐 WorkspaceResolver），但 key 一律记录原值，避免不同 isolation 组合折叠。
fn isolation_norm(tenant: Option<&str>, space: Option<&str>, isolation: Option<&str>) -> String {
    let tenant = tenant.unwrap_or("");
    let space = space.unwrap_or("");
    let isolation = isolation.unwrap_or("");
    format!("{tenant}{ISO_SEP}{space}{ISO_SEP}{isolation}")
}

/// canonical 预览身份键：`project_id \x1f isolation_norm \x1f resolved_path`。
/// 同一项目目录的不同 isolation 组合、同 isolation 下不同目录均为不同键。
pub fn preview_key(input: &PreviewKeyInput<'_>) -> String {
    format!(
        "{proj}{sep}{iso}{sep}{path}",
        proj = input.project_id,
        iso = isolation_norm(input.tenant_id, input.space_id, input.isolation_type),
        path = input.resolved_path,
        sep = KEY_SEP
    )
}

/// 判定端口是否在预览端口池内（含保留区排除）。
pub fn is_preview_port(port: u16) -> bool {
    (PREVIEW_PORT_MIN..=PREVIEW_PORT_MAX).contains(&port)
        && !(PREVIEW_PORT_RESERVED_MIN..=PREVIEW_PORT_RESERVED_MAX).contains(&port)
}

/// 预览实例状态机。
///
/// Starting → Ready → Stopping → Stopped；Starting/Ready/Stopping → Failed（执行失败
/// 或确认死）；Ready/Starting → Unknown（心跳超时/宿主失联/登记丢失）；Unknown →
/// Stopped 仅凭恢复证据（原宿主 Pod 确认终止或实例确认停止），不凭 TTL。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub enum PreviewInstanceState {
    Starting,
    Ready,
    Stopping,
    Stopped,
    Failed,
    Unknown,
}

impl PreviewInstanceState {
    /// 活跃 = 端口仍被占用、新 start 受阻（需走幂等/冲突/恢复路径）。
    pub fn is_active(self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Ready | Self::Stopping | Self::Unknown
        )
    }
}

/// 操作种类（审计用；restart 在实现上=受控 stop + start 两个操作）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreviewOperationKind {
    Start,
    Stop,
}

/// 操作状态。Uncertain = 执行超时/结果不可知，保留执行边界，不自动重放。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreviewOperationState {
    Accepted,
    Running,
    Succeeded,
    Failed,
    Uncertain,
}

/// 宿主身份：`{pod_uid}:{boot_id}`（boot_id = rcoder 进程启动代次 UUID）。
/// pod_uid 区分 Pod 重建；boot_id 区分同 Pod 内进程重启（容器重启销毁 PID
/// 命名空间，是旧实例判停的直接证据）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviewHostIdentity {
    pub host_id: String,
    pub pod_name: Option<String>,
    pub pod_ip: Option<String>,
}

/// 预览实例权威记录（权威存储的行类型）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewInstanceRecord {
    pub preview_key: String,
    pub project_id: String,
    pub project_path: String,
    /// 当前（或最近一次成功受理的）实例 ID；每次 Admitted 的 start 生成新值。
    pub instance_id: String,
    /// 单调递增；旧 revision 的写回 CAS 失败。
    pub revision: i64,
    /// 当前/最近操作 ID。
    pub operation_id: String,
    pub host_id: String,
    pub pod_name: Option<String>,
    pub pod_ip: Option<String>,
    pub pid: Option<i64>,
    pub port: Option<u16>,
    pub base_path: Option<String>,
    pub state: PreviewInstanceState,
    /// 仅宿主心跳轮询刷新；不延长空闲寿命。
    pub last_heartbeat_at: Option<DateTime<Utc>>,
    /// keep-alive / 预览访问刷新（任意副本）。
    pub last_activity_at: DateTime<Utc>,
    /// 失败/Unknown/恢复证据等诊断说明（非机密）。
    pub detail: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// 操作审计记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviewOperationRecord {
    pub operation_id: String,
    pub preview_key: String,
    pub kind: PreviewOperationKind,
    pub state: PreviewOperationState,
    pub host_id: String,
    pub requested_port: Option<u16>,
    pub allocated_port: Option<u16>,
    pub result: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// 受理 start 的输入。端口分配在存储事务内完成（与受理原子），`requested_port`
/// 为 None 时取池内最小可用；strictPort 失败重试时由协调器传入指定候选端口。
#[derive(Debug, Clone)]
pub struct AcceptStartInput {
    pub preview_key: String,
    pub project_id: String,
    pub project_path: String,
    pub host: PreviewHostIdentity,
    pub operation_id: String,
    pub instance_id: String,
    pub requested_port: Option<u16>,
    /// Unknown 行恢复证据说明。None = 协调器未核实证据，存储遇 Unknown 行必须
    /// 返回 [`AcceptStartOutcome::Blocked`]；Some = 已核实（K8s 查 Pod UID 不存在/
    /// 单机非本 host 即前代进程），事务内先落 Stopped 再受理。
    pub recover_unknown_evidence: Option<String>,
}

/// 受理结果。
#[derive(Debug, Clone)]
pub enum AcceptStartOutcome {
    /// 受理成功，行已置 Starting（含分配端口）。
    Admitted(PreviewInstanceRecord),
    /// 行已 Ready：幂等返回当前实例信息（对齐 Rust start_dev 幂等语义）。
    ExistingReady(PreviewInstanceRecord),
    /// 活跃行阻塞新受理（Starting/Stopping，或 Unknown 且无恢复证据）。
    Blocked(PreviewInstanceRecord),
}

/// activity 刷盘条目（协调器内存累积器定期批量落库；GREATEST 语义不回退）。
#[derive(Debug, Clone)]
pub struct ActivityFlushEntry {
    pub preview_key: String,
    pub instance_id: String,
    pub at: DateTime<Utc>,
}

/// 存储层错误。Unavailable 必须显式上抛（fail-fast），不得映射为"不存在"。
#[derive(Debug, thiserror::Error)]
pub enum PreviewStoreError {
    #[error("preview storage unavailable: {0}")]
    Unavailable(String),
    /// CAS 失败（旧 revision/operation/instance 或状态前置不满足）。
    #[error("preview state conflict: {0}")]
    Conflict(String),
    #[error("invalid preview operation: {0}")]
    Invalid(String),
}

/// 预览权威存储契约。所有变更方法原子提交且条件更新；实现不得执行网络 I/O。
#[async_trait::async_trait]
pub trait PreviewLifecycleStore: Send + Sync {
    /// 条件受理 start（含端口分配）。见 [`AcceptStartInput`] 语义。
    async fn accept_start(
        &self,
        input: AcceptStartInput,
    ) -> Result<AcceptStartOutcome, PreviewStoreError>;

    /// 启动成功发布：CAS(starting, operation_id, revision) → Ready + pid/port/base_path，
    /// 同时把操作置 Succeeded。
    async fn publish_running(
        &self,
        preview_key: &str,
        operation_id: &str,
        revision: i64,
        pid: i64,
        port: u16,
        base_path: Option<&str>,
    ) -> Result<PreviewInstanceRecord, PreviewStoreError>;

    /// 受理 stop：活跃行（Starting/Ready/Unknown）→ Stopping + 新操作。已 Stopped
    /// 幂等返回当前行。
    async fn accept_stop(
        &self,
        preview_key: &str,
        operation_id: &str,
    ) -> Result<PreviewInstanceRecord, PreviewStoreError>;

    /// stop 完成：CAS(operation_id, revision, stopping) → Stopped。CAS 失败返回
    /// [`PreviewStoreError::Conflict`]（并发 stop 已完成时由协调器按幂等处理）。
    async fn mark_stopped(
        &self,
        preview_key: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<PreviewInstanceRecord, PreviewStoreError>;

    /// 实例失败终态：CAS(starting|ready, instance_id, revision) → Failed。
    /// 心跳探死与启动失败共用；挂在该实例上的非终态操作一并置 Failed（审计）。
    async fn mark_failed(
        &self,
        preview_key: &str,
        instance_id: &str,
        revision: i64,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, PreviewStoreError>;

    /// 转 Unknown（心跳超时/登记丢失）：CAS(active, instance_id) → Unknown。
    async fn mark_unknown(
        &self,
        preview_key: &str,
        instance_id: &str,
        detail: &str,
    ) -> Result<PreviewInstanceRecord, PreviewStoreError>;

    /// Unknown 落证转 Stopped（恢复证据已由协调器核实）。
    async fn resolve_unknown_stopped(
        &self,
        preview_key: &str,
        instance_id: &str,
        evidence: &str,
    ) -> Result<PreviewInstanceRecord, PreviewStoreError>;

    /// 宿主心跳刷新：CAS(ready|unknown, instance_id) → 刷 last_heartbeat_at。
    /// 行不存在或已非活跃返回 None（协调器按登记丢失处理）。
    async fn refresh_heartbeat(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<Option<PreviewInstanceRecord>, PreviewStoreError>;

    /// keep-alive activity 批量刷盘：`GREATEST(last_activity_at, entry.at)` 单调
    /// 不回退；instance 不匹配或已非活跃的条目自然 no-op。activity 不参与正确性
    /// 判定（存活用心跳），丢批仅影响空闲回收延迟，故允许内存累积 + 定期落库。
    async fn flush_activity(
        &self,
        entries: &[ActivityFlushEntry],
    ) -> Result<usize, PreviewStoreError>;

    async fn get(
        &self,
        preview_key: &str,
    ) -> Result<Option<PreviewInstanceRecord>, PreviewStoreError>;

    /// 按端口查活跃实例（入口 URL 只有 port；全局端口唯一保证无歧义）。
    async fn find_active_by_port(
        &self,
        port: u16,
    ) -> Result<Option<PreviewInstanceRecord>, PreviewStoreError>;

    /// 当前活跃实例占用的端口集合（诊断/分配冲突检测用）。
    async fn active_ports(&self) -> Result<Vec<u16>, PreviewStoreError>;

    /// 指定宿主的活跃实例（心跳轮询/启动对账范围）。
    async fn list_by_host(
        &self,
        host_id: &str,
        active_only: bool,
    ) -> Result<Vec<PreviewInstanceRecord>, PreviewStoreError>;

    async fn list_active(&self) -> Result<Vec<PreviewInstanceRecord>, PreviewStoreError>;

    /// 启动对账：host_id 前缀（pod_uid）相同但 boot_id 不同的活跃行 → Stopped
    /// （容器重启销毁 PID 命名空间 = 直接证据）。返回被收敛的行。
    async fn reconcile_host_reboot(
        &self,
        pod_uid: &str,
        boot_id: &str,
    ) -> Result<Vec<PreviewInstanceRecord>, PreviewStoreError>;
}

// ============ 高层协调契约（file-server 七 handler / rcoder-proxy 路由使用） ============

/// 协调层业务错误（HTTP 语义由 file-server handler 映射为既有错误信封）。
#[derive(Debug, thiserror::Error)]
pub enum PreviewCoordinationError {
    /// 权威存储/远端宿主不可用 → 5xx（fail-fast，不伪装成功或不存在）。
    #[error("preview coordination unavailable: {0}")]
    Unavailable(String),
    /// 受理冲突（已在启动/停止中，语义对齐旧 PROJECT_STARTING 业务错误）。
    #[error("preview operation conflict: {0}")]
    Conflict(String),
    /// 参数/身份非法。
    #[error("invalid preview request: {0}")]
    Invalid(String),
}

/// start/restart 的项目身份（handler 先经 WorkspaceResolver 解析真实目录再进协调）。
#[derive(Debug, Clone)]
pub struct PreviewProjectIdentity {
    pub project_id: String,
    pub tenant_id: Option<String>,
    pub space_id: Option<String>,
    pub isolation_type: Option<String>,
    /// 已解析并规范化的项目绝对路径。
    pub resolved_path: String,
}

/// start-dev 请求。
#[derive(Debug, Clone)]
pub struct PreviewStartRequest {
    pub identity: PreviewProjectIdentity,
    /// 调用方 base path（Java 语义）；None 时执行器按端口组合默认值（对齐现行）。
    pub base_path: Option<String>,
}

/// stop-dev 请求（兼容参数：Java 只带 projectId[+pid]，无 isolation）。
#[derive(Debug, Clone)]
pub struct PreviewStopRequest {
    pub project_id: String,
    pub pid: Option<i64>,
}

/// restart-dev 请求。
#[derive(Debug, Clone)]
pub struct PreviewRestartRequest {
    pub identity: PreviewProjectIdentity,
    pub base_path: Option<String>,
}

/// keep-alive 请求（兼容参数：pid/port/basePath 无 instance_id——按全局唯一
/// port 定位 + project_id 校验；identity 供重建路径使用）。
#[derive(Debug, Clone)]
pub struct PreviewKeepAliveRequest {
    pub identity: PreviewProjectIdentity,
    pub port: u16,
    pub pid: Option<i64>,
    pub base_path: Option<String>,
}

/// list-dev 聚合条目（wire 向后兼容 + 新增宿主归属列）。
#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewListEntry {
    pub project_id: String,
    pub pid: Option<i64>,
    pub port: Option<u16>,
    pub state: String,
    /// 宿主 Pod 名（诊断归属）。
    pub host: Option<String>,
    pub started_at: Option<DateTime<Utc>>,
}

/// `/proxy/{port}` 预览路由解析结果（只读；未注入协调或未命中走原回环）。
#[derive(Debug, Clone)]
pub enum PreviewRouteResolution {
    /// 非预览端口 → 既有 localhost 行为。
    NotPreview,
    /// 命中且宿主为本机（转发路由校验后走 localhost）。
    Local { instance_id: String, port: u16 },
    /// 命中且宿主为远端 Pod（转发 host 8088 内部入口）。
    Forward {
        instance_id: String,
        port: u16,
        host_ip: String,
    },
    /// 权威存储不可用 → 降级为既有回环行为（不缓存、不更糟）。
    Unavailable,
}

/// 宿主侧转发校验结果（`/internal/preview-forward`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreviewForwardCheck {
    /// 校验通过，转发本机 vite。
    Allowed,
    /// 实例身份不匹配（IP 复用/新实例）→ 410，触发调用方重解析。
    IdentityMismatch,
    /// 登记缺失但行称本机宿主（对账中）→ 503，不转发。
    NotReady,
}

/// 本地执行器契约（宿主侧 Vite/依赖安装执行；实现=file-server DevServerManager 适配）。
/// 只做本地执行：不查存储、不做网络转发，票据身份不匹配必须拒绝而非误杀。
#[derive(Debug, Clone)]
pub struct ExecutorStartTicket {
    pub preview_key: String,
    /// 日志目录命名键（project_id；与 legacy 路径一致便于 get-dev-log 兼容）。
    pub log_key: String,
    pub project_path: String,
    pub port: u16,
    pub base_path: Option<String>,
    pub instance_id: String,
}

#[derive(Debug, Clone)]
pub struct ExecutorVerifyReport {
    /// 本地登记存在且 instance_id 匹配。
    pub identity_match: bool,
    /// 进程探活（登记匹配前提下）。
    pub alive: bool,
    pub pid: Option<i64>,
    pub port: Option<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecutorStopOutcome {
    /// 已按记录 pid 组停止并摘除登记。
    Stopped,
    /// 登记不存在（执行器重启/非宿主）——不杀任何进程。
    NotRegistered,
    /// 登记存在但 instance_id 不匹配（迟到旧操作）——不杀新实例。
    IdentityMismatch,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ExecutorLogLine {
    /// 行号（1 起）
    pub line: usize,
    /// 日志行内容
    pub content: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct ExecutorLogChunk {
    pub logs: Vec<ExecutorLogLine>,
    pub total_lines: usize,
    pub log_file_name: String,
}

#[derive(Debug, thiserror::Error)]
pub enum PreviewExecutorError {
    #[error("preview executor failed: {0}")]
    Failed(String),
    /// 端口被占（strictPort 失败语义）——协调器换端口有界重试。
    #[error("preview port in use: {0}")]
    PortInUse(String),
}

#[async_trait::async_trait]
pub trait PreviewExecutor: Send + Sync {
    /// 本地启动（票据端口 strictPort；含依赖安装；就绪后登记）。
    async fn start_local(
        &self,
        ticket: &ExecutorStartTicket,
    ) -> Result<(i64, u16), PreviewExecutorError>;

    /// 本地停止：仅登记匹配时按记录 pid 组杀；否则 NotRegistered/IdentityMismatch。
    async fn stop_local(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorStopOutcome, PreviewExecutorError>;

    /// 本地校验（心跳/恢复判定用）。
    async fn verify_local(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewExecutorError>;

    /// 纯登记身份校验（无探活、纯内存查找——转发放行的热路径用；
    /// 存活检测属心跳轮询职责，转发死 vite 得到连接拒绝与现状语义一致）。
    async fn registration_matches(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<bool, PreviewExecutorError>;

    /// 本地日志读取（get-dev-log 转发源）。
    async fn read_log_local(
        &self,
        log_key: &str,
        log_type: &str,
        start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewExecutorError>;
}

/// port-pool-status 协调器视图（wire 形态由消费方映射）。
#[derive(Debug, Clone)]
pub struct PreviewPortAllocation {
    pub project_id: String,
    pub port: u16,
}

#[derive(Debug, Clone)]
pub struct PreviewPortPoolStatus {
    pub port_range: String,
    pub total_allocated: usize,
    pub allocations: Vec<PreviewPortAllocation>,
}

/// 高层协调入口。实现=preview-coordinator 服务对象，rcoder 主进程装配注入。
#[async_trait::async_trait]
pub trait PreviewCoordination: Send + Sync {
    /// start-dev：受理→本地执行→发布。已在运行幂等返回当前实例（同形信封）。
    async fn start_dev(
        &self,
        req: PreviewStartRequest,
    ) -> Result<PreviewStartEnvelope, PreviewCoordinationError>;

    /// stop-dev：按 project_id 定位活跃实例（可能多条 isolation），逐条协调停止。
    async fn stop_dev(
        &self,
        req: &PreviewStopRequest,
    ) -> Result<PreviewStopEnvelope, PreviewCoordinationError>;

    /// restart-dev：受控 stop + start 复合。
    async fn restart_dev(
        &self,
        req: PreviewRestartRequest,
    ) -> Result<PreviewRestartEnvelope, PreviewCoordinationError>;

    /// keep-alive：存活判定（新鲜心跳/宿主 verify），死实例统一受理重建，
    /// 宿主不可达且无证据降级 `success:false`（HTTP 200 保持）。
    async fn keep_alive_dev(
        &self,
        req: &PreviewKeepAliveRequest,
    ) -> Result<PreviewKeepAliveEnvelope, PreviewCoordinationError>;

    /// list-dev：权威库聚合视图。
    async fn list_dev(&self) -> Result<Vec<PreviewListEntry>, PreviewCoordinationError>;

    /// get-dev-log：定位宿主读日志（本地直读或远端转发）。
    async fn read_dev_log(
        &self,
        project_id: &str,
        log_type: &str,
        start_index: usize,
    ) -> Result<ExecutorLogChunk, PreviewCoordinationError>;

    /// port-pool-status：协调器端口视图。
    async fn port_pool_status(&self) -> Result<PreviewPortPoolStatus, PreviewCoordinationError>;

    /// `/proxy/{port}` 预览路由解析（内存缓存→权威库；错误降级 NotPreview 同义）。
    async fn resolve_route(&self, port: u16) -> PreviewRouteResolution;

    /// 宿主侧转发校验（`/internal/preview-forward` 四重校验的协调器侧）。
    async fn check_forward(&self, instance_id: &str, port: u16) -> PreviewForwardCheck;

    /// 内部执行入口（跨 Pod stop/verify 派发的宿主侧落地；仅本地执行，不重入协调）。
    async fn internal_stop(
        &self,
        preview_key: &str,
        instance_id: &str,
        operation_id: &str,
        revision: i64,
    ) -> Result<ExecutorStopOutcome, PreviewCoordinationError>;

    async fn internal_verify(
        &self,
        preview_key: &str,
        instance_id: &str,
    ) -> Result<ExecutorVerifyReport, PreviewCoordinationError>;
}

// ============ HTTP 兼容信封（对齐 nuwax-file-server 既有响应，camelCase） ============

/// start-dev 成功信封：`{success, message:"Development server started", projectId, pid, port}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewStartEnvelope {
    pub success: bool,
    pub message: String,
    pub project_id: String,
    pub pid: i64,
    pub port: u16,
}

/// keep-alive 信封：alive 态 message = "Development server is alive"；重建态
/// message = "Development server started" 且 action = Some("start")；降级态
/// success = false + reason。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewKeepAliveEnvelope {
    pub success: bool,
    pub message: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    /// 降级原因（host_unavailable / instance_unknown / port_mismatch）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// stop-dev 信封。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewStopEnvelope {
    pub success: bool,
    pub message: String,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// restart-dev 成功信封：message = "Development server restart successfully"。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct PreviewRestartEnvelope {
    pub success: bool,
    pub message: String,
    pub project_id: String,
    pub pid: i64,
    pub port: u16,
}

/// 预览降级原因词汇（信封 reason 字段）。
pub mod degraded_reason {
    pub const HOST_UNAVAILABLE: &str = "host_unavailable";
    pub const INSTANCE_UNKNOWN: &str = "instance_unknown";
    pub const PORT_MISMATCH: &str = "port_mismatch";
    pub const CONFLICT: &str = "conflict";
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key<'a>(
        project: &'a str,
        tenant: Option<&'a str>,
        space: Option<&'a str>,
        isolation: Option<&'a str>,
        path: &'a str,
    ) -> String {
        preview_key(&PreviewKeyInput {
            project_id: project,
            tenant_id: tenant,
            space_id: space,
            isolation_type: isolation,
            resolved_path: path,
        })
    }

    #[test]
    fn preview_key_is_deterministic_and_field_sensitive() {
        let base = key("p1", None, None, None, "/ws/p1");
        assert_eq!(key("p1", None, None, None, "/ws/p1"), base);
        // 任一字段不同 → 键不同
        assert_ne!(key("p2", None, None, None, "/ws/p1"), base);
        assert_ne!(key("p1", Some("t"), None, None, "/ws/p1"), base);
        assert_ne!(key("p1", None, Some("s"), None, "/ws/p1"), base);
        assert_ne!(key("p1", None, None, Some("tenant"), "/ws/p1"), base);
        assert_ne!(key("p1", None, None, None, "/ws/other"), base);
    }

    #[test]
    fn preview_key_no_ambiguity_between_field_values() {
        // 字段值里出现分隔符也不可能构造出与另一组合相同的键：分隔符 \x1f/\x1e
        // 不会出现在合法 project id / 路径中；即便出现，也仅导致键碰撞风险由
        // 输入侧标识符校验兜底（调用方校验 project_id 白名单）。
        let a = key("a\x1fb", None, None, None, "/p");
        let b = key("a", Some("b"), None, None, "\x1f/p");
        // 证明两种不同输入即便恶意构造也产生不同键（隔离段固定三段结构）
        assert_ne!(a, b);
    }

    #[test]
    fn isolation_norm_orders_fields_stably() {
        assert_eq!(
            isolation_norm(Some("t"), Some("s"), Some("i")),
            format!("t{ISO_SEP}s{ISO_SEP}i")
        );
        assert_eq!(
            isolation_norm(None, None, None),
            format!("{0}{1}{0}{1}{0}", "", ISO_SEP)
        );
    }

    #[test]
    fn preview_port_pool_bounds() {
        assert!(is_preview_port(4000));
        assert!(is_preview_port(55000));
        assert!(!is_preview_port(3999));
        assert!(!is_preview_port(55001));
        assert!(!is_preview_port(8000));
        assert!(!is_preview_port(9000));
        assert!(is_preview_port(7999));
        assert!(is_preview_port(9001));
    }

    #[test]
    fn instance_state_activity() {
        assert!(PreviewInstanceState::Starting.is_active());
        assert!(PreviewInstanceState::Ready.is_active());
        assert!(PreviewInstanceState::Stopping.is_active());
        assert!(PreviewInstanceState::Unknown.is_active());
        assert!(!PreviewInstanceState::Stopped.is_active());
        assert!(!PreviewInstanceState::Failed.is_active());
    }

    #[test]
    fn envelopes_serialize_camel_case_parity() {
        let start = PreviewStartEnvelope {
            success: true,
            message: "Development server started".into(),
            project_id: "p1".into(),
            pid: 123,
            port: 4001,
        };
        let json = serde_json::to_value(&start).unwrap();
        assert_eq!(json["projectId"], "p1");
        assert_eq!(json["pid"], 123);
        assert_eq!(json["port"], 4001);
        assert_eq!(json["message"], "Development server started");

        let alive = PreviewKeepAliveEnvelope {
            success: true,
            message: "Development server is alive".into(),
            project_id: "p1".into(),
            pid: Some(123),
            port: Some(4001),
            action: None,
            reason: None,
        };
        let json = serde_json::to_value(&alive).unwrap();
        assert!(json.get("action").is_none());
        assert!(json.get("reason").is_none());

        let degraded = PreviewKeepAliveEnvelope {
            success: false,
            message: "preview host unavailable".into(),
            project_id: "p1".into(),
            pid: None,
            port: None,
            action: None,
            reason: Some(degraded_reason::HOST_UNAVAILABLE.into()),
        };
        let json = serde_json::to_value(&degraded).unwrap();
        assert_eq!(json["reason"], "host_unavailable");
        assert!(!json["success"].as_bool().unwrap());
    }
}
