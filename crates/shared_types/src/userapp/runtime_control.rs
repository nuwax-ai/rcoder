//! UserApp 运行态单一所有者协议契约（阶段二，specs/userapp-runtime-ownership）。
//!
//! app-cli `serve` 是唯一的运行态所有者：start/restart/deploy/stop 全部经
//! `/v1/runtime/operations` 受理、持久化、单 worker 串行执行。本模块是
//! rcoder（平台侧）与 app-cli（所有者侧）之间的 wire 单一事实源；身份、
//! 操作、事件、错误四族类型集中于此，配套 utoipa OpenAPI 完整描述。
//!
//! 关键身份二分（spec §3.1）：
//! - `runtime_instance_id`：进程每次启动新生成，旧实例请求一律不修改新实例；
//! - `deployment_generation_id`：持久运行代次，跨正常重建延续。
//! 两者不得合并。

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// 运行控制协议版本（identity.capabilities 探测用；v5 = 阶段二统一运行操作内核）。
pub const RUNTIME_CONTROL_PROTOCOL_VERSION: u32 = 5;

/// 受理冲突：同 operation_id 但 request_digest 不同（HTTP 409）。
pub const ERR_OPERATION_ID_CONFLICT: &str = "ERR_OPERATION_ID_CONFLICT";
/// 并发冲突：已有 active 操作进行中（HTTP 409，响应带进行中操作 ID）。
pub const ERR_OPERATION_IN_PROGRESS: &str = "ERR_OPERATION_IN_PROGRESS";
/// 身份不符：expected_runtime_instance_id 与当前实例不一致（HTTP 409）。
pub const ERR_RUNTIME_INSTANCE_MISMATCH: &str = "ERR_RUNTIME_INSTANCE_MISMATCH";
/// 修订过期：expected_revision 已被 stop/其他操作推进（HTTP 409）。
pub const ERR_REVISION_MISMATCH: &str = "ERR_REVISION_MISMATCH";
/// workspace 不符：请求 workspace_id 与所有者绑定的工作区不一致（HTTP 409）。
pub const ERR_WORKSPACE_MISMATCH: &str = "ERR_WORKSPACE_MISMATCH";
/// pending stop 屏障：已有停止意图待执行，其他修改被拒（HTTP 409）。
pub const ERR_STOP_PENDING: &str = "ERR_STOP_PENDING";
/// 恢复保护：上次执行结果未知/切换中断，未恢复前拒绝新副作用（HTTP 409）。
pub const ERR_RECOVERY_REQUIRED: &str = "ERR_RECOVERY_REQUIRED";
/// 能力缺失/协议过旧：旧客户端调新协议或反之（HTTP 400）。
pub const ERR_PROTOCOL_UNSUPPORTED: &str = "ERR_PROTOCOL_UNSUPPORTED";

/// 操作种类。`Deploy` 显式替换制品；`Start` 是确保运行（已运行保持现版本）；
/// `Restart` 按平台构建策略完成后重新编排；`Stop` 是持久化停止意图。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOperationKind {
    Start,
    Restart,
    Deploy,
    Stop,
}

impl RuntimeOperationKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            RuntimeOperationKind::Start => "start",
            RuntimeOperationKind::Restart => "restart",
            RuntimeOperationKind::Deploy => "deploy",
            RuntimeOperationKind::Stop => "stop",
        }
    }
}

impl std::fmt::Display for RuntimeOperationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 运行配置档：source（源码根 + devrun/run 命令）或 artifact（受管制品输入）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "profile", content = "input")]
pub enum RunProfileInput {
    /// 源码模式：serve 使用源码根；devrun 优先、run 兜底。
    Source {
        /// 逻辑 workspace 标识（所有者侧规范化 source_root 绑定）。
        workspace_id: String,
    },
    /// 制品模式：受限 artifact_id（登记的构建输出）或生产 URL。
    Artifact { artifact: ArtifactInput },
}

/// 制品输入适配（plan §3.4）：本地受限解析器与 URL 下载共用准备/激活内核。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case", tag = "source", content = "value")]
pub enum ArtifactInput {
    /// 登记的本地构建输出标识（不暴露任意路径激活）。
    ArtifactId { artifact_id: String },
    /// 生产发布 URL（下载后 sha256 校验，复用既有部署准备链）。
    Url { url: String, sha256: Option<String> },
}

/// 运行操作受理请求（`POST /v1/runtime/operations`）。
///
/// 幂等键 = `operation_id` + `request_digest`：平台生成的 operation_id 在
/// 网络重试前固定，不每次重发换 UUID。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RuntimeOperationRequest {
    /// 平台侧生成的操作幂等 ID（重试不变）。
    pub operation_id: String,
    /// 调用方观察到的运行实例 ID；与当前不符即拒（防旧实例迟到写入）。
    pub expected_runtime_instance_id: String,
    /// 调用方观察到的修订号；stop/其他操作推进后旧请求被拒。
    pub expected_revision: u64,
    /// 逻辑 workspace 标识（source 与 artifact 均必填；所有者核验绑定）。
    pub workspace_id: String,
    pub kind: RuntimeOperationKind,
    pub profile: RunProfileInput,
    /// 可选调用方请求标识（诊断/日志关联；不参与幂等摘要）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_context: Option<String>,
}

/// 所有者身份视图（`GET /v1/runtime/identity`）。不回显 secrets。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RuntimeIdentityView {
    /// 平台注入的应用 ID。
    pub application_id: String,
    /// 服务族（userapp dev/prod）。
    pub service_family: String,
    /// 所有者绑定的逻辑 workspace ID。
    pub workspace_id: String,
    /// 规范化 source_root（身份绑定；别名不形成独立锁域）。
    pub source_root: String,
    /// 进程本次启动的运行实例 ID。
    pub runtime_instance_id: String,
    /// 持久运行代次（跨正常重建延续）。
    pub deployment_generation_id: String,
    pub protocol_version: u32,
    /// 能力声明（探测旧/新协议客户端用）。
    pub capabilities: Vec<String>,
}

/// 期望运行态（desired）与观测健康（observed）分离（spec §5）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    Running,
    Stopped,
}

impl DesiredState {
    pub const fn as_str(self) -> &'static str {
        match self {
            DesiredState::Running => "running",
            DesiredState::Stopped => "stopped",
        }
    }
}

/// 观测运行健康（管理进程活着 ≠ 业务就绪）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ObservedHealth {
    /// 未知（启动中/无观测数据）。
    Unknown,
    Ready,
    Degraded,
    Stopped,
}

/// 运行状态视图（`GET /v1/runtime/status`）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RuntimeStatusView {
    pub desired: DesiredState,
    pub observed: ObservedHealth,
    /// 当前有效目标（active version；与新构建版本分离）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_target: Option<String>,
    /// 修订号：stop/受理成功推进；旧构建提交按此拒绝。
    pub revision: u64,
    /// 进行中操作（有即拒绝新 start/restart/deploy）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_operation_id: Option<String>,
    /// 恢复保护中（上次结果未知；写操作被拒直至恢复）。
    pub recovery_protection: bool,
    pub runtime_instance_id: String,
}

/// 操作执行状态（正常路径 Accepted → Preparing → Stopping → Activating
/// → Starting → Succeeded；异常路径进 Failed/Cancelled/RecoveryRequired）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeOperationState {
    /// 已持久化受理（尚未开始执行）。
    Accepted,
    Preparing,
    Stopping,
    Activating,
    Starting,
    Succeeded,
    Failed,
    /// 调用方取消（不等于 stop；已成功操作取消返回既有结果）。
    Cancelled,
    /// 结果未知/切换中断/停止未确认——保持互斥与恢复保护。
    RecoveryRequired,
}

impl RuntimeOperationState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            RuntimeOperationState::Succeeded
                | RuntimeOperationState::Failed
                | RuntimeOperationState::Cancelled
        )
    }
}

/// 操作记录视图（`GET /v1/runtime/operations/{id}`）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RuntimeOperationView {
    pub operation_id: String,
    pub kind: RuntimeOperationKind,
    pub state: RuntimeOperationState,
    /// 受理时刻记录的请求摘要（幂等重放比对）。
    pub request_digest: String,
    pub revision: u64,
    pub runtime_instance_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// 终态证据（阶段、最后进度、清理结果——启动失败诊断，spec §4）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<RuntimeFailureDetail>,
}

/// 启动失败结构化诊断（不含 secrets）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RuntimeFailureDetail {
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// 脱敏 stderr/日志尾部（有界）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    /// 清理结果：未确认时后续启动被阻止。
    pub cleanup_confirmed: bool,
}

/// 有序运行事件（`/v1/runtime/operations/{id}/events?after_seq=N` 重放单位）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RuntimeEventRecord {
    pub operation_id: String,
    /// 每操作单序列，从 1 起，先落盘再发布。
    pub sequence: u64,
    pub runtime_instance_id: String,
    pub stage: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// 兼容旧 UI 的事件名载荷（service_starting/service_start_ok/
    /// service_start_fail/Completed/Failed 等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
}

/// 受理响应（HTTP 202）。
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct RuntimeOperationAccepted {
    pub operation_id: String,
    pub state: RuntimeOperationState,
    /// 重放/查询地址。
    pub poll: String,
}

/// 请求摘要（服务端规范化后计算；同 ID 异摘要 → 409 OPERATION_ID_CONFLICT）。
///
/// 规范化规则：kind/profile/expected_revision/workspace_id 参与摘要；
/// operation_id 本身与 request_context 不参与（幂等键与摘要是两个概念）。
pub fn runtime_request_digest(request: &RuntimeOperationRequest) -> Result<String, String> {
    let canonical = serde_json::json!({
        "v": 1,
        "kind": request.kind,
        "profile": request.profile,
        "expected_revision": request.expected_revision,
        "workspace_id": request.workspace_id,
    });
    let encoded = serde_json::to_string(&canonical).map_err(|e| format!("encode digest: {e}"))?;
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(encoded.as_bytes());
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// 校验受理请求基础字段（identifier 预算与结构）。
pub fn validate_runtime_operation_request(request: &RuntimeOperationRequest) -> Result<(), String> {
    crate::validate_identifier(&request.operation_id, "operation_id")?;
    crate::validate_identifier(&request.workspace_id, "workspace_id")?;
    if request.expected_runtime_instance_id.trim().is_empty() {
        return Err("expected_runtime_instance_id must not be empty".into());
    }
    if let RunProfileInput::Artifact {
        artifact: ArtifactInput::ArtifactId { artifact_id },
    } = &request.profile
    {
        crate::validate_identifier(artifact_id, "artifact_id")?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> RuntimeOperationRequest {
        RuntimeOperationRequest {
            operation_id: "op-123".into(),
            expected_runtime_instance_id: "instance-a".into(),
            expected_revision: 7,
            workspace_id: "ws-1".into(),
            kind: RuntimeOperationKind::Restart,
            profile: RunProfileInput::Source {
                workspace_id: "ws-1".into(),
            },
            request_context: None,
        }
    }

    #[test]
    fn digest_is_stable_and_ignores_diagnostic_fields() {
        let base = runtime_request_digest(&sample_request()).expect("digest");
        let mut reissued = sample_request();
        reissued.request_context = Some("retry after network timeout".into());
        assert_eq!(base, runtime_request_digest(&reissued).expect("digest"));
    }

    #[test]
    fn digest_changes_with_kind_revision_and_profile() {
        let base = runtime_request_digest(&sample_request()).expect("digest");
        let mut other = sample_request();
        other.kind = RuntimeOperationKind::Stop;
        assert_ne!(base, runtime_request_digest(&other).expect("digest"));
        let mut rev = sample_request();
        rev.expected_revision = 8;
        assert_ne!(base, runtime_request_digest(&rev).expect("digest"));
        let mut artifact = sample_request();
        artifact.profile = RunProfileInput::Artifact {
            artifact: ArtifactInput::ArtifactId {
                artifact_id: "art-1".into(),
            },
        };
        assert_ne!(base, runtime_request_digest(&artifact).expect("digest"));
    }

    #[test]
    fn digest_ignores_operation_id_so_same_intent_replays() {
        // 同意图换 operation_id 是不同受理（新操作），摘要不含 ID 本身
        let mut other = sample_request();
        other.operation_id = "op-456".into();
        assert_eq!(
            runtime_request_digest(&sample_request()).expect("digest"),
            runtime_request_digest(&other).expect("digest"),
        );
    }

    #[test]
    fn validation_rejects_bad_identifiers() {
        let mut request = sample_request();
        request.operation_id = "../escape".into();
        assert!(validate_runtime_operation_request(&request).is_err());
        let mut empty_instance = sample_request();
        empty_instance.expected_runtime_instance_id = "  ".into();
        assert!(validate_runtime_operation_request(&empty_instance).is_err());
    }

    #[test]
    fn wire_kinds_use_snake_case() {
        let encoded = serde_json::to_string(&RuntimeOperationKind::Deploy).expect("json");
        assert_eq!(encoded, "\"deploy\"");
        let desired = serde_json::to_string(&DesiredState::Stopped).expect("json");
        assert_eq!(desired, "\"stopped\"");
        let state = serde_json::to_string(&RuntimeOperationState::RecoveryRequired).expect("json");
        assert_eq!(state, "\"recovery_required\"");
    }
}
